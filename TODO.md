# TODO

What's deliberately not built yet, in rough priority order. The spike's goal is
to prove the architecture; these are the known gaps between "proven" and
"product".

## Views as replicated objects (the next big design step)

Today views are addressed by name (`host.view("panel")`) over a dedicated
scene channel. That has the same weakness names always have: surfaces that are
*data* — a widget per buffer line, a decoration per diagnostic — can't be a
naming convention. The unification: **renderability becomes a feature of a
shared entity**. A home entity that implements `Render` replicates its display
list the way snapshots replicate state, so its projection on the other side
also implements `Render` and can be mounted anywhere in the element tree.

- `view("panel")` dissolves into `remote::<PanelSpec>("panel")`; the named form
  is just the named-mount special case.
- Inline surfaces are anonymous renderable refs traveling in payloads:
  `Vec<(BufferRow, SharedRef<InlayWidget>)>`, materialized and mounted by the
  host wherever its own layout puts them.
- Input flows backward along the same identity: mouse/key messages addressed to
  the entity, not a view id. Each mount drives resize like a window, as today.
- Composes with the OCAP layer for free: revoking a renderable ref unmounts it
  everywhere; attenuating away input methods yields a render-only capability.

Implementation notes for later: display lists want their own lane or delta
encoding (they're big; don't ride the snapshot channel naively). On surface
overhead: don't make windows lighter, make fewer windows. A gpui `Window`
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
  anticipate this: guest-homed ids carry a high bit).

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
- [ ] **Multi-subscriber homes**: per-sender sequence channels, so several
  projections of one home get correct acks.

## Zed integration

- [ ] **Mount points**: where plugin views attach in the workspace (panels,
  items, status bar) and how they're declared.
- [ ] **Packaging**: shipping components through the extension registry;
  versioning the WIT protocol.
- [ ] **Upstreaming**: `run_embedded`/`ApplicationHandle` is PR'd
  (zed-industries/zed#60574); the gpui git dependency moves to `main` once it
  lands.
