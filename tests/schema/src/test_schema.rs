//! Schemas for the integration tests in `tests`.

use embedded_gpui::SharedRef;

embedded_gpui::shared_schema! {
    entity TestCounterSpec as "test.counter" {
        snapshot TestCounterSnapshot { count: u32 }
        message "increment" TestIncrement { by: u32 } -> u32
    }
}

embedded_gpui::shared_schema! {
    entity ItemSpec as "test.item" {
        snapshot ItemSnapshot { label: String, bumps: u32 }
        message "bump" Bump {} -> u32
    }
}

embedded_gpui::shared_schema! {
    entity FactorySpec as "test.factory" {
        snapshot FactorySnapshot { created: u32 }
        message "create" CreateItem { label: String } -> SharedRef<ItemSpec>
    }
}

embedded_gpui::shared_schema! {
    entity VaultSpec as "test.vault" {
        snapshot VaultSnapshot { label: String }
        message "read" ReadSecret {} -> String
    }
}

embedded_gpui::shared_schema! {
    entity GatekeeperSpec as "test.gatekeeper" {
        snapshot GatekeeperSnapshot { guarded: u32 }
        message "guard" Guard { vault: SharedRef<VaultSpec> } -> SharedRef<VaultSpec>
    }
}

embedded_gpui::shared_schema! {
    entity ChameleonSpec as "test.chameleon" {
        snapshot ChameleonSnapshot { mode: String, pokes: u32 }
    }
}

/// Payload for the gatekeeper's raw "probe" method: exercise an arbitrary method on an
/// arbitrary item capability from the guest side, so tests can verify what a ref does
/// and does not permit from across the boundary.
#[derive(Clone, Debug, embedded_gpui::serde::Serialize, embedded_gpui::serde::Deserialize)]
#[serde(crate = "embedded_gpui::serde")]
pub struct ProbeRequest {
    pub target: embedded_gpui::SharedRef<ItemSpec>,
    pub method: String,
    pub payload: Vec<u8>,
}
