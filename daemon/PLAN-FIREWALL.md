# Plan: Replace libvirt firewall integration

## Goal

Preserve per-VM and network firewall controls without libvirt nwfilter or libvirt-owned chains.

## Stages

### Current implementation boundary

The firewall is native. `daemon/src/firewall.rs` renders one `virt-man`
nftables table and applies it with a single `nft -f` transaction. It owns what
libvirt's network driver used to: per-network NAT, isolation and port
forwarding, and per-VM MAC-scoped rules. There are no libvirt nwfilter chains,
no libvirt-owned chains, and no iptables backend.

- Network rules render from the daemon's own network store (subnet, mode,
  bridge, whether the bridge is up); a stopped network renders nothing. `in`
  and `out` are scoped by subnet. `forward` is scoped by the network's bridge
  (`iifname`/`oifname`), so one network owner's `forward` policy cannot
  blackhole another network's guests.
- A port forward is DNAT on the prerouting hook for traffic arriving from
  outside the network: the network's own bridge is excluded (`iifname !=`), so
  a guest's outbound connection on the forwarded port is not redirected to
  another guest. "Any WAN address" also excludes the guest subnet as a
  destination.
- Per-VM rules are scoped to the guest's interface MACs, for libvirt domains
  and for direct-QEMU VMs whose NIC is on a managed bridge. A QEMU VM is keyed
  by its id (the name `GET /qemu/vms` lists).
- A direct-QEMU VM on user-mode networking has no host bridge: its packets live
  inside QEMU's own NAT, so nftables cannot see or match them. The only inbound
  policy there is QEMU's `hostfwd` allowlist (`GET/PUT
  /qemu/vms/{id}/firewall`, which reports `guest_filtering` and
  `egress_filtering` as unsupported). Guest ingress and egress filtering are
  unsupported for user-mode NICs.
- A save is persisted before it is applied. If `nft -f` fails, the response is
  a 500 saying "rules saved, apply failed" with the daemon's reason, never
  "applied". The script is one transaction (`add table`, `flush table`, then
  the rendered block), so a refused script leaves the previous table in force
  rather than an empty one.

1. **Separate policy from enforcement.** Store declarative rules keyed by resource UUID and stable device/network identity; implement enforcement independently, initially targeting nftables with any iptables support explicitly scoped.
2. **Specify rule semantics.** Define direction, protocol, address/port matching, ordering, defaults, and established-connection behavior before translating existing policy.
3. **Bind rules to stable interfaces.** Match VM UUID plus device identity or daemon-managed tap, not ephemeral interface names or libvirt chain names.
4. **Apply atomically and idempotently.** Update complete policies transactionally where possible, preserve unrelated host rules, and reconcile after daemon or networking restarts.
5. **Coordinate NAT forwarding.** Keep DNAT and guest-input filtering consistent. Test the real packet path and counters rather than only asserting rules exist.
6. **Define fail-safe behavior.** Decide whether VM start or policy updates fail closed when enforcement cannot be applied. Expose partial state and errors; do not silently claim an unapplied policy is active.
7. **Migrate with a dry run.** Import existing policy into the new representation, show a diff, and apply only after confirmation. Do not alter libvirt chains during the compatibility period.

## Acceptance criteria

- Rules apply only to the intended guest/network and survive restart.
- Unrelated host firewall policy is preserved.
- Failures are observable and do not produce a false active-policy status.
- NAT-forwarded traffic and ordinary traffic follow the specified policy.
