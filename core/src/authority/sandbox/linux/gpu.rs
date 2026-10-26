//! The GPU as the envelope shows it: device nodes, and the sysfs a driver
//! discovers them by.  Found at each launch, since bwrap has no globs and a
//! host makes `nvidia-uvm` on its first CUDA call.  Bound by name, as
//! `render_dev`'s nodes are: kernel-owned trees no unprivileged process
//! renames into.

use super::{DenyMask, Options};
use crate::path::{LexicalPath, RealPath, utf8};
use crate::site::{Silent, site};
use std::path::PathBuf;

const DRI: &str = "/dev/dri";

/// Discovery's entry points, whose links lead into [`GpuNodes::devices`].
const SYSFS: [&str; 2] = ["/sys/class/drm", "/sys/dev/char"];

/// This host's GPU, by name.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct GpuNodes {
    /// Whether `/dev/dri` stands; withheld, it is emptied whole, `card*` included.
    pub(super) dri: bool,
    /// `/dev/dri`'s render nodes, never its `card*` modesetting ones.
    pub(super) render: Vec<String>,
    /// NVIDIA's nodes, AMD's `kfd` and WSL's `dxg`, in `/dev` itself.
    pub(super) nodes: Vec<String>,
    /// Each render node's device directory in sysfs.
    pub(super) devices: Vec<String>,
}

impl GpuNodes {
    pub(super) fn discover() -> Self {
        let render: Vec<String> = names(DRI)
            .into_iter()
            .filter(|n| n.starts_with("renderD"))
            .map(|n| format!("{DRI}/{n}"))
            .collect();
        Self {
            dri: crate::path::exists(DRI),
            devices: render.iter().filter_map(|node| device_of(node)).collect(),
            nodes: names("/dev")
                .into_iter()
                .filter(|n| is_node(n))
                .map(|n| format!("/dev/{n}"))
                .collect(),
            render,
        }
    }

    /// Into the envelope's own `/dev` and `/sys`.
    pub(super) fn admit(&self, c: &mut Options) {
        for node in self.render.iter().chain(&self.nodes) {
            c.args(["--dev-bind-try", node.as_str(), node.as_str()]);
        }
        for dir in SYSFS
            .into_iter()
            .chain(self.devices.iter().map(String::as_str))
        {
            c.args(["--ro-bind-try", dir, dir]);
        }
    }

    /// Out of the host's `/dev`: `/dev/dri` emptied, every other node unopenable.
    pub(super) fn withhold(&self, c: &mut Options) {
        if self.dri {
            DenyMask::EmptyDir.render(c, DRI);
        }
        for node in &self.nodes {
            DenyMask::UnopenableNode.render(c, node);
        }
    }
}

fn names(dir: &str) -> Vec<String> {
    site!(Silent::GpuDiscovery, std::fs::read_dir(dir))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect()
}

/// Where `/sys/class/drm/<node>/device` leads.
fn device_of(node: &str) -> Option<String> {
    let name = node.rsplit('/').next()?;
    let link = LexicalPath::absolute(&PathBuf::from(format!("/sys/class/drm/{name}/device")))?;
    let real = RealPath::of(&link).ok()?;
    Some(utf8(real.as_path()).ok()?.to_owned())
}

fn is_node(name: &str) -> bool {
    matches!(
        name,
        "nvidiactl" | "nvidia-uvm" | "nvidia-uvm-tools" | "kfd" | "dxg"
    ) || name
        .strip_prefix("nvidia")
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::is_node;

    #[test]
    fn the_gpu_nodes_are_named_and_nothing_else_is() {
        for name in [
            "nvidia0",
            "nvidia12",
            "nvidiactl",
            "nvidia-uvm",
            "nvidia-uvm-tools",
            "kfd",
            "dxg",
        ] {
            assert!(is_node(name), "{name}");
        }
        for name in [
            "nvidia",
            "nvidia-modeset",
            "nvidiax",
            "card0",
            "nvme0",
            "nvram",
            "kvm",
        ] {
            assert!(!is_node(name), "{name}");
        }
    }
}
