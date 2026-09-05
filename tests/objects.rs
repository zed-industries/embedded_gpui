//! End-to-end tests for the object model: a real wasm32-wasip2 plugin (see `test_plugin/`)
//! loaded into a wasmtime store, driven from GPUI's deterministic test executor.
//!
//! The bootstrap is symmetric and stringless: each end installs a root object at the
//! reserved address 0 (`share_root`) and attaches to the other end's root with `root`.
//! Every capability below is a method call on a remote: methods declared to return
//! refs resolve directly with connected `Remote`s.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use embedded_gpui::schema::TypeKind;
use embedded_gpui::surface::{
    Geometry, Modifiers, MouseButton, MouseButtonEvent, MouseEvent, Point, ViewApiCaller as _,
};
use embedded_gpui::{
    Interface, Payload, PluginHost, PluginHostHandle as _, PluginInstance, PluginOptions, Ref,
    Remote, Surface, TypeSchema, ViewApi, decode, encode, shared, typescript,
};
use embedded_gpui_util::{Attenuated, Audited, Mirror};
use gpui::{AppContext as _, Context, Entity, Task, TestAppContext};
use rand::prelude::*;
use test_schema::{
    Bump, ChameleonApi, ChameleonState, Count, CounterMilestone, FactoryApi, FactoryApiCaller as _,
    GatekeeperApi, GatekeeperApiCaller as _, Increment, ItemApiCaller as _, KeyApi,
    KeyApiCaller as _, TestCounterApi, TestCounterApiCaller as _, TestHost, TestPlugin,
    TestPluginCaller as _, VaultApi, VaultApiCaller as _, ViewProbeApiCaller as _,
};

/// Builds the test plugin once per process and returns the component path.
fn test_plugin_path() -> PathBuf {
    use std::sync::Once;
    static BUILD: Once = Once::new();
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let plugin_dir = manifest_dir.join("test_plugin");
    BUILD.call_once(|| {
        // Blocking is fine here: tests build their fixture once, up front.
        #[allow(clippy::disallowed_methods)]
        let output = std::process::Command::new("cargo")
            .args(["build", "--target", "wasm32-wasip2"])
            .current_dir(&plugin_dir)
            .output()
            .expect("failed to spawn cargo to build test_plugin");
        assert!(
            output.status.success(),
            "building test_plugin failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    });
    plugin_dir.join("target/wasm32-wasip2/debug/test_plugin.wasm")
}

/// The host's root object: what the plugin reaches through `root()`.
struct HostRoot {
    pings: u32,
}

#[shared]
impl TestHost for HostRoot {
    fn ping(&mut self, message: String, cx: &mut Context<Self>) -> String {
        self.pings += 1;
        cx.notify();
        format!("pong: {message}")
    }
}

/// A host without its root installed yet, for exercising the bootstrap race.
fn setup_without_root(cx: &mut TestAppContext) -> Entity<PluginHost> {
    setup_with_options(
        PluginOptions::new(Arc::new(gpui::NoopTextSystem::new())),
        cx,
    )
}

fn setup_with_options(options: PluginOptions, cx: &mut TestAppContext) -> Entity<PluginHost> {
    let path = test_plugin_path();
    let instance = cx.update(|_| {
        PluginInstance::new(&path, options).expect("failed to instantiate test plugin")
    });
    cx.new(|cx| PluginHost::new(instance, cx))
}

fn setup(cx: &mut TestAppContext) -> Entity<PluginHost> {
    let host = setup_without_root(cx);
    cx.update(|cx| {
        let root = cx.new(|_| HostRoot { pings: 0 });
        host.share_root(&root, cx);
    });
    host
}

/// Flush deferred effects and host-scheduled ticks deterministically.
fn settle(cx: &mut TestAppContext) {
    for _ in 0..5 {
        cx.executor().run_until_parked();
        cx.executor().advance_clock(Duration::from_millis(100));
    }
    cx.executor().run_until_parked();
}

/// Call one of the plugin root's capability-returning methods: the receipt resolves
/// with a live, connected `Remote` — the whole discovery story in one await.
macro_rules! from_root {
    ($fn_name:ident -> $spec:ty) => {
        async fn $fn_name(host: &Entity<PluginHost>, cx: &mut TestAppContext) -> Remote<$spec> {
            let root = cx.update(|cx| host.root::<TestPlugin>(cx));
            let receipt = cx.update(|cx| root.$fn_name(cx));
            settle(cx);
            receipt
                .await
                .expect(concat!(stringify!($fn_name), " remote"))
        }
    };
}

from_root!(counter -> TestCounterApi);
from_root!(factory -> FactoryApi);
from_root!(gatekeeper -> GatekeeperApi);
from_root!(chameleon -> ChameleonApi);

#[gpui::test]
async fn test_roots_bootstrap_both_directions(cx: &mut TestAppContext) {
    let host = setup(cx);

    // Host -> plugin: the plugin's root answers like any object.
    let root = cx.update(|cx| host.root::<TestPlugin>(cx));

    // Plugin -> host, from inside a handler: the plugin holds a remote to the host's
    // root and calls it, so one receipt exercises both bootstraps.
    let receipt = cx.update(|cx| root.ping_host("hello".to_string(), cx));
    settle(cx);
    assert_eq!(receipt.await.expect("ping_host"), "pong: hello");
}

#[gpui::test]
async fn test_bootstraps_may_race(cx: &mut TestAppContext) {
    // Let the plugin fully boot — its `root()` remote has already subscribed to a host
    // root that does not exist yet. That traffic queues instead of failing.
    let host = setup_without_root(cx);
    settle(cx);

    // Install the root late: the queued traffic drains, and the bootstrap completes
    // as if the orders had never crossed.
    cx.update(|cx| {
        let root = cx.new(|_| HostRoot { pings: 0 });
        host.share_root(&root, cx);
    });

    let plugin = cx.update(|cx| host.root::<TestPlugin>(cx));
    let receipt = cx.update(|cx| plugin.ping_host("late".to_string(), cx));
    settle(cx);
    assert_eq!(receipt.await.expect("ping_host"), "pong: late");
}

#[gpui::test]
async fn test_send_is_read_your_writes_by_ordering(cx: &mut TestAppContext) {
    let host = setup(cx);
    let counter = counter(&host, cx).await;

    let receipt = cx.update(|cx| counter.call(Increment { by: 3 }, cx));
    settle(cx);
    receipt.await.expect("send should be acknowledged");

    // Reads are calls, and FIFO ordering means any read issued after a send observes it.
    let count = cx.update(|cx| counter.count(cx));
    settle(cx);
    assert_eq!(count.await.expect("count"), 3);
}

#[gpui::test]
async fn test_calls_resolve_with_responses_in_order(cx: &mut TestAppContext) {
    let host = setup(cx);
    let counter = counter(&host, cx).await;

    let first = cx.update(|cx| counter.increment(2, cx));
    let second = cx.update(|cx| counter.increment(5, cx));
    settle(cx);

    // FIFO ordering makes responses deterministic prefix sums.
    assert_eq!(first.await.expect("first call"), 2);
    assert_eq!(second.await.expect("second call"), 7);
}

#[gpui::test]
async fn test_mirror_keeps_an_observable_local_copy(cx: &mut TestAppContext) {
    let host = setup(cx);
    let counter = counter(&host, cx).await;

    // A mirror is snapshots-as-a-library: it refetches `count` on every notify from the
    // home, starting with the notify that answers the subscription.
    let count = cx.update(|cx| Mirror::new(counter.clone(), Count {}, cx));
    let notified = Rc::new(RefCell::new(0));
    let _observation = cx.update(|cx| {
        let notified = notified.clone();
        cx.observe(&count, move |_, _| *notified.borrow_mut() += 1)
    });
    settle(cx);
    let observed = count.read_with(cx, |mirror, _| mirror.latest().copied());
    assert_eq!(observed, Some(0), "initial value arrives on its own");

    let receipt = cx.update(|cx| counter.call(Increment { by: 4 }, cx));
    settle(cx);
    receipt.await.expect("send");
    settle(cx);
    let observed = count.read_with(cx, |mirror, _| mirror.latest().copied());
    assert_eq!(observed, Some(4), "mirror follows the home's notifies");
    assert!(*notified.borrow() >= 2, "mirror notifies its observers");
}

#[gpui::test]
async fn test_events_cross_the_boundary(cx: &mut TestAppContext) {
    let host = setup(cx);
    let counter = counter(&host, cx).await;

    let milestones: Rc<RefCell<Vec<u32>>> = Rc::default();
    let _subscription = cx.update(|cx| {
        let milestones = milestones.clone();
        counter.subscribe::<CounterMilestone>(cx, move |event, _| {
            milestones.borrow_mut().push(event.count);
        })
    });

    let below = cx.update(|cx| counter.increment(7, cx));
    settle(cx);
    below.await.expect("first increment");
    assert!(
        milestones.borrow().is_empty(),
        "no milestone crossed at 7 clicks"
    );

    let crossing = cx.update(|cx| counter.increment(5, cx));
    settle(cx);
    crossing.await.expect("second increment");
    assert_eq!(
        milestones.borrow().as_slice(),
        &[12],
        "the home's cx.emit arrives at Remote::subscribe"
    );
}

#[gpui::test]
async fn test_shared_refs_build_object_graphs(cx: &mut TestAppContext) {
    let host = setup(cx);
    let factory = factory(&host, cx).await;

    // A call whose response is declared as a ref: the receipt resolves with a live
    // Remote to the freshly shared child — allocation over there, handle over here.
    let created = cx.update(|cx| factory.create("alpha".to_string(), cx));
    settle(cx);
    let item = created.await.expect("create should respond with a remote");
    let bumped = cx.update(|cx| item.bump(cx));
    settle(cx);
    assert_eq!(bumped.await.expect("bump"), 1);

    let info = cx.update(|cx| item.describe(cx));
    settle(cx);
    let info = info.await.expect("describe");
    assert_eq!(info.label, "alpha");
    assert_eq!(info.bumps, 1);

    // Distinct creations yield distinct capabilities.
    let created_again = cx.update(|cx| factory.create("beta".to_string(), cx));
    settle(cx);
    let second = created_again.await.expect("second create");
    assert_ne!(second.reference().entity_id(), item.reference().entity_id());
}

/// The host half of the membrane test: an entity whose secret is only reachable via a
/// capability, with a deliberately asynchronous read handler, and which hands out a
/// further capability (the key) so the membrane has a ref to wrap.
struct Vault {
    secret: String,
    key: Entity<Key>,
    key_ref: Option<Ref<KeyApi>>,
    host: Entity<PluginHost>,
}

#[shared]
impl VaultApi for Vault {
    fn read(&mut self, cx: &mut Context<Self>) -> Task<anyhow::Result<String>> {
        let secret = self.secret.clone();
        cx.spawn(async move |_, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(10))
                .await;
            Ok(secret)
        })
    }

    fn key(&mut self, cx: &mut Context<Self>) -> Ref<KeyApi> {
        if let Some(reference) = &self.key_ref {
            return reference.clone();
        }
        let reference = self.host.share(&self.key, cx);
        self.key_ref = Some(reference.clone());
        reference
    }
}

struct Key;

#[shared]
impl KeyApi for Key {
    fn unlock(&mut self, _cx: &mut Context<Self>) -> String {
        "opened".to_string()
    }
}

fn new_vault(host: &Entity<PluginHost>, cx: &mut TestAppContext) -> Entity<Vault> {
    cx.new(|cx| Vault {
        secret: "swordfish".to_string(),
        key: cx.new(|_| Key),
        key_ref: None,
        host: host.clone(),
    })
}

#[gpui::test]
async fn test_caretaker_membrane_forwards_and_revokes(cx: &mut TestAppContext) {
    let host = setup(cx);

    // A host-homed vault, shared anonymously: the ref is the only way in, and reads go
    // through an async handler.
    let vault = new_vault(&host, cx);
    let vault_ref = cx.update(|cx| host.share(&vault, cx));

    // Hand the vault capability to the plugin's gatekeeper; it wraps it in a caretaker
    // and returns a ref to *that*. The caller can't tell the difference.
    let gatekeeper = gatekeeper(&host, cx).await;
    let guarded = cx.update(|cx| gatekeeper.guard(vault_ref.clone(), cx));
    settle(cx);
    let guarded = guarded.await.expect("guard should respond with a remote");
    assert_ne!(guarded.reference().entity_id(), vault_ref.entity_id());

    // A read crosses the boundary four times: host -> caretaker (plugin) -> vault
    // (host), resolves in the vault's async handler, and unwinds back through the
    // caretaker.
    let read = cx.update(|cx| guarded.read(cx));
    settle(cx);
    assert_eq!(read.await.expect("read through membrane"), "swordfish");

    // Revocation: the caretaker drops the wrapped capability. Its auto-release cascades
    // to the vault's home, which drops its strong handle.
    let revoked = cx.update(|cx| {
        guarded
            .call_raw("revoke", encode(&()).expect("encode unit"), cx)
            .acknowledged()
    });
    settle(cx);
    revoked.await.expect("revoke");

    let read = cx.update(|cx| guarded.read(cx));
    settle(cx);
    let error = read.await.expect_err("reads after revocation must fail");
    assert!(
        error.to_string().contains("capability revoked"),
        "unexpected error: {error:#}"
    );

    // With the caretaker's handle released and ours dropped, nothing keeps the vault
    // alive: revocation reclaims the entity itself.
    let weak_vault = vault.downgrade();
    drop(vault);
    settle(cx);
    assert!(
        weak_vault.upgrade().is_none(),
        "vault should be reclaimed after revocation"
    );
}

#[gpui::test]
async fn test_dropping_last_remote_releases_the_capability(cx: &mut TestAppContext) {
    let host = setup(cx);
    let factory = factory(&host, cx).await;

    let created = cx.update(|cx| factory.create("ephemeral".to_string(), cx));
    settle(cx);
    let item = created.await.expect("create");
    // Keep the serializable ref around: it survives the remote it came from.
    let item_ref = item.reference();
    let bumped = cx.update(|cx| item.bump(cx));
    settle(cx);
    assert_eq!(bumped.await.expect("bump while held"), 1);

    // Clones share the guard, refcount-style: dropping one of two releases nothing.
    let sibling = item.clone();
    drop(sibling);
    cx.update(|cx| host.pump(cx));
    settle(cx);
    let bumped = cx.update(|cx| item.bump(cx));
    settle(cx);
    assert_eq!(bumped.await.expect("bump after dropping a clone"), 2);

    // Dropping the last handle queues the release; pump flushes it to the guest, whose
    // home drops the only strong handle to the item.
    drop(item);
    cx.update(|cx| host.pump(cx));
    settle(cx);

    // Re-connecting the same ref finds nobody home.
    let item = item_ref.connect();
    let bumped = cx.update(|cx| item.bump(cx));
    settle(cx);
    let error = bumped.await.expect_err("bump after release must fail");
    assert!(
        error.to_string().contains("entity dropped"),
        "unexpected error: {error:#}"
    );
}

#[gpui::test]
async fn test_attenuation_is_a_library_pattern(cx: &mut TestAppContext) {
    let host = setup(cx);
    let factory = factory(&host, cx).await;

    // Start from a FULL capability to a plugin-homed item...
    let created = cx.update(|cx| factory.create("gamma".to_string(), cx));
    settle(cx);
    let full = created.await.expect("create");

    // ...and derive a weaker one in pure userland: wrap the remote in an Attenuated
    // with an empty allowlist, share the wrapper, and hand out ITS ref. No core
    // protocol support involved, and no cooperation from the item's author.
    let readonly = cx.update(|cx| Attenuated::new(full.clone(), &[], cx));
    let readonly_ref = cx.update(|cx| host.share(&readonly, cx));
    assert_ne!(readonly_ref.entity_id(), full.reference().entity_id());

    // The plugin probes a write through the attenuated ref: rejected by the wrapper,
    // without the item ever hearing about it.
    let gatekeeper = gatekeeper(&host, cx).await;
    let denied = cx.update(|cx| {
        gatekeeper.probe(
            readonly_ref.clone(),
            "bump".to_string(),
            encode(&Bump {}).expect("encode bump").bytes,
            cx,
        )
    });
    settle(cx);
    let error = denied.await.expect_err("attenuated ref must reject writes");
    assert!(
        error.to_string().contains("not permitted"),
        "unexpected error: {error:#}"
    );

    // The full capability still writes.
    let bump = cx.update(|cx| full.bump(cx));
    settle(cx);
    assert_eq!(bump.await.expect("bump via full ref"), 1);

    // An allowlist that names the method lets it through, byte-for-byte.
    let writable = cx.update(|cx| Attenuated::new(full.clone(), &["bump"], cx));
    let writable_ref = cx.update(|cx| host.share(&writable, cx));
    let allowed = cx.update(|cx| {
        gatekeeper.probe(
            writable_ref,
            "bump".to_string(),
            encode(&Bump {}).expect("encode bump").bytes,
            cx,
        )
    });
    settle(cx);
    let response = allowed.await.expect("bump through allowlisted wrapper");
    let bumps: u32 =
        decode(&Payload::from_parts(response, Vec::new())).expect("decode bump response");
    assert_eq!(bumps, 2);
}

#[gpui::test]
async fn test_audited_wrapper_keeps_a_ledger(cx: &mut TestAppContext) {
    let host = setup(cx);
    let factory = factory(&host, cx).await;

    let created = cx.update(|cx| factory.create("ledgered".to_string(), cx));
    settle(cx);
    let item = created.await.expect("create");

    // Wrap the capability in an Audited and hand the WRAPPER's ref to the plugin. The
    // ledger stays with us, the wrapper's owner; the plugin just sees a working item.
    let audited = cx.update(|cx| Audited::new(item.clone(), cx));
    let audited_ref = cx.update(|cx| host.share(&audited, cx));

    let gatekeeper = gatekeeper(&host, cx).await;
    for _ in 0..2 {
        let bumped = cx.update(|cx| {
            gatekeeper.probe(
                audited_ref.clone(),
                "bump".to_string(),
                encode(&Bump {}).expect("encode bump").bytes,
                cx,
            )
        });
        settle(cx);
        bumped.await.expect("bump through audited wrapper");
    }

    // Every forwarded call is on the ledger, with its outcome.
    let records: Vec<_> = audited.read_with(cx, |audited, _| audited.records().to_vec());
    assert_eq!(records.len(), 2, "two calls, two records");
    for record in &records {
        assert_eq!(record.method, "bump");
        assert!(record.payload_len > 0);
        assert_eq!(record.completed, Some(true));
    }

    // The item actually changed: audit is accounting, not interference.
    let info = cx.update(|cx| item.describe(cx));
    settle(cx);
    assert_eq!(info.await.expect("describe").bumps, 2);
}

#[gpui::test]
async fn test_membrane_wraps_refs_transitively(cx: &mut TestAppContext) {
    let host = setup(cx);
    let vault = new_vault(&host, cx);
    let vault_ref = cx.update(|cx| host.share(&vault, cx));
    let gatekeeper = gatekeeper(&host, cx).await;
    let guarded = cx.update(|cx| gatekeeper.guard(vault_ref.clone(), cx));
    settle(cx);
    let guarded = guarded.await.expect("guard");

    // A ref returned *through* the membrane is not the vault's key but a wrapper minted
    // by the membrane: the ref table let the caretaker substitute it without parsing
    // the response.
    let key = cx.update(|cx| guarded.key(cx));
    settle(cx);
    let key = key.await.expect("key through membrane");
    let real_key_id = vault.read_with(cx, |vault, _| {
        vault.key_ref.as_ref().expect("key shared").entity_id()
    });
    assert_ne!(key.reference().entity_id(), real_key_id);

    // The wrapped key works like the real one...
    let unlocked = cx.update(|cx| key.unlock(cx));
    settle(cx);
    assert_eq!(unlocked.await.expect("unlock"), "opened");

    // ...until the membrane is revoked, which severs everything reached through it.
    let revoked = cx.update(|cx| {
        guarded
            .call_raw("revoke", encode(&()).expect("encode unit"), cx)
            .acknowledged()
    });
    settle(cx);
    revoked.await.expect("revoke");
    let unlocked = cx.update(|cx| key.unlock(cx));
    settle(cx);
    let error = unlocked
        .await
        .expect_err("keys obtained through a revoked membrane fail");
    assert!(
        error.to_string().contains("capability revoked"),
        "unexpected error: {error:#}"
    );

    // The real key is untouched: a fresh membrane around the same vault reaches it
    // again. The old membrane wrapped, it did not own.
    let guarded_again = cx.update(|cx| gatekeeper.guard(vault_ref, cx));
    settle(cx);
    let guarded_again = guarded_again.await.expect("guard again");
    let key_again = cx.update(|cx| guarded_again.key(cx));
    settle(cx);
    let key_again = key_again.await.expect("key through the new membrane");
    let unlocked = cx.update(|cx| key_again.unlock(cx));
    settle(cx);
    assert_eq!(
        unlocked.await.expect("unlock through the new membrane"),
        "opened"
    );
}

#[gpui::test]
async fn test_views_are_objects(cx: &mut TestAppContext) {
    let host = setup(cx);

    // A surface is an ordinary host entity, shared like any other object. The plugin
    // receives its ref through a typed method and opens a view on it.
    let surface = cx.new(Surface::new);
    let surface_ref = cx.update(|cx| host.share(&surface, cx));
    let root = cx.update(|cx| host.root::<TestPlugin>(cx));
    let probe = cx.update(|cx| root.mount(surface_ref, cx));
    settle(cx);
    let probe = probe.await.expect("mount");

    // The guest attached its view object to the surface.
    let view = surface
        .read_with(cx, |surface, _| surface.view().cloned())
        .expect("the guest attached a view");

    // Geometry is a method call on the view (layout would make this call; tests drive
    // it directly), and the guest renders at that size: a display list comes back.
    let geometry = Geometry {
        width: 200.,
        height: 100.,
        scale_factor: 2.,
    };
    cx.update(|cx| view.resize(geometry, cx));
    settle(cx);
    let seen = cx.update(|cx| probe.last_geometry(cx));
    settle(cx);
    assert_eq!(seen.await.expect("geometry"), Some(geometry));
    assert!(
        surface.read_with(cx, |surface, _| surface.has_scene()),
        "the surface received a display list"
    );

    // Input is a method call too; the guest's own dispatch hit-tests and runs listeners.
    let click = MouseButtonEvent {
        button: MouseButton::Left,
        position: Point { x: 10., y: 10. },
        modifiers: Modifiers::default(),
        click_count: 1,
    };
    cx.update(|cx| {
        view.mouse(MouseEvent::Down(click.clone()), cx);
        view.mouse(MouseEvent::Up(click), cx);
    });
    settle(cx);
    let clicks = cx.update(|cx| probe.clicks(cx));
    settle(cx);
    assert_eq!(clicks.await.expect("clicks"), 1);

    // The surface belongs to its owner: dropping it releases the view, and the guest
    // closes the window behind it.
    let alive = cx.update(|cx| probe.view_alive(cx));
    settle(cx);
    assert!(alive.await.expect("alive"));
    drop(view);
    drop(surface);
    cx.update(|cx| host.pump(cx));
    settle(cx);
    let alive = cx.update(|cx| probe.view_alive(cx));
    settle(cx);
    assert!(!alive.await.expect("alive"), "the view should be gone");
}

#[gpui::test]
async fn test_turn_budget_stops_a_runaway_plugin(cx: &mut TestAppContext) {
    let host = setup_with_options(
        PluginOptions::new(Arc::new(gpui::NoopTextSystem::new()))
            .with_turn_budget(Duration::from_millis(200)),
        cx,
    );
    cx.update(|cx| {
        let root = cx.new(|_| HostRoot { pings: 0 });
        host.share_root(&root, cx);
    });
    let root = cx.update(|cx| host.root::<TestPlugin>(cx));

    // A well-behaved call goes through.
    let pong = cx.update(|cx| root.ping_host("hi".to_string(), cx));
    settle(cx);
    assert_eq!(pong.await.expect("ping"), "pong: hi");

    // A turn that overruns the budget traps; the worker stops driving the guest, and
    // everything still in flight resolves with an error instead of hanging.
    let spun = cx.update(|cx| root.spin(5_000, cx));
    let after = cx.update(|cx| root.ping_host("again".to_string(), cx));
    for _ in 0..30 {
        settle(cx);
        std::thread::sleep(Duration::from_millis(50));
    }
    spun.await.expect_err("the spinning turn must not complete");
    after
        .await
        .expect_err("calls after the trap must fail, not hang");
}

#[gpui::test]
async fn test_interfaces_describe_themselves(_cx: &mut TestAppContext) {
    let schema = TestPlugin::schema();
    assert_eq!(schema.name, "TestPlugin");
    let names: Vec<_> = schema.methods.iter().map(|method| method.name).collect();
    assert_eq!(
        names,
        [
            "counter",
            "factory",
            "gatekeeper",
            "chameleon",
            "ping_host",
            "mount",
            "spin"
        ]
    );
    let ping = &schema.methods[4];
    assert!(ping.is_async);
    assert_eq!(ping.arguments.len(), 1);
    assert_eq!(ping.arguments[0].name, "message");
    assert_eq!(ping.arguments[0].ty, TypeSchema::String);
    let mount = &schema.methods[5];
    assert_eq!(mount.arguments[0].ty, TypeSchema::Ref("SurfaceApi"));
    assert_eq!(mount.response, TypeSchema::Ref("ViewProbeApi"));

    // Named payload types travel with the schema, transitively and once each.
    let counter = TestCounterApi::schema();
    assert_eq!(counter.events.len(), 1);
    assert_eq!(counter.events[0].name, "counter_milestone");
    assert_eq!(counter.events[0].ty, TypeSchema::Named("CounterMilestone"));
    let milestone = counter
        .types
        .iter()
        .find(|definition| definition.name == "CounterMilestone")
        .expect("event type is defined in the schema");
    let TypeKind::Struct(fields) = &milestone.kind else {
        panic!("CounterMilestone is a struct");
    };
    assert_eq!(fields[0].name, "count");
    assert_eq!(fields[0].ty, TypeSchema::Integer);

    // The same schema renders as TypeScript declarations.
    let declarations = typescript::declarations(&[ViewApi::schema(), TestCounterApi::schema()]);
    assert!(declarations.contains("export interface ViewApi {"));
    assert!(declarations.contains("  mouse(event: MouseEvent): Promise<void>;"));
    assert!(declarations.contains("export interface Geometry {"));
    assert!(declarations.contains("  | { Down: { keystroke: Keystroke; is_held: boolean } }"));
    assert!(declarations.contains("  | \"Left\""));
    assert!(declarations.contains("  increment(by: number): Promise<number>;"));
    assert!(declarations.contains(
        "export interface TestCounterApiEvents {\n  counter_milestone: CounterMilestone;"
    ));
}

#[gpui::test]
async fn test_chameleon_handles_methods_dynamically(cx: &mut TestAppContext) {
    let host = setup(cx);
    let chameleon = chameleon(&host, cx).await;

    // Default mode echoes.
    let poke = cx.update(|cx| {
        chameleon
            .call_raw("poke", encode(&"hello").unwrap(), cx)
            .decoded::<String>()
    });
    settle(cx);
    assert_eq!(poke.await.expect("poke"), "hello");

    // The entity reinterprets its own dispatch at runtime.
    let become_shout = cx.update(|cx| {
        chameleon
            .call_raw("become", encode(&"shout").unwrap(), cx)
            .acknowledged()
    });
    settle(cx);
    become_shout.await.expect("become");

    let poke = cx.update(|cx| {
        chameleon
            .call_raw("poke", encode(&"hello").unwrap(), cx)
            .decoded::<String>()
    });
    settle(cx);
    assert_eq!(poke.await.expect("poke"), "HELLO");

    // Unknown methods surface the entity's own error, not a protocol failure.
    let nonsense = cx.update(|cx| chameleon.call_raw("transmogrify", encode(&"x").unwrap(), cx));
    settle(cx);
    let error = nonsense.await.expect_err("must be rejected");
    assert!(error.to_string().contains("does not understand"));

    // The dynamic "state" method observed the writes: two pokes, shout mode.
    let state = cx.update(|cx| {
        chameleon
            .call_raw("state", encode(&()).unwrap(), cx)
            .decoded::<ChameleonState>()
    });
    settle(cx);
    let state = state.await.expect("state");
    assert_eq!(state.pokes, 2);
    assert_eq!(state.mode, "shout");
}

#[gpui::test(iterations = 10)]
async fn test_random_interleavings_stay_consistent(cx: &mut TestAppContext, mut rng: StdRng) {
    let host = setup(cx);
    let counter = counter(&host, cx).await;

    let mut expected_total = 0u32;
    let mut pending_calls = Vec::new();
    let mut pending_sends = Vec::new();

    for _ in 0..rng.random_range(5..25) {
        match rng.random_range(0..3) {
            0 => {
                let by = rng.random_range(1..10);
                expected_total += by;
                let receipt = cx.update(|cx| counter.increment(by, cx));
                // FIFO + single writer: each response must equal the running prefix sum.
                pending_calls.push((receipt, expected_total));
            }
            1 => {
                let by = rng.random_range(1..10);
                expected_total += by;
                let receipt = cx.update(|cx| counter.call(Increment { by }, cx));
                pending_sends.push(receipt);
            }
            _ => settle(cx),
        }
    }
    settle(cx);

    for (receipt, prefix_sum) in pending_calls {
        assert_eq!(receipt.await.expect("call"), prefix_sum);
    }
    for receipt in pending_sends {
        receipt.await.expect("send");
    }

    let final_count = cx.update(|cx| counter.count(cx));
    settle(cx);
    assert_eq!(final_count.await.expect("count"), expected_total);
}
