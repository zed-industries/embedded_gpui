//! End-to-end tests for shared entities: a real wasm32-wasip2 guest (see `test_plugin/`)
//! loaded into a wasmtime store, driven from GPUI's deterministic test executor.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use embedded_gpui::{
    PluginHost, PluginHostHandle as _, PluginInstance, PluginOptions, decode, encode, shared,
};
use embedded_gpui_util::{Attenuated, Audited, Mirror};
use gpui::{AppContext as _, Context, Entity, Task, TestAppContext};
use rand::prelude::*;
use test_schema::{
    Bump, ChameleonApi, ChameleonState, Count, CounterMilestone, FactoryApi, FactoryApiCaller as _,
    GatekeeperApi, GatekeeperApiCaller as _, Increment, ItemApiCaller as _, TestCounterApi,
    TestCounterApiCaller as _, VaultApi, VaultApiCaller as _,
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

fn setup(cx: &mut TestAppContext) -> Entity<PluginHost> {
    let path = test_plugin_path();
    let instance = cx.update(|_| {
        PluginInstance::new(
            &path,
            PluginOptions::new(Arc::new(gpui::NoopTextSystem::new())),
        )
        .expect("failed to instantiate test plugin")
    });
    cx.new(|cx| PluginHost::new(instance, cx))
}

/// Flush deferred effects and host-scheduled ticks deterministically.
fn settle(cx: &mut TestAppContext) {
    for _ in 0..5 {
        cx.executor().run_until_parked();
        cx.executor().advance_clock(Duration::from_millis(100));
    }
    cx.executor().run_until_parked();
}

#[gpui::test]
async fn test_send_is_read_your_writes_by_ordering(cx: &mut TestAppContext) {
    let host = setup(cx);
    let counter = cx.update(|cx| host.remote::<TestCounterApi>("guest-counter", cx));

    let receipt = cx.update(|cx| counter.send(Increment { by: 3 }, cx));
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
    let counter = cx.update(|cx| host.remote::<TestCounterApi>("guest-counter", cx));

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
    let counter = cx.update(|cx| host.remote::<TestCounterApi>("guest-counter", cx));

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

    let receipt = cx.update(|cx| counter.send(Increment { by: 4 }, cx));
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
    let counter = cx.update(|cx| host.remote::<TestCounterApi>("guest-counter", cx));

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
    let factory = cx.update(|cx| host.remote::<FactoryApi>("factory", cx));

    // A call whose response is a capability reference to a freshly shared child.
    let created = cx.update(|cx| factory.create("alpha".to_string(), cx));
    settle(cx);
    let item_ref = created.await.expect("create should respond with a ref");

    // Connect the ref: no names involved, the id addresses the entity directly.
    let item = cx.update(|cx| host.connect(item_ref, cx));
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
    let second_ref = created_again.await.expect("second create");
    assert_ne!(second_ref.entity_id(), item_ref.entity_id());
}

/// The host half of the membrane test: an entity whose secret is only reachable via a
/// capability, with a deliberately asynchronous read handler.
struct Vault {
    secret: String,
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
}

#[gpui::test]
async fn test_caretaker_membrane_forwards_and_revokes(cx: &mut TestAppContext) {
    let host = setup(cx);

    // A host-homed vault, shared anonymously: the ref is the only way in, and reads go
    // through an async handler.
    let vault = cx.new(|_| Vault {
        secret: "swordfish".to_string(),
    });
    let vault_ref = cx.update(|cx| host.share_anonymous(&vault, cx));

    // Hand the vault capability to the guest's gatekeeper; it wraps it in a caretaker
    // and returns a ref to *that*. The caller can't tell the difference.
    let gatekeeper = cx.update(|cx| host.remote::<GatekeeperApi>("gatekeeper", cx));
    let guarded = cx.update(|cx| gatekeeper.guard(vault_ref, cx));
    settle(cx);
    let guarded_ref = guarded.await.expect("guard should respond with a ref");
    assert_ne!(guarded_ref.entity_id(), vault_ref.entity_id());

    // A read crosses the boundary four times: host -> caretaker (guest) -> vault (host),
    // resolves in the vault's async handler, and unwinds back through the caretaker.
    let guarded = cx.update(|cx| host.connect(guarded_ref, cx));
    let read = cx.update(|cx| guarded.read(cx));
    settle(cx);
    assert_eq!(read.await.expect("read through membrane"), "swordfish");

    // Revocation: the caretaker drops the wrapped capability. Its auto-release cascades
    // to the vault's home, which drops its strong handle.
    let revoked =
        cx.update(|cx| guarded.call_raw::<()>("revoke", encode(&()).expect("encode unit"), cx));
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
    let factory = cx.update(|cx| host.remote::<FactoryApi>("factory", cx));

    let created = cx.update(|cx| factory.create("ephemeral".to_string(), cx));
    settle(cx);
    let item_ref = created.await.expect("create");

    let item = cx.update(|cx| host.connect(item_ref, cx));
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
    let item = cx.update(|cx| host.connect(item_ref, cx));
    let bumped = cx.update(|cx| item.bump(cx));
    settle(cx);
    let error = bumped.await.expect_err("bump after release must fail");
    assert!(
        error.to_string().contains("shared entity dropped"),
        "unexpected error: {error:#}"
    );
}

#[gpui::test]
async fn test_attenuation_is_a_library_pattern(cx: &mut TestAppContext) {
    let host = setup(cx);
    let factory = cx.update(|cx| host.remote::<FactoryApi>("factory", cx));

    // Start from a FULL capability to a guest-homed item...
    let created = cx.update(|cx| factory.create("gamma".to_string(), cx));
    settle(cx);
    let full_ref = created.await.expect("create");
    let full = cx.update(|cx| host.connect(full_ref, cx));

    // ...and derive a weaker one in pure userland: wrap the remote in an Attenuated
    // with an empty allowlist, share the wrapper, and hand out ITS ref. No core
    // protocol support involved, and no cooperation from the item's author.
    let readonly = cx.update(|cx| Attenuated::new(full.clone(), &[], cx));
    let readonly_ref = cx.update(|cx| host.share_anonymous(&readonly, cx));
    assert_ne!(readonly_ref.entity_id(), full_ref.entity_id());

    // The guest probes a write through the attenuated ref: rejected by the wrapper,
    // without the item ever hearing about it.
    let gatekeeper = cx.update(|cx| host.remote::<GatekeeperApi>("gatekeeper", cx));
    let denied = cx.update(|cx| {
        gatekeeper.probe(
            readonly_ref,
            "bump".to_string(),
            encode(&Bump {}).expect("encode bump"),
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
    let writable_ref = cx.update(|cx| host.share_anonymous(&writable, cx));
    let allowed = cx.update(|cx| {
        gatekeeper.probe(
            writable_ref,
            "bump".to_string(),
            encode(&Bump {}).expect("encode bump"),
            cx,
        )
    });
    settle(cx);
    let response = allowed.await.expect("bump through allowlisted wrapper");
    let bumps: u32 = decode(&response).expect("decode bump response");
    assert_eq!(bumps, 2);
}

#[gpui::test]
async fn test_audited_wrapper_keeps_a_ledger(cx: &mut TestAppContext) {
    let host = setup(cx);
    let factory = cx.update(|cx| host.remote::<FactoryApi>("factory", cx));

    let created = cx.update(|cx| factory.create("ledgered".to_string(), cx));
    settle(cx);
    let item_ref = created.await.expect("create");
    let item = cx.update(|cx| host.connect(item_ref, cx));

    // Wrap the capability in an Audited and hand the WRAPPER's ref to the guest. The
    // ledger stays with us, the wrapper's owner; the guest just sees a working item.
    let audited = cx.update(|cx| Audited::new(item.clone(), cx));
    let audited_ref = cx.update(|cx| host.share_anonymous(&audited, cx));

    let gatekeeper = cx.update(|cx| host.remote::<GatekeeperApi>("gatekeeper", cx));
    for _ in 0..2 {
        let bumped = cx.update(|cx| {
            gatekeeper.probe(
                audited_ref,
                "bump".to_string(),
                encode(&Bump {}).expect("encode bump"),
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
async fn test_chameleon_handles_methods_dynamically(cx: &mut TestAppContext) {
    let host = setup(cx);
    let chameleon = cx.update(|cx| host.remote::<ChameleonApi>("chameleon", cx));

    // Default mode echoes.
    let poke = cx.update(|cx| chameleon.call_raw::<String>("poke", encode(&"hello").unwrap(), cx));
    settle(cx);
    assert_eq!(poke.await.expect("poke"), "hello");

    // The entity reinterprets its own dispatch at runtime.
    let become_shout = cx.update(|cx| chameleon.send_raw("become", encode(&"shout").unwrap(), cx));
    settle(cx);
    become_shout.await.expect("become");

    let poke = cx.update(|cx| chameleon.call_raw::<String>("poke", encode(&"hello").unwrap(), cx));
    settle(cx);
    assert_eq!(poke.await.expect("poke"), "HELLO");

    // Unknown methods surface the entity's own error, not a protocol failure.
    let nonsense =
        cx.update(|cx| chameleon.call_raw::<String>("transmogrify", encode(&"x").unwrap(), cx));
    settle(cx);
    let error = nonsense.await.expect_err("must be rejected");
    assert!(error.to_string().contains("does not understand"));

    // The dynamic "state" method observed the writes: two pokes, shout mode.
    let state =
        cx.update(|cx| chameleon.call_raw::<ChameleonState>("state", encode(&()).unwrap(), cx));
    settle(cx);
    let state = state.await.expect("state");
    assert_eq!(state.pokes, 2);
    assert_eq!(state.mode, "shout");
}

#[gpui::test(iterations = 10)]
async fn test_random_interleavings_stay_consistent(cx: &mut TestAppContext, mut rng: StdRng) {
    let host = setup(cx);
    let counter = cx.update(|cx| host.remote::<TestCounterApi>("guest-counter", cx));

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
                let receipt = cx.update(|cx| counter.send(Increment { by }, cx));
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
