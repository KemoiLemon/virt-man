//! Structured configuration for direct-QEMU user-mode NAT networking.
use serde::{Deserialize, Serialize};
use std::net::Ipv4Addr;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, Default)]
pub struct UserNetwork {
    pub hostfwd: Vec<HostForward>,
}

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

impl UserNetwork {
    pub fn validate(&self) -> Result<(), String> {
        let mut ports = std::collections::BTreeSet::new();
        for f in &self.hostfwd {
            if f.host_port == 0 || f.guest_port == 0 {
                return Err("hostfwd ports must be between 1 and 65535".into());
            }
            if f.guest_ip.is_unspecified() || f.guest_ip.is_broadcast() || f.guest_ip.is_multicast()
            {
                return Err("hostfwd guest_ip must be a unicast IPv4 address".into());
            }
            let protocol = match f.protocol {
                Protocol::Tcp => 0,
                Protocol::Udp => 1,
            };
            if !ports.insert((protocol, f.host_port)) {
                return Err("hostfwd host port and protocol must be unique".into());
            }
        }
        Ok(())
    }

    /// The same netdev under another id, for a VM's extra interfaces.
    pub fn qemu_netdev_for(&self, id: &str) -> Result<String, String> {
        self.validate()?;
        let forwards = self
            .hostfwd
            .iter()
            .map(|f| {
                format!(
                    "hostfwd={}:{}-{}:{}",
                    match f.protocol {
                        Protocol::Tcp => "tcp",
                        Protocol::Udp => "udp",
                    },
                    f.host_port,
                    f.guest_ip,
                    f.guest_port
                )
            })
            .collect::<Vec<_>>();
        Ok(if forwards.is_empty() {
            format!("user,id={id}")
        } else {
            format!("user,id={id},{}", forwards.join(","))
        })
    }
}

/// Allowlisted inbound host forwards. User-mode networking does not provide
/// guest packet filtering or egress policy; this controls only hostfwd.
pub type InboundPolicy = UserNetwork;

#[cfg(test)]
mod user_tests {
    use super::*;
    #[test]
    fn forwards_render_as_data_and_are_validated() {
        let cfg = UserNetwork {
            hostfwd: vec![HostForward {
                protocol: Protocol::Tcp,
                host_port: 2222,
                guest_ip: "192.0.2.2".parse().unwrap(),
                guest_port: 22,
            }],
        };
        assert_eq!(
            cfg.qemu_netdev_for("net0").unwrap(),
            "user,id=net0,hostfwd=tcp:2222-192.0.2.2:22"
        );
        let mut invalid = cfg.clone();
        invalid.hostfwd[0].host_port = 0;
        assert!(invalid.validate().is_err());
        invalid = cfg;
        invalid.hostfwd.push(invalid.hostfwd[0].clone());
        assert!(invalid.validate().is_err());
    }
}
