# Plan: Replace libvirt firewall integration

## Goal

Preserve per-VM and network firewall controls without libvirt nwfilter or libvirt-owned chains.

## Stages

### Current implementation boundary

The direct-QEMU slice supports a persisted per-VM inbound host-forward
allowlist. Empty rules deny inbound host forwards; QEMU `hostfwd` arguments are
derived only from this policy. QEMU user-mode networking cannot filter guest
packets, so guest ingress filtering beyond forwarded ports and guest-egress
filtering are unsupported. This does not claim general firewall enforcement.
The existing libvirt-backed firewall API and nftables table are untouched.

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
