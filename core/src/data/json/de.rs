//! Structured serde bridge for audited structural representations only.

use super::syntax::{JsonKind, JsonMember};
use super::{JsonRef, on_json_stack};
use crate::fact::{
    Count, CountRule, DataStep, DecodeCause, DecodeFailure, DecodeLength, FiniteBitsFailure,
    JsonMapKeyFailure, JsonNumberFailure, JsonNumberFault, JsonNumberTarget, JsonShape,
    JsonShapeFailure, JsonStringFailure, UnknownField, UnknownTag,
};
use serde::de::{self, DeserializeSeed, EnumAccess, MapAccess, SeqAccess, VariantAccess, Visitor};
use serde::{Deserialize, Deserializer};
use std::borrow::Cow;
use std::fmt;

#[derive(Debug)]
enum Cause {
    Ready(Box<DecodeCause>),
    Type(JsonShape),
    Length(usize),
}

#[derive(Debug)]
struct Error {
    path: Vec<DataStep>,
    cause: Cause,
}

type Result<T> = std::result::Result<T, Error>;

impl Error {
    fn ready(cause: DecodeCause) -> Self {
        Self {
            path: Vec::new(),
            cause: Cause::Ready(Box::new(cause)),
        }
    }
    fn at(mut self, step: DataStep) -> Self {
        self.path.insert(0, step);
        self
    }
    fn finish(mut self, expected: &[JsonShape], length: Option<usize>) -> Self {
        self.cause = Cause::Ready(match self.cause {
            Cause::Ready(cause) => cause,
            Cause::Type(found) => Box::new(DecodeCause::JsonShape(JsonShapeFailure {
                expected: expected.to_vec(),
                found,
            })),
            Cause::Length(got) => Box::new(DecodeCause::Length(DecodeLength {
                expected: CountRule::Exactly(Count::from(length.expect("cardinality context"))),
                got: Count::from(got),
            })),
        });
        self
    }
    fn portable(self) -> DecodeFailure {
        let Cause::Ready(cause) = self.cause else {
            panic!("JSON bridge lost the enclosing structural expectation")
        };
        DecodeFailure {
            path: self.path,
            cause,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("structural JSON decoding failed")
    }
}
impl std::error::Error for Error {}

impl de::Error for Error {
    fn custom<T: fmt::Display>(_message: T) -> Self {
        panic!("semantic serde custom error in an unaudited JSON representation")
    }
    fn invalid_type(unexpected: de::Unexpected<'_>, _expected: &dyn de::Expected) -> Self {
        let found = match unexpected {
            de::Unexpected::Unit => JsonShape::Null,
            de::Unexpected::Bool(_) => JsonShape::Bool,
            de::Unexpected::Signed(_) | de::Unexpected::Unsigned(_) | de::Unexpected::Float(_) => {
                JsonShape::Number
            }
            de::Unexpected::Str(_) | de::Unexpected::Char(_) => JsonShape::String,
            de::Unexpected::Seq => JsonShape::Array,
            de::Unexpected::Map => JsonShape::Object,
            _ => panic!("non-JSON visitor mismatch in an unaudited JSON representation"),
        };
        Self {
            path: Vec::new(),
            cause: Cause::Type(found),
        }
    }
    /// `Finite` is the only producer: a non-finite float is its sole refusal.
    fn invalid_value(unexpected: de::Unexpected<'_>, _expected: &dyn de::Expected) -> Self {
        match unexpected {
            de::Unexpected::Float(float) if !float.is_finite() => {
                Self::ready(DecodeCause::FiniteBits(FiniteBitsFailure {
                    bits: Count::from_u64(float.to_bits()),
                }))
            }
            _ => panic!("semantic serde invalid_value in an unaudited JSON representation"),
        }
    }
    fn invalid_length(len: usize, _expected: &dyn de::Expected) -> Self {
        Self {
            path: Vec::new(),
            cause: Cause::Length(len),
        }
    }
    fn missing_field(field: &'static str) -> Self {
        Self::ready(DecodeCause::MissingField(field.into()))
    }
    fn duplicate_field(field: &'static str) -> Self {
        Self::ready(DecodeCause::DuplicateField(field.into()))
    }
    fn unknown_field(field: &str, expected: &'static [&'static str]) -> Self {
        Self::ready(DecodeCause::UnknownField(UnknownField {
            field: field.into(),
            offered: expected.iter().map(|s| (*s).into()).collect(),
        }))
    }
    fn unknown_variant(variant: &str, expected: &'static [&'static str]) -> Self {
        Self::ready(DecodeCause::UnknownTag(UnknownTag {
            tag: variant.into(),
            offered: expected.iter().map(|s| (*s).into()).collect(),
        }))
    }
}

/// Decode an audited structural raw graph.
///
/// Only use for structural raw graphs with no semantic serde validators,
/// buffered tagging, flattening, or untagged replay. Semantic serde `custom`
/// errors are not a fallback: they violate this audited structural contract.
///
/// # Errors
/// Returns a typed structural mismatch with its logical path.
pub fn decode_structural<'a, T: Deserialize<'a>>(
    value: JsonRef<'_, 'a>,
) -> std::result::Result<T, DecodeFailure> {
    T::deserialize(serde_stacker::Deserializer::new(Decoder(value))).map_err(Error::portable)
}

#[derive(Clone, Copy)]
struct Decoder<'d, 'a>(JsonRef<'d, 'a>);

fn wrong(value: JsonRef<'_, '_>, expected: &[JsonShape]) -> Error {
    Error::ready(DecodeCause::JsonShape(JsonShapeFailure {
        expected: expected.to_vec(),
        found: value.shape(),
    }))
}

fn length(expected: usize, got: usize) -> Error {
    Error::ready(DecodeCause::Length(DecodeLength {
        expected: CountRule::Exactly(Count::from(expected)),
        got: Count::from(got),
    }))
}

fn number_error(text: &str, target: JsonNumberTarget) -> Error {
    use JsonNumberTarget::{F32, F64, I128, U128};
    let cause = match target {
        F32 | F64 => JsonNumberFault::NonFinite,
        _ if text.contains(['.', 'e', 'E']) || (text == "-0" && !matches!(target, I128 | U128)) => {
            JsonNumberFault::NotIntegerRepresentation
        }
        _ => JsonNumberFault::OutOfRange,
    };
    Error::ready(DecodeCause::JsonNumber(JsonNumberFailure {
        text: text.into(),
        target,
        cause,
    }))
}

macro_rules! number {
    ($method:ident, $ty:ty, $visit:ident, $target:ident) => {
        fn $method<V: Visitor<'a>>(self, visitor: V) -> Result<V::Value> {
            let text = self
                .0
                .number()
                .ok_or_else(|| wrong(self.0, &[JsonShape::Number]))?;
            let value: $ty = serde_json::from_str(text)
                .map_err(|_| number_error(text, JsonNumberTarget::$target))?;
            visitor
                .$visit(value)
                .map_err(|e: Error| e.finish(&[JsonShape::Number], None))
        }
    };
}

impl<'a> Decoder<'_, 'a> {
    fn sequence<V: Visitor<'a>>(self, visitor: V, expected: Option<usize>) -> Result<V::Value> {
        let JsonKind::Array(items) = &self.0.node().kind else {
            return Err(wrong(self.0, &[JsonShape::Array]));
        };
        let mut access = Sequence {
            parent: self.0,
            items,
            at: 0,
        };
        let value = visitor
            .visit_seq(&mut access)
            .map_err(|e: Error| e.finish(&[JsonShape::Array], expected))?;
        if access.at != items.len() {
            return Err(length(expected.unwrap_or(access.at), items.len()));
        }
        Ok(value)
    }
    fn object<V: Visitor<'a>>(self, visitor: V) -> Result<V::Value> {
        let JsonKind::Object(members) = &self.0.node().kind else {
            return Err(wrong(self.0, &[JsonShape::Object]));
        };
        let mut access = Object {
            parent: self.0,
            members,
            at: 0,
        };
        let value = visitor
            .visit_map(&mut access)
            .map_err(|e: Error| e.finish(&[JsonShape::Object], None))?;
        assert_eq!(
            access.at,
            members.len(),
            "structural visitor must consume its object"
        );
        Ok(value)
    }
    fn text<V: Visitor<'a>>(self, visitor: V) -> Result<V::Value> {
        let JsonKind::String(text) = &self.0.node().kind else {
            return Err(wrong(self.0, &[JsonShape::String]));
        };
        visit_text(text, visitor)
    }
}

fn visit_text<'a, V: Visitor<'a>>(text: &Cow<'a, str>, visitor: V) -> Result<V::Value> {
    let result = match text {
        Cow::Borrowed(text) => visitor.visit_borrowed_str(text),
        Cow::Owned(text) => visitor.visit_str(text),
    };
    result.map_err(|e: Error| {
        if matches!(text, Cow::Owned(_)) && matches!(e.cause, Cause::Type(JsonShape::String)) {
            Error::ready(DecodeCause::JsonString(JsonStringFailure::BorrowedRequired))
        } else {
            e.finish(&[JsonShape::String], None)
        }
    })
}

impl<'a> Deserializer<'a> for Decoder<'_, 'a> {
    type Error = Error;

    fn deserialize_any<V: Visitor<'a>>(self, visitor: V) -> Result<V::Value> {
        match &self.0.node().kind {
            JsonKind::Null => self.deserialize_unit(visitor),
            JsonKind::Bool(_) => self.deserialize_bool(visitor),
            JsonKind::Number => {
                let text = self.0.number().expect("number node");
                let value: serde_json::Number = serde_json::from_str(text)
                    .map_err(|_| number_error(text, JsonNumberTarget::F64))?;
                let result = if let Some(n) = value.as_u64() {
                    visitor.visit_u64(n)
                } else if let Some(n) = value.as_i64() {
                    visitor.visit_i64(n)
                } else {
                    visitor.visit_f64(value.as_f64().expect("JSON number"))
                };
                result.map_err(|e: Error| e.finish(&[JsonShape::Number], None))
            }
            JsonKind::String(_) => self.text(visitor),
            JsonKind::Array(_) => self.sequence(visitor, None),
            JsonKind::Object(_) => self.object(visitor),
        }
    }
    fn deserialize_bool<V: Visitor<'a>>(self, visitor: V) -> Result<V::Value> {
        let value = self
            .0
            .as_bool()
            .ok_or_else(|| wrong(self.0, &[JsonShape::Bool]))?;
        visitor
            .visit_bool(value)
            .map_err(|e: Error| e.finish(&[JsonShape::Bool], None))
    }
    number!(deserialize_i8, i8, visit_i8, I8);
    number!(deserialize_i16, i16, visit_i16, I16);
    number!(deserialize_i32, i32, visit_i32, I32);
    number!(deserialize_i64, i64, visit_i64, I64);
    number!(deserialize_i128, i128, visit_i128, I128);
    number!(deserialize_u8, u8, visit_u8, U8);
    number!(deserialize_u16, u16, visit_u16, U16);
    number!(deserialize_u32, u32, visit_u32, U32);
    number!(deserialize_u64, u64, visit_u64, U64);
    number!(deserialize_u128, u128, visit_u128, U128);
    number!(deserialize_f32, f32, visit_f32, F32);
    number!(deserialize_f64, f64, visit_f64, F64);

    fn deserialize_str<V: Visitor<'a>>(self, visitor: V) -> Result<V::Value> {
        self.text(visitor)
    }
    fn deserialize_string<V: Visitor<'a>>(self, visitor: V) -> Result<V::Value> {
        self.text(visitor)
    }
    fn deserialize_char<V: Visitor<'a>>(self, visitor: V) -> Result<V::Value> {
        self.text(visitor)
    }
    fn deserialize_identifier<V: Visitor<'a>>(self, visitor: V) -> Result<V::Value> {
        self.text(visitor)
    }
    fn deserialize_bytes<V: Visitor<'a>>(self, visitor: V) -> Result<V::Value> {
        match self.0.shape() {
            JsonShape::String => self.text(visitor),
            JsonShape::Array => self.sequence(visitor, None),
            _ => Err(wrong(self.0, &[JsonShape::String, JsonShape::Array])),
        }
    }
    fn deserialize_byte_buf<V: Visitor<'a>>(self, visitor: V) -> Result<V::Value> {
        self.deserialize_bytes(visitor)
    }
    fn deserialize_option<V: Visitor<'a>>(self, visitor: V) -> Result<V::Value> {
        if self.0.shape() == JsonShape::Null {
            visitor.visit_none()
        } else {
            visitor.visit_some(self)
        }
    }
    fn deserialize_unit<V: Visitor<'a>>(self, visitor: V) -> Result<V::Value> {
        if self.0.shape() != JsonShape::Null {
            return Err(wrong(self.0, &[JsonShape::Null]));
        }
        visitor
            .visit_unit()
            .map_err(|e: Error| e.finish(&[JsonShape::Null], None))
    }
    fn deserialize_unit_struct<V: Visitor<'a>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value> {
        self.deserialize_unit(visitor)
    }
    fn deserialize_newtype_struct<V: Visitor<'a>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value> {
        visitor.visit_newtype_struct(self)
    }
    fn deserialize_seq<V: Visitor<'a>>(self, visitor: V) -> Result<V::Value> {
        self.sequence(visitor, None)
    }
    fn deserialize_tuple<V: Visitor<'a>>(self, len: usize, visitor: V) -> Result<V::Value> {
        self.sequence(visitor, Some(len))
    }
    fn deserialize_tuple_struct<V: Visitor<'a>>(
        self,
        _name: &'static str,
        len: usize,
        visitor: V,
    ) -> Result<V::Value> {
        self.sequence(visitor, Some(len))
    }
    fn deserialize_map<V: Visitor<'a>>(self, visitor: V) -> Result<V::Value> {
        self.object(visitor)
    }
    fn deserialize_struct<V: Visitor<'a>>(
        self,
        _name: &'static str,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value> {
        match self.0.shape() {
            JsonShape::Array => self.sequence(visitor, Some(fields.len())),
            JsonShape::Object => self.object(visitor),
            _ => Err(wrong(self.0, &[JsonShape::Array, JsonShape::Object])),
        }
    }
    fn deserialize_enum<V: Visitor<'a>>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value> {
        let (tag, payload) = match &self.0.node().kind {
            JsonKind::String(tag) => (tag, None),
            JsonKind::Object(members) => {
                if members.len() != 1 {
                    return Err(length(1, members.len()));
                }
                (&members[0].name, Some(self.0.child(members[0].value)))
            }
            _ => return Err(wrong(self.0, &[JsonShape::String, JsonShape::Object])),
        };
        visitor.visit_enum(Enumeration { tag, payload })
    }
    fn deserialize_ignored_any<V: Visitor<'a>>(self, visitor: V) -> Result<V::Value> {
        visitor.visit_unit()
    }
}

struct Sequence<'d, 'a> {
    parent: JsonRef<'d, 'a>,
    items: &'d [super::syntax::JsonId],
    at: usize,
}
impl<'a> SeqAccess<'a> for &mut Sequence<'_, 'a> {
    type Error = Error;
    fn next_element_seed<T: DeserializeSeed<'a>>(&mut self, seed: T) -> Result<Option<T::Value>> {
        let Some(id) = self.items.get(self.at) else {
            return Ok(None);
        };
        let index = self.at;
        self.at += 1;
        on_json_stack(|| seed.deserialize(Decoder(self.parent.child(*id))))
            .map(Some)
            .map_err(|e| e.at(DataStep::Index(Count::from(index))))
    }
    fn size_hint(&self) -> Option<usize> {
        Some(self.items.len() - self.at)
    }
}

struct Object<'d, 'a> {
    parent: JsonRef<'d, 'a>,
    members: &'d [JsonMember<'a>],
    at: usize,
}
impl<'a> MapAccess<'a> for &mut Object<'_, 'a> {
    type Error = Error;
    fn next_key_seed<K: DeserializeSeed<'a>>(&mut self, seed: K) -> Result<Option<K::Value>> {
        let Some(member) = self.members.get(self.at) else {
            return Ok(None);
        };
        seed.deserialize(Key {
            text: &member.name,
            source: self.parent.member_source(member),
        })
        .map(Some)
        .map_err(|e| {
            if matches!(&e.cause, Cause::Ready(cause) if matches!(cause.as_ref(), DecodeCause::UnknownField(_))) {
                e
            } else {
                e.at(DataStep::Field(member.name.to_string()))
            }
        })
    }
    fn next_value_seed<V: DeserializeSeed<'a>>(&mut self, seed: V) -> Result<V::Value> {
        let member = &self.members[self.at];
        self.at += 1;
        on_json_stack(|| seed.deserialize(Decoder(self.parent.child(member.value))))
            .map_err(|e| e.at(DataStep::Field(member.name.to_string())))
    }
    fn size_hint(&self) -> Option<usize> {
        Some(self.members.len() - self.at)
    }
}

struct Enumeration<'d, 'a> {
    tag: &'d Cow<'a, str>,
    payload: Option<JsonRef<'d, 'a>>,
}
impl<'a> EnumAccess<'a> for Enumeration<'_, 'a> {
    type Error = Error;
    type Variant = Self;
    fn variant_seed<V: DeserializeSeed<'a>>(self, seed: V) -> Result<(V::Value, Self)> {
        let value = seed.deserialize(Key {
            text: self.tag,
            source: b"",
        })?;
        Ok((value, self))
    }
}
impl<'a> VariantAccess<'a> for Enumeration<'_, 'a> {
    type Error = Error;
    fn unit_variant(self) -> Result<()> {
        match self.payload {
            None => Ok(()),
            Some(value) => <()>::deserialize(Decoder(value))
                .map_err(|e| e.at(DataStep::Payload(self.tag.to_string()))),
        }
    }
    fn newtype_variant_seed<T: DeserializeSeed<'a>>(self, seed: T) -> Result<T::Value> {
        let value = self
            .payload
            .ok_or_else(|| Error::ready(DecodeCause::MissingPayload(self.tag.to_string())))?;
        on_json_stack(|| seed.deserialize(Decoder(value)))
            .map_err(|e| e.at(DataStep::Payload(self.tag.to_string())))
    }
    fn tuple_variant<V: Visitor<'a>>(self, len: usize, visitor: V) -> Result<V::Value> {
        let value = self
            .payload
            .ok_or_else(|| Error::ready(DecodeCause::MissingPayload(self.tag.to_string())))?;
        Decoder(value)
            .sequence(visitor, Some(len))
            .map_err(|e| e.at(DataStep::Payload(self.tag.to_string())))
    }
    fn struct_variant<V: Visitor<'a>>(
        self,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value> {
        let value = self
            .payload
            .ok_or_else(|| Error::ready(DecodeCause::MissingPayload(self.tag.to_string())))?;
        Decoder(value)
            .deserialize_struct("", fields, visitor)
            .map_err(|e| e.at(DataStep::Payload(self.tag.to_string())))
    }
}

// Delegating a primitive map key preserves serde_json's treatment of escapes,
// exponents, negative zero, and 128-bit integer keys.
struct PrimitiveKey<T>(T);
impl<'a, T: Deserialize<'a>> Deserialize<'a> for PrimitiveKey<T> {
    fn deserialize<D: Deserializer<'a>>(decoder: D) -> std::result::Result<Self, D::Error> {
        struct One<T>(std::marker::PhantomData<T>);
        impl<'a, T: Deserialize<'a>> Visitor<'a> for One<T> {
            type Value = PrimitiveKey<T>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("one map key")
            }
            fn visit_map<A: MapAccess<'a>>(
                self,
                mut map: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let key = map.next_key()?.expect("one generated map entry");
                map.next_value::<de::IgnoredAny>()?;
                Ok(PrimitiveKey(key))
            }
        }
        decoder.deserialize_map(One(std::marker::PhantomData))
    }
}

#[derive(Clone, Copy)]
struct Key<'d, 'a> {
    text: &'d Cow<'a, str>,
    source: &'a [u8],
}

macro_rules! numeric_key {
    ($method:ident, $ty:ty, $visit:ident, $target:ident) => {
        fn $method<V: Visitor<'a>>(self, visitor: V) -> Result<V::Value> {
            let mut bytes = Vec::with_capacity(self.source.len() + 7);
            bytes.push(b'{');
            bytes.extend_from_slice(self.source);
            bytes.extend_from_slice(b":null}");
            let PrimitiveKey(value): PrimitiveKey<$ty> =
                serde_json::from_slice(&bytes).map_err(|_| {
                    Error::ready(DecodeCause::JsonMapKey(JsonMapKeyFailure {
                        text: self.text.to_string(),
                        lexeme: std::str::from_utf8(self.source)
                            .expect("validated JSON map key")
                            .into(),
                        target: JsonNumberTarget::$target,
                    }))
                })?;
            visitor
                .$visit(value)
                .map_err(|e: Error| e.finish(&[JsonShape::String], None))
        }
    };
}
impl<'a> Deserializer<'a> for Key<'_, 'a> {
    type Error = Error;
    fn deserialize_any<V: Visitor<'a>>(self, visitor: V) -> Result<V::Value> {
        visit_text(self.text, visitor)
    }
    numeric_key!(deserialize_i8, i8, visit_i8, I8);
    numeric_key!(deserialize_i16, i16, visit_i16, I16);
    numeric_key!(deserialize_i32, i32, visit_i32, I32);
    numeric_key!(deserialize_i64, i64, visit_i64, I64);
    numeric_key!(deserialize_i128, i128, visit_i128, I128);
    numeric_key!(deserialize_u8, u8, visit_u8, U8);
    numeric_key!(deserialize_u16, u16, visit_u16, U16);
    numeric_key!(deserialize_u32, u32, visit_u32, U32);
    numeric_key!(deserialize_u64, u64, visit_u64, U64);
    numeric_key!(deserialize_u128, u128, visit_u128, U128);
    numeric_key!(deserialize_f32, f32, visit_f32, F32);
    numeric_key!(deserialize_f64, f64, visit_f64, F64);
    fn deserialize_bool<V: Visitor<'a>>(self, visitor: V) -> Result<V::Value> {
        match self.source {
            b"\"true\"" => visitor.visit_bool(true),
            b"\"false\"" => visitor.visit_bool(false),
            _ => Err(Error::ready(DecodeCause::JsonShape(JsonShapeFailure {
                expected: vec![JsonShape::Bool],
                found: JsonShape::String,
            }))),
        }
    }
    fn deserialize_option<V: Visitor<'a>>(self, visitor: V) -> Result<V::Value> {
        visitor.visit_some(self)
    }
    fn deserialize_newtype_struct<V: Visitor<'a>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value> {
        visitor.visit_newtype_struct(self)
    }
    fn deserialize_enum<V: Visitor<'a>>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value> {
        visitor.visit_enum(Enumeration {
            tag: self.text,
            payload: None,
        })
    }
    serde::forward_to_deserialize_any! {
        <V: Visitor<'a>>
        char str string bytes byte_buf unit unit_struct seq tuple tuple_struct map struct identifier ignored_any
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ready_cause_keeps_its_allocation_through_finish_and_portable() {
        let cause = DecodeCause::MissingField("required".into());
        let error = Error::ready(cause.clone()).at(DataStep::Field("outer".into()));
        let Cause::Ready(ready) = &error.cause else {
            panic!("ready cause")
        };
        let address = std::ptr::from_ref(ready.as_ref());
        let failure = error.finish(&[JsonShape::Object], None).portable();
        assert_eq!(std::ptr::from_ref(failure.cause.as_ref()), address);
        assert_eq!(*failure.cause, cause);
        assert_eq!(failure.path, [DataStep::Field("outer".into())]);
    }
}
