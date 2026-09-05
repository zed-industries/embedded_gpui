// The JavaScript half of the js_runtime component. Everything a script sees is defined
// here in terms of a few functions the Rust side installs on `__rt`:
//   __rt.call(targetId, method, json, refs, resolve, reject)
//   __rt.observe(targetId, callback)
//   __rt.openView(surfaceId) -> viewKey
//   __rt.render(viewKey, json)
//   __rt.respond(requestId, json, refs, isError)
//   __rt.log(message)
// Object ids are strings: they are u64s, which JS numbers cannot hold.

const REMOTE = Symbol("embedded_gpui.remote");
const remotes = new Map();

function makeRemote(id) {
  if (remotes.has(id)) return remotes.get(id);
  const target = {
    [REMOTE]: id,
    call(method, args) {
      const { json, refs } = encode(args === undefined ? {} : args);
      return new Promise((resolve, reject) =>
        __rt.call(id, method, json, refs, resolve, reject));
    },
    observe(callback) {
      __rt.observe(id, callback);
      return { cancel() {} };
    },
  };
  // Names the language itself probes (`await` looks for `then`, JSON.stringify for
  // `toJSON`) must not become method calls.
  const NOT_METHODS = new Set(["then", "toJSON", "constructor", "valueOf", "toString"]);
  const remote = new Proxy(target, {
    get(target, property) {
      if (property in target || typeof property === "symbol" || NOT_METHODS.has(property)) {
        return target[property];
      }
      return (args) => target.call(property, args);
    },
  });
  remotes.set(id, remote);
  return remote;
}

// Payloads are JSON whose refs are `{"$ref": index}` into a table of ids. Remotes in a
// value become table entries; table entries in a payload become remotes.
function encode(value) {
  const refs = [];
  const json = JSON.stringify(value, (_, v) => {
    if (v !== null && typeof v === "object" && v[REMOTE] !== undefined) {
      refs.push(v[REMOTE]);
      return { $ref: refs.length - 1 };
    }
    return v;
  });
  return { json: json === undefined ? "null" : json, refs };
}

function decode(json, refs) {
  return JSON.parse(json, (_, v) => {
    if (v !== null && typeof v === "object" && typeof v.$ref === "number") {
      return makeRemote(refs[v.$ref]);
    }
    return v;
  });
}

// A call from the host to this plugin's root: answer from `plugin.root`, synchronously
// or when the returned promise settles.
function dispatch(requestId, method, json, refs) {
  const respond = (value, isError) => {
    if (isError) {
      __rt.respond(requestId, JSON.stringify(String(value?.stack ?? value)), [], true);
      return;
    }
    const { json, refs } = encode(value === undefined ? null : value);
    __rt.respond(requestId, json, refs, false);
  };
  try {
    const handler = plugin.root?.[method];
    if (typeof handler !== "function") {
      throw new Error(`plugin.root has no method ${JSON.stringify(method)}`);
    }
    const result = handler.call(plugin.root, decode(json, refs));
    if (result instanceof Promise) {
      result.then((v) => respond(v, false), (e) => respond(e, true));
    } else {
      respond(result, false);
    }
  } catch (error) {
    respond(error, true);
  }
}

// UI is data. A node is `{ type, style, children, text, on_click }`; functions become
// handler ids the Rust side invokes back through `invokeHandler`.
const handlers = new Map();
let nextHandler = 1;

function serializeTree(node) {
  if (node === null || node === undefined || node === false) return null;
  if (typeof node === "string" || typeof node === "number") {
    return { type: "text", text: String(node) };
  }
  const out = { type: node.type, style: node.style ?? {}, children: [] };
  if (node.text !== undefined) out.text = String(node.text);
  if (typeof node.on_click === "function") {
    const id = nextHandler++;
    handlers.set(id, node.on_click);
    out.on_click = id;
  }
  for (const child of node.children ?? []) {
    const serialized = serializeTree(child);
    if (serialized) out.children.push(serialized);
  }
  return out;
}

function invokeHandler(id) {
  const handler = handlers.get(id);
  if (handler) {
    Promise.resolve().then(handler).catch((error) => __rt.log(`handler failed: ${error}`));
  }
}

class View {
  constructor(key) {
    this.key = key;
  }
  render(tree) {
    handlers.clear();
    __rt.render(this.key, JSON.stringify(serializeTree(tree)));
  }
}

globalThis.plugin = {
  root: {},
  openView(surface) {
    return new View(__rt.openView(surface[REMOTE]));
  },
};

globalThis.div = (style, ...children) => ({ type: "div", style, children });
globalThis.text = (text, style) => ({ type: "text", text, style });

globalThis.console = {
  log: (...args) => __rt.log(args.map(String).join(" ")),
  error: (...args) => __rt.log("ERROR " + args.map(String).join(" ")),
};

globalThis.__prelude = { makeRemote, decode, dispatch, invokeHandler };

// The host's root object: address 0, from either end.
globalThis.host = makeRemote("0");
