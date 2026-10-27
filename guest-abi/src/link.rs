use std::net::Ipv4Addr;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Link {
    pub address: Ipv4Addr,
    pub prefix: u8,
    pub gateway: Ipv4Addr,
}

pub const LINK: Link = Link {
    address: Ipv4Addr::new(10, 0, 2, 15),
    prefix: 24,
    gateway: Ipv4Addr::new(10, 0, 2, 2),
};
pub const PROXY_PORT: u16 = 3128;
pub const MTU: usize = 1500;
