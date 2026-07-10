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

- A guest-side GPUI platform: windows, retained display lists,
  mouse and keyboard input, timers/async, SVG and image rendering, text via
  host-side shaping — all over a WIT protocol (`wit/plugin.wit`).
- A host runtime: loads a component with wasmtime, replays its display
  lists as native GPUI primitives (text hits the host's real rasterizer), and
  never calls into wasm from the frame path.
- **Shared entities**: an entity lives on one side ("home"); the other side
  holds a `Remote` that behaves as much like an `Entity<T>` as a sandbox wall
  allows — typed method calls with responses, `observe` for the home's
  `cx.notify`, `subscribe` for its `cx.emit` events, refcounted auto-release
  on drop, and capability references you can embed in payloads (`SharedRef`).
- **Typed interfaces**: one attribute on a trait (`#[shared_interface]`) makes
  one name the whole interface — `Remote<CounterApi>` for callers,
  `#[shared_home] impl CounterApi for MyEntity` for the implementation, both
  compile-time checked against the same schema.
- **OCAP utilities** (`embedded_gpui_util`): `Revocable` (caretaker/membrane),
  `Attenuated` (allowlist), `Audited` (call ledger), and `Mirror` (a local,
  observable cache of remote state — snapshots as a library, not a protocol).

<img width="750" height="643" alt="Screenshot 2026-07-08 at 12 12 14 AM" src="https://github.com/user-attachments/assets/81d10dfa-bad7-4fb2-9385-5629880c11ca" />


## Embedding

```rust
// Compile + instantiate on a background thread; the store never blocks your UI.
let plugin = PluginHost::load(path, PluginOptions::new(text_system), cx);

// Later: views by name, placed like any other element. Creation is lazy (the
// guest sees the measured slot size), and layout changes become window resizes.
div()
    .w(px(480.))
    .h(px(320.))
    .child(plugin.view("panel", cx))
```

The WASI sandbox grants nothing but stdout/stderr by default; every additional
authority is an explicit `PluginOptions::with_wasi` choice.

## Quick start

Requires the `wasm32-wasip2` target (`rustup target add wasm32-wasip2`; the
pinned toolchain in `rust-toolchain.toml` installs it automatically).

```sh
cargo run -p example_host
```

(the demo builds its wasm plugin automatically on first run)

The demo window shows two embedded plugin views (a counter button and a panel
with text input, SVG, image, and an animated path) plus a native button — all
three mutate the same shared counter entity.

```sh
cargo test -p tests -- --test-threads 1   # protocol tests
```

## Layout

- `embedded_gpui/` — the one crate both sides use: schema layer (always), host runtime
  (native), guest platform (wasm32). The WIT protocol lives in `embedded_gpui/wit/`.
- `embedded_gpui_macros/` — the `#[shared_interface]` / `#[shared_home]` /
  `#[shared_data]` proc macros.
- `embedded_gpui_util/` — object-capability patterns (`Revocable`, `Attenuated`,
  `Audited`, `Mirror`).
- `example/` — the demo: `host/` (native window) and `plugin/` (the wasm component).
- `tests/` — protocol integration tests plus their `test_plugin/` fixture.

## Reading order

1. `DESIGN.md` — architecture and invariants.
2. `embedded_gpui/wit/plugin.wit` — the wire protocol, heavily commented.
3. `example/plugin/` — what plugin code looks like.

GPUI is consumed as a git dependency on the zed repository (the
`gpui-embedded-in-gpui` branch until the small upstream hook it needs merges).

## License

Apache 2.0, like GPUI itself — see [LICENSE-APACHE](LICENSE-APACHE).
