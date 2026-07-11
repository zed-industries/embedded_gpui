# GPUI Embedded in GPUI

An experimental spike: run GPUI itself inside a Wasm component (`wasm32-wasip2`), and embed
its rendered output inside a native GPUI host application. This models a future "UI
extensions" system for Zed and exists to hammer out the guest-side `gpui_plugin` platform.

This repository is standalone: it consumes `gpui` and `gpui_platform` as git dependencies
on the zed repository (currently the `gpui-embedded-in-gpui` branch, which carries the one
upstream hook the guest runtime needs: `Application::run_embedded`, which returns an
`ApplicationHandle` so an embedder whose `Platform::run` returns immediately can keep the
app alive and re-enter it whenever the external run loop yields control).

## Layout

- `embedded_gpui/` — **one crate, both sides of the boundary**:
  - `wit/plugin.wit` — the wire protocol (package `gpui:embedded`, world `plugin`); the
    single source of truth both sides bind against.
  - `src/embedded_gpui.rs` — the always-compiled object layer: `Remote`, `Receipt`,
    `Ref`, specs/messages/events, and the `Shared` home trait.
  - `src/host.rs` (+ `src/host/`) — native targets only: wasmtime glue, host-side shared
    entities, and the element that replays guest display lists.
  - `src/guest.rs` (+ `src/guest/`) — wasm32 targets only: GPUI's
    `Platform`/`PlatformWindow`/`PlatformDispatcher`/`PlatformTextSystem`/`PlatformAtlas`
    over the WIT boundary, `Plugin`/`register_plugin!`, and the guest half of shared
    entities.
- `embedded_gpui_macros/` — the `#[shared_interface]` / `#[shared]` / `#[shared_data]`
  proc macros.
- `embedded_gpui_util/` — side-agnostic OCAP patterns (`Revocable`, `Attenuated`,
  `Audited`, `Mirror`) built on `Remote`.
- `example/` — the demo pair: `host/` (native window, `cargo run -p example_host`;
  builds the plugin automatically) and `plugin/` (the wasm component, its own workspace
  since it only compiles to `wasm32-wasip2`).
- `tests/` — the host-driven integration tests for the shared-entity protocol, with
  their guest fixture in `tests/test_plugin/`.

## Architecture (agreed invariants)

1. **The host never calls into the guest synchronously from the frame path.** The guest
   renders when *it* is ticked; output is a retained display list the host caches in an
   entity and replays cheaply every host frame.
2. **The wasmtime store lives on a background worker.** The host never calls wasm from
   the UI thread: every interaction is a queued request to the worker that owns the
   store (strictly one call at a time), and each call's effects are applied back on the
   foreground in the same order. A slow or hung plugin cannot stall the UI; FIFO
   request/effect pairing preserves all ordering guarantees below.
3. **Re-entrancy is forbidden by the component model.** Guest imports (`request-tick`,
   `update-scene`, …) must NOT call back into the guest. Host import implementations only
   mutate state on the wasmtime `Store`'s data; the host drains that pending state after
   each guest call returns and acts on it then.
4. **Text is shaped and rasterized by the host.** The guest's `PlatformTextSystem` proxies
   shaping over imports (with guest-side caching via GPUI's own `LineLayoutCache`). The
   guest never rasterizes; its sprite atlas fabricates tiles and remembers
   `tile -> RenderGlyphParams` so the scene serializer can emit symbolic `glyph` primitives.
   The host replays those through `Window::paint_glyph` / `paint_emoji`, hitting the host's
   real atlas, rasterizer, and gamma handling.
5. **Coordinates on the wire are logical pixels, slot-relative.** The guest divides its
   `ScaledPixels` scene values by the scale factor when serializing. The host adds the
   slot origin and paints through public `Window::paint_*` APIs, which re-apply scaling,
   snapping, and the host's content mask. Guest content masks are re-applied via
   `Window::with_content_mask` after translating, intersected with the slot bounds.
6. **Z-order**: guest primitives carry their scene `order` (u32). The host replays groups of
   ascending `order` inside `Window::paint_layer` calls so each group gets a fresh host
   order, preserving guest stacking (including guest-side deferred draws / overlays).
7. **Input**: the host forwards raw mouse events (slot-relative logical coordinates) to the
   guest via `handle-mouse`; the guest window's own dispatch does hit-testing and runs
   listeners. No callback registry crosses the boundary. Cursor styles flow back via the
   `set-cursor-style` import.
8. **Scheduling**: the guest dispatcher queues runnables/timers locally and asks the host
   for wakeups via `request-tick(delay-ms)`. The host calls the `tick` export, which drains
   due work and then pumps each plugin window's `request_frame` callback (GPUI itself
   decides whether a window is dirty and needs to redraw; a redraw ends in
   `PlatformWindow::draw(scene)`, which serializes and calls `update-scene`).

## Status

Working end to end on macOS: quads (rounded corners, borders), text (host-shaped and
host-rasterized via symbolic glyph replay, including wrapping and exact subpixel-variant
positioning), tessellated paths, images (premultiplied-BGRA payloads shipped once, cached
per instance), SVGs (guest-rasterized alpha masks, tint baked per color), keyboard input
(host focus → forwarded keystrokes → guest focus dispatch, with unhandled printable keys
falling through to the focused `EntityInputHandler`, Linux-backend style), hover styles,
mouse input, cursor styles, and shared entity state across two plugin views backed by one
guest App. The release component (all of gpui + taffy, no fonts, no glyph rasterizers) is
~3.8 MB.

Run it:

```sh
cargo run -p example_host
```

## Why two type systems? (WIT and the object model)

A fair question: the WIT interface is a type system, and the shared-entity schema layer
is another. Why both? Because they type different things, with opposite change profiles:

- **WIT is the syscall boundary** — display lists, input, text shaping, scheduling, and
  the handful of functions that move opaque entity traffic. It changes when the
  *platform* changes: rarely, owned by one team, with hard commitments (a signature
  mismatch fails instantiation outright). That hardness is right for the substrate and
  wrong for an app API. Note the WIT here is already almost entirely machine protocol;
  the whole object model rides on eight small functions with opaque payloads.
- **The object model is userspace** — the evolving semantic surface (what a host app
  exposes, what plugins expose to each other), side-blind and peer-to-peer (see
  "Symmetry" below), with soft, runtime-negotiated
  commitments: unknown methods fail as handleable errors, payload fields evolve by serde
  defaulting, a plugin built against an old schema degrades at specific calls instead of
  failing to load. Evolution is a library release, not a flag day: old and new methods are just
  entries in the same dispatch table. Wayland-style version negotiation is expressible
  as a plain shared entity (a registry whose `list` call returns interface/version/ref) —
  and the bootstrap primitive already exists as the root object (Wayland's object 1,
  Cap'n Proto's bootstrap capability), with refs (objects) below it.

Two things the static layer structurally cannot express, which is why "just put it all
in WIT" loses: **capability semantics** (WIT imports are ambient and identical for every
plugin; per-plugin grants, attenuation, revocation, and refs minted at runtime require
the dynamic layer) and **ecosystem growth** (two plugins agreeing on a new interface via
a shared schema crate, without the host knowing or any world recompilation).

The honest trade: dynamic calls are slower than generated WIT functions (serde plus
string dispatch). The split encodes the rule — hot or foundational goes in WIT (display
lists, input, text: already there), evolving semantics go through objects — and since
the object wire is just bytes, encodings are swappable and any method that gets hot can
be promoted into WIT. Precedents for the two-layer shape: syscalls vs. D-Bus, TCP vs.
HTTP APIs, Wayland's fixed wire vs. versioned interfaces.

## Performance philosophy

The design picks slow-and-flexible only where humans are the clock, and
fast-and-static where the GPU is the clock:

- **The frame path contains none of the flexible machinery.** A quiescent plugin costs
  ~nothing per frame: the host replays a retained display list through gpui's normal
  paint path — no wasm call, no serialization, no dispatch. Animating views cost one
  wasm render plus one binary display-list ship per *dirty* frame. Pixels, text, and
  input never touch JSON or string dispatch.
- **The control path is slow only by hot-loop standards.** A shared-entity call is
  serde_json on a small payload, a HashMap method lookup, and a few executor turns of
  queueing — unmeasurable at human interaction rates, and far cheaper than the
  inter-process JSON-RPC that the largest existing extension ecosystem (VS Code) runs
  on without anyone feeling it.
- **Every flexible choice has a named escape hatch.** The wire is bytes, so the payload
  encoding is swappable per schema (bincode when JSON shows up in a profile); reads are
  pull-based, so chatty state costs exactly what the reader asks for (notifies are
  idempotent and coalesce; a mirror folds any burst into one trailing fetch); anything
  genuinely hot can be promoted into a WIT function; large surfaces get per-region damage
  tracking. Flexibility was never bought in a way that forecloses speed.
- **Known risks, ranked**: display-list volume for large animated views, serde_json
  under chatty state, and turn-taking latency in deep synchronous call chains. All are
  "optimize when observed"; none are architectural.

## Shared entities

Entities cannot literally cross the boundary (separate linear memories, separately compiled
types), so shared state is built on three rules:

1. **One home per entity.** The home side owns the state as a normal GPUI entity and is
   the only holder of it. The other side holds `Remote<S>` handles: they call methods on
   the home, observe its `cx.notify`, and subscribe to its `cx.emit` events. State never
   replicates at the protocol level — reads are calls, and anything that needs a local
   copy builds one in userland (`embedded_gpui_util::Mirror`).
2. **Dynamic dispatch on the wire, types on top.** All traffic is actor-style messages
   `(entity_id, method: string, payload: bytes)` one way and events
   `(entity_id, name: string, payload: bytes)` the other. The schema layer types this —
   `#[shared_interface]` generates the spec, the message types, and typed caller
   methods — while `send_raw` / `call_raw` / `Methods::on` (with a `"*"` wildcard)
   remain available, so plugins can define their own entity kinds and methods without
   protocol changes. What crosses the boundary is data with a name, never memory with a
   type.
3. **Single-threaded, queue-ordered, reentrancy-safe.** Everything runs on the host main
   thread; messages, responses, and events ride the same deferred-effects machinery as
   display lists, so there are no synchronization concerns and wasm is never re-entered
   from within a render or another delivery.

Identity is refs only. There are no names anywhere in the protocol: strings survive
solely as schema method/event names (codegen vocabulary, like Wayland's interface
names). Object ids are random, nonzero u64s minted by the homing end — globally
unique for practical purposes (collisions are birthday-bounded), so a ref is
universally applicable: nothing is namespaced per end, and an id can only be
*known*, never guessed or enumerated. Discovery starts from **one root object per
end** — see "Bootstrap" below — and every other object is reached through a method
call, resolving directly as a connected `Remote`. `SharedSpec::TYPE_NAME` survives
purely as diagnostic metadata in error messages; nothing on the wire checks it.

### Bootstrap: one root object per end

The one reserved id is 0, "your root": a connection-local *address* (never an
identity in a payload) that each end answers with its own root object. At boundary
creation each end installs its root (`share_root(&entity, cx)`) and attaches to the
other end's with `root::<S>()` — synchronous, like taking a handle. That exchange is
the entire bootstrap: the host calls the plugin root's methods for plugin features,
the plugin calls the host root's methods for host features, and every ref-returning
method extends the reachable world — its receipt resolves with a live, connected
`Remote`, so discovery reads as allocation:

```rust
let plugin = host.root::<DemoPlugin>(cx);
let palette = plugin.palette(cx).await?; // Remote<PaletteApi>, ready to call
```

Authority is reachability from your root — hand a plugin an `Attenuated` root and
its whole world is attenuated; hand it a fake root and you have mocked the entire
host for testing.

The two bootstraps may race freely: messages addressed to a root that has not been
installed yet queue in the registry and are delivered, in order, when `share_root`
runs (an end that never installs a root leaves such calls pending, like a server
that never starts; messages to any other unknown id fail soft). The root schema is
the de facto compatibility surface (for Zed: the
extension API), so it wants explicit versioning discipline — a version method, or
probe-and-degrade — before anything ships against it.

### The `Entity<T>` analogy

The whole model is three ordinary words: an **entity** lives on one end, a **`Remote`**
is how the other end holds it, and a **`Ref`** is how it travels. `Remote<S>` is
deliberately shaped like holding an `Entity<T>` that happens to live in another
sandbox:

| local gpui                   | across the boundary                                     |
| ---------------------------- | ------------------------------------------------------- |
| calling methods in `update`  | `remote.call(...)` / `.send(...)`, or typed caller fns  |
| `cx.observe(&entity, ...)`   | `remote.observe(cx, ...)`                               |
| `cx.subscribe(&entity, ...)` | `remote.subscribe::<Event>(cx, ...)`                    |
| clones share the entity      | clones share the projection (auto-release on last drop) |
| `entity.read(cx)`            | a method call returning state (`Mirror` caches it)      |

The one seam that cannot be papered over is synchronous reads: state lives at the home,
so reading it is asynchronous. `Mirror` covers the rendering case in userland: it
refetches on every notify and holds the latest value in an ordinary observable entity.

### Consistency: FIFO ordering and receipts

There are no sequence numbers, no acks, and no replicas to keep consistent: both
directions are FIFO end to end, and that alone carries the consistency story. A read
issued after a write is itself a message, so it observes the write — read-your-writes by
ordering. `send` returns a `Receipt` that resolves once the home has applied the message
(handler errors arrive as `Err`, crossing the boundary as strings); `call` returns a
`Receipt<R>` carrying the handler's decoded return value; `forward` returns
`Receipt<Vec<u8>>`, the undecoded forwarding primitive. One type, three decoders.
Dropping a receipt is fire-and-forget; the message is unaffected.

Every projection is born bound — `connect` always has the ref's id, and root ids are
fixed — so there is no unresolved-name state and no pending-send queue. The cost is
that a ref returned by a method call must round-trip before you can call through it;
Cap'n-Proto-style promise pipelining (calling through a not-yet-resolved ref) is
deliberately not built yet (see TODO).

### Events: `cx.notify` and `cx.emit`, across the wall

A home entity's reactivity crosses the boundary in the same shape gpui gives it locally:

- every `cx.notify` on the home becomes a `$notify` event, firing `Remote::observe`
  callbacks on the other side (notifies are idempotent, so bursts coalesce trivially);
- `cx.emit(SomeEvent)` on the home becomes a named, typed event for `Remote::subscribe`,
  provided the schema declares it (`events = [SomeEvent]`) and the home type is an
  ordinary gpui `EventEmitter<SomeEvent>` — emitting is completely standard GPUI code.

Events only flow while the other side holds a live remote: a remote's creation sends a
`$subscribe` control message, answered by an initial notify, so a new remote's observers
always fire at least once. This is what replaced snapshots: the protocol no longer
blesses one serialized state type per entity. State transfer is just a method call, and
*when to look again* is the only thing the wire signals.

### Symmetry

The object model is one side-blind module (`registry`), compiled identically into both
ends. A registry knows exactly two things: *local* objects (homes, entities whose
state lives here) and *remote* objects (projections of the other end's homes). No
API, type, or log line in it says host or guest; the ends differ only in the single
piece of configuration the boundary hands them — a byte-transport sink. Ids need no
per-end namespace at all (they are random; "is this mine" is a map lookup), so the
model is fully peer-to-peer; the wasm surface (scenes, input, ticks) is the only
directional part, and it lives outside the object model entirely.

Because ids are global rather than perspective-relative, payloads stay opaque: a
caretaker forwards bytes verbatim and any refs inside keep meaning the same objects,
through any number of hands. Both directions are exercised by the demo: a host-homed
counter driven by wasm buttons, and plugin-homed text/palette entities mirrored
natively.

### References and capabilities (OCAP)

Everything moves by reference: `Ref<S>` is a serializable entity reference (a
bare id on the wire) that travels *inside* message and event payloads, including call
responses. A home shares an entity (`share` returns the ref), embeds the ref wherever
it likes, and the receiving end connects a remote to it (`connect`). The two roots are
the only refs that exist by convention; every other ref was minted by a method call.
The demo's command palette works this way: the plugin publishes
`[(label, Ref<CommandApi>)]`, the host renders native buttons for the labels,
and clicking one invokes the ref. Holding a ref *is* the authority to use it — and
because ids are random, that sentence is load-bearing: a ref can only be learned from
a payload that carried it, so enumerating the other end's objects is infeasible.
(This is bearer-secret authority, Waterken-style, not grant tracking; ids should be
treated as secrets in logs.)

Lifetimes are own-only, like `Entity<T>` itself: sharing holds the entity strongly in
the registry until the other end's last remote drops, at which point a `$release`
control message lets it go (revocation-by-drop's principled replacement is
`Revocable`). Remotes carry a refcounted guard shared by all clones; drops queue the
release, flushed on the next pump on either end. Connecting the same ref twice yields
the same projection and the same guard. Sharing the same entity twice mints two
independent refs (dedup is future work).

### Attenuation, revocation, and membranes

Refs can be weakened and severed without any cooperation from the entity's author. All
three wrappers below hold a `Remote` (so the same code runs in the guest and on the
host), and all implement `Shared` (so sharing one is exactly like sharing any other
entity):

- **Attenuation** is a library pattern, not a protocol feature:
  `embedded_gpui_util::Attenuated` wraps any capability you hold with an allowlist —
  permitted methods forward byte-for-byte, everything else is rejected before reaching
  the entity. Monotonic by construction (a wrapper can only forward what it can itself
  call). The core deliberately has no `$attenuate` control: userland can build this,
  so core doesn't.
- **Accounting** is `embedded_gpui_util::Audited`: a transparent forwarder that records
  every call (method, payload size, eventual outcome) in a ledger readable by whoever
  holds the wrapper entity — capability accountability without interference.
- **Revocation** is `embedded_gpui_util::Revocable`: wrap any capability you hold in a
  caretaker entity, share the wrapper, hand out *its* ref. Notifies and events pass
  through, and a wildcard handler forwards every method — including ones the wrapper has
  never heard of — to the wrapped capability as raw bytes (`Remote::forward`).
  `revoke()` drops the inner remote (auto-release
  cascades to its home) and fails all further calls. The integration tests drive a full
  membrane (host vault → guest caretaker → host caller) through it.

### Async handlers

A handler can return work instead of a value: an `async fn` in the schema (or a raw
`Methods::on_async` registration) produces a `Task` whose value becomes the response when
it resolves. The response flows only then, so an entity can await calls on *other* refs
while answering one. Forwarders, aggregators, and caretakers are all this pattern.

### Typed interfaces: `#[shared_interface]`

The wire stays dynamic; types are sugar, and the sugar is one attribute:

```rust
#[shared_interface(events = [Milestone])]
pub trait CounterApi {
    fn increment(&mut self, by: u32, cx: &mut Context<Self>) -> u32;
    fn clicks(&mut self, cx: &mut Context<Self>) -> u32;
}
```

One name is the whole interface: hold a `Remote<CounterApi>`, reference a
`Ref<CounterApi>`, and implement a home by keeping the same name on the impl block:

```rust
#[shared]
impl CounterApi for Counter { ... }
```

Under the hood the trait syntax is consumed: it becomes the spec struct, one message type
per method, `SharedEvent` wiring for each declared event, and a `CounterApiCaller`
extension trait implemented for `Remote<CounterApi>`, giving remotes typed
`.increment(by, cx) -> Receipt<u32>`. `#[shared]` turns the block's methods into
ordinary methods of the entity and registers each one through schema-generated functions
taking checked function pointers — a signature mismatch against the schema is a compile
error — then implements `Shared`, which is what `share` and `share_root` need
(and what makes the spec inferable at share sites: no turbofish). A method declared to
return `Ref<T>` gets a caller that resolves with a connected `Remote<T>`
instead of the bare ref — the home mints and returns the ref as data; the calling
side's receipt connects it on arrival. Allocation over there, handle over here.

Fully dynamic entities skip the schema: `share_with(&entity, |methods| ...)`
registers handlers by name at runtime, including the `"*"` wildcard the wrappers use.

Home transfer is not implemented (if ever needed: a serialize-and-swap barrier message;
FIFO ordering makes it race-free by construction).

## Known spike limitations (intentional)

- No video `Surface` primitives; no gradient backgrounds (solid color fallback); no sprite
  transformation matrices (painted untransformed with a warning).
- Subpixel *rendering* is decided by the host at replay time (the wire is symbolic), so
  extension text automatically follows host policy; the guest itself always requests
  grayscale.
- No OS-level IME composition (marked text) for guests: printable keys are synthesized into
  `replace_text_in_range` like GPUI's Linux backends. Dead keys/CJK composition would need
  the host to proxy its `PlatformInputHandler` into the guest.
- Image/SVG payloads are cached per instance and never evicted; inset shadows are skipped.
- Guest runs on the host's main thread via a synchronous wasmtime store.
- Font fallback inside a run is whatever the host's `layout_line` returns; fonts are
  identified by host-global `FontId`s which are session-scoped.
