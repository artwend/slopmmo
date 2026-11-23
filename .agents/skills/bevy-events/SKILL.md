---
name: bevy-events
description: Reference for events, messages, and observers in Bevy — Message/MessageReader/MessageWriter, Event/EntityEvent, triggers, observers, and propagation.
metadata:
  crate: bevy_ecs
  bevy: "0.20"
---

## Two kinds of events

1. `Message` — buffered queue, consumed next frame, good for frequent events
2. `Event` / `EntityEvent` — immediate observers, good for infrequent events with entity scope

## Messages (buffered, delayed by 1 frame)

### Defining

```rust
#[derive(Message)]
struct PlayerDetected(Entity);
```

### Registering

```rust
app.add_message::<PlayerDetected>();
app.add_message::<PlayerKilled>();
```

### Writing

```rust
fn detect(mut messages: MessageWriter<PlayerDetected>) {
  messages.write(PlayerDetected(entity));
}
```

### Reading

```rust
fn react(mut messages: MessageReader<PlayerDetected>) {
  for msg in messages.read() { }
}
```

Messages are double-buffered — systems see messages from the current and previous frame. Unconsumed messages are cleaned up and dropped silently after two updates. To skip a system entirely when there are no new messages, use `PopulatedMessageReader<T>` or the `on_message::<T>` run condition.

## Events (immediate observers)

### Defining

- `Event` — global event, defined with a `GlobalTrigger`
- `EntityEvent` — entity-scoped event, defined with an `EntityTrigger`, or with a `PropagateEntityTrigger` when propagation is enabled

```rust
#[derive(Event)]
struct GameStarted;

#[derive(EntityEvent)]
struct BossKilled { entity: Entity }
```

### Broadcast observer

```rust
fn on_respawn(event: On<Add<Enemy>>, query: Query<(&Enemy, &Position)>) {
  let (enemy, pos) = query.get(event.entity).unwrap();
}

app.add_observer(on_respawn);
```

### Entity observer

```rust
fn on_boss_killed(event: On<BossKilled>, query: Query<&Enemy>) {
  let enemy = query.get(event.entity).unwrap();
}

let entity = commands.spawn(Enemy).observe(on_boss_killed).id();
commands.trigger(BossKilled { entity });
```

### Triggering

```rust
commands.trigger(SomeEvent);
commands.trigger(SomeEntityEvent { entity });
```

### Built-in lifecycle events

| Event | Triggers when |
|-------|---------------|
| `On<Add<T>>` | Component T is added |
| `On<Insert<T>>` | Component T is inserted or overwritten |
| `On<Discard<T>>` | Component T is removed, whether or not it is replaced |
| `On<Remove<T>>` | Component T is removed without replacement |
| `On<Despawn<T>>` | Component T is despawned |

Put the observed bundle inside the lifecycle event: `On<Add<(Enemy, Person)>>` triggers when either component is added. Bevy 0.20 no longer accepts the separate `On<E, B>` bundle parameter.

## Event propagation

Propagation is opt-in. With `auto_propagate` it happens automatically;
otherwise an observer must call `On::propagate(true)`. The traversal defaults to
`ChildOf` (child to parent) and can be changed with
`propagate = &'static SomeRelationship`.

```rust
#[derive(EntityEvent)]
#[entity_event(auto_propagate, propagate = &'static ChildOf)]
struct LocationTravelled {
  #[event_target]
  entity: Entity,
}
```

An `EntityEvent` with propagation enabled bubbles up its hierarchy, stopping
when the chain ends or an observer manually stops it.

## Choosing messages vs events

| | Events | Messages |
|--|--------|----------|
| Frequency | Infrequent | Frequent |
| Latency | Immediate with `World::trigger`; next sync point with `Commands::trigger` | Up to 1 frame |
| Scope | World or Entity | World |
| Ordering | No explicit order | Ordered |
| Coupling | High | Low |
| Propagation | Bubbling | None |
