//! The daemon's own IPAM: DHCP and DNS for guest networks, in place of the
//! dnsmasq libvirt runs. See `store` for the state and why it is kept here.

pub mod dhcp;
pub mod dns;
pub mod flows;
pub mod server;
pub mod store;
pub mod xml;
