//! Provider API-key storage.
//!
//! SQLite stores only [`KEYCHAIN_MARKER`] in the historical
//! `api_key_encrypted` column. The secret itself lives in the OS credential
//! store, keyed by endpoint id. The trait keeps unit tests independent of a
//! logged-in desktop/keychain session.

#[cfg(test)]
use std::collections::HashMap;
#[cfg(test)]
use std::sync::Mutex;

use crate::storage::GlobalDb;

const SERVICE: &str = "lost-harness-provider";
pub const KEYCHAIN_MARKER: &[u8] = b"keychain:v1";

pub trait ProviderSecretStore: Send + Sync {
    fn get(&self, endpoint_id: &str) -> Result<Option<String>, String>;
    fn set(&self, endpoint_id: &str, secret: &str) -> Result<(), String>;
    fn delete(&self, endpoint_id: &str) -> Result<(), String>;
}

pub struct OsProviderSecretStore;

impl OsProviderSecretStore {
    pub fn new() -> Self {
        Self
    }

    fn entry(endpoint_id: &str) -> Result<keyring::Entry, String> {
        keyring::Entry::new(SERVICE, endpoint_id)
            .map_err(|e| format!("couldn't open OS credential entry: {e}"))
    }
}

impl ProviderSecretStore for OsProviderSecretStore {
    fn get(&self, endpoint_id: &str) -> Result<Option<String>, String> {
        match Self::entry(endpoint_id)?.get_password() {
            Ok(secret) => Ok(Some(secret)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(format!("couldn't read provider credential: {e}")),
        }
    }

    fn set(&self, endpoint_id: &str, secret: &str) -> Result<(), String> {
        Self::entry(endpoint_id)?
            .set_password(secret)
            .map_err(|e| format!("couldn't store provider credential: {e}"))
    }

    fn delete(&self, endpoint_id: &str) -> Result<(), String> {
        match Self::entry(endpoint_id)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(format!("couldn't delete provider credential: {e}")),
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SecretMigrationReport {
    pub migrated: usize,
    pub failed: usize,
}

/// Move legacy plaintext blobs to the credential store. A row is marked only
/// after `set` succeeds, so an interrupted/failed migration retries safely on
/// the next boot and never destroys the only copy of a credential.
pub fn migrate_legacy_provider_secrets(
    db: &GlobalDb,
    secrets: &dyn ProviderSecretStore,
) -> SecretMigrationReport {
    let mut report = SecretMigrationReport::default();
    let endpoints = match db.list_endpoints() {
        Ok(rows) => rows,
        Err(_) => {
            report.failed = 1;
            return report;
        }
    };
    for endpoint in endpoints {
        let Some(blob) = endpoint.api_key_marker.as_deref() else {
            continue;
        };
        if blob == KEYCHAIN_MARKER {
            continue;
        }
        let Ok(secret) = std::str::from_utf8(blob) else {
            report.failed += 1;
            continue;
        };
        if secrets.set(&endpoint.id, secret).is_err() {
            report.failed += 1;
            continue;
        }
        match db.mark_endpoint_secret_in_keychain(&endpoint.id) {
            Ok(true) => report.migrated += 1,
            _ => report.failed += 1,
        }
    }
    report
}

/// Deterministic fake used by IPC and migration tests.
#[cfg(test)]
#[derive(Default)]
pub struct MemoryProviderSecretStore {
    values: Mutex<HashMap<String, String>>,
}

#[cfg(test)]
impl ProviderSecretStore for MemoryProviderSecretStore {
    fn get(&self, endpoint_id: &str) -> Result<Option<String>, String> {
        Ok(self.values.lock().unwrap().get(endpoint_id).cloned())
    }

    fn set(&self, endpoint_id: &str, secret: &str) -> Result<(), String> {
        self.values
            .lock()
            .unwrap()
            .insert(endpoint_id.to_string(), secret.to_string());
        Ok(())
    }

    fn delete(&self, endpoint_id: &str) -> Result<(), String> {
        self.values.lock().unwrap().remove(endpoint_id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::Endpoint;

    #[test]
    fn legacy_plaintext_moves_once_and_sqlite_keeps_only_a_marker() {
        let db = GlobalDb::open_in_memory().unwrap();
        db.insert_endpoint(&Endpoint {
            id: "cloud-1".into(),
            name: "Cloud".into(),
            base_url: "https://api.example.com/v1".into(),
            api_key_marker: Some(b"sk-legacy-secret".to_vec()),
            kind: "cloud".into(),
            created_at: 1,
            supports_native_tools: false,
        })
        .unwrap();
        let secrets = MemoryProviderSecretStore::default();

        assert_eq!(
            migrate_legacy_provider_secrets(&db, &secrets),
            SecretMigrationReport { migrated: 1, failed: 0 }
        );
        assert_eq!(secrets.get("cloud-1").unwrap().as_deref(), Some("sk-legacy-secret"));
        let row = db.get_endpoint("cloud-1").unwrap().unwrap();
        assert_eq!(row.api_key_marker.as_deref(), Some(KEYCHAIN_MARKER));

        assert_eq!(
            migrate_legacy_provider_secrets(&db, &secrets),
            SecretMigrationReport::default(),
            "the marker makes migration idempotent"
        );
    }

    #[test]
    fn failed_keychain_write_never_erases_the_only_plaintext_copy() {
        struct FailingStore;
        impl ProviderSecretStore for FailingStore {
            fn get(&self, _endpoint_id: &str) -> Result<Option<String>, String> {
                Err("unavailable".into())
            }
            fn set(&self, _endpoint_id: &str, _secret: &str) -> Result<(), String> {
                Err("unavailable".into())
            }
            fn delete(&self, _endpoint_id: &str) -> Result<(), String> {
                Err("unavailable".into())
            }
        }

        let db = GlobalDb::open_in_memory().unwrap();
        db.insert_endpoint(&Endpoint {
            id: "cloud-2".into(),
            name: "Cloud".into(),
            base_url: "https://api.example.com/v1".into(),
            api_key_marker: Some(b"sk-only-copy".to_vec()),
            kind: "cloud".into(),
            created_at: 1,
            supports_native_tools: false,
        })
        .unwrap();

        assert_eq!(
            migrate_legacy_provider_secrets(&db, &FailingStore),
            SecretMigrationReport { migrated: 0, failed: 1 }
        );
        assert_eq!(
            db.get_endpoint("cloud-2")
                .unwrap()
                .unwrap()
                .api_key_marker
                .as_deref(),
            Some(b"sk-only-copy".as_slice())
        );
    }
}
