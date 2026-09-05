# What plugin rendering needs from the retained node engine

Notes against zed-industries/zed#63800 ("gpui: Add an experimental retained view-node
engine"), written from the plugin side. Each item says what `embedded_gpui` does today,
what it would do with the node engine, and the specific capability that needs.

## Where we stand

A plugin is a GPUI `App` inside a wasm component. Every host **surface** a plugin draws
on is a guest `Window`: the guest renders it to a `Scene`, serializes the whole scene as a
display list, and the host replays the list into its own frame. Input is forwarded as raw
mouse/key events into the guest window's dispatch. This is correct and fast enough for a
handful of panels. It does not scale to data-shaped UI (a widget per buffer line), and it
reships a whole scene when one leaf changes.

The workaround we were about to build — one hidden composition window per guest, with
surfaces as absolutely positioned regions and the scene sliced per region at
serialization — fakes three things a `Window` owns: scale factor, focus, and the input
dispatch tree. The node engine makes the honest version possible instead.

## 1. A surface is a view node, not a window

**Need:** a stable identity for a mounted view occurrence (`Entity<ViewNode>`) whose
retained output — Taffy subtree, geometry, recorded scene fragment — can be addressed
from outside the render pass.

**Use:** in the guest, every host surface becomes a node in one guest window rather than
a window of its own. The serializer ships each surface node's fragment as that surface's
display list. Hundreds of widgets become hundreds of nodes in one window, which is GPUI's
design envelope, instead of hundreds of windows.

**Specifically:** a public way to (a) mark a node as a "fragment root" and (b) read its
recorded scene fragment and geometry after a frame, by node identity. The PR's "store
scene fragments with child-node references" is exactly the data; it needs an
outside-the-frame reader.

## 2. Only dirty fragments cross the boundary

**Need:** per-node dirtiness and fragment stability across frames, as the PR already
tracks for reuse.

**Use:** the guest ships only fragments whose scope rebuilt; the host splices them into
its own retained fragment for that surface. Scene volume for large or animating plugin
views — the top "known risk" in `DESIGN.md` — stops being a risk.

**Specifically:** a frame-level report of which fragment roots changed this frame (the
retained-frame statistics almost have this), and a guarantee that an unchanged fragment
root's child-node references remain valid, so a host-side splice can keep them.

## 3. The host side: a plugin's output as a retained fragment

**Need:** a way for a host element to contribute a pre-recorded scene fragment (with
child-node references) as its paint output, and to be treated as a clean scope until the
fragment changes.

**Use:** today `Surface::render` replays a display list through `paint_*` calls every
frame. With the node engine the surface *is* a node whose fragment is the guest's output:
an idle plugin costs what a clean scope costs, and a changed guest fragment is exactly one
dirty scope on the host.

**Specifically:** `Window::paint_fragment(fragment)` or equivalent — the inverse of
reading a fragment out. Fragments would need a serializable form; ours already exists
(`display-list` in `plugin.wit`) and could converge on the engine's.

## 4. Recorded input effects, replayable by the host

**Need:** input effects (hit regions, cursor styles, listeners' presence) recorded per
node as part of the retained output, as the PR describes.

**Use:** the host could hit-test a plugin's nodes itself and route input to the specific
guest node, instead of replaying raw mouse events into the guest window and letting the
guest re-hit-test. Cursor style becomes retained output rather than a `set_cursor` call
after the fact.

**Specifically:** recorded hit regions keyed by node identity, exportable alongside the
fragment. Listener *invocation* stays in the guest (that's where the closures live); only
the routing decision moves.

## 5. Scale factor per node (or per fragment root)

**Need:** the ambient inputs a clean scope checks before reuse should include scale
factor, and a fragment root should be able to render at a scale different from its
window's.

**Why:** two surfaces of one plugin can sit on two host windows with different scale
factors. Today that's fine only because each surface is its own guest window. With one
guest window per plugin, per-node scale is what keeps text crisp on both displays.

## 6. Focus per node, with an explicit boundary

This is the deep one, and the one the plugin API has barely touched. GPUI's focus is
per-window; the plugin boundary needs:

- **Activation mirroring**: host focuses/blurs a surface → the guest's corresponding node
  (today: window) activates/deactivates. GPUI has this concept for windows; it would need
  it for fragment roots.
- **Traversal handoff**: Tab inside a plugin must be able to *leave*. The guest signals
  "focus escaped forward/backward" and the host resumes its own traversal; entering a
  surface tells the guest to focus its first/last element. If focus becomes a per-node
  property with a traversal order, a fragment root is a natural boundary where the engine
  could raise "left the subtree" instead of us inventing a message.
- **Programmatic focus as a capability**: the guest may *request* focus for a surface;
  the host decides. Nothing in the engine needs to change for this; it's protocol.
- **Synchronous key precedence**: the host cannot await "did the guest handle this
  keystroke" mid-dispatch. The workable answer is declared bindings — the guest publishes
  the keystrokes its focused context handles — so the host resolves precedence
  synchronously. If the node engine records the focused node's key context as retained
  output, that publication is free.

## 7. Two things that would help and are cheap

- **Deferred-draw and overlay ordering as data.** Today guest primitives carry a scene
  `order` and the host replays ordered groups in `paint_layer`s. If fragments carry their
  stacking as part of the recorded output, the host's replay loses its one bit of
  interpretation.
- **Path geometry copies.** The PR's checklist already lists this; display lists ship
  tessellated triangle lists across the boundary, so any reduction in copies helps twice.

## What we can do without any of this

Everything above is about scale and polish, not correctness: the object model, surfaces
as objects, membranes, resource limits, and the JS runtime are independent of how scenes
are recorded. The plugin layer's asks of the node engine are items 1–3 (fragment roots
readable and paintable from outside the frame, with change reports); 4–6 are where the
plugin boundary would inform the engine's design rather than consume it.
