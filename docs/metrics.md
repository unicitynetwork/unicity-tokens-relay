# Metrics

The relay exposes Prometheus-format metrics at `GET /metrics` on the same port as the WebSocket. The endpoint is unauthenticated — scope it via your reverse proxy / network ACL when running in production.

All metric series are defined in `src/server.rs::create_metrics`.

## Hot-path metrics (event-driven)

These are updated synchronously as traffic hits the relay.

### Connection lifecycle

| Metric | Type | Labels | Description |
|---|---|---|---|
| `nostr_connections_total` | counter | — | New WebSocket connections accepted (cumulative) |
| `nostr_connections_active` | gauge | — | Currently-open WebSocket connections |
| `nostr_disconnects_total` | counter | `reason` | Disconnects, by cause: `shutdown`, `timeout`, `send_error`, `write_timeout`, `normal`, `error`, `broadcast_closed`. `write_timeout` means a `ws_stream.send` exceeded the 30s write cap (peer didn't drain — usually a dead-but-not-RST'd connection that would otherwise stall the task for the full TCP retransmit window). `broadcast_closed` means the broadcast sender went away (db_writer panic / relay teardown) and the connection closed because no more events would arrive. |

### Subscription lifecycle

| Metric | Type | Labels | Description |
|---|---|---|---|
| `nostr_subscriptions_active` | gauge | — | Currently-active REQ subscriptions across all clients |
| `nostr_query_abort_total` | counter | `reason` | Server-side query aborts |

### Protocol commands

Counters for raw command frames received from clients.

| Metric | Type | Labels | Description |
|---|---|---|---|
| `nostr_cmd_req_total` | counter | — | `REQ` (subscribe) commands |
| `nostr_cmd_event_total` | counter | — | `EVENT` (publish) commands |
| `nostr_cmd_close_total` | counter | — | `CLOSE` (unsubscribe) commands |
| `nostr_cmd_auth_total` | counter | — | `AUTH` (NIP-42) commands |

### Event flow

| Metric | Type | Labels | Description |
|---|---|---|---|
| `nostr_events_received_by_kind_total` | counter | `kind` | Successfully parsed events received from clients, by kind. The accounting identity `received = persisted + ephemeral_broadcast + rejected` holds across all kinds (see the three counters below). |
| `nostr_events_persisted_by_kind_total` | counter | `kind` | Events successfully written to the database, by kind. Ephemeral events (kinds in `[20000, 30000)`) are *not* counted here — see the next row. |
| `nostr_events_ephemeral_broadcast_by_kind_total` | counter | `kind` | Ephemeral events broadcast to subscribers without being persisted, by kind. |
| `nostr_events_rejected_total` | counter | `reason` | Events rejected before persistence. Reasons: `expired`, `future_dated`, `kind_blacklist`, `kind_allowlist`, `pubkey_not_whitelisted`, `not_admitted`, `insufficient_balance`, `pubkey_not_registered`, `admission_check_error`, `nip05_invalid`, `nip05_missing`, `nip05_error`, `grpc_denied`, `duplicate`, `write_error` |
| `nostr_events_sent_total` | counter | `source` | Events sent to subscribers. `source` = `db` (historical query result) or `realtime` (broadcast match) |
| `nostr_broadcast_lagged_total` | counter | — | Broadcast events dropped because a per-connection receiver fell behind (the receiver's slot in the broadcast channel overflowed `broadcast_buffer`). Spikes here indicate slow consumers — usually backpressure from a stuck `ws_stream.send`. |

### Latency histograms

Default Prometheus buckets (`5ms` … `10s`).

| Metric | Type | Description |
|---|---|---|
| `nostr_query_seconds` | histogram | End-to-end subscription response time |
| `nostr_filter_seconds` | histogram | Single SQL filter execution time |
| `nostr_events_write_seconds` | histogram | Event-write latency |

## State metrics (background-collected)

A tokio task in `start_server` polls the database on two cadences:

- **Fast (every 30 s, first pass 5 s after boot)**: total events, distinct authors, DB size. On Postgres these are cheap planner-stat reads (`pg_class.reltuples`, `pg_stats.n_distinct`, `pg_database_size`); on SQLite they are direct counts.
- **Slow (every 10 min, first pass 15 s after boot)**: per-kind event counts. The query is a `GROUP BY kind` which is a sequential scan on Postgres; running it less often keeps the buffer cache from being thrashed. The collector updates labels in-place and only removes labels for kinds that disappeared between cycles, so dashboards don't see scrape gaps.

If `pg_stats` has no row for `event.pub_key` (typical right after a fresh load before `ANALYZE` has run, or if the connecting role lacks `SELECT` on the table) the distinct-authors gauge reads 0. The relay logs a one-shot warning so the operator sees the cause.

| Metric | Type | Labels | Description |
|---|---|---|---|
| `nostr_db_events_total` | gauge | — | Estimated total event rows |
| `nostr_db_events_by_kind` | gauge | `kind` | Event count per Nostr kind in the database |
| `nostr_db_authors_distinct` | gauge | — | Distinct authors (estimate on Postgres, exact on SQLite) |
| `nostr_db_size_bytes` | gauge | — | Database size. On Postgres this is `pg_database_size(current_database())`. On SQLite it is `page_count * page_size` of the **main DB file only** — the `-wal` and `-shm` sidecars (which can be substantial in WAL mode) are not included. Production runs Postgres, where this is exact. |
| `nostr_db_connections` | gauge | — | Active DB connections in use |

## Process metrics (relay process: CPU, RSS, FDs)

When built on Linux, the relay registers `prometheus::process_collector::ProcessCollector::for_self()` so `/metrics` also includes:

| Metric | Type | Description |
|---|---|---|
| `process_cpu_seconds_total` | counter | Total CPU time consumed by the relay process |
| `process_resident_memory_bytes` | gauge | Resident set size |
| `process_virtual_memory_bytes` | gauge | Virtual memory size |
| `process_open_fds` | gauge | Open file descriptors |
| `process_max_fds` | gauge | File descriptor limit |
| `process_threads` | gauge | OS threads |
| `process_start_time_seconds` | gauge | Unix-time start of the process |

These are sourced from `procfs` and only present on Linux. macOS dev builds will not show `process_*` series — the Docker/ECS deployment will.

## Host / container / DB metrics (out-of-process)

The relay does not expose machine-level CPU / RAM / disk / network — those come from sidecars:

- **Container metrics** (Fargate) — use [ECS/Fargate Container Insights](https://docs.aws.amazon.com/AmazonCloudWatch/latest/monitoring/Container-Insights.html). Enable it on the cluster and AWS publishes per-task and per-container CPU / memory / network / storage metrics into the `ECS/ContainerInsights` CloudWatch namespace. Cross-import to Grafana via the same CloudWatch integration as RDS (see below). cAdvisor is *not* an option in Fargate — the runtime hides `/sys`, `/var/lib/docker`, and `/dev/kmsg`, all of which cAdvisor needs to read.
- **Bare-host metrics** (non-containerized VMs) — use `node_exporter` instead. Not bundled here; add it as another scrape target in `alloy/config.alloy` if needed.
- **RDS database host metrics** — RDS does not allow installing exporters. Use one of:
  - **CloudWatch → Grafana Cloud**: enable the [Grafana Cloud AWS integration](https://grafana.com/docs/grafana-cloud/monitor-infrastructure/aws/cloudwatch/) to pull RDS namespace metrics (`AWS/RDS`: CPUUtilization, FreeableMemory, DatabaseConnections, FreeStorageSpace, ReadIOPS / WriteIOPS, etc.).
  - **`cloudwatch_exporter`**: self-hosted Prometheus exporter that reads the same data; useful if you don't run Grafana Cloud.

  These are independent of this repo and configured at the AWS / Grafana account level.

The relay's own `nostr_db_size_bytes` gauge gives the **logical** Postgres database size (`pg_database_size`); CloudWatch's `FreeStorageSpace` gives the underlying RDS volume free space.

## Notes on label cardinality

`kind` is the Nostr event kind as decimal string. In typical operation the relay sees a small bounded set of kinds (single-digit count for a focused relay; a few dozen for a general-purpose one). If unbounded "kind spam" becomes a concern, consider gating with `limits.event_kind_allowlist` in `config.toml` — that simultaneously enforces policy and bounds metric cardinality.

`reason` labels are all enumerated above and bounded.

## Scraping

Direct Prometheus scrape:

```yaml
scrape_configs:
  - job_name: nostr-rs-relay
    static_configs:
      - targets: ['relay:8080']
    scrape_interval: 15s
```

Grafana Alloy → Grafana Cloud is wired up via `docker-compose.metrics.yml` and `alloy/config.alloy`. Required env vars: `GRAFANA_REMOTE_WRITE_URL`, `GRAFANA_USERNAME`, `GRAFANA_API_TOKEN`.
