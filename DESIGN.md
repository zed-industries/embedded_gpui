# GPUI Embedded in GPUI

An experimental spike: run GPUI itself inside a Wasm component (`wasm32-wasip2`), and embed
its rendered output inside a native GPUI host application. This models a future "UI
extensions" system for Zed and exists to hammer out the guest-side `gpui_plugin` platform.

This repository is standalone: it consumes `gpui` and `gpui_platform` from the zed
repository, on top of the node engine (zed-industries/zed#63800, which memoizes view
output and makes a frame an ordered list of roots) plus one small addition the guest
needs: `Window::attach_root` / `Window::take_root_scene`, which draw a view as an extra
root of a window at fixed bounds and read that root's scene back on its own. (The other
hook, `Application::run_embedded`, is already on `main`.)

## Layout

- `embedded_gpui/` — **one crate, both sides of the boundary**:
  - `wit/plugin.wit` — the wire protocol (package `gpui:embedded`, world `plugin`); the
    single source of truth both sides bind against.
  - `src/embedded_gpui.rs` — the always-compiled object layer: `Remote`, `Receipt`,
    `Ref`, `Payload`, `Registry`, interfaces/messages/events, and the `Shared` home
    trait.
  - `src/registry.rs` — the side-blind object registry both ends run.
  - `src/surface.rs` — the two well-known UI schemas, `SurfaceApi` (host-homed: a
    place pixels go) and `ViewApi` (guest-homed: the thing drawing there), plus the
    input/geometry/cursor data types they carry.
  - `src/host.rs` (+ `src/host/`) — native targets only: wasmtime glue, the turn
    transport, and the `Surface` entity that replays guest display lists.
  - `src/guest.rs` (+ `src/guest/`) — wasm32 targets only: GPUI's
    `Platform`/`PlatformWindow`/`PlatformDispatcher`/`PlatformTextSystem`/`PlatformAtlas`
    over the WIT boundary, `Plugin`/`register_plugin!`/`open_view`, and the guest half
    of the registry.
- `embedded_gpui_macros/` — the `#[interface]` / `#[shared]` / `#[data]`
  proc macros.
- `embedded_gpui_util/` — side-agnostic OCAP patterns (`Revocable`, `Attenuated`,
  `Audited`, `Mirror`) built on `Remote`.
- `embedded_gpui_js/` — the optional JavaScript layer: `JsRuntimeApi` (compiled
  everywhere; how a host loads a script), `typescript::declarations` (`.d.ts` from any
  schemas), and, on `wasm32` only, the QuickJS runtime (`JsPlugin`, `JsRoot`). See
  "Plugins in other languages".
- `example/` — the demo: `host/` (native window, `cargo run -p example_host`; builds
  the components automatically), `plugin/` (a Rust plugin component), and
  `js_runtime/` (the component that registers `embedded_gpui_js::JsPlugin`, with the
  demo's `plugins/counter.js`). Component crates are their own workspaces since they
  only compile to `wasm32-wasip2`.
- `tests/` — the host-driven integration tests for the object protocol, with
  their guest fixture in `tests/test_plugin/`.

## The object model is the only model

The spike proved the object model *works*; this pass made it the only one. Five rules:

1. **Views are objects.** A slot is a host-homed `SurfaceApi` object; the thing drawing
   on it is a guest-homed `ViewApi` object. The host hands a `Ref<SurfaceApi>` to the
   plugin through an ordinary typed method; the guest builds a view for it, shares a
   view object, and calls `surface.attach(view)`. Input, resize, and cursor are method
   calls on those two objects. There are no view ids, no view names, and no directional
   view/input functions in the WIT. A surface belongs to its owner (it is shared with
   `keep_alive = false`): drop the entity and the guest's view is released and its root
   is detached.
2. **Refs are enumerable.** Every call and response carries a `refs` table; payload
   bytes name refs by table index. Forwarders rewrite the table without parsing the
   payload, which is what makes transitive membranes (`Revocable` wrapping every ref
   that crosses it, in both directions) and future in-flight GC accounting possible.
3. **One frame type, one FIFO per direction.** `frame` is
   `call | response | subscribe | release`. Both directions ride the `tick` export:
   `tick(inbound: list<frame>) -> turn { frames, scenes, wake-after-ms }`. Imports
   cannot re-enter the guest because there are none left to re-enter with (text shaping
   stays a synchronous import because layout needs the answer mid-call). The WIT is pure
   substrate: objects, scheduling, pixels, text. One boundary crossing per burst: the
   host worker coalesces every frame queued since the last turn into one `tick`.
4. **A ref knows where it came from.** `Ref<S>` binds to the registry that delivered
   it, so `ref.connect()` works anywhere a ref is held — in handlers, after awaits, on
   either end. `connect` stops being a host/guest entry point.
5. **The interface schema is a runtime value.** `Interface::schema()` describes methods,
   argument types (structurally, via `Describe`, which `#[data]` implements), events,
   and every named payload type, so a bindings generator (`typescript::declarations`)
   or an inspector consumes the same artifact the macros compile against. Refs encode
   as `{"$ref": index}`, so a guest with no schema at all can still find them.

## Plugins in other languages

The object model is language-neutral by construction — JSON payloads, a ref table, a
root object — and `embedded_gpui_js` is the proof: an optional crate whose runtime
embeds QuickJS (via `rquickjs`; the wasi-sdk is fetched by its build script, no
toolchain setup) and runs scripts against the same host objects a Rust plugin sees.
The host's only knowledge of it is the tiny `JsRuntimeApi` schema — `load(source)` —
and a host that never loads scripts never compiles a line of it.

- **It is just a plugin, and a plugin is a directory.** The host mounts the plugin's
  directory read-only at `/plugin` (`PluginOptions::with_plugin_dir`, a grant any
  plugin can use for assets) and shares its root; the runtime runs `/plugin/index.js`
  and forwards every method on its root to the script's `plugin.root`. The host
  addresses a JS plugin exactly as it addresses a Rust one and never learns which it
  got. When `index.js` changes on disk the runtime tears the previous script's views,
  observers, and remotes down, starts a fresh context, and **replays every call the
  host made to the root** — so the views the host mounted come back without the host
  knowing anything happened. `load(source)` exists for tools and tests that push
  scripts directly. The sandbox is the wasm boundary, not QuickJS: a bug in the engine
  cannot reach the host.
- **Remotes are proxies.** `host.counter()` returns a promise of a remote;
  `counter.increment({ by: 1 })` is `call("increment", { by: 1 })`; `observe` mirrors
  `cx.notify`. Object ids travel as strings (u64 does not fit a JS number). No schema
  is consulted at runtime: refs are recognized by their `$ref` shape.
- **JS never holds a GPUI context.** While a script runs, every request it makes is
  queued as an operation; when the run returns, Rust drains the queue with the real
  `App` — issues the calls, opens views, applies rendered trees — and later resolves
  the script's promises from the receipts. QuickJS's job queue is pumped after every
  entry into JS.
- **UI is data.** `plugin.openView(surface)` opens a GPUI view on a host surface;
  `view.render(tree)` sets a tree of `div`/`text` nodes with a small flat style
  vocabulary; functions in the tree become handler ids the runtime invokes on input.
  Nothing JS-specific reaches the host: it sees a `ViewApi` object like any other.
- **Types are optional and free.** `typescript::declarations` renders the Rust
  schemas as `.d.ts`, so a script author (or an agent) type-checks against the same
  schema the Rust side compiled, while the runtime stays schema-free.

The release component is ~4.9 MB (GPUI + QuickJS). QuickJS is a bytecode interpreter;
the rule for it is the same as for every plugin: events and notifies, not animation.

## Architecture (agreed invariants)

1. **The host never calls into the guest synchronously from the frame path.** The guest
   renders when *it* is ticked; output is a retained display list the host caches in an
   entity and replays cheaply every host frame.
2. **The wasmtime store lives on a background worker.** The host never calls wasm from
   the UI thread: every interaction is a queued request to the worker that owns the
   store (strictly one call at a time), and each call's effects are applied back on the
   foreground in the same order. A slow or hung plugin cannot stall the UI; FIFO
   request/effect pairing preserves all ordering guarantees below.
3. **Re-entrancy is impossible by construction.** The guest's only imports are the
   synchronous text-shaping functions, which answer from the host's text system and
   never touch the guest. Everything else the guest produces travels in the `turn` a
   `tick` returns; the host applies it after the call.
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
7. **Input and geometry are method calls** on the guest-homed `ViewApi` object a
   surface has attached: `resize`, `mouse`, `key` (slot-relative logical coordinates).
   The guest translates them into the window mirror (below) and lets GPUI's own
   dispatch do hit-testing and run listeners; no callback registry crosses the boundary.
   Cursor styles flow back as `set_cursor` on the host-homed `SurfaceApi`. Because
   `ViewApi` handlers run inside the registry's `App` borrow and GPUI's window callbacks
   re-enter the app, the view queues events on the window and the pump applies them
   once the borrow is released — same turn, same order.
8. **Guest windows mirror host windows; a surface is a root of its window.** A
   surface's `Geometry` says where its slot is: its bounds in a host window, and that
   window's identity, viewport, and scale factor. The guest keeps one window per host
   window it hears of, with the host window's size and scale factor, and attaches each
   surface's view to it at the slot's real origin with `Window::attach_root` — a node of
   the node engine like any mounted view, so it is memoized, hit-tested, focused, and
   dispatched to by position, and hundreds of surfaces are hundreds of nodes rather than
   hundreds of windows. Because the guest window *is* the host window's shape, nothing
   inside a view is a lie: `window.viewport_size()` is the host's viewport, a popover
   clamps to the host window's edges, the scale factor is the display's, and two views
   cannot interact unless the host overlaps their slots. A surface that moves to another
   host window moves its root to that window's mirror; a mirror with no roots left
   closes. After each frame the pump reads every root's scene on its own with
   `Window::take_root_scene`, which answers only for roots whose node was redrawn, so an
   idle surface ships nothing and a changed one ships exactly its own display list
   (translated back to slot-relative coordinates). A view is not drawn until the host
   has said where it is, so its first frame is at the slot's real size. Window-level
   state (active, appearance) has its natural place now — the mirror — and is the next
   thing `Geometry`'s `HostWindow` grows.
9. **Scheduling**: the guest dispatcher queues runnables/timers locally. Every `tick`
   drains due work, pumps the window's `request_frame` callback (GPUI decides whether it
   is dirty), ships the changed roots' scenes into the turn's `scenes`, and reports the
   earliest remaining timer as `wake-after-ms`, which the host schedules.

## Status

Working end to end on macOS: quads (rounded corners, borders), text (host-shaped and
host-rasterized via symbolic glyph replay, including wrapping and exact subpixel-variant
positioning), tessellated paths, images (premultiplied-BGRA payloads shipped once, cached
per instance), SVGs (guest-rasterized alpha masks, tint baked per color), keyboard input
(host focus → forwarded keystrokes → guest focus dispatch, with unhandled printable keys
falling through to the focused `EntityInputHandler`, Linux-backend style), hover styles,
mouse input, cursor styles, and shared object state across two plugin surfaces backed by
one guest App and one guest window. The release component (all of gpui + taffy, no
fonts, no glyph rasterizers) is ~6 MB unoptimized for size.

Run it:

```sh
cargo run -p example_host
```

## Why two type systems? (WIT and the object model)

A fair question: the WIT interface is a type system, and the object schema layer
is another. Why both? Because they type different things, with opposite change profiles:

- **WIT is the syscall boundary** — display lists, text shaping, and the two exports
  (`init`, `tick`) that carry frames both ways. It changes when the *platform* changes:
  rarely, owned by one team, with hard commitments (a signature mismatch fails
  instantiation outright). That hardness is right for the substrate and wrong for an
  app API. The WIT here is entirely machine protocol: nothing in it has UI meaning, and
  the whole object model rides on one `frame` type.
- **The object model is userspace** — the evolving semantic surface (what a host app
  exposes, what plugins expose to each other), side-blind and peer-to-peer (see
  "Symmetry" below), with soft, runtime-negotiated
  commitments: unknown methods fail as handleable errors, payload fields evolve by serde
  defaulting, a plugin built against an old schema degrades at specific calls instead of
  failing to load. Evolution is a library release, not a flag day: old and new methods are just
  entries in the same dispatch table. Wayland-style version negotiation is expressible
  as a plain shared object (a registry whose `list` call returns interface/version/ref) —
  and the bootstrap primitive already exists as the root object (Wayland's object 1,
  Cap'n Proto's bootstrap capability), with refs (objects) below it.

Two things the static layer structurally cannot express, which is why "just put it all
in WIT" loses: **capability semantics** (WIT imports are ambient and identical for every
plugin; per-plugin grants, attenuation, revocation, and refs minted at runtime require
the dynamic layer) and **ecosystem growth** (two plugins agreeing on a new interface via
a shared schema crate, without the host knowing or any world recompilation).

The honest trade: dynamic calls are slower than generated WIT functions (serde plus
string dispatch). The split encodes the rule — bulk pixels and mid-call text shaping go
in WIT, everything with meaning (including input, at a few microseconds per event) goes
through objects — and since the object wire is just bytes, encodings are swappable and
any method that gets hot can be promoted into WIT. Precedents for the two-layer shape: syscalls vs. D-Bus, TCP vs.
HTTP APIs, Wayland's fixed wire vs. versioned interfaces.

## Performance philosophy

The design picks slow-and-flexible only where humans are the clock, and
fast-and-static where the GPU is the clock:

- **The frame path contains none of the flexible machinery.** A quiescent plugin costs
  ~nothing per frame: the host replays a retained display list through gpui's normal
  paint path — no wasm call, no serialization, no dispatch. Animating views cost one
  wasm render plus one binary display-list ship per *dirty* frame. Pixels, text, and
  input never touch JSON or string dispatch.
- **The control path is slow only by hot-loop standards.** An object call is
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

## Shared objects

Entities cannot literally cross the boundary (separate linear memories, separately compiled
types), so shared state is built on three rules:

1. **One home per entity.** The home side owns the state as a normal GPUI entity and is
   the only holder of it. The other side holds `Remote<S>` handles: they call methods on
   the home, observe its `cx.notify`, and subscribe to its `cx.emit` events. State never
   replicates at the protocol level — reads are calls, and anything that needs a local
   copy builds one in userland (`embedded_gpui_util::Mirror`).
2. **Dynamic dispatch on the wire, types on top.** All traffic is actor-style calls
   `(target, method: string, payload: bytes + refs)`. The payload's bytes name refs by
   index into its ref table, so the registry never parses payloads yet every capability
   in transit is enumerable. The schema layer types this — `#[interface]` generates the
   spec, the message types, typed caller methods, and a runtime `schema()` — while
   `call_raw` / `Methods::on` (with a `"*"` wildcard) remain available, so plugins can
   define their own entity kinds and methods without protocol changes. The registry
   itself stores exactly one dispatch closure per object and never interprets method
   names: the name-keyed table (and its wildcard) is a userspace convention that
   `Methods` compiles down to. What crosses the boundary is data with a name, never
   memory with a type.
3. **Single-threaded, queue-ordered, reentrancy-safe.** Everything runs on the host main
   thread; messages and responses ride the same deferred-effects machinery as display
   lists (events are just messages to observer objects), so there are no
   synchronization concerns and wasm is never re-entered from within a render or
   another delivery.

Identity is refs only. There are no names anywhere in the protocol: strings survive
solely as schema method/event names (codegen vocabulary, like Wayland's interface
names). Object ids are random, nonzero u64s minted by the homing end — globally
unique for practical purposes (collisions are birthday-bounded), so a ref is
universally applicable: nothing is namespaced per end, and an id can only be
*known*, never guessed or enumerated. Discovery starts from **one root object per
end** — see "Bootstrap" below — and every other object is reached through a method
call, resolving directly as a connected `Remote`. Interface names survive purely as
diagnostic metadata in error messages; nothing on the wire checks them.

Lifetimes are reference counted, and reference counting cannot collect cycles: two
objects on opposite ends that hold remotes to each other, with nothing else owning
either, live forever. Objects whose lifetime belongs to an owner outside the graph opt
out with `#[shared(keep_alive = false)]` (a `Surface` lives as long as the embedder's
UI keeps it; the guest's view is the other half of exactly such a cycle). A tracing
collector would need each end's retention edges, which gpui entities do not expose;
if cycles become a practical problem, the route is explicit retention
(`Remote::owned_by`) plus cycle detection, not a tracer.

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
| a view in the element tree   | a `Surface` entity in the tree, a `ViewApi` object drawing on it |
| calling methods in `update`  | `remote.call(...)` (drop the receipt to fire-and-forget) |
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
ordering. There are exactly two verbs: `call` returns a `Receipt<R>` carrying the
handler's decoded return value (handler errors arrive as `Err`, crossing the boundary
as strings), and `call_raw` returns `Receipt<Vec<u8>>`, the undecoded forwarding
primitive — chain `.decoded::<R>()` or `.acknowledged()` to interpret it. The decoder
lives in the receipt, not the verb. Dropping a receipt is fire-and-forget; the message
is unaffected.

Every projection is born bound — a `Ref` carries its id and the registry it arrived
through, so `ref.connect()` needs nothing else — and there is no unresolved-name state
and no pending-send queue. The cost is
that a ref returned by a method call must round-trip before you can call through it;
Cap'n-Proto-style promise pipelining (calling through a not-yet-resolved ref) is
deliberately not built yet (see TODO).

### Events: `cx.notify` and `cx.emit`, across the wall

A home entity's reactivity crosses the boundary in the same shape gpui gives it locally:

- every `cx.notify` on the home becomes a `$notify` call to observers, firing
  `Remote::observe` callbacks on the other side (notifies are idempotent, so bursts
  coalesce trivially);
- `cx.emit(SomeEvent)` on the home becomes a named, typed event for `Remote::subscribe`,
  provided the schema declares it (`events = [SomeEvent]`) and the home type is an
  ordinary gpui `EventEmitter<SomeEvent>` — emitting is completely standard GPUI code.

Under the hood there is no event channel at all — events are messages flowing the
other way. The first `observe`/`subscribe` on a projection mints a hidden *observer
object* and sends a `subscribe` frame carrying its ref; the home then calls that
observer (`$notify`, or the typed event's name) as ordinary calls, starting with one
initial notify so observers always fire at least once. Lifetime is the ordinary
release machinery: a `release` frame makes the home forget its observers, and
releasing a projection removes its observer object. Subscribe and release are not
method names but structural wire variants — the operations constitutive of objecthood
get constitutive framing, and the method namespace belongs entirely to user schemas
(`$notify` survives only as a private convention between the two registries' own
observer objects). A projection nobody listens to costs the wire nothing,
and homes naturally support any number of observers (the multi-plugin future). This is
what replaced snapshots: the protocol no longer blesses one serialized state type per
entity. State transfer is just a method call, and *when to look again* is the only
thing the wire signals.

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

Everything moves by reference: `Ref<S>` is a serializable entity reference that
travels *inside* message and event payloads, including call responses. On the wire a
payload's bytes name a ref by its index into the frame's ref table, so the table is the
complete list of capabilities a message carries: opaque to the registry, enumerable to
forwarders, and rewritable without parsing. A home shares an entity (`share` returns
the ref), embeds the ref wherever it likes, and the receiving end connects a remote to
it (`ref.connect()` — the ref remembers which registry delivered it). The two roots are
the only refs that exist by convention; every other ref was minted by a method call.
The demo's command palette works this way: the plugin publishes
`[(label, Ref<CommandApi>)]`, the host renders native buttons for the labels,
and clicking one invokes the ref. Holding a ref *is* the authority to use it — and
because ids are random, that sentence is load-bearing: a ref can only be learned from
a payload that carried it, so enumerating the other end's objects is infeasible.
(This is bearer-secret authority, Waterken-style, not grant tracking; ids should be
treated as secrets in logs.)

Lifetimes are own-only, like `Entity<T>` itself: sharing holds the entity strongly in
the registry until the other end's last remote drops, at which point a `release`
frame lets it go (revocation-by-drop's principled replacement is
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
- **Revocation** is `embedded_gpui_util::Revocable`, and it is a *membrane*: wrap any
  capability you hold in a caretaker entity, share the wrapper, hand out *its* ref.
  Notifies and events pass through, and a wildcard handler forwards every method —
  including ones the wrapper has never heard of — to the wrapped capability as raw
  bytes (`Remote::call_raw`). Every ref that crosses in either direction (arguments,
  responses, events) is rewritten in the ref table to a wrapper sharing the same
  revocation switch, and wrapper refs coming back are unwrapped, so the wrapped object
  sees its own objects. `revoke()` severs every wrapper at once (auto-release cascades
  to the homes) and fails all further calls through any of them. The integration tests
  drive a full membrane (host vault → guest caretaker → host caller) through it and
  verify that a key obtained *through* the membrane dies with it. Refs homed on the
  wrapper's own end pass through unwrapped (loopback connects are not supported).

### Async handlers

A handler can return work instead of a value: an `async fn` in the schema (or a raw
`Methods::on_async` registration) produces a `Task` whose value becomes the response when
it resolves. The response flows only then, so an entity can await calls on *other* refs
while answering one. Forwarders, aggregators, and caretakers are all this pattern.

### Typed interfaces: `#[interface]`

The wire stays dynamic; types are sugar, and the sugar is one attribute:

```rust
#[interface(events = [Milestone])]
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

The schema is also a value: `Interface::schema()` returns the interface's name,
methods (argument names and types, response type, whether it returns a ref, whether it
is async), and events — the same artifact the macros compile against, so a
dynamic-language guest or an inspector can bind to it without generated code.

Home transfer is not implemented (if ever needed: a serialize-and-swap barrier message;
FIFO ordering makes it race-free by construction).

## Prior art: OCapN / CapTP and Spritely Goblins

Points of reference: the [OCapN implementation guide](https://github.com/ocapn/ocapn/blob/main/implementation-guide/Implementation%20Guide.md)
and [Spritely Goblins](https://codeberg.org/spritely/goblins). By largely convergent
evolution, this design is a CapTP-shaped system specialized to a two-party,
in-process boundary.

**Policy: converge on OCapN semantics wherever possible; diverge on encoding; record
every divergence as either *flattened* (their design, optimized for our setting) or
*specialized* (their mechanism solves a problem that does not exist inside one
process).** The long-game payoff is an OCapN netlayer bridge that extends real
distributed OCAP networks into Zed transparently: a boundary whose sink speaks
CapTP/syrup to the network instead of wasm effects, session crypto and a powerbox
membrane at that edge only, Goblins objects appearing inside plugins as ordinary
`Remote`s. The mapping, including the divergences:

| OCapN / CapTP                          | here                                                     |
| -------------------------------------- | -------------------------------------------------------- |
| netlayer (channel-agnostic CapTP)      | the side-blind registry over a `WireSink`; wasmtime is the netlayer |
| session setup (fresh keys, crossed hellos) | the wasm boundary *is* the session; the host owns the guest's memory, so no cryptographic identity |
| bootstrap object at export position 0  | the root object at address 0                              |
| swiss-nums (unguessable object names)   | random u64 ids (bearer refs)                              |
| per-session import/export positions, perspective-flipped | global random ids: no flip, so payload bytes pass through membranes unrewritten; refs travel in a per-frame table (their structural descriptors), which is what a membrane rewrites |
| `op:deliver`                            | the `call` frame                                          |
| resolver objects (`resolve-me-desc`)    | `request-id` + response record — a flattened resolver (the optimization CapTP itself evolves into via answer tables); resolver *refs* on calls, restoring delegation-of-reply, ride the ref-table pass |
| promises + `op:listen`                  | `Receipt`s (one-shot); observer objects are our listen    |
| pipelining via `answer-pos` (questions/answers) | the caller-allocated-ids design in TODO — same idea, reached from random ids |
| `op:gc-exports` with wire-deltas        | release frames + drop guards; the ref table makes in-flight mention accounting possible (not built yet) |
| third-party handoffs (gifter/receiver/exporter certificates) | multi-plugin routing stays host-mediated (the host is the powerbox); handoffs are the reference for unmediated introductions |
| syrup (canonical s-expressions)         | JSON today; a protobuf-subset codec planned — version-skew tolerance via field tags is the requirement syrup meets only by convention |
| vats                                    | the two gpui event loops, exactly (minus transactional turns) |

## Known spike limitations (intentional)

- The JS runtime keeps every remote a script has ever received connected until the
  next reload, and scripts cannot cancel observers. Reload replays the host's root
  calls verbatim, which is right for mount-style calls and wrong for calls that were
  meant to happen once; the runtime cannot tell them apart. Replayed calls are
  fire-and-forget: one that fails against the new script is dropped silently and the
  rest of the history keeps going (see TODO, "Reload replay can desync").
- Reload is detected by polling the entry point's modification time twice a second;
  WASI has no file watching.

- No video `Surface` primitives; no gradient backgrounds (solid color fallback); no sprite
  transformation matrices (painted untransformed with a warning).
- Subpixel *rendering* is decided by the host at replay time (the wire is symbolic), so
  extension text automatically follows host policy; the guest itself always requests
  grayscale.
- No OS-level IME composition (marked text) for guests: printable keys are synthesized into
  `replace_text_in_range` like GPUI's Linux backends. Dead keys/CJK composition would need
  the host to proxy its `PlatformInputHandler` into the guest.
- Image/SVG payloads are cached per instance and never evicted; inset shadows are skipped.
- The wasmtime store is synchronous; it lives on a background worker, one turn at a time.
- A plugin's `Surface`s and the images its scenes reference are per plugin instance;
  a surface handed from one plugin to another (composition) is expressible in the
  object model but the host does not yet route scenes across instances.
- Font fallback inside a run is whatever the host's `layout_line` returns; fonts are
  identified by host-global `FontId`s which are session-scoped.
