# TODO

What's deliberately not built yet, in rough priority order. The spike's goal is
to prove the architecture; these are the known gaps between "proven" and
"product".

## Surfaces and views (done: views are objects)

Views are `SurfaceApi`/`ViewApi` objects; see `DESIGN.md`. What remains:

- [x] **Data-shaped surfaces at scale**: guest windows mirror host windows; every
  surface is a root attached to its window at its real origin (`Window::attach_root`,
  a node-engine node), and only roots whose node was redrawn ship a display list
  (`Window::take_root_scene`). Scale factor is per window, as it should be.
- [x] **Overlays**: deferred draws, tooltips, drag previews and prompts ship as a
  second display list per surface and are painted above the host window, with the
  guest's hit regions as input regions. Open: a size cap.
- [x] **Window state mirroring**: `HostWindow` carries id, viewport, scale factor,
  active state and appearance; the mirror reports them to GPUI as a platform would.
  Modifiers and hover come from the events themselves.
- [x] **IME / marked text and key precedence**: a synchronous `input-query` export
  (see DESIGN invariant 11), with the whole `EntityInputHandler` contract relayed and a
  query budget of its own.
- [x] **Clipboard** as a host-homed object (DESIGN invariant 12). Open: a host cannot
  wrap its own home in a `Revocable` today (loopback connects), so a clipboard *loan*
  waits on loopback routing; a plugin can wrap the ref it holds for its own
  sub-components.
- [x] **Display-list limits** and a stopped state shown on every surface (DESIGN
  invariant 13).
- [x] **Focus traversal** across the boundary (DESIGN invariant 14). Open: a host that
  does not do Tab traversal itself (GPUI core has no default binding) leaves the key
  unhandled and the surface focused, so the next Tab re-enters the view at its first
  stop — harmless, but a host that wants traversal binds Tab as the demo does.
- [ ] **Moving a surface between host windows** moves its root between mirrors; focus
  inside it is lost on the way. Fine for a drag between windows; worth a test.
- [ ] **Host-side retained replay**: the host `Surface` still replays its display list
  through `paint_*` every host frame. With the node engine on the host too, a surface
  is a node whose scene is the guest's output (`docs/node_engine_needs.md` §3): an
  idle plugin then costs a clean scope.
- [ ] **Cross-instance scenes**: a surface ref can already travel from one plugin to
  another through the object model, but each `PluginHost` routes scenes only to
  surfaces shared through its own registry, and image caches are per instance. Routing
  scenes by surface id across hosts is the composition story.
- [ ] **Display-only views** as an `open_view` option (attach an `Attenuated<ViewApi>`
  allowing only `resize`), so a plugin can lend a surface it cannot be poked through.
- [ ] **Delta-encoded scenes**: display lists are the one bulk path; frame-to-frame
  diffing when a profile asks for it.

## Object model follow-ups (from the root-object pass)

- [x] **Promise pipelining**: caller-allocated ids (see "Promise pipelining" in
  DESIGN.md). Open: a promised id is one more id per allocation until share dedup
  collapses it with the actual one; and a router between plugins would have to learn
  promised ids as they are minted.
- [ ] **Share dedup**: sharing the same entity twice mints two independent ids.
  Dedup wants a per-entity identity map (and interacts with release: both refs
  share one strong hold).
- [ ] **Root versioning discipline**: the root schema is the real compatibility
  surface (for Zed: the extension API). Unknown methods already fail soft and
  `Interface::schema()` makes probing possible, but it wants an explicit
  convention — a `version()` method, or a `describe()` returning the schema —
  before anything ships against it.
- [x] **Bindings from the schema**: `Describe` gives `Interface::schema()` structural
  types and every named definition; `typescript::declarations` renders `.d.ts`.
- [ ] **Reload replay can desync** (`embedded_gpui_js`): replay re-issues the host's
  root calls to the new script fire-and-forget (`JsRoot::load` detaches every result), so
  a replayed call that fails against the edited script is dropped silently and the rest
  of the history keeps replaying against a half-initialized script. Ordinary edits —
  renaming a root method, changing an argument shape, a mount handler that now expects
  state an earlier call used to set up — leave the script disagreeing with what the host
  believes it mounted, and neither side gets a signal. Decision: bail on the first
  replay failure and report it through `load`'s error (method name + position in the
  history); whatever mounted before that point stays up. Still open: whether
  history should be pruned by releases (a surface the host has since released should
  not be re-attached) and whether a ref in history is even guaranteed live; whether
  scripts should mark which root methods are replayable, or whether replay should move
  to the host entirely (a `reloaded` event on the runtime root — the host knows which of
  its calls were mounts and can re-issue them itself).
- [ ] **JS runtime, next steps** (`embedded_gpui_js`): let a script opt out of replay
  for one-shot root calls (or mark a method replayable); `observe` cancellation; typed events
  (`subscribe`) in the prelude; a `.d.ts` bundle emitted by the demo host next to
  `counter.js`; a richer element vocabulary (input, images, scroll) or, better, a
  generic `Styled` mapping; script errors surfaced to the host as `ErrorReport`s
  with JS stacks; a QuickJS interrupt handler as a second, finer budget inside the
  wasm turn budget; TypeScript source via a type-stripper if JS + `.d.ts` proves
  insufficient.
- [x] **A public symmetric handle**: `Registry` — reachable from any `Remote`, `Ref`,
  or inbound `Payload` — shares, connects, and reaches the root on either end. The
  host's `PluginHostHandle` and the guest's free functions are thin wrappers over it.
- [ ] **Payload codec: evolvable schemas**: method calls
  are RPC, and the published schema crates are the protocol, so payloads need
  protobuf-style evolution — a hand-rolled protobuf-subset codec owned by the
  `#[interface]`/`#[data]` macros (field tags from declaration order, explicit
  `#[tag(n)]` override, skip-unknown-fields, append-only discipline as lintable
  API rules). Methods and payloads are one versioned axis underneath (the wire
  method *is* the message type), so message-name + field-tags covers all of it;
  keeping the wire format an honest protobuf subset lets non-Rust ends speak it
  with stock tooling. Encoding moves into the schema layer (`Message` owns its
  bytes); core never mentions a codec.
- [x] **Ref-table packets**: `(method, payload, refs: list<u64>)`; a `Ref` serializes
  as an index via the encode/decode context (strict: a `Ref` outside `encode` is an
  error). Still to do on top of it: wire-delta mention accounting (see "In-flight ref
  accounting" below), hop-by-hop rewriting for multi-plugin routing, and resolver refs
  as the general reply route on call frames (request id stays as the fast path).
- [ ] **Object-graph inspector**: the registry already holds the whole graph —
  homes (with `std::any::type_name` labels), projections, observers, strong vs
  released, pending requests. Expose it as *another shared object* (a debug
  `Interface` whose home is the registry itself) and any end — or a dev-tools
  plugin — can render a live object-graph view. Dogfooding as observability;
  the ref table supplies the who-holds-what edges.

## Platform completeness

- [x] **Resource limits**: per-turn epoch deadline, query budget, memory cap, and
  display-list caps (`PluginOptions`); a plugin that overruns stops, in-flight calls
  fail, and its surfaces show why.
- [x] **IME / marked text**: the host's `Surface` is an `EntityInputHandler` whose
  answers come from the guest over the synchronous `input-query` export.
- [ ] **Rendering completeness**: gradient backgrounds (solid fallback today),
  video `Surface` primitives, sprite transformation matrices, inset shadows.
- [ ] **Atlas hygiene**: image/SVG payloads are cached per instance and never
  evicted; `FontId`s are host-global and session-scoped (a persisted display
  list from a previous session would replay wrong glyphs).
- [ ] **Multi-plugin routing**: several stores behind one host. The registry is
  already peer-to-peer, and random ids are globally unique, so the host can
  route between plugins by looking up who homes an id — no rewriting at any
  hop, Cap'n-Proto vats without the four tables. This is also the
  inter-plugin API story: plugins never share memory, only routed messages.
  Discovery is a host-homed registry entity - Wayland-style, a `list`
  call returns (plugin, interface, version, ref) - and being listed is opt-in, so
  discoverability is itself a capability. The only contract between two
  cooperating plugins is a shared schema crate they both compiled against;
  the host never needs to know the interface exists. Routing through the
  host makes it the policy chokepoint: per-grant membranes, cross-plugin
  audit, or powerbox-style user consent before a ref is forwarded — and the
  platform-observability surface: the host reads its own routing state to
  report the plugin relationship graph, per-edge liveness, and backpressure
  (queue depths), rendered by the object-graph inspector. In-process, the
  host as trusted introducer is what makes handoff certificates unnecessary.
  Depends on ref-table packets (rewriting and grant tracking at the table). OCapN's
  third-party handoffs are the reference design if unmediated introductions
  are ever wanted — and if semantics keep converging with CapTP, an OCapN
  netlayer bridge could someday put plugin objects on a real distributed
  OCAP network (Goblins interop).

## Advanced OCAPs

- [x] **Tagged refs on the wire**: every call and response carries a ref table;
  payload bytes name refs by index. Forwarders rewrite the table without parsing.
- [x] **Deep membrane**: `Revocable` wraps every ref crossing it in either direction
  and unwraps its own wrappers coming back; one revoke severs the whole graph.
- [ ] **In-flight ref accounting**: a home shared into a payload the other end never
  connects is kept alive forever. With the ref table, homes can count mentions out
  and receivers can ack them in (CapTP's `gc-exports` wire deltas). Every forwarder
  must participate.
- [ ] **Cycle collection**: refcounting cannot collect a host object and a guest
  object that hold each other. `keep_alive = false` is the owner-side escape hatch
  today; the principled next step is explicit retention (`Remote::owned_by(&entity)`)
  so the registry has edges to run cycle detection over.
- [ ] **Loopback routing**: a guest materializing a ref to its own home (needed
  to stack same-side caretakers, e.g. Revocable over Audited in one guest, and
  for a membrane to wrap refs homed on its own end — today they pass through
  unwrapped). The host is already the router; it would reflect guest-addressed
  traffic back, rewriting request ids.
- [ ] **Expiring / N-use grants**: a `Revocable` that severs itself after a
  deadline or call budget.
- [ ] **Sealer/unsealer pairs**: rights amplification, for when plugins trade
  refs among each other.
- [ ] **Multi-subscriber homes**: a subscriber count instead of a `subscribed`
  bool, so one home keeps events flowing to several remotes (the multi-plugin
  case; a single host/guest pair never needs it).

## Zed integration

- [ ] **Mount points**: where plugin surfaces attach in the workspace (panels,
  items, status bar) and how they're declared — as methods on the root
  objects taking `Ref<SurfaceApi>`, not as a naming convention.
- [ ] **Packaging**: shipping components through the extension registry;
  versioning the WIT protocol.
- [ ] **Upstreaming**: `run_embedded`/`ApplicationHandle` landed on `main`.
  `Window::attach_root`/`take_root_scene` (branch `gpui-multi-root-embedded`, on top of
  the node engine, zed-industries/zed#63800) wants a PR once the node engine merges; the
  gpui dependency moves to `main` after that.
