# TODO

What's deliberately not built yet, in rough priority order. The spike's goal is
to prove the architecture; these are the known gaps between "proven" and
"product".

## Views as replicated objects (do this first)

Today views are addressed by name (`host.view("panel")`) over a dedicated
scene channel. That has the same weakness names always have: surfaces that are
*data* — a widget per buffer line, a decoration per diagnostic — can't be a
naming convention. The unification: **renderability becomes a feature of a
shared object**. A home entity that implements `Render` streams its display
list to the other side, where a remote to it can be mounted anywhere in the
element tree.

- `view("panel")` dissolves into a typed method on the plugin's root object
  returning a renderable ref — the roots exist now, so this is the last string
  identifier in the protocol; no view names, no view ids.
- Inline surfaces are anonymous renderable refs traveling in payloads:
  `Vec<(BufferRow, Ref<InlayWidget>)>`, connected and mounted by the
  host wherever its own layout puts them.
- Input flows backward along the same identity: mouse/key messages addressed to
  the entity, not a view id. Each mount drives resize like a window, as today.
- Composes with the OCAP layer for free: revoking a renderable ref unmounts it
  everywhere; attenuating away input methods yields a render-only capability.

Likely rendering shape (sketch): scenes upload to **tracks**, identified by a
shared object id rather than a view id — keeping today's `PluginElement`-style
retained replay, but with each track a disjoint rendering timeline the host
composites out of a cache. That may need small gpui changes on the host side;
display lists are big, so tracks want their own lane and eventually a delta
encoding (frame-to-frame scene diffing), not the object-message channel.

Implementation notes for later: on surface overhead, don't make windows
lighter, make fewer windows. A gpui `Window`
carries real baseline weight (two retained frames, a Taffy arena, per-window
frame pump) that's irrelevant for a handful of panels but wasteful for hundreds
of widgets — while "one window, thousands of elements" is exactly gpui's design
envelope. So the guest should mount all renderable entities as
absolutely-positioned regions inside a single hidden composition window and
slice the scene into per-region display lists at serialization (a serializer
change, not a protocol change). Single-window focus matches the host's
one-focus model; window-granular dirtiness (one animating widget redraws the
composition) is fine at gpui's normal scale, with per-region damage tracking as
a later optimization.

## Object model follow-ups (from the root-object pass)

- [ ] **Promise pipelining**: a ref returned by a method call round-trips before
  you can call through it (the demo's views render a brief "connecting" state;
  the receipts already resolve to connected `Remote`s, so only the latency is
  left). Random ids open the cleanest design: *caller-allocated ids*, where the
  calling end mints the id for the object a method will return and sends it in
  the request — the returned `Remote` is usable immediately, sends FIFO behind
  the allocating call, and no promise tables exist. Needs home-side binding of
  the pre-minted id.
- [ ] **Share dedup**: sharing the same entity twice mints two independent ids.
  Dedup wants a per-entity identity map (and interacts with release: both refs
  share one strong hold).
- [ ] **Root versioning discipline**: the root schema is the real compatibility
  surface (for Zed: the extension API). Unknown methods already fail soft, but
  it wants an explicit convention — a `version()` method or probe-and-degrade —
  before anything ships against it.
- [ ] **A public symmetric handle**: `PluginHostHandle` (host) and the free
  functions (plugin) expose the same operations with the same names; a shared
  `Peer`-style handle type both ends hand out would finish the symmetry at the
  API-surface level too.

## Platform completeness

- [ ] **Resource limits**: wasmtime epoch interruption for runaway plugins, store
  memory caps, per-plugin fuel budgets. Cheap to add, essential for trust.
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
  audit, or powerbox-style user consent before a ref is forwarded. Depends
  on tagged refs (id rewriting across plugin id-spaces, grant tracking) and
  multi-subscriber homes.

## Advanced OCAPs

- [ ] **Tagged refs on the wire**: `Ref` crosses as a bare u64 inside
  opaque payloads, so nothing can find refs in transit. Making refs a
  distinguished wire type enables everything below, plus host-side capability
  accounting (knowing exactly which refs each plugin holds).
- [ ] **Deep membrane**: wrap an object *graph* so every ref passing through in
  either direction is auto-wrapped, and one revoke severs the whole surface.
  Requires tagged refs.
- [ ] **Loopback routing**: a guest materializing a ref to its own home (needed
  to stack same-side caretakers, e.g. Revocable over Audited in one guest).
  The host is already the router; it would reflect guest-addressed traffic
  back, rewriting request ids.
- [ ] **Expiring / N-use grants**: a `Revocable` that severs itself after a
  deadline or call budget.
- [ ] **Sealer/unsealer pairs**: rights amplification, for when plugins trade
  refs among each other.
- [ ] **Multi-subscriber homes**: a subscriber count instead of a `subscribed`
  bool, so one home keeps events flowing to several remotes (the multi-plugin
  case; a single host/guest pair never needs it).

## Zed integration

- [ ] **Mount points**: where plugin views attach in the workspace (panels,
  items, status bar) and how they're declared — as methods on the root
  objects (see "stringless discovery"), not as a naming convention.
- [ ] **Packaging**: shipping components through the extension registry;
  versioning the WIT protocol.
- [ ] **Upstreaming**: `run_embedded`/`ApplicationHandle` is PR'd
  (zed-industries/zed#60574); the gpui git dependency moves to `main` once it
  lands.
