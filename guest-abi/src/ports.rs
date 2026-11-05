pub const CONTROL: u32 = 1729;
pub const NET: u32 = 1730;
pub const FACTORY: u32 = 1731;
// 1732 is reserved for the smoke probe.
pub const MOUNTS: u32 = 1733;
pub const PLAN9: u32 = 564;

const PORTS: [u32; 6] = [CONTROL, NET, FACTORY, 1732, MOUNTS, PLAN9];
const _: () = {
    let mut i = 0;
    while i < PORTS.len() {
        let mut j = i + 1;
        while j < PORTS.len() {
            assert!(PORTS[i] != PORTS[j]);
            j += 1;
        }
        i += 1;
    }
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_ports_are_distinct() {
        let ports = PORTS;
        for (index, port) in ports.iter().enumerate() {
            assert!(
                !ports[index + 1..].contains(port),
                "port {port} is repeated"
            );
        }
    }
}
