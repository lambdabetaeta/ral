pub const CONTROL: u32 = 1729;
pub const NET: u32 = 1730;
pub const FACTORY: u32 = 1731;
pub const PLAN9: u32 = 564;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_ports_are_distinct() {
        let ports = [CONTROL, NET, FACTORY, PLAN9];
        for (index, port) in ports.iter().enumerate() {
            assert!(
                !ports[index + 1..].contains(port),
                "port {port} is repeated"
            );
        }
    }
}
