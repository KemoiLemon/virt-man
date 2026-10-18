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

    pub fn qemu_netdev(&self) -> Result<String, String> {
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
            "user,id=net0".into()
        } else {
            format!("user,id=net0,{}", forwards.join(","))
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
            cfg.qemu_netdev().unwrap(),
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

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct NetworkConfig {
    pub id: String,
    pub bridge: String,
    pub subnet: String,
    pub dhcp_start: String,
    pub dhcp_end: String,
    pub mode: Mode,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Isolated,
    Nat,
}

impl NetworkConfig {
    /// Reject ambiguous or host-dangerous values before they reach a future
    /// host-network backend. This does not imply that the network is applied.
    pub fn validate(&self) -> Result<(), String> {
        if !uuid::Uuid::parse_str(&self.id)
            .is_ok_and(|uuid| uuid.hyphenated().to_string() == self.id)
        {
            return Err("network id must be a canonical UUID".into());
        }
        if self.bridge.len() > 15
            || self.bridge.is_empty()
            || !self
                .bridge
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.')
        {
            return Err("bridge must be a valid Linux interface name".into());
        }
        let (network, prefix) = parse_cidr(&self.subnet)?;
        if prefix == 0 || prefix > 30 {
            return Err("subnet prefix must be between /1 and /30".into());
        }
        let start = self
            .dhcp_start
            .parse::<Ipv4Addr>()
            .map_err(|_| "invalid DHCP start address")?;
        let end = self
            .dhcp_end
            .parse::<Ipv4Addr>()
            .map_err(|_| "invalid DHCP end address")?;
        let mask = u32::MAX << (32 - prefix);
        let base = u32::from(network) & mask;
        let broadcast = base | !mask;
        let start = u32::from(start);
        let end = u32::from(end);
        if start < base + 1 || end > broadcast - 1 || start > end {
            return Err("DHCP range must be ordered host addresses inside the subnet".into());
        }
        Ok(())
    }
}

fn parse_cidr(value: &str) -> Result<(Ipv4Addr, u32), String> {
    let (ip, prefix) = value.split_once('/').ok_or("subnet must be IPv4 CIDR")?;
    let ip = ip
        .parse::<Ipv4Addr>()
        .map_err(|_| "subnet must be IPv4 CIDR")?;
    let prefix = prefix.parse::<u32>().map_err(|_| "invalid subnet prefix")?;
    if prefix > 32 || value.matches('/').count() != 1 {
        return Err("invalid subnet prefix".into());
    }
    Ok((ip, prefix))
}

/// Render narrowly scoped nftables forwarding/NAT expressions. The table
/// lifecycle and atomic application are intentionally not wired until the
/// persistent host-network owner is implemented.
pub fn nat_rule_lines(config: &NetworkConfig) -> Result<Vec<String>, String> {
    config.validate()?;
    if config.mode != Mode::Nat {
        return Ok(Vec::new());
    }
    Ok(vec![format!(
        "ip saddr {} oifname != \"{}\" masquerade",
        config.subnet, config.bridge
    )])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(mode: Mode) -> NetworkConfig {
        NetworkConfig {
            id: "550e8400-e29b-41d4-a716-446655440000".into(),
            bridge: "vmbr-test".into(),
            subnet: "192.0.2.0/24".into(),
            dhcp_start: "192.0.2.10".into(),
            dhcp_end: "192.0.2.200".into(),
            mode,
        }
    }

    #[test]
    fn validates_range_interface_and_prefix() {
        assert!(sample(Mode::Nat).validate().is_ok());
        let mut c = sample(Mode::Nat);
        c.dhcp_start = "192.0.2.255".into();
        assert!(c.validate().is_err());
        let mut c = sample(Mode::Nat);
        c.bridge = "bad name".into();
        assert!(c.validate().is_err());
        let mut c = sample(Mode::Nat);
        c.subnet = "192.0.2.0/0".into();
        assert!(c.validate().is_err());
    }

    #[test]
    fn nat_rendering_is_scoped_and_isolated_has_no_rules() {
        assert_eq!(
            nat_rule_lines(&sample(Mode::Isolated)).unwrap(),
            Vec::<String>::new()
        );
        let rules = nat_rule_lines(&sample(Mode::Nat)).unwrap();
        assert_eq!(
            rules,
            ["ip saddr 192.0.2.0/24 oifname != \"vmbr-test\" masquerade"]
        );
    }
}
