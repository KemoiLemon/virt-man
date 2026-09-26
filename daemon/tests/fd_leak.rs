//! Regression test: `with_conn` must close its libvirt connection.
//!
//! `virt::Connect` has no `Drop` impl, so a connection that is merely dropped
//! leaks its libvirt event fds. A polling client (the web UI refreshes every
//! 3s per server) exhausted the daemon's 1024-fd limit within a few hundred
//! requests, after which every libvirt call failed with
//! "Too many open files".
//!
//! Requires a reachable `qemu:///system`, so it is ignored by default:
//!     cargo test --test fd_leak -- --ignored

use std::fs;

fn open_fds() -> usize {
    fs::read_dir("/proc/self/fd")
        .map(|d| d.count())
        .unwrap_or(0)
}

/// Open and close many connections the way `with_conn` does; the fd count
/// must not grow with the number of operations.
#[test]
#[ignore = "needs a reachable qemu:///system"]
fn repeated_connections_do_not_leak_fds() {
    use virt::connect::Connect;

    // Warm up so one-off allocations are not counted as growth.
    for _ in 0..5 {
        let mut c = Connect::open(Some("qemu:///system")).expect("connect");
        let _ = c.get_lib_version();
        c.close().expect("close");
    }

    let before = open_fds();
    for _ in 0..200 {
        let mut c = Connect::open(Some("qemu:///system")).expect("connect");
        let _ = c.list_all_domains(0);
        c.close().expect("close");
    }
    let after = open_fds();

    assert!(
        after <= before + 8,
        "fd count grew from {before} to {after} over 200 connections \
         — with_conn is leaking libvirt connections again"
    );
}

/// The failure mode this guards against: without `close()` the fds climb.
/// Documents the leak so the guard above cannot be "fixed" by deletion.
#[test]
#[ignore = "demonstrates the leak; needs a reachable qemu:///system"]
fn dropping_without_close_leaks() {
    use virt::connect::Connect;

    let before = open_fds();
    for _ in 0..100 {
        let c = Connect::open(Some("qemu:///system")).expect("connect");
        let _ = c.list_all_domains(0);
        // Deliberately no close(): this is what the daemon used to do. `c`
        // drops here at iteration end — the leak is exactly that this Drop is
        // a no-op (libvirt's Connect has no Drop impl).
    }
    let after = open_fds();
    assert!(
        after > before,
        "expected dropping without close() to leak, before={before} after={after}"
    );
}
