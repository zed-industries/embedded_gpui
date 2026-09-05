// A plugin in JavaScript. This directory is the plugin: the host mounts it for the
// js_runtime component, which runs index.js (and reruns it when the file changes — edit
// this while the demo is running). The host then calls `show_button(surface)` on the
// plugin's root, exactly as it does on the Rust demo plugin; it never learns that the
// answer comes from JavaScript. Everything here is the object model: `host` is the
// host's root, `host.counter()` returns a remote, remotes have promise-returning methods
// and `observe`, and the UI is a tree of data handed to `view.render`.

plugin.root = {
  async show_button({ surface }) {
    const counter = await host.counter();
    const view = plugin.openView(surface);
    let clicks = await counter.clicks();

    const draw = () =>
      view.render(
        div(
          {
            size_full: true, flex: true, flex_col: true,
            items_center: true, justify_center: true, gap: 4,
            bg: "#3b2f5c", rounded: 10, border: 2, border_color: "#8a7ac0",
            on_click: () => counter.increment({ by: 1 }),
          },
          text(`JS clicked ${clicks}x`, { text_color: "#f0eaff" }),
          text("QuickJS inside wasm inside GPUI", { text_color: "#b8a9e8", text_size: 11 }),
        ),
      );

    counter.observe(async () => {
      clicks = await counter.clicks();
      draw();
    });
    draw();
    console.log("counter.js: button mounted");
  },
};
