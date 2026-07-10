# TODO

What's deliberately not built yet, in rough priority order. The spike's goal is
to prove the architecture; these are the known gaps between "proven" and
"product".

## One root object per side: stringless discovery (do this first)

Sequenced deliberately *before* views-as-objects: this deletes protocol
machinery (announcements, name binding) that the views work would otherwise
have to route around, and once both roots exist, "give me a surface" is just
another typed method returning a renderable ref — the two designs snap
together instead of being retrofitted.

Well-known names were the bootstrap namespace; for Zed extensions they should
not survive. Strings are ambient authority (any plugin that can spell
"workspace" can attach to it) and a second identifier system living alongside
refs. The replacement: **one starting object per side**. At init the host hands
the plugin a single capability — the host root, whose typed methods return
everything else (`fn workspace(&mut self, cx) -> SharedRef<WorkspaceApi>`) —
and symmetrically the plugin's `Plugin::new` returns *its* root, through which
the host reaches every plugin surface. Everything participates in one object
system, and authority becomes reachability from your root.

- `share(name)` / `remote(name)` and the whole announcement/name-binding
  machinery (unclaimed announcements, `TYPE_NAME` checks at bind, name-keyed
  projections) get deleted; the wire keeps only refs. Names retreat into
  schema method names, where they belong — codegen, not identifiers (the same
  place Wayland keeps its interface strings).
- The root makes the policy chokepoint concrete and *total*: hand a plugin an
  `Attenuated` root and its whole reachable world is attenuated; a deep
  membrane around the root covers the entire API surface transitively;
  powerbox-style consent is a root method that asks the user before minting a
  ref. Per-plugin tailoring is just constructing different roots.
- Discovery *is* the root schema: optional capabilities are methods returning
  refs (or an error to degrade on); the plugin-to-plugin registry (see
  multi-plugin routing) becomes an object reachable from the root, opt-in as
  before.
- Converges with views-as-objects below: `create_view(name)` is also a string
  API. Renderable refs plus a root means the host asks the plugin's root for
  surfaces, and no string identifiers remain anywhere in the protocol.
- The deeper simplification: today the host provides an assortment of things
  (view slots, shares, mounts) and the plugin provides an assortment of things
  (named views, named entities), each with its own bespoke plumbing. With one
  entry point per side, all of it becomes plain method calls: the host calls
  the plugin root's methods to get the plugin's features, the plugin calls the
  host root's methods to get the host's features, and the initial exchange of
  the two roots is the *entire* bootstrap. The `Plugin` trait shrinks toward
  "construct your root object"; the WIT world trends toward pure transport
  (scenes, input, text, scheduling, one object channel) with every *feature*
  living in the two root schemas, where it can be typed, versioned, attenuated,
  and audited like everything else.
- The caution: the root schema becomes the real compatibility surface for Zed
  extensions — the de facto extension API crate. Unknown methods already fail
  soft, but it wants explicit versioning discipline (a version method, or
  probe-and-degrade conventions), because it can never break casually once
  extensions ship against it.

## Views as replicated objects (after the root object)

Today views are addressed by name (`host.view("panel")`) over a dedicated
scene channel. That has the same weakness names always have: surfaces that are
*data* — a widget per buffer line, a decoration per diagnostic — can't be a
naming convention. The unification: **renderability becomes a feature of a
shared entity**. A home entity that implements `Render` streams its display
list to the other side, where a remote to it can be mounted anywhere in the
element tree.

- `view("panel")` dissolves into a typed method on the plugin's root object
  returning a renderable ref (see "one root object per side"); no view names,
  no view ids.
- Inline surfaces are anonymous renderable refs traveling in payloads:
  `Vec<(BufferRow, SharedRef<InlayWidget>)>`, connected and mounted by the
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
- [ ] **Multi-plugin routing**: several stores behind one host, with the host
  routing shared-entity traffic between plugins (the id spaces already
  anticipate this: guest-homed ids carry a high bit; loopback routing is the
  single-store special case of the same reflection logic). This is also the
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

- [ ] **Tagged refs on the wire**: `SharedRef` crosses as a bare u64 inside
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
