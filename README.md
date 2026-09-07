# embedded_gpui

**Experimental.** GPUI running *inside* a Wasm component, embedded back into a
native GPUI application. A plugin is an ordinary GPUI program — entities,
elements, flexbox layout, input handlers, async tasks — compiled to
`wasm32-wasip2` and rendered by a host app that treats it as just another
element in its tree.

This is a spike exploring a possible UI-extension model for
[Zed](https://github.com/zed-industries/zed). Expect sharp edges and breaking
changes; nothing here is a supported API yet.

## What works today

- A guest-side GPUI platform: guest windows that mirror the host's windows (same
  size and scale factor), in which every host surface is a root of the frame at its
  real origin (a memoized node-engine node, not a window), per-surface display lists
  shipped only when a root changed, overlays (popovers, tooltips, drag previews)
  painted above the host window, real window state (active, appearance, modifiers,
  hover), mouse and keyboard input, timers/async, SVG and image rendering, text via
  host-side shaping. The WIT protocol (`wit/plugin.wit`) is pure substrate — `init`,
  `tick(inbound frames) -> turn`, and synchronous text shaping; nothing in it
  has UI meaning.
- A host runtime: loads a component with wasmtime on a background worker,
  replays its display lists as native GPUI primitives (text hits the host's
  real rasterizer), and never calls into wasm from the frame path.
- **Views are objects**: a host-homed `SurfaceApi` (a place pixels go) and a
  guest-homed `ViewApi` (the thing drawing there). Geometry, input, and
  cursor are method calls on them; there are no view ids or names. Every
  capability tool below applies to views too.
- **Shared objects**: an entity lives on one end (its "home"); the other end
  holds a `Remote` that behaves as much like an `Entity<T>` as a sandbox wall
  allows — typed method calls with responses, `observe` for the home's
  `cx.notify`, `subscribe` for its `cx.emit` events, refcounted auto-release
  on drop, and capability references you can embed in payloads (`Ref`).
  The object model is symmetric and stringless: each end installs one **root
  object** at the reserved address 0 (`share_root`), attaches to the other
  end's root with `root()`, and discovers everything else through method
  calls — ref-returning methods resolve directly to connected `Remote`s, and
  authority is reachability from your root. Object ids are random u64s, so a
  ref can be known but never guessed. Every payload carries a **ref table**
  naming the refs it mentions, so forwarders rewrite capabilities without
  parsing payloads, and a `Ref` knows which registry it arrived through, so
  `ref.connect()` works anywhere.
- **Typed interfaces**: one attribute on a trait (`#[interface]`) makes one
  name the whole interface — `Remote<CounterApi>` for callers,
  `#[shared] impl CounterApi for MyEntity` for the implementation, both
  compile-time checked against the same schema — and `CounterApi::schema()`
  describes it at runtime.
- **OCAP utilities** (`embedded_gpui_util`): `Revocable` (a transitive
  membrane: every ref that crosses it is wrapped, one revoke severs them all),
  `Attenuated` (allowlist), `Audited` (call ledger), and `Mirror` (a local,
  observable cache of remote state — snapshots as a library, not a protocol).
- **Plugins in JavaScript** (`embedded_gpui_js`, an optional crate on top):
  a QuickJS runtime that is itself a plugin. A JS plugin is a directory with
  an `index.js`; the host mounts the directory (a generic grant) and addresses
  the plugin's root like any other — it never learns JavaScript is involved.
  Scripts see the same object model — `host` is the host's root, remotes are
  proxies with promise-returning methods and `observe`, refs cross as
  `{"$ref": n}` so no schema is needed at runtime — and render UI as a data
  tree that becomes GPUI elements on a host surface. Edit `index.js` and the
  runtime reloads it, replaying the host's calls so views come back.
  `typescript::declarations` renders the Rust schemas as `.d.ts` for authors.
- **Resource limits**: a per-turn budget (wasmtime epoch deadline) and a memory
  cap on `PluginOptions`; a plugin that overruns stops and its in-flight calls
  fail instead of hanging.

<img width="750" height="643" alt="Screenshot 2026-07-08 at 12 12 14 AM" src="https://github.com/user-attachments/assets/81d10dfa-bad7-4fb2-9385-5629880c11ca" />

## Embedding

```rust
// Compile + instantiate on a background thread; the store never blocks your UI.
let plugin = PluginHost::load(path, PluginOptions::new(text_system), cx);

// The bootstrap: install your root, take theirs. Every capability either way
// is a method call from here on.
host.share_root(&my_root_entity, cx);
let plugin = host.root::<DemoPlugin>(cx);
let palette = plugin.palette(cx).await?; // a connected Remote<PaletteApi>

// A surface is a place pixels go: an ordinary entity you own and place like any
// element. Share it, hand the ref to the plugin through *its* schema, and the
// guest attaches a view; layout changes and input become method calls on it.
let panel = cx.new(Surface::new);
plugin.show_panel(host.share(&panel, cx), cx);
div()
    .w(px(480.))
    .h(px(320.))
    .child(panel)
```

On the guest, drawing on a surface is `open_view(surface, view, cx)`: the view becomes
a root of the guest window mirroring the surface's host window, at the slot's real
origin; its scene goes to that surface whenever it changes.

The WASI sandbox grants nothing but stdout/stderr by default; every additional
authority is an explicit `PluginOptions::with_wasi` choice — and everything the
plugin can *do* to your app is reachable from the one root object you install
with `share_root`, so tailoring (or faking) a plugin's world is constructing a
different root.

## Quick start

Requires the `wasm32-wasip2` target (`rustup target add wasm32-wasip2`; the
pinned toolchain in `rust-toolchain.toml` installs it automatically).

```sh
cargo run -p example_host
```

(the demo builds its wasm plugin automatically on first run)

The demo window shows two Rust plugin surfaces (a counter button and a panel
with text input, SVG, image, and an animated path), a JavaScript plugin's
button (`example/js_runtime/plugins/counter.js`, with a reload button — edit the
file and click it), and a native button — all of them mutating the same shared
counter entity.

```sh
cargo test -p tests -- --test-threads 1   # protocol tests, and tests/js_plugin.js through the JS runtime
```

## Layout

- `embedded_gpui/` — the one crate both sides use: schema layer (always), host runtime
  (native), guest platform (wasm32). The WIT protocol lives in `embedded_gpui/wit/`.
- `embedded_gpui_macros/` — the `#[interface]` / `#[shared]` / `#[data]`
  proc macros.
- `embedded_gpui_util/` — object-capability patterns (`Revocable`, `Attenuated`,
  `Audited`, `Mirror`).
- `embedded_gpui_js/` — JavaScript plugins: the QuickJS runtime (guest only), its
  `JsRuntimeApi` schema, and the `.d.ts` generator.
- `example/` — the demo: `host/` (native window), `plugin/` (the Rust wasm
  component), `js_runtime/` (the one-line component that registers
  `embedded_gpui_js`, plus `plugins/counter.js`), and `schema/`.
- `tests/` — protocol integration tests plus their `test_plugin/` fixture.

## Reading order

1. `DESIGN.md` — architecture and invariants.
2. `embedded_gpui/wit/plugin.wit` — the wire protocol, heavily commented.
3. `example/plugin/` — what plugin code looks like.

GPUI is consumed from the zed repository: the node-engine branch
(zed-industries/zed#63800) plus `Window::attach_root` / `take_root_scene`, the hook that
lets a surface be a root of the guest's window (branch `gpui-multi-root-embedded`).

## License

Apache 2.0, like GPUI itself — see [LICENSE-APACHE](LICENSE-APACHE).
