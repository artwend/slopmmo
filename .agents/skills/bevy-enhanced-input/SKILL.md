---
name: bevy-enhanced-input
description: Practical guide to bevy_enhanced_input (simgine/bevy_enhanced_input), the observer-based, Unreal-Enhanced-Input-style input manager for the Bevy engine. Use this skill whenever the user mentions bevy_enhanced_input, BEI, EnhancedInputPlugin, InputAction, add_input_context, actions!/bindings! macros, On<Fire<..>>, Start/Fire/Complete/Cancel action events, Action<A>, ActionOf, Bindings, ContextActivity, ContextPriority, input conditions (Hold, Tap, Chord, Combo, Pulse, Cooldown...) or input modifiers (DeadZone, Scale, Negate, SwizzleAxis, SmoothNudge, DeltaScale...), or asks how to bind keys, mouse, or gamepad input to actions in Bevy, build rebindable controls, layer input contexts (on foot / in car / menu), mock input for tests or AI, or migrate bevy_enhanced_input across Bevy versions, even if they only paste code using `Action::<Move>::new()` or `#[action_output(Vec2)]`. Also use it when reviewing or debugging Bevy character controllers, camera controls or fly-cams that read input through this crate.
---

# bevy_enhanced_input (BEI)

Observer-based input manager for Bevy. Design is borrowed from Unreal Engine's Enhanced Input: physical inputs are **bindings**, bindings feed **actions**, actions live in **contexts**. Everything is an ordinary ECS entity or component.

Repo: `github.com/simgine/bevy_enhanced_input` · Docs: `docs.rs/bevy_enhanced_input` · Dual MIT/Apache-2.0 · `#![no_std]` · Rust 2024.

---

## 0. First step: pin the version

The API has had many breaking renames. **Before writing code, check `Cargo.toml` / `Cargo.lock` and match the table.** If the user has no version yet, default to the newest that matches their Bevy.

| bevy   | bevy_enhanced_input |
| ------ | ------------------- |
| 0.20   | 0.27                |
| 0.19   | 0.26                |
| 0.18   | 0.21–0.25           |
| 0.17   | 0.19–0.20           |
| 0.16   | 0.11–0.18           |
| 0.15   | 0.4–0.10            |
| 0.14   | 0.1–0.3             |

This skill documents the **0.21+ API** (examples verified against master, 0.27). Differences worth knowing:

| Era | Looks like | Notes |
| --- | --- | --- |
| ≥0.23 | `ActionEvents::START / FIRE / CANCEL / COMPLETE` | bitflags renamed from `STARTED/FIRED/...` in 0.23 |
| ≥0.19 (Bevy 0.17) | `On<Fire<A>>`, `On<Start<A>>`, `On<Complete<A>>`, `On<Cancel<A>>` | renamed from `Trigger<Fired<A>>`, `Started`, `Completed`, `Cancelled`. Deprecated aliases (`Started<A>`, `ActionState`) may still compile with warnings |
| ≥0.21 | `#[derive(InputAction)] #[action_output(Vec2)]` | older: `#[input_action(output = Vec2)]` |
| ≥0.21 | `TriggerState` component | `ActionState` is a deprecated alias |
| 0.27 | BSN support; `EnhancedInputSystems::Apply` split into `Apply` + `Trigger` | see §12 |

Default cargo features: `reflect`, `state`. Optional: `serialize` (serde). Bevy is pulled in with only `mouse`, `keyboard`, `gamepad`, `touch` input features.

```toml
[dependencies]
bevy = "0.20"
bevy_enhanced_input = "0.27"
```

If the user is on an older version, say which API you are using and avoid mixing eras (e.g. never `On<Fire<A>>` with `#[input_action(..)]`).

---

## 1. Mental model

- **Action**: something the player can do (`Jump`, `Move`, `OpenMenu`). A Rust type implementing `InputAction`, spawned as an entity with component `Action<A>`. Output type is one of `bool`, `f32`, `Vec2`, `Vec3`.
- **Binding**: a physical source (key, mouse button, mouse motion/wheel, gamepad button/axis, ...). An entity with component `Binding`, related to an action via `BindingOf` / `Bindings`.
- **Context**: a component (marker or data) on some entity that says "these actions are evaluated for this entity". Actions are related to it via `ActionOf<C>` / `Actions<C>`. Contexts can be toggled (`ContextActivity<C>`) and ordered (`ContextPriority<C>`).
- **Modifiers** transform values; **conditions** decide when an action is considered triggered. Both are plain components, attachable to an action or to an individual binding.
- Reacting: **push** (observers on `Start/Fire/Complete/...` events) or **pull** (query `Action<A>`, `TriggerState`, `ActionEvents`).

Entity layout:

```
player entity ── Player (context component) ── Actions<Player> ──┬─ action entity: Action<Jump> + [conditions/modifiers] ── Bindings ──┬─ binding entity: Binding::Keyboard(Space)
                                                                 │                                                                    └─ binding entity: Binding::GamepadButton(South)
                                                                 └─ action entity: Action<Move> ...
```

Despawning the context entity despawns its actions and bindings.

---

## 2. Minimal working setup

```rust
use bevy::prelude::*;
use bevy_enhanced_input::prelude::*;

fn main() {
    App::new()
        .add_plugins((DefaultPlugins, EnhancedInputPlugin))
        .add_input_context::<Player>()      // every context type must be registered
        .add_observer(on_move)
        .add_observer(on_jump)
        .add_systems(Startup, setup)
        .run();
}

#[derive(Component)]
struct Player;                              // context marker

#[derive(InputAction)]
#[action_output(Vec2)]
struct Move;

#[derive(InputAction)]
#[action_output(bool)]
struct Jump;

fn setup(mut commands: Commands) {
    commands.spawn((
        Player,
        actions!(Player[
            (
                Action::<Move>::new(),
                DeadZone::default(),                       // modifiers/conditions are components
                Bindings::spawn((
                    Cardinal::wasd_keys(),                 // preset: 4 keys -> Vec2
                    Axial::left_stick(),                   // preset: stick -> Vec2
                )),
            ),
            (
                Action::<Jump>::new(),
                bindings![KeyCode::Space, GamepadButton::South],
            ),
        ]),
    ));
}

fn on_move(trigger: On<Fire<Move>>, mut q: Query<&mut Transform>) {
    // `context` = entity that holds the context component, `value` = action output type
    let mut t = q.get_mut(trigger.context).unwrap();
    t.translation += trigger.value.extend(0.0);
}

fn on_jump(_t: On<Start<Jump>>) { /* jumped this frame */ }
```

Checklist that fixes ~90% of "nothing happens" reports:
1. `EnhancedInputPlugin` added.
2. `app.add_input_context::<C>()` called for every context type.
3. The context component `C` is on the same entity that carries `actions!(C[...])`.
4. Observers use the right event (`Fire` for held/continuous, `Start` for press-edge, `Complete` for release-edge).
5. Plugin was added **before** adding condition/modifier registrations for custom types (see §10).

---

## 3. Actions

```rust
#[derive(InputAction)]
#[action_output(f32)]
struct Zoom;
```

- The derive's only attribute is `action_output(T)` with `T ∈ {bool, f32, Vec2, Vec3}`.
- Implementing `InputAction` manually is possible (`InputAction::Output`); the derive is the norm.
- Bindings produce values of whatever dimension the source has (key = bool, axis = f32, stick/mouse motion = Vec2). Values are converted to the action's output dimension via `ActionValue::convert`. Use `SwizzleAxis` to choose *which* axis survives when narrowing (e.g. mouse wheel's Y → a 1‑D action).
- `Action<A>` stores the current value and derefs to the output type; `ActionValue` is the dynamically typed value.
- **`ActionSettings`** (component on the action):
  - `consume_input` (default true): when an action triggers it consumes its binding's input so lower-priority actions bound to the same input won't see it. Turn off to let several actions share an input.
  - `require_reset`: the binding must be released first before it counts (prevents carry-over when contexts switch).
  - `accumulation` (`Accumulation`): how values from multiple simultaneously-active bindings combine (e.g. cumulative vs max-abs).
- Action-level conditions/modifiers apply to the combined value; binding-level ones apply per binding (see §5).

Spawn forms:

```rust
actions!(Player[ (Action::<Jump>::new(), bindings![KeyCode::Space]) ])    // macro, context type given
// or dynamically, inside a system:
commands.entity(player).with_related::<ActionOf<Player>>(( Action::<Jump>::new(), bindings![KeyCode::Space] ));
```

When despawned or when its context is removed/deactivated, an action transitions to `TriggerState::None` with a zero value, firing the corresponding events (usually `Cancel`/`Complete`).

---

## 4. Bindings and presets

`bindings![...]` accepts anything convertible to a binding bundle: `KeyCode::*`, `MouseButton::*`, `GamepadButton::*`, `GamepadAxis::*`, and `Binding` values.

```rust
bindings![KeyCode::Space, GamepadButton::South]
Bindings::spawn((
    Spawn(Binding::mouse_motion()),                    // single entities need Spawn(...)
    Spawn((Binding::mouse_wheel(), SwizzleAxis::YXZ)), // per-binding modifier
    Cardinal::wasd_keys(),                             // preset (a SpawnableList)
    Axial::right_stick().with((Scale::splat(100.0), Negate::x())), // modifiers on every binding of the preset
    Bidirectional::new(GamepadButton::DPadUp, GamepadButton::DPadDown), // +1 / -1 on one axis
))
```

Notes:
- `Binding` is an enum covering keyboard, mouse buttons, mouse motion, mouse wheel, gamepad buttons/axes, touch-related sources and `Binding::AnyKey` (any button; added in 0.18). Look at the `Binding` enum docs for the full list and for modifier-key constructors (`ModKeys`, `InputModKeys`).
- Keyboard modifier chords (Ctrl+X): the context sorts an action's candidates so bindings with *more* modifier keys are evaluated first, so `Ctrl+X` correctly wins over plain `X`.
- Presets in the prelude: `Cardinal` (4 directions, e.g. `wasd_keys()`), `Axial` (sticks, e.g. `left_stick()`, `right_stick()`), `Bidirectional`, `Ordinal` (8 directions), `Spatial` (6 directions, 3‑D). Check the `preset` module for the exact constructor list (arrow keys, d-pad, etc.) before inventing names.
- Gamepad selection: by default input is read from **all** connected gamepads. Add `GamepadDevice::Single(entity)` / `GamepadDevice::None` / `GamepadDevice::Any` to the *context entity* for per-player pads.
- Custom (non-Bevy) inputs: `CustomInput` / `CustomInputs` resource (see docs.rs `context::input_reader::custom`).

---

## 5. Evaluation pipeline (what actually happens each frame)

For each active context (ordered by schedule, then `ContextPriority` descending, then reverse spawn order) and each action in it (in insertion order; actions whose bindings use more modifier keys first):

1. If the action has an `ActionMock` → use the mocked value/state directly. Otherwise:
2. For each binding: read value (zero if already consumed by an earlier action) → apply **binding-level modifiers** → evaluate **binding-level conditions**.
3. Take the bindings with the most significant `TriggerState`, combine values via `ActionSettings::accumulation`.
4. Convert to the action's output dimension.
5. Apply **action-level modifiers**, then **action-level conditions**; convert again.
6. Write `ActionValue`/`TriggerState`/`ActionEvents`/`ActionTime`.
7. If final state ≠ `None` and `consume_input`, consume the used binding inputs.

If an action has **no conditions**, it behaves as if it had `Down` with zero threshold (fires every frame the value is non-zero / button down).

System sets (in each schedule where a context is registered): `EnhancedInputSystems::Prepare` (PreUpdate) → `Update` (read inputs, evaluate) → `Apply` (write to components, trigger events). In 0.27 `Apply` is split into `Apply` (components updated) and `Trigger` (observers fire). Order your own systems relative to these sets when you poll action components.

Context registration schedule: `add_input_context::<C>()` evaluates in `PreUpdate`. For gameplay in the fixed timestep use `add_input_context_to::<FixedPreUpdate, C>()`. Then `Start`/`Complete` fire once per schedule run instead of being duplicated or lost when `FixedUpdate` runs 0 or >1 times per frame.

---

## 6. Modifiers (transform values)

Attach as components to an **action** or to a **binding** entity. They run in the order they appear in the bundle.

| Modifier | Use |
| --- | --- |
| `DeadZone` | Ignore small stick values; also normalizes non-uniformly so diagonals aren't faster (works for digital + analog) |
| `Scale` (`Scale::splat(10.0)`, per-axis ctor) | Multiply by constant (speed, sensitivity) |
| `DeltaScale` | Multiply by frame delta time, for frame-rate independent continuous input (movement; also mouse motion in the official example) |
| `Negate` (`Negate::all()`, `Negate::x()`, ...) | Flip sign of chosen axes |
| `SwizzleAxis` (`YXZ`, `ZYX`, ...) | Reorder axes (e.g. wheel Y → X of a 1‑D action; map W/S to Y) |
| `SmoothNudge` | Exponential smoothing |
| `ExponentialCurve` | Non-linear response curve |
| `LinearStep` | Quantize/step values |
| `Clamp` | Clamp values |
| `AccumulateBy` | Accumulate this action's value by another action |

Typical stacks:
- Fly-cam/character move: `DeadZone` + `SmoothNudge` + `DeltaScale` + `Scale::splat(speed)`.
- Mouse look: binding `Binding::mouse_motion()` + `Negate::all()`; stick look: `Axial::right_stick().with((Scale::splat(100.0), Negate::x()))`; plus `DeltaScale` on the action if you want rate-based.
- Zoom: wheel + `SwizzleAxis::YXZ`, `Scale::splat(0.1)`.

Verify constructor signatures in the `modifier` module docs before relying on a specific variant.

---

## 7. Conditions (decide *when* an action triggers)

Prelude conditions: `Down`, `Press`, `Release`, `Hold`, `HoldAndRelease`, `Tap`, `Pulse`, `Chord`, `Combo`, `Cooldown`, `Toggle`, `Flick`, `BlockBy`.

| Condition | Gist |
| --- | --- |
| `Down` | Fire every update while active (the default when none are set) |
| `Press` | Fire once on the press edge |
| `Release` | Fire once on release |
| `Hold` | `Ongoing` while held, `Fire` after the duration elapses |
| `HoldAndRelease` | Fire on release *after* the hold duration was met |
| `Tap` | Fire on release if released quickly enough |
| `Pulse` | Repeated fires at an interval while held |
| `Chord` | Requires other action(s) to be active (multi-key combos / "modifier actions") |
| `Combo` | Ordered sequence of steps with optional cancel actions (references other actions by entity; 0.27 BSN supports named entity references) |
| `Cooldown` | Rate-limit after firing |
| `Toggle` | Latch on/off |
| `Flick` | Quick directional flick |
| `BlockBy` | Suppress this action while another is active |

Conditions have a `ConditionKind`: *explicit* (must pass, ANDed), *implicit* (at least one must pass, ORed) and *blocker* (can only cancel, never contribute to firing; events still trigger). Combine them freely on one action.

Pattern: charge attack with `Hold`:
- `On<Complete<Attack>>` (or `Fire` for `HoldAndRelease`) → strong attack;
- `On<Cancel<Attack>>` → released early → regular attack.

Check constructors (`Hold::new(secs)`-style vs builder methods) in the `condition` module docs of the pinned version.

---

## 8. Reacting to actions

### Events (push)

All events are `EntityEvent`-style targeted at the **context entity**, observed with `On<E>`.

| Event | Meaning |
| --- | --- |
| `Start<A>` | Began triggering (press edge). Fired before `Fire`/`Ongoing` |
| `Ongoing<A>` | In progress but not yet firing (e.g. holding before duration is met) |
| `Fire<A>` | Triggering now (every update for `Down`) |
| `Complete<A>` | Stopped after having fired (release) |
| `Cancel<A>` | Stopped while only `Ongoing` (aborted before firing) |

State → event transitions: `None→Ongoing` = `Start`+`Ongoing`; `None→Fired` = `Start`+`Fire`; `Ongoing→Fired` = `Fire`; `Fired→Fired` = `Fire`; `Ongoing→None` = `Cancel`; `Fired→None` = `Complete`. Transitions from a state to itself also emit events.

Event fields you will use: `.context` (Entity holding the context component), `.value` (the action's output type, e.g. `Vec2`), plus state and timing metadata (see the `action::events` docs).

```rust
fn apply_movement(m: On<Fire<Move>>, mut tf: Query<&mut Transform>) {
    let mut t = tf.get_mut(m.context).unwrap();
    let v = m.value.extend(0.0).xzy();          // Vec2 -> Vec3 on XZ plane
    t.translation += t.rotation * v;
}
```

Observers are global; filter by `context` entity if you have several players. For "one controller entity drives another entity" setups, store the target in a component on the context entity.

### Pull (query)

```rust
fn read(
    jump: Single<&ActionEvents, With<Action<Jump>>>,
    mv:   Single<&Action<Move>>,
    mut tf: Single<&mut Transform, With<Player>>,
) {
    if jump.contains(ActionEvents::START) { /* jumped this frame */ }
    tf.translation += mv.extend(0.0);        // &Action<Move> derefs to Vec2
}

// multi-context / multi-player
fn read_all(
    jumps: Query<&ActionEvents, With<Action<Jump>>>,
    moves: Query<&Action<Move>>,
    players: Query<(&mut Transform, &Actions<Player>)>,
) { /* jumps.iter_many(actions).next(), moves.iter_many(actions).next() */ }
```

Components: `Action<A>` (typed value), `ActionValue` (dynamic), `TriggerState`, `ActionEvents` (bitflags: `START | ONGOING | FIRE | CANCEL | COMPLETE`, records the last transition), `ActionTime` (elapsed/fired time). Bevy change detection works; components only change when values actually change.

Use push for simple one-action logic, pull when one system needs several actions at once.

---

## 9. Contexts in depth

```rust
app.add_input_context::<OnFoot>()
   .add_input_context::<InCar>();

commands.spawn((
    OnFoot,
    InCar,
    ContextPriority::<InCar>::new(1),        // higher = evaluated first; default 0
    ContextActivity::<InCar>::INACTIVE,      // toggle without removing
    actions!(OnFoot[ /* ... */ ]),
    actions!(InCar [ /* ... */ ]),
));
```

- `ContextActivity<C>`: `ACTIVE` / `INACTIVE` / `.toggled()` (immutable component: re-insert to change). Making a context inactive resets its actions (events fire) and, for actions with `require_reset`, forces bindings to be released before other contexts can see them. Prefer it over removal when toggling frequently (menu open/close, vehicle enter/exit).
- `ContextPriority<C>`: ordering matters because actions *consume* input. Put the overlay context (menu, vehicle) at higher priority than the base one.
- Removing a context from an entity (but keeping the entity): `entity.remove_with_requires::<C>().despawn_related::<Actions<C>>()`. Actions are **not** auto-despawned since `Actions<C>` isn't a required component.
- With the `state` feature: `ActiveInStates` + `StateContextAppExt` enable contexts only in specific Bevy `States` (check docs for the exact API).
- UI interplay: BEI doesn't integrate `bevy_input_focus`. The `ActionSources` resource lets you disable input sources (keyboard/mouse/gamepad) while UI is focused so gameplay actions don't trigger.
- Multiple players: spawn one context entity per player, set `GamepadDevice::Single(pad_entity)` on each; filter observers by `trigger.context`.
- Controller-entity pattern (character controllers like KCC crates): context lives on a "controller"/player entity and observers apply results to the controlled entity — good for AI/NPCs using the same actions via mocking.

---

## 10. Extending: custom conditions & modifiers

Implement `InputCondition` / `InputModifier` on a `Component`, then register on the app:

```rust
app.add_input_condition::<MyCondition>()   // InputConditionAppExt
   .add_input_modifier::<MyModifier>();    // InputModifierAppExt
```

Registration must happen **before `App::finish`** (the plugin builds per-schedule systems in `finish` from the registries). Built-in ones are registered by `EnhancedInputPlugin`. Condition `kind()` returns explicit/implicit/blocker as in §7; conditions get access to the action's value, delta time and other actions' state.

Custom bindings: `CustomInputs` resource lets you feed values from sources Bevy doesn't expose.

---

## 11. Mocking (tests, cutscenes, AI, replicated input)

- Add `ActionMock` to an action entity to drive its `ActionValue` and `TriggerState` directly instead of reading bindings. `MockSpan` controls duration: `Updates(n)`, `Duration(d)`, or `Manual` (stays until you disable it).
- `ExternallyMocked` marker: skip evaluation for this action entirely; you write its components yourself.
- Helpers live on `MockEntityCommandsExt` / `MockEntityWorldMutExt` (prelude). Mocking works even without Bevy's `InputPlugin`, so headless tests are easy: build `App::new().add_plugins(EnhancedInputPlugin).add_input_context::<C>()`, spawn the context, mock the action, `app.update()`, assert on events/state.
- The crate is commonly paired with replication crates (the same org makes `bevy_replicon`): send action values over the network and re-apply them as mocks on the server.

---

## 12. BSN (0.27 / Bevy 0.20)

0.27 added BSN support: actions, bindings, conditions and modifiers can be declared in BSN scenes, with named entity references for cross-action references (e.g. `Combo` steps/cancel actions). Exact BSN syntax tracks Bevy's still-evolving BSN API, so **read the repo's BSN example and `condition/combo.rs` docs for the pinned version** rather than guessing the syntax.

---

## 13. Common pitfalls

- **Forgot `add_input_context::<C>()`** → no events, no panic.
- **`Fire` vs `Start`**: with default `Down`, `Fire` runs every update while held (use for movement); use `Start`/`Press` for one-shot actions (jump, interact), `Complete` for "on release".
- **Continuous values + frame rate**: add `DeltaScale` for per-second quantities. Don't also multiply by `time.delta_secs()` in the observer.
- **Diagonal speed-up**: use `DeadZone` (it normalizes) on movement actions; digital WASD gives |v|=√2 diagonally otherwise.
- **Two actions on one key**: first-evaluated wins (input consumption). Fix with context priority, action order, `ActionSettings::consume_input = false`, or `BlockBy`/`Chord` conditions.
- **Switching contexts and inputs "sticking"**: use `ContextActivity` and `ActionSettings::require_reset`.
- **Fixed-timestep gameplay**: register the context with `add_input_context_to::<FixedPreUpdate, C>()`, otherwise edge events can be missed/duplicated relative to `FixedUpdate`.
- **Narrowing dimensions**: wheel and mouse give Vec2; for `f32` actions pick the axis explicitly with `SwizzleAxis`.
- **Removing a context component leaves orphan actions** unless you also `despawn_related::<Actions<C>>()`.
- **Old tutorials/blog posts** use `Trigger<Fired<A>>`, `#[input_action(output = ..)]`, `ActionState`, `ActionEvents::STARTED`. Translate them with the table in §0. (Note: some doc comments in released versions still show `ActionEvents::STARTED`; the real constant since 0.23 is `START`.)
- **Debugging**: `RUST_LOG=bevy_enhanced_input=debug cargo run` (or `=trace`) logs registration, evaluation and per-action states.

---

## 14. How to use this skill when answering

1. Identify Bevy + BEI versions (ask only if truly unknowable; otherwise state the assumed version).
2. Give a complete minimal snippet: action types, context component, `add_input_context`, spawn with `actions!`/`bindings!`, observer or query.
3. For anything not listed above (exact constructor of a condition/modifier, a specific `Binding` variant, mock API, state integration, BSN syntax), **look it up** (docs.rs for the pinned version, or the repo's `examples/`) instead of guessing. Useful starting points in the repo: `examples/basic_action_management.rs` (fly-cam: WASD + sticks, mouse look, zoom, cursor capture), plus the other files in `examples/` and `CHANGELOG.md` for per-version breaking changes.
4. When reviewing a user's code, check in order: version/API era → context registered → context component on same entity as `actions!` → right event for the intent → modifiers for frame-rate/diagonal → input consumption conflicts → schedule choice for fixed timestep.
