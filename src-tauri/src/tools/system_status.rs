//! `system_status` — a read-only snapshot of the app's local environment (PLAN
//! §8 M3 item 10). Answers "what can you see about this setup?": OS/arch, the
//! storage root, how many profiles exist, and whether the on-device model files
//! (privacy classifier / memory embedder) are installed. `RiskClass::Safe` ⇒
//! pre-trusted. Purely local — touches no network, mutates nothing.

use std::future::Future;
use std::pin::Pin;

use serde_json::json;

use crate::storage::Storage;
use crate::tools::{Capability, ExecCtx, Tool, ToolInput, ToolResult};

/// Reports a factual, local-only status snapshot. Holds a `Storage` handle to
/// read the storage root + profile list (a cheap `Arc` clone).
pub struct SystemStatusTool {
    storage: Storage,
}

impl SystemStatusTool {
    pub fn new(storage: Storage) -> Self {
        Self { storage }
    }
}

impl Tool for SystemStatusTool {
    fn name(&self) -> &str {
        "system_status"
    }

    fn description(&self) -> &str {
        "Report a read-only snapshot of this device's setup: OS/arch, storage \
         location, how many profiles exist, and whether the on-device models are \
         installed. No args."
    }

    fn requires(&self) -> &[Capability] {
        &[]
    }

    // risk() defaults to Safe (read-only, on-device) → pre-trusted.

    fn run<'a>(
        &'a self,
        _input: ToolInput,
        _ctx: &'a ExecCtx,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        Box::pin(async move {
            let root = self.storage.base_path().to_path_buf();
            let profile_count = self
                .storage
                .list_profile_names()
                .map(|v| v.len())
                .unwrap_or(0);
            // The on-device model dirs (installed out-of-band in dev; bundled at
            // packaging, M9). Their presence is what flips the classifier/memory
            // from the rules-only / keyword-only fallback to the full model path.
            let classifier_installed = root.join("models").join("classifier").is_dir();
            let embedder_installed = root
                .join("models")
                .join("embedder")
                .join("model.int8.onnx")
                .is_file();
            ToolResult::Ok(json!({
                "os": std::env::consts::OS,
                "arch": std::env::consts::ARCH,
                "storage_root": root.display().to_string(),
                "profiles": profile_count,
                "models": {
                    "privacy_classifier_installed": classifier_installed,
                    "memory_embedder_installed": embedder_installed,
                },
            }))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reports_a_local_snapshot() {
        let mut root = std::env::temp_dir();
        root.push(format!("lhp-sysstatus-{}", uuid::Uuid::new_v4()));
        let storage = Storage::open(&root).unwrap();
        // Create a profile so the count is non-trivial.
        storage.open_profile("personal").unwrap();

        let tool = SystemStatusTool::new(storage.clone());
        match tool.run(ToolInput::empty(), &ExecCtx::default()).await {
            ToolResult::Ok(v) => {
                assert_eq!(v["os"], std::env::consts::OS);
                assert_eq!(v["arch"], std::env::consts::ARCH);
                assert_eq!(v["profiles"], 1);
                // No models installed in a fresh temp storage.
                assert_eq!(v["models"]["memory_embedder_installed"], false);
                assert!(v["storage_root"]
                    .as_str()
                    .unwrap()
                    .contains("lhp-sysstatus-"));
            }
            ToolResult::Err(e) => panic!("expected Ok, got Err({e})"),
        }
        let _ = std::fs::remove_dir_all(root);
    }
}
