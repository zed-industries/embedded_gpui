# TODO

What's deliberately not built yet, in rough priority order. The spike's goal is
to prove the architecture; these are the known gaps between "proven" and
"product".

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
