//! Construct resolved authority from authored grants and the host environment.

pub mod freeze;
pub(crate) mod host;
mod paths;
mod resolve;
pub mod toolchains;

pub(crate) use freeze::Placed;
pub(crate) use paths::Polar;
pub(crate) use resolve::{Keyed, name_rules};

use crate::authority::policy::{Gpu, Net};
use crate::data::FOValue;
use crate::data::datum::{Datum as _, tag};
use std::collections::BTreeMap;

/// The grant value that pins every axis to its least: pushed as a layer, it
/// withholds all positive authority.
pub fn deny_all() -> FOValue {
    let none = || Vec::<FOValue>::new().encode();
    let table = |key: &str| BTreeMap::from([(key.to_owned(), none())]).encode();
    BTreeMap::from([
        ("editor".to_owned(), none()),
        ("exec".to_owned(), table("allow")),
        ("fs".to_owned(), table("read")),
        ("gpu".to_owned(), tag(Gpu::Withheld.into(), None)),
        ("net".to_owned(), tag(Net::Offline.into(), None)),
        ("shell".to_owned(), none()),
    ])
    .encode()
}
