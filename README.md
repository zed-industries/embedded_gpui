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

- A guest-side GPUI platform (`plugin/`): windows, retained display lists,
  mouse and keyboard input, timers/async, SVG and image rendering, text via
  host-side shaping — all over a WIT protocol (`wit/plugin.wit`).
- A host runtime (`src/`): loads a component with wasmtime, replays its display
  lists as native GPUI primitives (text hits the host's real rasterizer), and
  never calls into wasm from the frame path.
- **Shared entities**: state lives on one side ("home"), the other side holds a
  live replica. Typed messages and calls with responses, read-your-writes
  ordering, capability references you can embed in payloads (`SharedRef`),
  attenuation, async handlers, and refcounted auto-release on drop.
- **Typed interfaces**: declare a trait with `#[shared_interface]` and get the
  schema, typed callers for both sides, and handler registration generated.
- **OCAP utilities** (`embedded_gpui_utils`): `Revocable`, a generic
  caretaker/membrane that wraps any capability with pass-through snapshots,
  method forwarding, and revocation.

## Quick start

Requires the `wasm32-wasip2` target (`rustup target add wasm32-wasip2`; the
pinned toolchain in `rust-toolchain.toml` installs it automatically).

```sh
./build_plugin.sh                        # compile the example plugin to wasm
cargo run --bin gpui_embedded_demo       # run the native host demo
```

The demo window shows two embedded plugin views (a counter button and a panel
with text input, SVG, image, and an animated path) plus a native button — all
three mutate the same shared counter entity.

```sh
cargo test --test shared_entities -- --test-threads 1   # protocol tests
```

## Reading order

1. `DESIGN.md` — architecture and invariants.
2. `wit/plugin.wit` — the wire protocol, heavily commented.
3. `example_plugin/` — what plugin code looks like.

GPUI is consumed as a git dependency on the zed repository (the
`gpui-embedded-in-gpui` branch until the small upstream hook it needs merges).

## License

Apache 2.0, like GPUI itself — see [LICENSE-APACHE](LICENSE-APACHE).
