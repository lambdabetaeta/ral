//! Serialization representations, never ral types.

use super::{
    JsonDecode, JsonEncode, JsonFormat, JsonRef, decode_structural, on_json_stack,
    serialize_structural,
};
use crate::data::datum::Datum;
use crate::data::{FOValue, NoExt, byte_encoding};
use crate::fact::{Count, DecodeFailure, JsonEncodeFailure};
use serde::{Deserialize, Serialize};
use std::marker::PhantomData;

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RawFOValue<X = NoExt> {
    Unit,
    Bool {
        value: bool,
    },
    Int {
        value: i64,
    },
    Float {
        value: u64,
    },
    String {
        value: String,
    },
    Bytes {
        value: String,
    },
    List {
        items: Vec<Self>,
    },
    Map {
        entries: Vec<(String, Self)>,
    },
    Variant {
        label: String,
        payload: Option<Box<Self>>,
    },
    Ext(X),
}

impl<X> RawFOValue<X> {
    /// Extension adapters own their semantic validation and internal paths.
    ///
    /// # Errors
    /// Returns malformed finite/base64 leaves or extension failures, with their paths.
    pub fn validate_with<Y>(
        &self,
        mut extension: impl FnMut(&X) -> Result<Y, DecodeFailure>,
    ) -> Result<FOValue<Y>, DecodeFailure> {
        self.validate_inner(&mut extension)
    }

    fn validate_inner<Y>(
        &self,
        extension: &mut impl FnMut(&X) -> Result<Y, DecodeFailure>,
    ) -> Result<FOValue<Y>, DecodeFailure> {
        on_json_stack(|| match self {
            Self::Unit => Ok(FOValue::Unit),
            Self::Bool { value } => Ok(FOValue::Bool { value: *value }),
            Self::Int { value } => Ok(FOValue::Int { value: *value }),
            Self::Float { value } => crate::data::Finite::from_bits(*value)
                .map(|value| FOValue::Float { value })
                .map_err(|e| e.field("value").payload("float")),
            Self::String { value } => Ok(FOValue::String {
                value: value.clone(),
            }),
            Self::Bytes { value } => byte_encoding::decode_base64(value)
                .map(|value| FOValue::Bytes { value })
                .map_err(|e| e.field("value").payload("bytes")),
            Self::List { items } => {
                let mut values = Vec::with_capacity(items.len());
                let items = items.iter().enumerate();
                for (i, item) in items {
                    match item.validate_inner(extension) {
                        Ok(value) => values.push(value),
                        Err(error) => {
                            for value in values {
                                drop_value(value);
                            }
                            return Err(error.index(Count::from(i)).field("items").payload("list"));
                        }
                    }
                }
                Ok(FOValue::List { items: values })
            }
            Self::Map { entries } => {
                let mut values = Vec::with_capacity(entries.len());
                let entries = entries.iter().enumerate();
                for (i, (key, item)) in entries {
                    match item.validate_inner(extension) {
                        Ok(value) => values.push((key.clone(), value)),
                        Err(error) => {
                            for (_, value) in values {
                                drop_value(value);
                            }
                            return Err(error
                                .index(Count::from_u64(1))
                                .index(Count::from(i))
                                .field("entries")
                                .payload("map"));
                        }
                    }
                }
                Ok(FOValue::Map { entries: values })
            }
            Self::Variant { label, payload } => {
                let payload = payload
                    .as_ref()
                    .map(|p| p.validate_inner(extension).map(Box::new))
                    .transpose()
                    .map_err(|e| e.field("payload").payload("variant"))?;
                Ok(FOValue::Variant {
                    label: label.clone(),
                    payload,
                })
            }
            Self::Ext(value) => extension(value)
                .map(FOValue::Ext)
                .map_err(|e| e.payload("ext")),
        })
    }

    /// # Errors
    /// Returns an extension encoding failure with its enclosing path.
    pub fn from_value_with<Y>(
        value: FOValue<Y>,
        mut extension: impl FnMut(Y) -> Result<X, JsonEncodeFailure>,
    ) -> Result<Self, JsonEncodeFailure> {
        Self::from_value_inner(value, &mut extension)
    }

    fn from_value_inner<Y>(
        value: FOValue<Y>,
        extension: &mut impl FnMut(Y) -> Result<X, JsonEncodeFailure>,
    ) -> Result<Self, JsonEncodeFailure> {
        on_json_stack(|| match value {
            FOValue::Unit => Ok(Self::Unit),
            FOValue::Bool { value } => Ok(Self::Bool { value }),
            FOValue::Int { value } => Ok(Self::Int { value }),
            FOValue::Float { value } => Ok(Self::Float {
                value: value.get().to_bits(),
            }),
            FOValue::String { value } => Ok(Self::String { value }),
            FOValue::Bytes { value } => Ok(Self::Bytes {
                value: byte_encoding::encode_base64(&value),
            }),
            FOValue::List { items } => {
                let mut values = Vec::with_capacity(items.len());
                let mut items = items.into_iter().enumerate();
                while let Some((i, item)) = items.next() {
                    match Self::from_value_inner(item, extension) {
                        Ok(value) => values.push(value),
                        Err(error) => {
                            for (_, remaining) in items {
                                drop_value(remaining);
                            }
                            drop(values);
                            return Err(error.index(Count::from(i)).field("items").payload("list"));
                        }
                    }
                }
                Ok(Self::List { items: values })
            }
            FOValue::Map { entries } => {
                let mut values = Vec::with_capacity(entries.len());
                let mut entries = entries.into_iter().enumerate();
                while let Some((i, (key, item))) = entries.next() {
                    match Self::from_value_inner(item, extension) {
                        Ok(value) => values.push((key, value)),
                        Err(error) => {
                            for (_, (_, remaining)) in entries {
                                drop_value(remaining);
                            }
                            drop(values);
                            return Err(error
                                .index(Count::from_u64(1))
                                .index(Count::from(i))
                                .field("entries")
                                .payload("map"));
                        }
                    }
                }
                Ok(Self::Map { entries: values })
            }
            FOValue::Variant { label, payload } => {
                let payload = payload
                    .map(|p| Self::from_value_inner(*p, extension).map(Box::new))
                    .transpose()
                    .map_err(|e| e.field("payload").payload("variant"))?;
                Ok(Self::Variant { label, payload })
            }
            FOValue::Ext(value) => extension(value)
                .map(Self::Ext)
                .map_err(|e| e.payload("ext")),
        })
    }
}

// Drain the spine before dropping children, including partially decoded trees.
impl<X> Drop for RawFOValue<X> {
    fn drop(&mut self) {
        fn take_children<X>(value: &mut RawFOValue<X>, pending: &mut Vec<RawFOValue<X>>) {
            match value {
                RawFOValue::List { items } => pending.append(items),
                RawFOValue::Map { entries } => {
                    pending.extend(std::mem::take(entries).into_iter().map(|(_, v)| v));
                }
                RawFOValue::Variant { payload, .. } => {
                    if let Some(value) = payload.take() {
                        pending.push(*value);
                    }
                }
                _ => {}
            }
        }
        let mut pending = Vec::new();
        take_children(self, &mut pending);
        while let Some(mut value) = pending.pop() {
            take_children(&mut value, &mut pending);
        }
    }
}

/// Iterative disposal of the first-order spine; extensions remain owner-owned.
pub fn drop_value<X>(value: FOValue<X>) {
    let mut pending = vec![value];
    while let Some(value) = pending.pop() {
        match value {
            FOValue::List { items } => pending.extend(items),
            FOValue::Map { entries } => pending.extend(entries.into_iter().map(|(_, v)| v)),
            FOValue::Variant {
                payload: Some(value),
                ..
            } => pending.push(*value),
            _ => {}
        }
    }
}

impl RawFOValue {
    /// # Errors
    /// Returns malformed finite/base64 leaves with their enclosing paths.
    pub fn validate(&self) -> Result<FOValue, DecodeFailure> {
        self.validate_with(|never| Ok(never.clone()))
    }
    /// # Panics
    /// Only if the impossible extension encoding of first-order data fails.
    pub fn from_value(value: FOValue) -> Self {
        Self::from_value_with(value, |never| match never {})
            .expect("first-order data has no fallible encoding leaf")
    }
}

/// Its JSON representation is the existing first-order value alone.
#[derive(Debug, Serialize, Deserialize)]
#[serde(transparent, bound(serialize = "", deserialize = ""))]
pub struct RawDatum<T> {
    pub value: RawFOValue,
    #[serde(skip)]
    marker: PhantomData<fn() -> T>,
}

impl<T> RawDatum<T> {
    pub fn new(value: RawFOValue) -> Self {
        Self {
            value,
            marker: PhantomData,
        }
    }
}

impl<T: Datum> RawDatum<T> {
    /// # Errors
    /// Returns raw leaf failures or the canonical `Datum` validation failure.
    pub fn validate(&self) -> Result<T, DecodeFailure> {
        let value = self.value.validate()?;
        let result = T::decode(&value);
        drop_value(value);
        result
    }
    pub fn from_datum(value: T) -> Self {
        Self::new(RawFOValue::from_value(value.encode()))
    }
}

impl JsonDecode for FOValue {
    fn decode_json(value: JsonRef<'_, '_>) -> Result<Self, DecodeFailure> {
        decode_structural::<RawFOValue>(value)?.validate()
    }
}
impl JsonEncode for FOValue {
    fn encode_json(&self, format: JsonFormat) -> Result<Vec<u8>, JsonEncodeFailure> {
        // The existing leaf serializers are total for an already validated FOValue.
        Ok(serialize_structural(self, format))
    }
}
