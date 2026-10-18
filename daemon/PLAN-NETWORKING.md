# Plan: Replace libvirt networking (interfaces and NAT)

## Goal

Provide VM and container networking without libvirt's network driver, including host interfaces, bridges, DHCP/DNS integration, NAT, and forwarding.

## Stages

### Current implementation boundary

The first operational direct-QEMU slice provides persisted, validated
per-VM user-mode NAT configuration and QEMU `-netdev user` hostfwd. It supports
TCP/UDP forwarding to unicast guest IPv4 addresses. Bridge/tap mode, DHCP/DNS,
and daemon-managed host firewall rules are unavailable; QEMU owns this mode's
NAT. Existing libvirt networking remains unchanged.

1. **Choose initial modes.** Define support for bridged interfaces, daemon-managed NAT networks, and optionally isolated networks. Start only with modes that can be configured and recovered reliably.
2. **Build a host-network backend.** Manage bridges, tap devices, addresses, routes, forwarding, and firewall/NAT rules through a defined Linux networking layer. Do not depend on libvirt-created bridges or dnsmasq.
3. **Define persistent network configuration.** Store network UUID, bridge, subnet, DHCP range, DNS settings, forwarding mode, and attached resources in daemon state/config. Validate overlaps and host-interface conflicts before applying.
4. **Attach QEMU interfaces explicitly.** Create tap devices, apply ownership/permissions and optional rate limits, pass them to QEMU, and record enough state to recover or clean up orphaned taps.
5. **Reconnect IPAM/DHCP/DNS.** Reuse the existing daemon implementation only after replacing libvirt-network discovery and lifecycle hooks with the new network store.
6. **Add NAT and port forwarding.** Implement idempotent masquerade and DNAT rules. Detect the firewall backend and report exactly which rules were installed; do not depend on libvirt chains.
7. **Implement recovery and safe deletion.** Reconcile bridges and rules on daemon start. Refuse network deletion while interfaces or forwards depend on it.
8. **Migrate gradually.** Import existing definitions without disrupting guests, then migrate one stopped guest at a time. Keep existing libvirt networks intact during the transition.

## Acceptance criteria

- A QEMU guest receives the expected address and can reach only the intended networks.
- Tap, bridge, DHCP/DNS, NAT, and forwarding state survive daemon restart without duplicate or stale rules.
- Network deletion is refused while resources remain attached.
- NAT and forwarding are validated through packet-path tests, not merely by checking that rules exist.

## Migration and rollback

Import and compare network definitions before switching guests. Avoid simultaneous ownership of the same bridge/subnet by libvirt and the new backend. Keep libvirt definitions until all attached guests have been migrated and rollback has been tested.
