# Dialogue decisions — the owner's turns, verbatim

Three ChatGPT conversations shaped this design. Everything in
[`decisions/`](decisions/README.md) traces back to a turn listed here.

- **A** = `6b00b935-e7bc-83ed-aada-d1a9fb90f3f2` — protocol type, world sharding, transport, netcode
  library.
- **B** = `6b00b91c-f064-83ed-8c51-391e80aa2d54` — physics model, character controller, collision
  ownership, server tick.
- **C** = `6b00b8fb-d7b0-83ed-9622-4e362b120273` — movement packet consolidation.

Quotes are copied verbatim, including the owner's spelling (`зашешённым`, `денамическими`,
`четарись`). Where a decision came from the assistant's proposal, the turn says so explicitly —
these are the owner's picks, not mine.

---

## A — protocol, world, transport

### A0 — which protocol type

> «Какой тип протокола лучше выбрать для ММОРПГ с 10к+ игроками на сервере - вариант как в старой
> lineage с сообщениями, где каждый айди соответствует какому то действию или использовать
> репликацию?»

*Which protocol type should an MMORPG with 10k+ players on one server use — the old Lineage style
with messages, where each id maps to an action, or replication?*

→ **Decision (accepted from the assistant's comparison): hybrid.** Event messages where an event is
what happened, state replication where state is continuous. → ADR 001.

### A1 — 10 000 players in one PvP zone; moving "towers"

> «10 000 игроков одновременно в одной PvP-зоне - заучить заманчиво! А как называется технология
> использующаяся в сотовых сетях с handover клиента на другую вышку, но идея в том, что "вышки"
> тоже могут перемещаться.»

*10 000 players at once in a single PvP zone — tempting to learn! And what is the technology used in
cellular networks, with handover of the client to another tower, except the idea is that the "towers"
can also move?*

→ **Decision:** the target is 10 000 concurrent players in one PvP zone, and the partitioning model
is **dynamic spatial partitioning with seamless handoff and actor migration**. → ADR 002.

### A2 — Voronoi on GameLift

> «Продолжи исследования в данной область. Можно ли использовать диаграмму воронова для
> кластеризации? Предположим что сервера будут находится в amazon Gamelift.»

*Continue the research in this area. Can a Voronoi diagram be used for clustering? Assume the
servers will be hosted on Amazon GameLift.*

→ **Decision:** weighted **Voronoi / power diagram**, recomputed adaptively from load, with
GameLift game sessions as simulation nodes. → ADR 002.

### A3 — static shards for towns

> «Нужно что бы шарды для городов и других мирных локаций (нуболок) были статичными, возможно даже
> на эти локации нужно несколько шаров(каналов)»

*The shards for cities and other peaceful locations (newbie zones) need to be static; possibly these
locations even need several shards (channels).*

→ **Decision:** static shards + multiple channels for towns and starting areas. → ADR 003.

### A4 — separate secured server for social and economy

> «Также я хочу выделить такие взаимодействия как торговлю, почту, чат и прочее в отдельный сервер
> с максимально зашешённым соединением. Это реально?»

*I also want to separate interactions such as trade, mail, chat and the rest onto a separate server
with a maximally secured connection. Is that realistic?*

→ **Decision:** separate Social & Economy layer with its own secured transport and its own storage;
restated in A8. → ADR 004.

### A5 — transport for chat

> «Помоги выбрать транспорт для чата. Я предварительно выбрал quic + tls но не уверен. Распиши
> плюсы и минусы. Хотелось бы унифицыровать протоколы обмена что бы не увеличивать количество
> зависимостей.»

*Help me choose the transport for chat. I had preliminarily chosen QUIC + TLS but I'm not sure.
Write out the pros and cons. I would like to unify the exchange protocols so as not to increase the
number of dependencies.*

→ **Decision:** QUIC + TLS 1.3 for the backend; unification is achieved by one schema and one token
rather than one transport. → ADR 005.

### A6 — replicon or Lightyear

> «Для соединения клиент<-> game server я хочу использовать это
> https://docs.rs/bevy_replicon/latest/bevy_replicon/ или
> https://cbournhonesque.github.io/lightyear/book/»

*For the client ↔ game server connection I want to use this: bevy_replicon or Lightyear.*

→ Question posed; answered in A7.

### A7 — Lightyear chosen

> «Я выбираю lightyear + встроенный транспорт. Только проверь, что у него есть возможность
> реализации перемещения с помощью reliable sequential пакеты. И объ ясни модели движения. Какие
> обеспечивают наилучшую отзывчивость но при этом не будут давать преимуществ игрокам с более
> высокой частотой обновлений?»

*I choose Lightyear + its built-in transport. Just verify that it can implement movement using
reliable sequential packets. And explain the movement models. Which provide the best responsiveness
while not giving an advantage to players with a higher update rate?*

→ **Decision:** **Lightyear with its built-in transport**, plus the requirement that the movement
model must not reward a higher packet rate. → ADR 006.

### A8 — correction: movement must be unreliable

> «Я перепутал, для движения обязательно нужно использовать Unreliable. И мы же договорились
> вынести инвентарь, торговлю, почту и чат на отдельный сервер/сервера.»

*I mixed it up — movement must use Unreliable. And we did agree to move inventory, trade, mail and
chat to a separate server/servers.*

→ **Decisions:** movement on an **unreliable** channel (ADR 006, ADR 007); social and economy stay on
a separate backend (ADR 004).

### A9 — next steps

> «Что дальше?»

*What next?*

→ The assistant listed next steps, ending with a choice of physics model. That choice is B0.

---

## B — physics, controller, collision ownership

### B0 — hybrid physics

> «гибридная модель физики»

*Hybrid physics model.*

→ **Decision:** hybrid — kinematic character moved explicitly, geometry resolution delegated to a
real collide-and-slide implementation. → ADR 008.

### B1 — Avian `virtual_character` as the basis

> «Ты можешь использовать это
> https://github.com/avianphysics/avian/blob/virtual-character/src/character_controller/virtual_character.rs
> как основу.»

*You can use this as the basis.*

→ **Decision:** `MoveAndSlide` from Avian's virtual character as the geometry layer; explicitly not
`AutoMoveAndSlide`, because the automatic loop takes over the velocity. → ADR 008.

### B2 — character feel

> «Для нашего Avian-based Character Controller выбираю: Rotation: camera Air control: moderate
> Character collision: soft. Разработай подробную техническую спецификацию KCC, используя Avian
> MoveAndSlide как основу и учитывая Lightyear prediction/rollback.»

*For our Avian-based Character Controller I choose: Rotation: camera, Air control: moderate,
Character collision: soft. Develop a detailed technical specification of the KCC, using Avian
MoveAndSlide as the basis and accounting for Lightyear prediction/rollback.*

→ **Decision:** camera-relative rotation, moderate air control, soft character collision, and the KCC
must be replayable under prediction/rollback. → ADR 014, ADR 010.

### B3 — server must not solve static/dynamic collisions

> «Уточнение. Сервер не должен производить вычисления столкновений с статическими и денамическими
> объектами, это слишком затратно.»

*Clarification. The server must not perform collision computations against static and dynamic
objects; that is too expensive.*

→ **Decision:** no server-side terrain or prop collision. → ADR 009.

### B4 — server holds no landscape

> «Да у сервера не должно быть данных о ландшафте. Только коллизии с другими игроками, нпц и
> динамическими объектами, информация о которых хранится на сервере.»

*Right, the server must not have landscape data. Only collisions with other players, NPCs and
dynamic objects, information about which is stored on the server.*

→ **Decision:** the server's collider set is players, NPCs and dynamic objects; no landscape data
at all. → ADR 009.

### B5 — correction: bosses and PvP players only

> «Коррекция. На сервере должны проверяться столкновения с боссами и другими игроками в пвп режиме.
> Остальные столкновения должны обрабатывается на клиенте для эффективности.»

*Correction. The server must check collisions with bosses and with other players in PvP mode. The
remaining collisions must be handled on the client for efficiency.*

→ **Decision:** server-authoritative set = **bosses + PvP players**; everything else is client-side.
→ ADR 009.

### B6 — "Hybrid" collision model

> «Hybrid»

*Hybrid.*

→ **Decision:** all three collision modes exist, per collider, not one model for the game. → ADR 010.

### B7 — nearest only; performance before anti-cheat

> «Только с ближайшими. И ещё. Не стоит ставить борьбу с читерами во главу угла. Надо воспринимать
> читеров как неизбежность. В первую очередь всегда стоит думать о производительности а потом уже о
> читерах. Наша задача прост не дать четерись слишком просто.»

*Only with the nearest ones. And also: fighting cheaters should not be put at the centre. Cheaters
must be treated as inevitable. Always think about performance first and about cheaters after. Our
task is simply not to let cheating be too easy.*

→ **Decisions:** neighbour-bounded collision (ADR 011); **performance first, best-effort anti-cheat**
as an architectural rule (ADR 012).

### B8 — "Далее"

> «Далее»

*Next.*

→ Accepted the continuing pipeline description.

### B9 — "Продолжи"

> «Продолжи»

*Continue.*

→ Accepted the eight-phase server tick and the per-system frequency table. → ADR 013.

---

## C — movement packet

### C0 — "Продолжи"

> «Продолжи»

*Continue.*

→ Accepted three separate messages: `PlayerInput`, `MovementReport`,
`EntityCollisionCorrection`, all tick-bound, with the rule that **arrival time must not define event
time**.

### C1 — merge input and report

> «Я хочу объединить PlayerInput и MovementReport.»

*I want to merge PlayerInput and MovementReport.*

→ **Decision:** one `PlayerMovementPacket` carrying both, sent at 30 Hz while the KCC runs at 60 Hz,
with the last two inputs; Lightyear's built-in input buffer is not a good fit, a custom message type
is needed. → ADR 007.

### C2 — accepting the packet design

> «Отличное предложение»

*Excellent proposal.*

→ **Decision (accepted as a package):** Unreliable Sequenced; 30 sends/s; two consecutive input
frames per packet; one current movement snapshot; the server keeps the previous accepted snapshot for
swept collision; collision events carry their own sequence ids and are acknowledged by the client; a
redelivered correction must not re-apply an impulse. → ADR 007.

---

## Where the owner overrode or refined a decision

| Original | Override | Turn |
| --- | --- | --- |
| movement on reliable sequential | movement must be unreliable | A8 |
| three separate messages | one merged movement packet | C1 |
| server holds "dynamic objects" for collision | only bosses and PvP players are server-solved | B5 |
| one collision model | hybrid: three modes per collider | B6 |
| anti-cheat as a design driver | performance first, cheating is inevitable | B7 |

The last row is the one that shapes the architecture most: it is why the server has no terrain
(ADR 009), why corrections are velocity-only, and why every validation in `sim::authority` is
arithmetic rather than a re-simulation.
