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

- `wit/plugin.wit` — the wire protocol (package `gpui:embedded`, world `plugin`). This is the
  single source of truth both sides bind against.
- `src/gpui_embedded.rs`, `src/shared_entities.rs`, `src/plugin_element.rs` — **host**:
  wasmtime glue, host-side shared entities, and the element that replays guest display lists.
- `src/main.rs` — host demo binary (`cargo run --bin gpui_embedded_demo`).
- `shared/` — `gpui_embedded_shared`: schema traits, receipts, the `SharedCaller`
  capability surface, and the `shared_schema!` macro used by both sides of the boundary.
- `shared/macros/` — `gpui_embedded_shared_macros`: the `#[shared_interface]` proc macro
  (trait in, complete typed interface out).
- `utils/` — `embedded_gpui_utils`: side-agnostic OCAP patterns (`Revocable`, a generic
  caretaker/membrane) built purely on `SharedCaller`.
- `plugin/` — **guest platform** crate `gpui_plugin` (its own workspace, compiled to
  `wasm32-wasip2`). Implements GPUI's `Platform`/`PlatformWindow`/
  `PlatformDispatcher`/`PlatformTextSystem`/`PlatformAtlas` over the WIT boundary, plus the
  guest half of shared entities.
- `example_plugin/` — **guest demo** crate (own workspace): a small GPUI UI that runs inside
  the plugin platform.
- `test_plugin/` + `tests/` — a guest fixture and the host-driven integration tests for the
  shared-entity protocol.

## Architecture (agreed invariants)

1. **The host never calls into the guest synchronously from the frame path.** The guest
   renders when *it* is ticked; output is a retained display list the host caches in an
   entity and replays cheaply every host frame.
2. **All calls into the guest happen from host foreground tasks or event handlers** (spike
   simplification; a real integration would move the store to a background thread like
   `extension_host` does). Guest calls are cheap; the guest must not block.
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
./build_plugin.sh
cargo run --bin gpui_embedded_demo
```

## Shared entities

Entities cannot literally cross the boundary (separate linear memories, separately compiled
types), so shared state is built on three rules:

1. **One home per entity.** The home side owns the state as a normal GPUI entity and is the
   only writer. The other side holds a *projection*: a replica entity refreshed by
   serialized snapshots, observable with plain `cx.observe`.
2. **Dynamic dispatch on the wire, types on top.** All writes are actor-style messages
   `(entity_id, method: string, payload: bytes)`. The `gpui_embedded_shared` crate layers a
   typed veneer over this: a `SharedSpec` names an entity kind and its snapshot type, a
   `SharedMessage` names a method and its payload type, and `Remote::send` /
   `Methods::on::<M>()` are sugar over the raw wire. `send_raw` / `on_raw` remain available,
   so plugins can define their own entity kinds and methods without protocol changes —
   what crosses the boundary is data with a name, never memory with a type.
3. **Single-threaded, queue-ordered, reentrancy-safe.** Everything runs on the host main
   thread; messages and snapshots ride the same deferred-effects machinery as display
   lists, so there are no synchronization concerns and wasm is never re-entered from within
   a render or another delivery.

Identity is a well-known string binding (`host.share(&entity, "clicks", ...)` /
`gpui_plugin::shared::remote::<CounterSpec>("clicks", cx)`), type-checked at announcement
time via `SharedSpec::TYPE_NAME`. Snapshots publish automatically on every `cx.notify` of
the home entity.

### Consistency: sequences, acks, receipts

Every message carries a per-entity monotonic `sequence` assigned by the sender; every
snapshot carries `acked-sequence`, the highest sender sequence the state already includes.
`send` returns a `SendReceipt` future that resolves only after a snapshot with
`acked-sequence >= sequence` has been applied to the *local replica* — so
`send(msg).await` gives read-your-writes. Homes that handle a message without notifying
still publish an acking snapshot (deduped via `published_ack`), so receipts always resolve.
Dropping a receipt is fire-and-forget; the message is unaffected.

Sends to a not-yet-announced entity queue in order and flush on binding — which is promise
pipelining in miniature: messages addressed "through" an unresolved reference are ordered
behind its resolution, never lost or reordered.

**Calls** are messages with a `request-id`: the home handler's return value flows back as
a response, decoded by a typed `CallReceipt<R>` (`remote.call(Increment { by: 1 }).await
-> u32`). Responses are delivered strictly *after* the snapshot acking their call, on both
sides — so when a call resolves, the local replica already reflects it. Handler errors
arrive as `Err`, crossing the boundary as strings.

### Symmetry

Both directions are implemented: host-homed entities with guest projections (the demo's
click counter — mutated by a native button and the wasm button, observed everywhere), and
guest-homed entities with host projections (the demo input line's text, mirrored live in
the native header). Guest-homed ids set the high bit so the two sides' ids never collide.

### References and capabilities (OCAP)

Well-known names are only the *bootstrap* namespace — rendezvous roots, like mounts in a
filesystem or globals in the Wayland registry. Everything else moves by reference:
`SharedRef<S>` is a serializable entity reference (a bare id on the wire) that travels
*inside* snapshot and message payloads. A home shares an entity anonymously
(`share_anonymous` returns the ref), embeds the ref wherever it likes, and the receiving
side materializes a remote from it (`remote_from_ref`) — no name involved. The demo's
command palette works this way: the plugin publishes `[(label, SharedRef<CommandSpec>)]`
in a snapshot, the host renders native buttons for the labels, and clicking one invokes
the ref. Holding a ref *is* the authority to use it.

Lifetimes follow gpui's own model, stretched across the boundary. Named shares borrow
their entity (the sharer owns the lifetime); anonymous shares own theirs. Remotes
materialized from refs carry a refcounted guard shared by all clones: when the last clone
drops, a `$release` control message tells the home to drop its strong handle. The guest
releases from `Drop` directly; the host queues releases and flushes them on the next
guest interaction (or `PluginHost::pump`). Materializing the same ref twice yields the
same replica and the same guard.

### Attenuation, revocation, and membranes

Refs can be weakened and severed without any cooperation from the entity's author:

- **Attenuation** (`remote.attenuate(&["read"])`) is a generic `$attenuate` control
  method: the home clones the dispatch entry with the method table intersected against
  the caller's own — monotonic, so a facet can only shrink. Facets alias the entity's
  state; snapshots fan out to all of them.
- **Revocation** is `embedded_gpui_utils::Revocable`: wrap any capability you hold in a
  caretaker entity, share the wrapper, hand out *its* ref. Snapshots pass through, and a
  wildcard handler forwards every method — including ones the wrapper has never heard
  of — to the wrapped capability as raw bytes (`SharedCaller::forward_shared`).
  `revoke()` drops the inner remote (auto-release cascades to its home), freezes the last
  snapshot, and fails all further calls. Because it is generic over `SharedCaller`, the
  same code runs in the guest and on the host; the integration tests drive a full
  membrane (host vault → guest caretaker → host caller) through it.

### Async handlers

A handler can return work instead of a value: `HandlerResponse::Pending(Task<...>)`
(registered via `Methods::on_async` / `on_raw_async`, or the `HandleSharedAsync` trait).
The delivery machinery holds the ack and the response until the task resolves, preserving
snapshot-before-response ordering — which is what lets an entity await calls on *other*
refs while answering one. Forwarders, aggregators, and caretakers are all this pattern.

### Typed interfaces: `#[shared_interface]`

The wire stays dynamic; types are sugar, and the sugar is now one attribute:

```rust
#[shared_interface(spec = CommandSpec, type_name = "demo.command", snapshot = CommandSnapshot)]
pub trait CommandApi {
    fn invoke(&mut self, cx: &mut Context<Self>) -> String;
}
```

generates the `SharedSpec`, one message struct per method, a `CommandApiCaller` extension
trait blanket-implemented for every `SharedCaller<CommandSpec>` (so guest `Remote` and
host `HostRemote` both get a typed `.invoke(cx) -> CallReceipt<String>`), and
`register_command_api` to wire a trait implementation into a home's dispatch table. The
trait itself is the home-side handler surface. `shared_schema!` remains for
snapshot-plus-messages declarations without a trait.

Not yet implemented: async methods in `#[shared_interface]` (register those manually via
`Methods::on_async`), and home transfer (if ever needed: a serialize-and-swap barrier
message; FIFO ordering makes it race-free by construction).

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
