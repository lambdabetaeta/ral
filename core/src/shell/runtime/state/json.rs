//! The serialized context, with validation outside serde's error channel.

use super::Context;
use crate::authority::policy::json::RawGrantStack;
use crate::environ::EnvVars;
use crate::fact::{
    DecodeCause, DecodeFailure, JsonEncodeCause, JsonEncodeCauseNonUnicodePath, JsonEncodeFailure,
};
use crate::path::CwdPath;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct RawContext<H> {
    env_overrides: imbl::HashMap<String, String>,
    cwd: String,
    grants: RawGrantStack,
    handlers: H,
    args: Vec<String>,
}

impl<H> RawContext<H> {
    /// The handler adapter owns its descendants; this envelope adds only its field.
    pub(crate) fn validate_with<G>(
        self,
        handlers: impl FnOnce(H) -> Result<G, DecodeFailure>,
    ) -> Result<Context<G>, DecodeFailure> {
        Ok(Context {
            env_overrides: EnvVars::from_json_map(self.env_overrides)
                .map_err(|error| error.field("env_overrides"))?,
            cwd: CwdPath::new(self.cwd.into()).map_err(|error| {
                DecodeFailure::new(DecodeCause::Path((&error).into())).field("cwd")
            })?,
            grants: self
                .grants
                .validate()
                .map_err(|error| error.field("grants"))?,
            handlers: handlers(self.handlers).map_err(|error| error.field("handlers"))?,
            args: self.args,
        })
    }

    pub(crate) fn from_value_with<G>(
        value: &Context<G>,
        handlers: impl FnOnce(&G) -> Result<H, JsonEncodeFailure>,
    ) -> Result<Self, JsonEncodeFailure> {
        let cwd = crate::path::utf8(value.cwd.as_path()).map_err(|_| {
            JsonEncodeFailure::new(JsonEncodeCause::NonUnicodePath(
                JsonEncodeCauseNonUnicodePath {
                    shown: crate::text::quote_native(value.cwd.as_path().as_os_str()),
                },
            ))
            .field("cwd")
        })?;
        Ok(Self {
            env_overrides: value
                .env_overrides
                .iter()
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect(),
            cwd: cwd.into(),
            grants: RawGrantStack::from(&value.grants),
            handlers: handlers(&value.handlers).map_err(|error| error.field("handlers"))?,
            args: value.args.clone(),
        })
    }
}
