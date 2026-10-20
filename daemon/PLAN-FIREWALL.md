# Plan: native firewall

## Goal

Provide per-VM and per-network firewall controls without nwfilter, iptables or
any external firewall backend.

## Current implementation boundary

The firewall is native. `daemon/src/firewall.rs` renders one `virt-man`
nftables table and applies it with a single `nft -f` transaction. It owns
per-network NAT, isolation and port forwarding, and per-VM MAC-scoped rules.
There is no nwfilter chain, no iptables backend and no rules from another
manager.

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
- Per-VM rules are scoped to the guest's interface MACs, for QEMU VMs whose NIC
  is on a managed bridge. A QEMU VM is keyed by its id (the name `GET
  /qemu/vms` lists).
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

## Remaining stages

1. **Test the real packet path.** NAT, forwarding and per-VM rules are checked
   through traffic and counters, not only by asserting that rules exist.
2. **Keep fail-safe behavior explicit.** A refused apply reports partial state
   and never claims an unapplied policy is active.

## Not in scope

- Migrating existing policy: this host is greenfield, so there is nothing to
  import, diff or roll back.
- iptables support, and any rule that depends on an external firewall manager.

## Acceptance criteria

- Rules apply only to the intended guest/network and survive restart.
- Unrelated host firewall policy is preserved.
- Failures are observable and do not produce a false active-policy status.
- NAT-forwarded traffic and ordinary traffic follow the specified policy.
