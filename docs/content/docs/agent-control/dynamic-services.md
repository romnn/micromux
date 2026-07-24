---
title: Dynamic services
weight: 2
---

# Dynamic services

Beyond the services declared in `micromux.yaml`, the control plane can create **runtime (dynamic) services** — processes an agent starts on the fly, for example to run a one-off script or bring up a helper alongside your stack. They are **disabled by default** and bounded by an explicit policy, because a runtime service is a same-user command with real blast radius.

## Enabling them

Runtime services are off until you opt in, in the target session's config. The policy is **latched when the session starts**, so changing it requires a session restart:

```yaml
control:
  dynamic_services:
    enabled: true
    allowed_working_roots: ["."]   # roots a dynamic service may run in
    max_services: 4                # max live (non-retired) dynamic services
    max_lifetime: 12h              # default & maximum lease
```

| Field | Default | Meaning |
|---|---|---|
| `enabled` | `false` | Permit runtime-created services through the control plane. |
| `allowed_working_roots` | `["."]` | Working-dir roots, resolved and canonicalized relative to the config directory. |
| `max_services` | `4` | Maximum live (non-retired) dynamic services. |
| `max_lifetime` | `12h` | Default and maximum lease. `none` permits session-lifetime leases. |

> [!WARNING]
> This policy limits accidental blast radius for a same-user local tool; it is **not a security sandbox**. Dynamic-service commands still run as the session user.

## Leases

Dynamic services are **bounded to the configured lifetime by default** and always stop with the session. Set `max_lifetime: none` to allow session-lifetime leases — callers may then request `expires_after: none`. `renew_dynamic_service` extends or removes a live service's deadline without restarting it.

## The lifecycle over MCP

Agents manage runtime services with a small set of tools:

- `start_dynamic_service` / `start_dynamic_service_and_wait` — create one (the `_and_wait` form bundles create + wait-for-healthy + logs + events + diagnosis). Supply an `idempotency_key` so a creation retry is safe.
- `replace_dynamic_service` — replace a running one.
- `renew_dynamic_service` — extend or clear its lease.
- `stop_dynamic_service` — retire it while preserving its post-mortem state.

Before creating one, an agent checks `capabilities.dynamic_services` on `list_sessions`. From the shell, `micromux ctl stop-dynamic <id>` retires a dynamic service while keeping its logs and state for inspection.

Environment values can be sent inward or cloned server-side with `from_service`; snapshots omit the environment entirely, and receipts return only key names.
