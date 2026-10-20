# Plan: native networking (interfaces and NAT)

## Goal

Provide VM networking without an external network manager: host interfaces,
bridges, DHCP/DNS, NAT, forwarding and rate limits, all owned by the daemon.

## Current implementation boundary

Two NIC paths share the daemon but not the plumbing. Nothing is imported: the
store starts empty, and no existing network definition is adopted, converted or
renamed.

Network, IPAM and firewall are IPv4-only. IPv6 guest networking is out of
scope: nothing addresses, filters or forwards it.

- **Bridge-backed QEMU VMs.** Networks are the daemon's own, stored in
  `<data_dir>/networks.json`. A managed network owns a Linux bridge (`vbr` + 12
  hex of its id) and gives it its address. One dnsmasq runs per active network
  from `/run/virt-man-daemon/dnsmasq-<id>.conf`: DHCP is static-only (a
  `dhcp-host` exists only for a running guest on that bridge), every named
  reservation answers DNS, and other names are forwarded upstream. A supervisor
  (every 5 s) pre-allocates a permanent reservation for each guest NIC, so an
  address exists before anything asks for one. Guests attach with
  `<interface type='bridge'><source bridge='vbr…'/>`; the daemon resolves the
  network key to its bridge. NAT, isolation and port forwarding render into one
  IPv4 nftables table (`virt-man`) and are applied as a single transaction on
  every lifecycle change. The access log's DNS rows come from dnsmasq's query
  log. A stopped network answers nothing.
- **Direct-QEMU VMs.** Either user-mode NAT (`-netdev user`, QEMU's own NAT,
  TCP/UDP forwarding to unicast guest IPv4) or a stored network's bridge through
  a daemon-created tap, per NIC and for hot-added NICs. A bridge NIC gets a
  deterministic MAC and tap, and feeds the same IPAM reservations, DHCP/DNS and
  per-VM firewall rules as any other guest. Rate limits are applied on the tap
  with `tc`: RX is an egress htb class, TX an ingress policer, and clearing a
  direction removes its qdisc.

## Remaining stage

1. Keep the packet-path tests in step: addresses, DNS, forwarding and shaping
   are verified on the wire (udhcpc, `dig`, `tc`), not by reading rules back.

## Acceptance criteria

- A guest receives the expected address and can reach only the intended networks.
- Tap, bridge, DHCP/DNS, NAT, and forwarding state survive daemon restart
  without duplicate or stale rules.
- Network deletion is refused while resources remain attached.
- NAT and forwarding are validated through packet-path tests, not merely by
  checking that rules exist.
