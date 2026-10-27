//! Host terminals masked before the payload starts.

use crate::site::{Silent, site};

#[derive(Default)]
pub(crate) struct PtyNodes(pub(super) Vec<String>);

impl PtyNodes {
    pub(super) fn discover() -> Result<Self, String> {
        let entries = site!(Silent::PtyDiscovery, std::fs::read_dir("/dev/pts"))
            .map_err(|e| format!("sandbox: cannot list host terminals in /dev/pts: {e}"))?;
        let mut nodes = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| {
                format!("sandbox: cannot read a host terminal entry in /dev/pts: {e}")
            })?;
            let name = entry.file_name();
            let Ok(name) = name.into_string() else {
                continue;
            };
            if !name.is_empty() && name.bytes().all(|b| b.is_ascii_digit()) {
                nodes.push(format!("/dev/pts/{name}"));
            }
        }
        nodes.sort();
        Ok(Self(nodes))
    }
}
