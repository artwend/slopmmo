# ADR 004 — Trade, mail, chat and inventory live in a separate secured backend

- **Status:** Open.
- **Decided by:** project owner, first dialogue, turn 5 (restated in turn 9).
- **Verbatim:** *«Также я хочу выделить такие взаимодействия как торговлю, почту, чат и прочее в
  отдельный сервер с максимально зашешённым соединением. Это реально?»* and later *«И мы же
  договорились вынести инвентарь, торговлю, почту и чат на отдельный сервер/сервера.»*

## Context

A game simulation node is a soft-real-time process: it runs at 60 Hz, it must never block, and
it is the easiest thing in the system to attack (thousands of UDP packets from untrusted
clients). Economy operations are the opposite: transactional, auditable, latency-tolerant by
comparison, and catastrophic when duplicated or reordered.

Putting gold and items behind the same process that accepts untrusted movement packets couples
the blast radius of a crash, a memory bug or a DDoS to the economy.

## Decision

Split the backend into layers:

```
Game Client
  ├── movement channel  ──▶ Game Simulation Layer (movement, combat, AI, physics)
  └── social channel    ──▶ Social & Economy Gateway (chat, mail, trade, auction,
                            guild, friends, inventory, currency)
                                │
                                ▼
                        Backend services + persistent storage
                        (character DB, inventory DB, economy ledger, mail DB, audit log)
```

Rules that follow from the decision:

- The simulation node **never** writes inventory, currency or mail. It requests changes from the
  economy service over authenticated internal RPC/events.
- Economy mutations are **transactions** with idempotency keys; a retried request must not
  duplicate an item or a payment.
- The social channel is a **separate client connection** with its own authentication and TLS
  transport (ADR 005), so compromising the game socket does not grant economy access.
- The services scale independently of the simulation fleet.

## Consequences

- Two long-lived client connections and one identity that both must accept (a session token from
  the auth service, not "trust the game server's word").
- Cross-layer flows need explicit design: picking up loot (simulation decides, economy records),
  trading while moving, mail with an item attached, death dropping items.
- Item ownership has exactly one home: the economy ledger. The simulation may hold a cached view
  for combat stats but it is not the source of truth.
- Chat needs its own abuse/rate-limit story, now decoupled from the game's anti-cheat.

## What this repository does

Nothing. There is no auth, no database, no second channel.

## Open questions

- Loot pickup is the hard case: the simulation node must reserve the item before the player sees
  it, or two players can pick the same drop. Reservation with a short TTL is the cheap answer.
- Does combat consume consumables directly (needs a synchronous economy call in the tick loop —
  unacceptable), or does the client pre-consume through the social channel and the simulation
  only validates a token?
