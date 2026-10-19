//! The daemon's own networks: the stored definitions (`store`), the supervisor
//! that keeps bridges and dnsmasq in step with the guests (`server`), and the
//! guest connection log (`flows`).

pub(crate) mod dnslog;
pub mod flows;
pub mod server;
pub mod store;
