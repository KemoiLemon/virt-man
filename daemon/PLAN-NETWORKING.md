# Plan: Replace libvirt networking (interfaces and NAT)

## Goal

Provide VM and container networking without libvirt's network driver: host
interfaces, bridges, DHCP/DNS, NAT, and forwarding, all owned by the daemon.

## Current implementation boundary

Two paths share the daemon but not the plumbing.

There is no upgrade or import path: the store starts empty, and networks
defined in libvirt are not adopted, converted or renamed.

- **libvirt-managed VMs and containers.** Networks are the daemon's own, stored
  in `<data_dir>/networks.json`; none is defined in libvirt. A managed network
  owns a Linux bridge (`vbr` + 12 hex of its id) and gives it its address. One
  dnsmasq runs per active network from `/run/virt-man-daemon/dnsmasq-<id>.conf`:
  DHCP is static-only (a `dhcp-host` exists only for a running guest on that
  bridge), every named reservation answers DNS, and other names are forwarded
  upstream. A supervisor (every 5 s) pre-allocates a permanent reservation for
  each guest NIC, so an address exists before anything asks for one. Guests
  attach with `<interface type='bridge'><source bridge='vbr…'/>`; the daemon
  resolves the network key to its bridge. NAT, isolation and port forwarding
  render into one IPv4 nftables table (`virt-man`) and are applied as a single
  transaction on every lifecycle change. The access log's DNS rows come from
  dnsmasq's query log. A stopped network answers nothing.
- **Direct-QEMU VMs.** Persisted, validated per-VM user-mode NAT
  (`-netdev user`) with TCP/UDP forwarding to unicast guest IPv4. QEMU owns this
  mode's NAT. Bridge/tap mode, DHCP/DNS and daemon-managed host firewall rules
  are not implemented on this path.

## Remaining stages

1. Give direct-QEMU VMs the same bridge/tap path: create the tap, apply
   ownership and rate limits, pass it to QEMU, and record enough state to
   recover or clean up orphaned taps.
2. IPv6: the store and the nftables table are IPv4-only, and IPv6 is neither
   filtered nor forwarded.
3. Keep the packet-path tests in step: addresses, DNS and forwarding are
   verified on the wire (udhcpc, `dig`, tc), not by reading rules back.

## Acceptance criteria

- A guest receives the expected address and can reach only the intended networks.
- Tap, bridge, DHCP/DNS, NAT, and forwarding state survive daemon restart
  without duplicate or stale rules.
- Network deletion is refused while resources remain attached.
- NAT and forwarding are validated through packet-path tests, not merely by
  checking that rules exist.
