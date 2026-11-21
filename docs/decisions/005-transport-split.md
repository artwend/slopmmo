# ADR 005 — QUIC + TLS 1.3 for backend traffic, UDP netcode for the game channel

- **Status:** Open.
- **Decided by:** project owner, first dialogue, turn 6 (still a preference, never overridden).
- **Verbatim:** *«Помоги выбрать транспорт для чата. Я предварительно выбрал quic + tls но не
  уверен. … Хотелось бы унифицыровать протоколы обмена что бы не увеличивать количество
  зависимостей.»*

## Context

The owner's prior was QUIC + TLS, with two constraints: do not accumulate dependencies, and
ideally have **one** protocol family for everything. The comparison at the time:

| | QUIC | WebSocket | gRPC |
| --- | --- | --- | --- |
| Transport | UDP | TCP | TCP (HTTP/2) |
| Encryption | TLS 1.3 mandatory | optional | TLS |
| Multiplexing | yes, independent streams | no | yes, one TCP stream |
| Connection migration | yes | no | no |
| Unreliable delivery | datagrams | no | no |
| Head-of-line blocking | none between streams | yes | yes |
| Browser | via WebTransport | native | via gRPC-Web |
| Implementation cost | high | low | medium |

## Decision

- **Social & Economy layer: QUIC + TLS 1.3.** Multiplexing (chat does not block a trade
  request), connection migration (mobile clients changing networks), mandatory encryption, and a
  datagram extension if a low-latency social signal ever needs it.
- **Game channel: Lightyear's UDP netcode** (ADR 006). Not QUIC, even though QUIC datagrams could
  carry it: the simulation needs per-tick sequencing, redundancy, prediction history and
  snapshot delta compression, which is netcode, not transport.
- **Unify the schema, not necessarily the wire.** Both channels share the same serialization
  approach (compact binary, no JSON) and the same identity/session token, so there is one
  protocol *family* even though there are two connections.

Rationale for not forcing one transport: the dependency-count argument is satisfied by having one
serialization scheme and one auth token. Making the 60 Hz movement stream ride on QUIC would add
a second reliability/ordering layer on top of one that already exists in the netcode.

## Consequences

- Two client connections (see ADR 004), one token, one schema.
- QUIC in Rust means `quinn` (or `s2n-quic`); hosting it behind GameLift's UDP ports is fine, but
  the Social & Economy Gateway is a separate deployment, not a GameLift game session.
- TLS certificate management becomes a real operational concern (rotation, pinning, and whether
  clients pin a CA).
- Internal service-to-service traffic uses the same stack (QUIC/gRPC over TLS with mTLS), not
  plaintext, because it crosses machine boundaries.

## What this repository does

Nothing: the prototype's UDP socket is plain, unencrypted, and unauthenticated — acceptable only
because it binds to `127.0.0.1`.

## Open questions

- Does the client keep the QUIC connection open while playing, or connect lazily when a social
  panel opens? Lazy costs a round trip at the worst moment; always-on costs battery on mobile.
- Web client support: WebTransport-only would remove WebSockets from the design entirely, at the
  cost of browser maturity.
