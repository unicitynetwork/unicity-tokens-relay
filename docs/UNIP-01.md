# UNIP-01: Single-Owner Identity Bindings

`active` `optional`

Unicity NIP — a Unicity-specific extension layered on the Nostr protocol.

## Abstract

UNIP-01 defines a relay-enforced ownership model for **identity-binding
events** so that a given namespaced identifier (for example, a Unicity ID /
nametag) maps to a single, stable owner key. Ownership is determined by relay
receive order and is independent of self-asserted event timestamps, making
binding resolution deterministic and robust.

## Motivation

Unicity publishes identity bindings as Nostr parameterized-replaceable events
(NIP-33, kind `30078`): the `d` tag is a hash of the identifier
(`SHA-256("unicity:nametag:" + name)`), and the content carries routing keys
and addresses.

Two properties of standard NIP-33 make resolution non-deterministic for an
identifier that more than one key publishes a binding for:

1. **Parameterized-replaceable events are scoped per author.** The replacement
   identity is `(kind, author, d-tag)`, so bindings for the same identifier from
   different authors coexist on the relay rather than collapsing to one. The
   relay does not, by itself, designate which author "owns" the identifier.

2. **`created_at` is self-asserted.** Each event author sets its own
   `created_at` and signs over it. A client that selects among coexisting
   bindings by `created_at` (lowest or highest) is ordering on a value that the
   publisher chooses, so `created_at` is not an authoritative ownership signal
   and cannot, on its own, guarantee a stable resolution result.

UNIP-01 hardens identity resolution by giving the relay a deterministic,
receive-time-ordered notion of ownership for marked namespaces, so a namespaced
identifier resolves to one stable owner regardless of self-asserted timestamps.

## Specification

### 1. Namespace marker

An identity binding that opts into UNIP-01 MUST include:

- the standard parameterized-replaceable `["d", "<identifier-hash>"]` tag, and
- a NIP-32 label tag `["L", "<namespace>"]` naming the UNIP-01 namespace
  (e.g. `["L", "unicity:nametag"]`).

Relays maintain a configurable allowlist of UNIP-01 namespaces
(`authorization.uniqueness_namespaces`). Events that do not carry a configured
namespace marker are handled by normal Nostr/NIP-33 rules and are unaffected by
UNIP-01. Gating on the marker (rather than on `kind` alone) keeps shared kinds
such as `30078` usable by unrelated applications.

### 2. Ownership record

For each `(namespace, d-tag)` pair, a UNIP-01 relay records:

- the **owner** — the author of the first binding the relay accepts for that
  pair, and
- `first_seen` — the relay receive time of that first accepted binding.

### 3. Acceptance rule

On ingest of an event carrying a configured namespace marker, the relay accepts
it **iff**:

- no owner is recorded for `(namespace, d-tag)`, in which case the relay records
  the event's author as owner and accepts the event; **or**
- the recorded owner equals the event's author, in which case the relay accepts
  the event (the owner updating its own binding, subject to ordinary NIP-33
  replacement among that author's events).

Otherwise the relay MUST reject the event and SHOULD return a NIP-20
`["OK", <id>, false, "blocked: identifier is owned by another key"]`.

The owner assignment and the acceptance check MUST be performed atomically with
respect to concurrent writes (e.g. within the same transaction, using a unique
constraint on `(namespace, d-tag)`), so that exactly one author is recorded as
owner when multiple first-claims arrive concurrently.

### 4. Ordering

Ownership is determined solely by relay receive order (`first_seen`). The
event's `created_at` MUST NOT influence ownership.

### 5. Timestamp bounds (defense in depth)

Relays SHOULD additionally bound acceptable `created_at` values (NIP-22), both
future (`reject_future_seconds`) and past (`reject_past_seconds`), to reduce
reliance on self-asserted timestamps. This is independent of, and complementary
to, the ownership rule above.

### 6. Resolution (client behavior)

- Clients SHOULD resolve UNIP-01 namespaced identifiers only through a
  configured set of UNIP-01 relays.
- Clients MUST NOT order candidate bindings by `created_at`. Against a UNIP-01
  relay, a resolved identifier yields a single owner.
- If a client observes more than one owner for the same identifier across its
  relay set (for example, relays in differing states of convergence), it SHOULD
  treat the identifier as **ambiguous / unresolved** for value-bearing
  operations rather than selecting one heuristically.
- Clients SHOULD pin previously-resolved mappings (trust-on-first-use) and
  surface a change in the resolved owner to the user.
- Publishers SHOULD treat a `blocked:` rejection as a definitive
  "identifier already owned" result.

### 7. Migration / backfill

When enabling UNIP-01 on a relay with existing bindings, the relay SHOULD
backfill the ownership records by assigning each `(namespace, d-tag)` to the
author with the minimum `first_seen` among existing matching events. Because
`first_seen` is the relay's own receive time (not a self-asserted value), this
preserves the earliest genuine claim the relay observed.

### 8. Owner transfer (optional, future extension)

An owner MAY authorize a change of the owning key by publishing a hand-off proof
signed by the current owner key; a relay MAY honor such a proof to reassign the
ownership record. The exact proof format is left to a future revision.

## Compatibility and trust model

- UNIP-01 is **additive**: events remain valid Nostr events. A relay that does
  not implement UNIP-01 simply applies standard NIP-33 semantics; the
  single-owner property holds only across UNIP-01 relays. Clients therefore
  resolve through a known UNIP-01 relay set.
- Under UNIP-01, ownership is **asserted by the relay set**. This is a federated
  trust model: the guarantee is as strong as the agreement and integrity of the
  relays a client trusts.
- Deployments that require a trust-minimized authority can anchor ownership in an
  external source (e.g. an on-chain identity registry) and treat the Nostr
  binding as an advertisement that the client verifies against that source. That
  is a stronger model and is out of scope for UNIP-01; UNIP-01 and such an
  anchor compose, with the relay rule serving as a fast first-line check.

## Reference implementation

- Relay enforcement: `unicity-tokens-relay` — `namespace_owner` table and the
  acceptance check in `persist_event` (`src/repo/sqlite.rs`, `src/repo/postgres.rs`),
  configuration in `src/config.rs`, timestamp bounds in `src/event.rs`.
- Client emission and resolution: `nostr-js-sdk` and `nostr-sdk` (Java) — the
  `["L", "unicity:nametag"]` marker on binding events and receive-order-based
  resolution. The two SDKs MUST remain behaviorally identical.
