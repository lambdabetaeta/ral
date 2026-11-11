//! Complete serialization mirrors; domain checks precede semantic hydration.
use super::{
    CancelSite, Count, CountRule, CredentialFailure, CutShort, DecodeCause, DecodeFailure, Fields,
    JsonEncodeFailure, JsonRef, Limit, Mirror, ProviderError, ProviderRefusalCause, RawDatum,
    RawTimestamp, RawVia, RecordingFailure, Refusal, RetryableProviderCause, Selected, Serialize,
    ServiceFailure, Tuning, UnknownField, duplicate, external, length, missing, on_json_stack,
    required_payload,
};
#[derive(Debug, Serialize)]
pub(crate) struct RawSelected {
    model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    context_window: Option<u64>,
    label: String,
    service: String,
    account: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    via: Option<RawVia>,
    tuning: Tuning,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
}
impl Mirror for RawSelected {
    type Value = Selected;
    fn read(value: JsonRef<'_, '_>) -> Result<Self, DecodeFailure> {
        let fields = Fields::product(value)?;
        read_fields!(fields, true, Self, { model: String => required, context_window: Option<u64> => optional, label: String => required, service: String => required, account: String => required, via: Option<RawVia> => optional, tuning: Tuning => required, max_tokens: Option<u32> => optional })
    }
    fn check(&self) -> Result<(), DecodeFailure> {
        self.model.check().map_err(|e| e.field("model"))?;
        self.context_window
            .check()
            .map_err(|e| e.field("context_window"))?;
        self.label.check().map_err(|e| e.field("label"))?;
        self.service.check().map_err(|e| e.field("service"))?;
        self.account.check().map_err(|e| e.field("account"))?;
        self.via.check().map_err(|e| e.field("via"))?;
        self.tuning.check().map_err(|e| e.field("tuning"))?;
        self.max_tokens.check().map_err(|e| e.field("max_tokens"))?;
        Ok(())
    }
    fn hydrate(&self) -> Self::Value {
        Selected {
            model: self.model.hydrate(),
            context_window: self.context_window.hydrate(),
            label: self.label.hydrate(),
            service: self.service.hydrate(),
            account: self.account.hydrate(),
            via: self.via.hydrate(),
            tuning: self.tuning.hydrate(),
            max_tokens: self.max_tokens.hydrate(),
        }
    }
    fn from_value(value: &Self::Value) -> Result<Self, JsonEncodeFailure> {
        Ok(Self {
            model: <String as Mirror>::from_value(&value.model).map_err(|e| e.field("model"))?,
            context_window: <Option<u64> as Mirror>::from_value(&value.context_window)
                .map_err(|e| e.field("context_window"))?,
            label: <String as Mirror>::from_value(&value.label).map_err(|e| e.field("label"))?,
            service: <String as Mirror>::from_value(&value.service)
                .map_err(|e| e.field("service"))?,
            account: <String as Mirror>::from_value(&value.account)
                .map_err(|e| e.field("account"))?,
            via: <Option<RawVia> as Mirror>::from_value(&value.via).map_err(|e| e.field("via"))?,
            tuning: <Tuning as Mirror>::from_value(&value.tuning).map_err(|e| e.field("tuning"))?,
            max_tokens: <Option<u32> as Mirror>::from_value(&value.max_tokens)
                .map_err(|e| e.field("max_tokens"))?,
        })
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct RawRefusal {
    limit: Limit,
    #[serde(skip_serializing_if = "Option::is_none")]
    resets_at: Option<RawTimestamp>,
    received: RawTimestamp,
    cause: RawDatum<ProviderRefusalCause>,
}
impl Mirror for RawRefusal {
    type Value = Refusal;
    fn read(value: JsonRef<'_, '_>) -> Result<Self, DecodeFailure> {
        let fields = Fields::product(value)?;
        read_fields!(fields, false, Self, { limit: Limit => required, resets_at: Option<RawTimestamp> => optional, received: RawTimestamp => required, cause: RawDatum<ProviderRefusalCause> => required })
    }
    fn check(&self) -> Result<(), DecodeFailure> {
        self.limit.check().map_err(|e| e.field("limit"))?;
        self.resets_at.check().map_err(|e| e.field("resets_at"))?;
        self.received.check().map_err(|e| e.field("received"))?;
        self.cause.check().map_err(|e| e.field("cause"))?;
        Ok(())
    }
    fn hydrate(&self) -> Self::Value {
        Refusal {
            limit: self.limit.hydrate(),
            resets_at: self.resets_at.hydrate(),
            received: self.received.hydrate(),
            cause: self.cause.hydrate(),
        }
    }
    fn from_value(value: &Self::Value) -> Result<Self, JsonEncodeFailure> {
        Ok(Self {
            limit: <Limit as Mirror>::from_value(&value.limit).map_err(|e| e.field("limit"))?,
            resets_at: <Option<RawTimestamp> as Mirror>::from_value(&value.resets_at)
                .map_err(|e| e.field("resets_at"))?,
            received: <RawTimestamp as Mirror>::from_value(&value.received)
                .map_err(|e| e.field("received"))?,
            cause: <RawDatum<ProviderRefusalCause> as Mirror>::from_value(&value.cause)
                .map_err(|e| e.field("cause"))?,
        })
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RawProviderError {
    Unavailable(RawDatum<RecordingFailure>),
    Cancelled(CancelSite),
    Transient {
        cause: RawDatum<RetryableProviderCause>,
        attempts: u32,
    },
    Refused(RawRefusal),
    Api {
        model: String,
        cause: RawDatum<ServiceFailure>,
    },
    Overflow {
        model: String,
        provider: Option<String>,
        sent: Option<u64>,
        limit: Option<u64>,
        listed: Option<u64>,
        cause: RawDatum<ServiceFailure>,
    },
    Truncated {
        cause: Box<RawCutShort>,
    },
    Terminal(RawDatum<ServiceFailure>),
    Credential(RawDatum<CredentialFailure>),
}
impl Mirror for RawProviderError {
    type Value = ProviderError;
    fn read(value: JsonRef<'_, '_>) -> Result<Self, DecodeFailure> {
        on_json_stack(|| {
            let (tag, payload) = external(
                value,
                &[
                    "unavailable",
                    "cancelled",
                    "transient",
                    "refused",
                    "api",
                    "overflow",
                    "truncated",
                    "terminal",
                    "credential",
                ],
            )?;
            match tag {
                "unavailable" => {
                    <RawDatum<RecordingFailure>>::read(required_payload(payload, tag)?)
                        .map(Self::Unavailable)
                        .map_err(|e| e.payload(tag))
                }
                "cancelled" => <CancelSite>::read(required_payload(payload, tag)?)
                    .map(Self::Cancelled)
                    .map_err(|e| e.payload(tag)),
                "transient" => {
                    let fields = Fields::product(required_payload(payload, tag)?)
                        .map_err(|e| e.payload(tag))?;
                    (|| read_fields!(fields, false, Self::Transient, { cause: RawDatum<RetryableProviderCause> => required, attempts: u32 => required }))().map_err(|e: DecodeFailure| e.payload(tag))
                }
                "refused" => <RawRefusal>::read(required_payload(payload, tag)?)
                    .map(Self::Refused)
                    .map_err(|e| e.payload(tag)),
                "api" => {
                    let fields = Fields::product(required_payload(payload, tag)?)
                        .map_err(|e| e.payload(tag))?;
                    (|| read_fields!(fields, false, Self::Api, { model: String => required, cause: RawDatum<ServiceFailure> => required }))().map_err(|e: DecodeFailure| e.payload(tag))
                }
                "overflow" => {
                    let fields = Fields::product(required_payload(payload, tag)?)
                        .map_err(|e| e.payload(tag))?;
                    (|| read_fields!(fields, false, Self::Overflow, { model: String => required, provider: Option<String> => optional, sent: Option<u64> => optional, limit: Option<u64> => optional, listed: Option<u64> => optional, cause: RawDatum<ServiceFailure> => required }))().map_err(|e: DecodeFailure| e.payload(tag))
                }
                "truncated" => {
                    let fields = Fields::product(required_payload(payload, tag)?)
                        .map_err(|e| e.payload(tag))?;
                    (|| read_fields!(fields, false, Self::Truncated, { cause: Box<RawCutShort> => required }))().map_err(|e: DecodeFailure| e.payload(tag))
                }
                "terminal" => <RawDatum<ServiceFailure>>::read(required_payload(payload, tag)?)
                    .map(Self::Terminal)
                    .map_err(|e| e.payload(tag)),
                "credential" => {
                    <RawDatum<CredentialFailure>>::read(required_payload(payload, tag)?)
                        .map(Self::Credential)
                        .map_err(|e| e.payload(tag))
                }
                _ => unreachable!("external checked tag"),
            }
        })
    }
    fn check(&self) -> Result<(), DecodeFailure> {
        on_json_stack(|| match self {
            Self::Unavailable(value) => value.check().map_err(|e| e.payload("unavailable")),
            Self::Cancelled(value) => value.check().map_err(|e| e.payload("cancelled")),
            Self::Transient { cause, attempts } => (|| {
                cause.check().map_err(|e| e.field("cause"))?;
                attempts.check().map_err(|e| e.field("attempts"))?;
                Ok(())
            })()
            .map_err(|e: DecodeFailure| e.payload("transient")),
            Self::Refused(value) => value.check().map_err(|e| e.payload("refused")),
            Self::Api { model, cause } => (|| {
                model.check().map_err(|e| e.field("model"))?;
                cause.check().map_err(|e| e.field("cause"))?;
                Ok(())
            })()
            .map_err(|e: DecodeFailure| e.payload("api")),
            Self::Overflow {
                model,
                provider,
                sent,
                limit,
                listed,
                cause,
            } => (|| {
                model.check().map_err(|e| e.field("model"))?;
                provider.check().map_err(|e| e.field("provider"))?;
                sent.check().map_err(|e| e.field("sent"))?;
                limit.check().map_err(|e| e.field("limit"))?;
                listed.check().map_err(|e| e.field("listed"))?;
                cause.check().map_err(|e| e.field("cause"))?;
                Ok(())
            })()
            .map_err(|e: DecodeFailure| e.payload("overflow")),
            Self::Truncated { cause } => (|| {
                cause.check().map_err(|e| e.field("cause"))?;
                Ok(())
            })()
            .map_err(|e: DecodeFailure| e.payload("truncated")),
            Self::Terminal(value) => value.check().map_err(|e| e.payload("terminal")),
            Self::Credential(value) => value.check().map_err(|e| e.payload("credential")),
        })
    }
    fn hydrate(&self) -> Self::Value {
        on_json_stack(|| match self {
            Self::Unavailable(value) => ProviderError::Unavailable(value.hydrate()),
            Self::Cancelled(value) => ProviderError::Cancelled(value.hydrate()),
            Self::Transient { cause, attempts } => ProviderError::Transient {
                cause: cause.hydrate(),
                attempts: attempts.hydrate(),
            },
            Self::Refused(value) => ProviderError::Refused(value.hydrate()),
            Self::Api { model, cause } => ProviderError::Api {
                model: model.hydrate(),
                cause: cause.hydrate(),
            },
            Self::Overflow {
                model,
                provider,
                sent,
                limit,
                listed,
                cause,
            } => ProviderError::Overflow {
                model: model.hydrate(),
                provider: provider.hydrate(),
                sent: sent.hydrate(),
                limit: limit.hydrate(),
                listed: listed.hydrate(),
                cause: Box::new(cause.hydrate()),
            },
            Self::Truncated { cause } => ProviderError::Truncated {
                cause: cause.hydrate(),
            },
            Self::Terminal(value) => ProviderError::Terminal(value.hydrate()),
            Self::Credential(value) => ProviderError::Credential(value.hydrate()),
        })
    }
    fn from_value(value: &Self::Value) -> Result<Self, JsonEncodeFailure> {
        on_json_stack(|| match value {
            ProviderError::Unavailable(value) => {
                <RawDatum<RecordingFailure> as Mirror>::from_value(value)
                    .map(Self::Unavailable)
                    .map_err(|e| e.payload("unavailable"))
            }
            ProviderError::Cancelled(value) => <CancelSite as Mirror>::from_value(value)
                .map(Self::Cancelled)
                .map_err(|e| e.payload("cancelled")),
            ProviderError::Transient { cause, attempts } => (|| {
                Ok(Self::Transient {
                    cause: <RawDatum<RetryableProviderCause> as Mirror>::from_value(cause)
                        .map_err(|e| e.field("cause"))?,
                    attempts: <u32 as Mirror>::from_value(attempts)
                        .map_err(|e| e.field("attempts"))?,
                })
            })()
            .map_err(|e: JsonEncodeFailure| e.payload("transient")),
            ProviderError::Refused(value) => <RawRefusal as Mirror>::from_value(value)
                .map(Self::Refused)
                .map_err(|e| e.payload("refused")),
            ProviderError::Api { model, cause } => (|| {
                Ok(Self::Api {
                    model: <String as Mirror>::from_value(model).map_err(|e| e.field("model"))?,
                    cause: <RawDatum<ServiceFailure> as Mirror>::from_value(cause)
                        .map_err(|e| e.field("cause"))?,
                })
            })()
            .map_err(|e: JsonEncodeFailure| e.payload("api")),
            ProviderError::Overflow {
                model,
                provider,
                sent,
                limit,
                listed,
                cause,
            } => (|| {
                Ok(Self::Overflow {
                    model: <String as Mirror>::from_value(model).map_err(|e| e.field("model"))?,
                    provider: <Option<String> as Mirror>::from_value(provider)
                        .map_err(|e| e.field("provider"))?,
                    sent: <Option<u64> as Mirror>::from_value(sent).map_err(|e| e.field("sent"))?,
                    limit: <Option<u64> as Mirror>::from_value(limit)
                        .map_err(|e| e.field("limit"))?,
                    listed: <Option<u64> as Mirror>::from_value(listed)
                        .map_err(|e| e.field("listed"))?,
                    cause: <RawDatum<ServiceFailure> as Mirror>::from_value(cause.as_ref())
                        .map_err(|e| e.field("cause"))?,
                })
            })()
            .map_err(|e: JsonEncodeFailure| e.payload("overflow")),
            ProviderError::Truncated { cause } => (|| {
                Ok(Self::Truncated {
                    cause: <Box<RawCutShort> as Mirror>::from_value(cause)
                        .map_err(|e| e.field("cause"))?,
                })
            })()
            .map_err(|e: JsonEncodeFailure| e.payload("truncated")),
            ProviderError::Terminal(value) => {
                <RawDatum<ServiceFailure> as Mirror>::from_value(value)
                    .map(Self::Terminal)
                    .map_err(|e| e.payload("terminal"))
            }
            ProviderError::Credential(value) => {
                <RawDatum<CredentialFailure> as Mirror>::from_value(value)
                    .map(Self::Credential)
                    .map_err(|e| e.payload("credential"))
            }
        })
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RawCutShort {
    OutputCap { stop_reason: String },
    Stalled(RawProviderError),
}
impl Mirror for RawCutShort {
    type Value = CutShort;
    fn read(value: JsonRef<'_, '_>) -> Result<Self, DecodeFailure> {
        on_json_stack(|| {
            let (tag, payload) = external(value, &["output_cap", "stalled"])?;
            match tag {
                "output_cap" => {
                    let fields = Fields::product(required_payload(payload, tag)?)
                        .map_err(|e| e.payload(tag))?;
                    (|| read_fields!(fields, false, Self::OutputCap, { stop_reason: String => required }))().map_err(|e: DecodeFailure| e.payload(tag))
                }
                "stalled" => <RawProviderError>::read(required_payload(payload, tag)?)
                    .map(Self::Stalled)
                    .map_err(|e| e.payload(tag)),
                _ => unreachable!("external checked tag"),
            }
        })
    }
    fn check(&self) -> Result<(), DecodeFailure> {
        on_json_stack(|| match self {
            Self::OutputCap { stop_reason } => (|| {
                stop_reason.check().map_err(|e| e.field("stop_reason"))?;
                Ok(())
            })()
            .map_err(|e: DecodeFailure| e.payload("output_cap")),
            Self::Stalled(value) => value.check().map_err(|e| e.payload("stalled")),
        })
    }
    fn hydrate(&self) -> Self::Value {
        on_json_stack(|| match self {
            Self::OutputCap { stop_reason } => CutShort::OutputCap {
                stop_reason: stop_reason.hydrate(),
            },
            Self::Stalled(value) => CutShort::Stalled(value.hydrate()),
        })
    }
    fn from_value(value: &Self::Value) -> Result<Self, JsonEncodeFailure> {
        on_json_stack(|| match value {
            CutShort::OutputCap { stop_reason } => (|| {
                Ok(Self::OutputCap {
                    stop_reason: <String as Mirror>::from_value(stop_reason)
                        .map_err(|e| e.field("stop_reason"))?,
                })
            })()
            .map_err(|e: JsonEncodeFailure| e.payload("output_cap")),
            CutShort::Stalled(value) => <RawProviderError as Mirror>::from_value(value)
                .map(Self::Stalled)
                .map_err(|e| e.payload("stalled")),
        })
    }
}

// A truncated raw provider chain may be arbitrarily long, including on errors.
impl Drop for RawCutShort {
    fn drop(&mut self) {
        fn next(value: &mut RawCutShort) -> Option<RawCutShort> {
            match value {
                RawCutShort::Stalled(RawProviderError::Truncated { cause }) => {
                    Some(std::mem::replace(
                        cause.as_mut(),
                        RawCutShort::OutputCap {
                            stop_reason: String::new(),
                        },
                    ))
                }
                RawCutShort::OutputCap { .. }
                | RawCutShort::Stalled(
                    RawProviderError::Unavailable(_)
                    | RawProviderError::Cancelled(_)
                    | RawProviderError::Transient { .. }
                    | RawProviderError::Refused(_)
                    | RawProviderError::Api { .. }
                    | RawProviderError::Overflow { .. }
                    | RawProviderError::Terminal(_)
                    | RawProviderError::Credential(_),
                ) => None,
            }
        }
        let mut pending = next(self);
        while let Some(mut value) = pending {
            pending = next(&mut value);
        }
    }
}
