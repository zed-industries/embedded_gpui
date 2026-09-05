# TODO

What's deliberately not built yet, in rough priority order. The spike's goal is
to prove the architecture; these are the known gaps between "proven" and
"product".

## Surfaces and views (done: views are objects)

Views are `SurfaceApi`/`ViewApi` objects; see `DESIGN.md`. What remains:

- [ ] **Data-shaped surfaces at scale**: a widget per buffer line means hundreds of
  surfaces. Don't make windows lighter, make fewer windows: the guest should mount
  every view as an absolutely-positioned region inside one hidden composition window
  and slice the scene into per-surface display lists at serialization (a serializer
  change, not a protocol change). Single-window focus matches the host's one-focus
  model; per-region damage tracking is a later optimization.
- [ ] **Cross-instance scenes**: a surface ref can already travel from one plugin to
  another through the object model, but each `PluginHost` routes scenes only to
  surfaces shared through its own registry, and image caches are per instance. Routing
  scenes by surface id across hosts is the composition story.
- [ ] **Display-only views** as an `open_view` option (attach an `Attenuated<ViewApi>`
  allowing only `resize`), so a plugin can lend a surface it cannot be poked through.
- [ ] **Delta-encoded scenes**: display lists are the one bulk path; frame-to-frame
  diffing when a profile asks for it.

## Object model follow-ups (from the root-object pass)

- [ ] **Promise pipelining**: a ref returned by a method call round-trips before
  you can call through it (the demo's views render a brief "connecting" state;
  the receipts already resolve to connected `Remote`s, so only the latency is
  left). Random ids open the cleanest design: *caller-allocated ids*, where the
  calling end mints the id for the object a method will return and sends it in
  the request — the returned `Remote` is usable immediately, sends FIFO behind
  the allocating call, and no promise tables exist. Needs home-side binding of
  the pre-minted id. (This is CapTP's `answer-pos` mechanism reached from the
  random-ids direction; see the prior-art section in DESIGN.md.)
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
  believes it mounted, and neither side gets a signal. Think through: stop on the first
  replay failure and report it through `load`'s error (method name + position in the
  history), degrade to "loaded, nothing mounted" rather than half-mounted; whether
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

- [x] **Resource limits**: per-turn epoch deadline and memory cap (`PluginOptions`);
  a trapped plugin stops and in-flight calls fail. Still open: a display-list size
  cap at scene submission, and a UI for the host to show a stopped plugin.
- [ ] **IME / marked text**: guests currently synthesize printable keys through
  `replace_text_in_range` (Linux-backend style). Dead keys and CJK composition
  need the host to proxy its `PlatformInputHandler` into the guest.
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
- [ ] **Upstreaming**: `run_embedded`/`ApplicationHandle` is PR'd
  (zed-industries/zed#60574); the gpui git dependency moves to `main` once it
  lands.
