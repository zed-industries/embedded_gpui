// The JavaScript plugin behind tests/js_runtime.rs: exercised through the whole system —
// host -> wasmtime -> the js_runtime component -> QuickJS -> back.

let clicks = 0;

plugin.root = {
  // A plain call answered synchronously.
  greet({ name }) {
    return `hello, ${name}`;
  },

  // A call that goes back through the host's root and returns asynchronously.
  async echo_ping({ message }) {
    return await host.ping({ message });
  },

  // A ref argument becomes a remote; a ref result is a remote handed back. The
  // membrane and factory tests use the same shapes from Rust.
  async relay({ target }) {
    return { forwarded: await target.ping({ message: "via js" }), again: target };
  },

  // The UI path: open a view on a host surface and count the clicks it receives.
  mount({ surface }) {
    const view = plugin.openView(surface);
    const draw = () =>
      view.render(
        div(
          { size_full: true, bg: "#336699", on_click: () => { clicks += 1; draw(); } },
          text(`${clicks} clicks`),
        ),
      );
    draw();
    return "mounted";
  },

  clicks() {
    return clicks;
  },
};
