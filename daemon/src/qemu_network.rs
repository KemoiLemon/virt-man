//! The wire shape of user-mode NAT port forwarding.
//!
//! The direct-QEMU VM's definition is its domain XML, which has no hostfwd
//! policy: a user-mode NIC is plain `-netdev user`. The `/qemu/vms/{id}/firewall`
//! request still names this shape, so a client that sends rules is answered
//! with an explicit refusal instead of a silent no-op.
use serde::{Deserialize, Serialize};
use std::net::Ipv4Addr;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct HostForward {
    pub protocol: Protocol,
    pub host_port: u16,
    pub guest_ip: Ipv4Addr,
    pub guest_port: u16,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    Tcp,
    Udp,
}
