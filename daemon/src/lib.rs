//! Library face of virt-man-daemon, so integration tests (`tests/`) can drive
//! the modules directly. The binary in `src/main.rs` remains the real entry
//! point and owns `main()`; this exposes the same modules under a crate name.

pub mod api;
pub mod auth;
pub mod config;
pub mod ct;
pub mod ctxml;
pub mod disks;
pub mod domainxml;
pub mod firewall;
pub mod firmware;
pub mod hostnet;
pub mod ipam;
pub mod lxc;
pub mod netxml;
pub mod pools;
pub mod pty;
pub mod qemu;
pub mod qemu_network;
pub mod rfb;
pub mod tls;
pub mod volume_titles;
