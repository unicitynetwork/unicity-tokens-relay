# DevOps note: relay config changes after recent PRs

This batches the config-relevant changes from the connection-stability work. Read through once, run the audit, then deploy.

## 1. Required config audit

Inspect the production `config.toml` (mounted into the Fargate task at `/usr/src/app/config.toml`).

### `[network] ping_interval_seconds` — default lowered 300 → 45

The keep-alive ping now defaults to 45 s instead of 300 s.

| What you find | Action |
|---|---|
| `ping_interval = N` (no `_seconds`) | This was a long-standing example-config typo and has **always been silently ignored** — your value never took effect. After this rollout the canonical key is `ping_interval_seconds`. If `N` was intentional, rename the key. If you don't care, delete the line and accept the new 45 s default. |
| `ping_interval_seconds = N` already present | No action. Your explicit value still wins. |
| Neither key present | No action. New default 45 s applies. |

`ping_interval_seconds = 0` is now rejected at startup with a `ConfigError`. Don't set it to zero.

### `[limits] ws_write_timeout_seconds` — **new setting, please add explicitly**

Hard cap on a single WebSocket send. Default is 30 s, which is generous for the 128 KB default message size.

**You need to override this** — production runs `max_ws_message_bytes = 1 MiB`. At 1 MiB, a legitimate slow client (mobile / CGNAT, ~25 KB/s downlink) needs ~41 s to drain a single message. Default 30 s would disconnect them.

**Recommended setting**:

```toml
[limits]
ws_write_timeout_seconds = 60
```

Bump to 90 if mobile-heavy traffic shows write-timeout disconnects. `0` is rejected at startup like the ping interval.

### `[network]` `max_quiet_time` — no longer separately configurable

Previously hardcoded at 20 min. Now derived from ping interval as `max(300 s, ping_interval_seconds * 4)`. With the new 45 s ping default this evaluates to 5 min. No knob to set.

## 2. Infrastructure (no changes required)

- **ALB**: idle timeout already at 3600 s, unaffected.
- **CloudFormation / IAM / target group / scaling**: no parameters touched.
- **DNS / certs / Cloudflare**: not in path for this subdomain.
- **RDS**: no schema or connection-config changes.

## 3. New metrics surfaced at `/metrics`

Add to Grafana dashboards if you watch these:

| Metric | Type | Notes |
|---|---|---|
| `nostr_broadcast_lagged_total` | counter | Count of broadcast events dropped because a per-connection receiver fell behind. Spikes here = slow consumers (network or stuck client). |
| `nostr_disconnects_total{reason="write_timeout"}` | counter (new label) | Single send exceeded `ws_write_timeout_seconds`. If this rises after deploy, raise the timeout. |
| `nostr_disconnects_total{reason="broadcast_closed"}` | counter (new label) | Connection was closed because the broadcast sender went away (would only happen on db_writer panic / relay teardown). Should stay near 0; non-zero is a red flag worth investigating. |

The full disconnect-reason set is now: `shutdown`, `timeout`, `send_error`, `write_timeout`, `normal`, `error`, `broadcast_closed`.

## 4. Post-deploy verification (24–48 h)

- `rate(nostr_disconnects_total{reason="send_error"}[5m])` — expected to **drop** from the ~2 % baseline if middlebox idle timeouts were the dominant cause of the original Sphere reports. If flat, the baseline was client-side noise (browser crashes, mobile suspension); ping interval can be revisited.
- `rate(nostr_disconnects_total{reason="timeout"}[5m])` — may tick up modestly. Expected: quiet timeout went 1200 s → 300 s, so dead-but-not-yet-RST'd clients are reaped sooner.
- `nostr_disconnects_total{reason="write_timeout"}` — should be near zero. Non-zero rate means slow-but-legitimate clients are tripping the cap; bump `ws_write_timeout_seconds`.
- `nostr_broadcast_lagged_total` — should be flat or only rare blips. Sustained increase = a connection is stuck on `ws_stream.send` and missing the firehose; investigate the client / network path.
- `nostr_disconnects_total{reason="broadcast_closed"}` — expect 0. Anything else means `db_writer` exited unexpectedly (runtime panic). Page someone.
- Bandwidth — negligible rise: one ~5-byte ping every 45 s instead of every 300 s.

## 5. Rollback (config-only, no redeploy)

| Symptom | Fix |
|---|---|
| `send_error` rate goes **up** post-deploy (very unlikely) | `ping_interval_seconds = 300` to restore old behavior |
| `write_timeout` ticks on healthy connections | `ws_write_timeout_seconds = 90` (or higher) |
| Quiet-timeout disconnects spike beyond a modest tick-up | `ping_interval_seconds = 75` or higher (raises `max_quiet_time` floor proportionally) |

## 6. Suggested final `[network]` and `[limits]` blocks for prod

Minimal additions to your existing `config.toml`:

```toml
[network]
# (existing keys — port, address, remote_ip_header, etc. unchanged)
ping_interval_seconds = 45    # explicit; matches new default

[limits]
# (existing keys unchanged — keep your 1 MiB max_event_bytes / max_ws_message_bytes / max_ws_frame_bytes)
ws_write_timeout_seconds = 60 # bumped above 30 s default because prod uses 1 MiB messages
```

If you have any `ping_interval = ...` lines from the old example, **delete them** — they're noise.
