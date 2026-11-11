//! Pure domain leaves beneath the record mirrors.
use super::{
    Count, CountRule, DecodeCause, DecodeFailure, Fields, JsonEncodeCause,
    JsonEncodeCauseNonUnicodePath, JsonEncodeFailure, JsonRef, Mirror, NonEmpty, PathBuf,
    SealSpellingFailure, Serialize, Span, Sum, TimestampParseFailure, UnknownField, Via,
    ViaFailure, ViaFailureDuplicateTag, duplicate, length, missing,
};

#[derive(Debug, Serialize)]
#[serde(transparent)]
pub(crate) struct RawPath(String);
impl Mirror for RawPath {
    type Value = PathBuf;
    fn read(v: JsonRef<'_, '_>) -> Result<Self, DecodeFailure> {
        String::read(v).map(Self)
    }
    fn check(&self) -> Result<(), DecodeFailure> {
        Ok(())
    }
    fn hydrate(&self) -> PathBuf {
        self.0.clone().into()
    }
    fn from_value(v: &PathBuf) -> Result<Self, JsonEncodeFailure> {
        ral_core::path::utf8(v)
            .map(|s| Self(s.into()))
            .map_err(|_| {
                JsonEncodeFailure::new(JsonEncodeCause::NonUnicodePath(
                    JsonEncodeCauseNonUnicodePath {
                        shown: ral_core::text::quote_native(v.as_os_str()),
                    },
                ))
            })
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct RawSpan {
    from: u64,
    to: u64,
}
impl Mirror for RawSpan {
    type Value = Span;
    fn read(value: JsonRef<'_, '_>) -> Result<Self, DecodeFailure> {
        let fields = Fields::product(value)?;
        read_fields!(fields, true, Self, { from: u64 => required, to: u64 => required })
    }
    fn check(&self) -> Result<(), DecodeFailure> {
        Span::new(self.from, self.to)
            .map(drop)
            .map_err(|e| DecodeFailure::new(DecodeCause::TurnSpan(e.into())))
    }
    fn hydrate(&self) -> Span {
        Span::new(self.from, self.to).expect("span checked before hydration")
    }
    fn from_value(v: &Span) -> Result<Self, JsonEncodeFailure> {
        Ok(Self {
            from: v.from(),
            to: v.to(),
        })
    }
}

#[derive(Debug, Serialize)]
#[serde(transparent)]
pub(crate) struct RawSum(String);
impl Mirror for RawSum {
    type Value = Sum;
    fn read(v: JsonRef<'_, '_>) -> Result<Self, DecodeFailure> {
        String::read(v).map(Self)
    }
    fn check(&self) -> Result<(), DecodeFailure> {
        if self.0.len() == 16
            && self
                .0
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        {
            Ok(())
        } else {
            Err(DecodeFailure::new(DecodeCause::SealSpelling(
                SealSpellingFailure {
                    text: self.0.clone(),
                },
            )))
        }
    }
    fn hydrate(&self) -> Sum {
        Sum(u64::from_str_radix(&self.0, 16).expect("sum checked before hydration"))
    }
    fn from_value(v: &Sum) -> Result<Self, JsonEncodeFailure> {
        Ok(Self(v.to_string()))
    }
}

pub(super) fn nonempty(got: usize) -> Result<(), DecodeFailure> {
    if got == 0 {
        Err(length(CountRule::AtLeast(1u64.into()), got))
    } else {
        Ok(())
    }
}

#[derive(Debug, Serialize)]
#[serde(transparent)]
pub(crate) struct RawNonEmpty<T>(Vec<T>);
impl<T: Mirror> Mirror for RawNonEmpty<T> {
    type Value = NonEmpty<T::Value>;
    fn read(v: JsonRef<'_, '_>) -> Result<Self, DecodeFailure> {
        Vec::read(v).map(Self)
    }
    fn check(&self) -> Result<(), DecodeFailure> {
        nonempty(self.0.len())?;
        self.0.check()
    }
    fn hydrate(&self) -> Self::Value {
        NonEmpty::collect(self.0.hydrate()).expect("nonempty checked before hydration")
    }
    fn from_value(v: &Self::Value) -> Result<Self, JsonEncodeFailure> {
        v.iter()
            .enumerate()
            .map(|(i, v)| T::from_value(v).map_err(|e| e.index(i.into())))
            .collect::<Result<Vec<_>, _>>()
            .map(Self)
    }
}

#[derive(Debug, Serialize)]
#[serde(transparent)]
pub(crate) struct RawVia(Vec<String>);
impl Mirror for RawVia {
    type Value = Via;
    fn read(v: JsonRef<'_, '_>) -> Result<Self, DecodeFailure> {
        Vec::read(v).map(Self)
    }
    fn check(&self) -> Result<(), DecodeFailure> {
        let failure = if self.0.is_empty() {
            Some(ViaFailure::Empty)
        } else if self.0.iter().any(String::is_empty) {
            Some(ViaFailure::EmptyTag)
        } else {
            let mut seen = std::collections::BTreeSet::new();
            self.0
                .iter()
                .find(|s| !seen.insert(*s))
                .map(|tag| ViaFailure::DuplicateTag(ViaFailureDuplicateTag { tag: tag.clone() }))
        };
        failure.map_or(Ok(()), |cause| {
            Err(DecodeFailure::new(DecodeCause::Via(cause)))
        })
    }
    fn hydrate(&self) -> Via {
        Via::try_from(self.0.clone()).expect("via checked before hydration")
    }
    fn from_value(v: &Via) -> Result<Self, JsonEncodeFailure> {
        Ok(Self(v.tags().to_vec()))
    }
}

#[derive(Debug, Serialize)]
#[serde(transparent)]
pub(crate) struct RawTimestamp(String);
impl RawTimestamp {
    fn parse(&self) -> Result<jiff::Timestamp, DecodeFailure> {
        self.0.parse::<jiff::Timestamp>().map_err(|error| {
            DecodeFailure::new(DecodeCause::Timestamp(TimestampParseFailure {
                text: self.0.clone(),
                detail: error.to_string(),
            }))
        })
    }
}
impl Mirror for RawTimestamp {
    type Value = jiff::Timestamp;
    fn read(v: JsonRef<'_, '_>) -> Result<Self, DecodeFailure> {
        String::read(v).map(Self)
    }
    fn check(&self) -> Result<(), DecodeFailure> {
        self.parse().map(drop)
    }
    fn hydrate(&self) -> Self::Value {
        self.parse().expect("timestamp checked before hydration")
    }
    fn from_value(v: &Self::Value) -> Result<Self, JsonEncodeFailure> {
        Ok(Self(v.to_string()))
    }
}
