//! Wave 5.3 / M8 (REVISION 2026-07-28c) — HuggingFace model **search** + the
//! per-model file/quant listing that feeds the interactive calculator.
//!
//! This is the discovery half of the product redirect: instead of a hardcoded
//! catalog that goes stale, we search HuggingFace live (exactly what LM Studio
//! does). Two anonymous, public HF endpoints, both host-allowlisted through
//! [`crate::models::download::host_allowed`] (SSRF discipline unchanged — HF
//! only, https only, and the shared redirect policy re-checks the allowlist on
//! EVERY redirect hop):
//!   - **search** `GET /api/models?search=&filter=gguf&sort=&limit=` → the
//!     result rows ([`HfModelSummary`]),
//!   - **tree** `GET /api/models/{id}/tree/{revision}` → the `*.gguf` files,
//!     each carrying its `lfs.oid` (the real sha256, 64-hex) + `lfs.size`
//!     ([`QuantOption`]).
//!
//! ## Provenance architecture (P09 / H-08 — supply-chain boundary)
//!
//! The expected SHA-256 for a curated model never comes from the live HF API
//! (`lfs.oid`). Instead a **signed manifest** on disk maps each curated model
//! id to an immutable commit revision and the exact file hashes at that
//! revision. The app verifies the manifest's Ed25519 signature against a
//! compiled-in public key, then uses the pinned revision for all download URLs
//! and the manifest's hashes for integrity verification. This decouples the
//! trust root from the content host.
//!
//! **This resolution is FAIL-CLOSED.** A missing manifest, an unreadable or
//! malformed manifest, a bad signature, an unsupported schema, a replayed
//! (rolled-back) manifest, a build with no signing key compiled in, or a model
//! that simply is not listed — every one of those yields
//! [`Provenance::Community`] and NO pinned revision. There is no
//! publisher-allowlist path to the `Curated` label and no silent degradation:
//!
//! - **Curated** ⇔ the manifest loaded, its signature verified against the
//!   compiled-in key, and it lists this model at an immutable 40-hex commit.
//!   Download URLs use that commit and the expected hashes come from the
//!   manifest, so a compromised HF repo cannot forge a verified download.
//! - **Community** ⇔ everything else, including allowlisted publishers. The UI
//!   must warn and the backend requires explicit consent before download. The
//!   *reason* is not swallowed: [`HfModelDetail::manifest`] carries the exact
//!   [`ManifestState`] (absent / not-listed / invalid-with-reason).
//!
//! [`CURATED_PUBLISHERS`] survives only as a **discovery** filter for the
//! Staff-picks list ([`HfModelSummary::curated_publisher`]); it never sets a
//! trust label.
//!
//! The network functions are thin; every parse/classify step is a **pure**
//! helper unit-tested with fixtures (the live endpoints are exercised only by an
//! env-gated, self-skipping integration test, mirroring
//! `live_native_tool_call_roundtrip`).

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::models::download::{allowlisted_redirect_policy, host_allowed, is_real_sha256};

/// Publishers whose GGUF repos we treat as curated for the Staff-picks default
/// view and for suppressing the community-provenance warning. Two groups:
/// well-known official model orgs, and the community requantizers the
/// design names explicitly (`lmstudio-community`/`ggml-org`/`unsloth`/
/// `bartowski`). Anything not on this list is [`Provenance::Community`] — the
/// conservative default (more warnings, never fewer). Matched case-insensitively
/// against the publisher (the segment before `/` in a repo id).
///
/// ⚠ This is a **discovery** allowlist, not a trust root and not a label. It
/// only decides which rows the Staff-picks default view shows
/// ([`HfModelSummary::curated_publisher`]). Being on this list grants a model
/// NOTHING: [`Provenance::Curated`] comes exclusively from a verified
/// [`ModelManifest`] entry (see [`resolve_manifest`]), so an allowlisted
/// publisher with no manifest entry is still [`Provenance::Community`] and
/// still consent-gated.
const CURATED_PUBLISHERS: &[&str] = &[
    // Trusted community requantizers (design §22b + 22c note).
    "lmstudio-community",
    "ggml-org",
    "unsloth",
    "bartowski",
    // Well-known official model orgs that publish (or whose GGUFs are mirrored
    // under) these names. Not exhaustive by design — an unlisted publisher is
    // Community, which only ever ADDS a provenance warning.
    // (This is a curation allowlist; manifest-hash verification is the
    // real trust root — see [`ModelManifest`].)
    "qwen",
    "google",
    "meta-llama",
    "mistralai",
    "microsoft",
    "deepseek-ai",
    "nvidia",
    "allenai",
    "ibm-granite",
    "huggingfaceh4",
    "tiiuae",
    "cohereforai",
    "01-ai",
];

/// How much we vouch for a model's bytes. This enum replaces the pre-P09
/// "Trusted" label (which conflated namespace curation with cryptographic
/// verification). There are exactly two states and only ONE way in:
///
/// | Verified manifest entry? | Label | Trust root | Revision |
/// |---|---|---|---|
/// | Yes | `Curated`   | signed manifest        | pinned 40-hex commit |
/// | No  | `Community` | none — consent-gated   | `main` (mutable) |
///
/// The publisher allowlist deliberately does NOT appear in that table: it
/// cannot produce `Curated`. See [`resolve_manifest`] for the fail-closed
/// resolution and [`ManifestState`] for the reason a model landed in
/// `Community`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    /// A verified [`ModelManifest`] entry exists: the download URL is pinned to
    /// an immutable commit and the expected hash comes from the manifest, not
    /// from the host serving the bytes.
    Curated,
    /// Everything else — no manifest, bad signature, replayed manifest, or the
    /// model simply is not listed. The UI must show the "community model —
    /// provenance is the publisher's" warning and the backend requires an
    /// explicit acknowledgement before downloading.
    Community,
}

/// Sort order for a search, mapped to the HF API `sort` parameter. All are
/// descending (`direction=-1`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchSort {
    Downloads,
    Likes,
    Trending,
    LastModified,
}

impl SearchSort {
    fn as_api_param(self) -> &'static str {
        match self {
            SearchSort::Downloads => "downloads",
            SearchSort::Likes => "likes",
            SearchSort::Trending => "trendingScore",
            SearchSort::LastModified => "lastModified",
        }
    }
}

/// One search-result row (the list view). Mirrors the fields the HF
/// `/api/models` list endpoint returns for a GGUF repo, plus the derived
/// `publisher`/`provenance` the UI needs for labelling. `downloads`/`likes` are
/// `Option` — HF can omit or null them, and an absent count is honestly absent,
/// never a fabricated `0`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HfModelSummary {
    /// Full repo id, e.g. `"Qwen/Qwen3-0.6B-GGUF"`.
    pub id: String,
    /// The org/user segment before `/` (e.g. `"Qwen"`). Empty if the id has no
    /// slash (rare for GGUF repos).
    pub publisher: String,
    #[serde(default)]
    pub downloads: Option<u64>,
    #[serde(default)]
    pub likes: Option<u64>,
    #[serde(default)]
    pub tags: Vec<String>,
    /// The trust label. [`Provenance::Curated`] ONLY when the verified manifest
    /// lists this exact id; otherwise [`Provenance::Community`].
    pub provenance: Provenance,
    /// Is the publisher on the [`CURATED_PUBLISHERS`] discovery allowlist? This
    /// is what the Staff-picks view filters on. It is NOT a trust signal and
    /// must never be rendered as one — a row can be `curated_publisher: true`
    /// and `provenance: community` at the same time (the normal state before a
    /// signed manifest exists).
    pub curated_publisher: bool,
}

/// One downloadable GGUF file in the repo tree. The `sha256` is the LFS `oid`
/// — what [`crate::models::download::verify_and_install`] checks the downloaded
/// bytes against.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuantOption {
    /// The parsed quant label, e.g. `"Q4_K_M"`, `"Q8_0"`, `"IQ4_XS"`. `None`
    /// when the filename carries no recognisable quant token (still listed, so
    /// the user can pick it — just unlabelled).
    pub quant: Option<String>,
    /// The file's path in the repo (its filename), e.g.
    /// `"Qwen3-0.6B-Q8_0.gguf"`.
    pub filename: String,
    /// The pinned resolve URL for this exact file (host-allowlist checked).
    pub url: String,
    /// The real sha256 (LFS `oid`, 64-hex) — the download-verify trust value.
    pub sha256: String,
    /// The file's byte size (LFS `size`).
    pub size_bytes: u64,
    /// A multi-part GGUF (`*-00001-of-00003.gguf`) — see [`QuantGroup`]. `None`
    /// for single-file quants.
    #[serde(default)]
    pub part: Option<PartInfo>,
}

/// Split-file GGUF part info, parsed from the `-NNNNN-of-MMMMM` filename token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartInfo {
    pub index: u32,
    pub total: u32,
}

/// One LOGICAL downloadable quant: a single file, or a complete multi-part set
/// grouped together. This is the unit the detail-pane dropdown renders and the
/// unit the calculator sizes — `total_size_bytes` is the SUM across parts (the
/// design's multi-part rule), never a single part's size masquerading as the
/// whole model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuantGroup {
    /// The quant label shared by the group (e.g. `"Q4_K_M"`).
    pub quant: Option<String>,
    /// Sum of all part files' sizes — the calculator's `weight_file_bytes`.
    pub total_size_bytes: u64,
    /// The files to download, in part order (1 for single-file quants).
    pub files: Vec<QuantOption>,
    /// True when this is a single file OR a complete `1..=total` part set. An
    /// incomplete set is surfaced but NOT downloadable — its size would lie.
    pub complete: bool,
}

/// The full detail view for one model: provenance plus every downloadable quant
/// grouped into logical units, so the UI's "Download Options" dropdown is a
/// direct render.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HfModelDetail {
    pub id: String,
    pub publisher: String,
    pub provenance: Provenance,
    /// Why this model has the provenance it has — the fail-closed resolution
    /// result, surfaced instead of swallowed. `Verified { revision }` is the
    /// only state that pairs with [`Provenance::Curated`].
    pub manifest: ManifestState,
    /// Is the publisher on the discovery allowlist? Same caveat as
    /// [`HfModelSummary::curated_publisher`] — not a trust signal.
    pub curated_publisher: bool,
    pub quants: Vec<QuantGroup>,
}

// ---------------------------------------------------------------------------
// Curated-model manifest (P09 / H-08 — supply-chain trust root)
// ---------------------------------------------------------------------------

/// Ed25519 public key — standard base64 of the RAW 32 key bytes (not a PEM
/// SubjectPublicKeyInfo blob) — used to verify the curated-model manifest.
///
/// `None` means **this build has no manifest trust root compiled in**.
/// Verification then fails closed with [`ManifestError::NoKeyConfigured`], so
/// no model can be labelled [`Provenance::Curated`], nothing is pinned, and
/// every download is consent-gated as [`Provenance::Community`]. That is the
/// intended, safe shipping state until the ceremony below is performed.
///
/// There is deliberately NO placeholder value here: a syntactically plausible
/// dummy key is worse than `None`, because it reads like a configured trust
/// root while verifying nothing.
///
/// # NEEDS-LUKAS — manifest key ceremony (human-only, offline)
///
/// 1. On an **offline** machine, generate the keypair:
///    ```text
///    openssl genpkey -algorithm ed25519 -out lh-manifest-private.pem
///    chmod 600 lh-manifest-private.pem
///    ```
///    The private key NEVER leaves that machine and is NEVER committed. Store
///    it encrypted (e.g. an offline volume / password manager attachment) with
///    a written recovery plan; losing it means a key rotation, and leaking it
///    means an attacker can mint "Curated" labels.
/// 2. Extract the raw 32-byte public key and base64 it (openssl emits a
///    44-byte DER SPKI whose last 32 bytes are the key):
///    ```text
///    openssl pkey -in lh-manifest-private.pem -pubout -outform DER \
///      | tail -c 32 | base64
///    ```
/// 3. Paste that string here as `Some("…")` and commit ONLY the public half.
/// 4. Build the manifest JSON (schema in [`ModelManifest`]), then emit the
///    exact bytes to sign — do not hand-canonicalise it:
///    ```text
///    LHP_MANIFEST_IN=/path/model-manifest.json \
///    LHP_MANIFEST_PAYLOAD_OUT=/path/payload.bin \
///      cargo test -p lost-harness-product --lib \
///      models::hf_search::tests::emit_manifest_signing_payload -- --ignored --nocapture
///    ```
/// 5. Sign those bytes and paste the hex into the manifest's `signature`:
///    ```text
///    openssl pkeyutl -sign -inkey lh-manifest-private.pem -rawin \
///      -in /path/payload.bin | xxd -p | tr -d '\n'
///    ```
/// 6. Install the manifest at `<storage base>/model-manifest.json` and confirm
///    the app reports `manifest: {"state":"verified", …}` for a listed model.
///    Bump `serial` on every re-sign — the app records the highest serial it
///    has accepted and refuses anything older (see [`ManifestError::Rollback`]).
const MANIFEST_PUBLIC_KEY_B64: Option<&str> = None;

/// Domain-separation prefix for the signed payload. Keeps a manifest signature
/// from ever being valid for some other Ed25519 message in the product, and
/// pins the payload framing to a version so it can be changed deliberately.
const MANIFEST_PAYLOAD_DOMAIN: &str = "lost-harness/model-manifest/v1";

/// The only manifest schema version this build accepts.
pub const MANIFEST_SCHEMA_VERSION: u32 = 1;

/// The manifest filename, inside the app storage base directory.
pub const MANIFEST_FILENAME: &str = "model-manifest.json";

/// Anti-rollback high-water mark: the highest manifest `serial` this
/// installation has ever accepted. A validly-signed but OLDER manifest (a
/// replay of a superseded one, e.g. re-adding a model that was pulled for a
/// bad hash) is refused.
pub const MANIFEST_SERIAL_FILENAME: &str = "model-manifest.serial";

/// The mutable-tip revision. Only ever used for models that are explicitly NOT
/// curated — [`Provenance::Community`], consent-gated. By construction a
/// `Curated` model can never carry this: `Curated` comes only from
/// [`ManifestState::Verified`], whose revision is a validated 40-hex commit.
pub const UNPINNED_REVISION: &str = "main";

/// Every way manifest verification can fail. There is no success-with-warning
/// variant on purpose: any of these means "not curated, nothing pinned".
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ManifestError {
    #[error("no signed model manifest at {0}")]
    Missing(String),
    #[error("the model manifest could not be read: {0}")]
    Io(String),
    #[error("the model manifest is malformed: {0}")]
    Malformed(String),
    #[error("this build has no manifest signing key compiled in")]
    NoKeyConfigured,
    #[error("the compiled-in manifest public key is unusable: {0}")]
    BadKey(String),
    #[error("the model manifest signature does not verify against the compiled-in public key")]
    BadSignature,
    #[error(
        "model manifest schema version {found} is not supported (this build accepts {expected})"
    )]
    UnsupportedSchema { found: u32, expected: u32 },
    #[error(
        "model manifest serial {found} is older than the highest serial already accepted ({floor}) — refusing a rollback/replay"
    )]
    Rollback { found: u64, floor: u64 },
}

/// A manifest entry: a pinned commit revision plus the expected SHA-256 of
/// every file the app is allowed to download at that revision. This is the
/// authoritative trust root — it never merges with the live API's
/// self-reported values, it replaces them, and a file the manifest does not
/// list is not downloadable for a verified model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestEntry {
    /// Immutable git commit SHA (full 40-hex). Validated at load time —
    /// `"main"` or any other mutable ref is rejected outright.
    pub revision: String,
    /// File path → expected SHA-256 (64 hex). Validated at load time.
    pub files: BTreeMap<String, String>,
}

impl ManifestEntry {
    fn validate(&self, model_id: &str) -> Result<(), ManifestError> {
        if !is_pinned_revision(&self.revision) {
            return Err(ManifestError::Malformed(format!(
                "model {model_id:?} pins revision {:?}, which is not an immutable 40-hex commit sha",
                self.revision
            )));
        }
        if self.files.is_empty() {
            return Err(ManifestError::Malformed(format!(
                "model {model_id:?} lists no files"
            )));
        }
        for (path, sha) in &self.files {
            if !safe_tree_path(path) {
                return Err(ManifestError::Malformed(format!(
                    "model {model_id:?} lists an unsafe file path {path:?}"
                )));
            }
            if !is_real_sha256(sha) {
                return Err(ManifestError::Malformed(format!(
                    "model {model_id:?} file {path:?} has no real sha256"
                )));
            }
        }
        Ok(())
    }
}

/// Independently-signed manifest of curated model revisions and hashes.
///
/// JSON shape (all fields required):
/// ```json
/// {
///   "version": 1,
///   "serial": 1,
///   "signed_at": "2026-07-29T00:00:00Z",
///   "signature": "<128 hex chars — ed25519 over the signing payload>",
///   "models": {
///     "Qwen/Qwen3-0.6B-GGUF": {
///       "revision": "<40-hex commit sha>",
///       "files": { "Qwen3-0.6B-Q8_0.gguf": "<64-hex sha256>" }
///     }
///   }
/// }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelManifest {
    /// Schema version — must equal [`MANIFEST_SCHEMA_VERSION`].
    pub version: u32,
    /// Monotonic counter, bumped on every re-sign. Anti-rollback: an
    /// installation refuses a manifest whose serial is below the highest it has
    /// already accepted.
    pub serial: u64,
    /// ISO-8601 timestamp of when the manifest was signed. Covered by the
    /// signature; must not contain a newline (it is a framed payload line).
    pub signed_at: String,
    /// Ed25519 signature, hex, over [`ModelManifest::signing_payload`].
    pub signature: String,
    /// model_id → pinned revision + file hashes.
    pub models: BTreeMap<String, ManifestEntry>,
}

impl ModelManifest {
    /// The exact bytes the signature covers: a domain-separated, newline-framed
    /// header over `version`/`serial`/`signed_at` followed by the canonical
    /// JSON of `models` (`BTreeMap` ⇒ deterministic key order).
    ///
    /// Framing the header INTO the signed bytes is what makes the anti-rollback
    /// serial meaningful — an attacker cannot lift a valid signature onto a
    /// manifest with a different serial or schema version.
    pub fn signing_payload(&self) -> Result<Vec<u8>, ManifestError> {
        if self.signed_at.contains('\n') {
            return Err(ManifestError::Malformed(
                "signed_at must not contain a newline".to_string(),
            ));
        }
        let models = serde_json::to_string(&self.models)
            .map_err(|e| ManifestError::Malformed(format!("models are not serialisable: {e}")))?;
        Ok(format!(
            "{MANIFEST_PAYLOAD_DOMAIN}\nversion={}\nserial={}\nsigned_at={}\n{models}",
            self.version, self.serial, self.signed_at
        )
        .into_bytes())
    }

    /// Look up a model. Only reachable on a signature-verified manifest, so a
    /// hit here is a genuine trust decision.
    #[must_use]
    pub fn lookup(&self, model_id: &str) -> Option<&ManifestEntry> {
        self.models.get(model_id)
    }

    fn validate(&self) -> Result<(), ManifestError> {
        if self.version != MANIFEST_SCHEMA_VERSION {
            return Err(ManifestError::UnsupportedSchema {
                found: self.version,
                expected: MANIFEST_SCHEMA_VERSION,
            });
        }
        for (id, entry) in &self.models {
            if !valid_model_id(id) {
                return Err(ManifestError::Malformed(format!(
                    "manifest lists a malformed model id {id:?}"
                )));
            }
            entry.validate(id)?;
        }
        Ok(())
    }
}

/// Verify a manifest document. FAIL-CLOSED: every failure path returns `Err`,
/// and there is no way to obtain a [`ModelManifest`] value except through this
/// function, so an unverified manifest cannot reach a trust decision.
///
/// `public_key_b64` is the trust root (`None` ⇒ refuse everything).
/// `serial_floor` is the highest serial already accepted on this installation.
pub fn verify_manifest_json(
    text: &str,
    public_key_b64: Option<&str>,
    serial_floor: u64,
) -> Result<ModelManifest, ManifestError> {
    use base64::Engine as _;
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};

    let manifest: ModelManifest = serde_json::from_str(text)
        .map_err(|e| ManifestError::Malformed(format!("not valid manifest JSON: {e}")))?;

    // The trust root must exist before anything is believed.
    let key_b64 = public_key_b64.ok_or(ManifestError::NoKeyConfigured)?;
    let key_bytes = base64::engine::general_purpose::STANDARD
        .decode(key_b64.trim())
        .map_err(|e| ManifestError::BadKey(format!("not valid base64: {e}")))?;
    let key_bytes: [u8; 32] = key_bytes.try_into().map_err(|v: Vec<u8>| {
        ManifestError::BadKey(format!("expected 32 raw key bytes, got {}", v.len()))
    })?;
    let verifying_key = VerifyingKey::from_bytes(&key_bytes)
        .map_err(|e| ManifestError::BadKey(format!("not a valid ed25519 public key: {e}")))?;

    let sig_bytes = hex::decode(manifest.signature.trim())
        .map_err(|e| ManifestError::Malformed(format!("signature is not hex: {e}")))?;
    let signature = Signature::from_slice(&sig_bytes)
        .map_err(|e| ManifestError::Malformed(format!("signature is not 64 bytes: {e}")))?;

    let payload = manifest.signing_payload()?;
    verifying_key
        .verify(&payload, &signature)
        .map_err(|_| ManifestError::BadSignature)?;

    // Only now is any field of the manifest believable.
    manifest.validate()?;
    if manifest.serial < serial_floor {
        return Err(ManifestError::Rollback {
            found: manifest.serial,
            floor: serial_floor,
        });
    }
    Ok(manifest)
}

/// The recorded anti-rollback high-water mark, or `0` when there is none
/// (first run) or the file is unreadable/garbage. A missing floor cannot make
/// anything MORE trusted — the signature check is independent — it only means
/// no replay has been observed yet.
fn read_serial_floor(dir: &Path) -> u64 {
    std::fs::read_to_string(dir.join(MANIFEST_SERIAL_FILENAME))
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0)
}

/// Record a newly-accepted serial as the high-water mark. Best-effort: a write
/// failure is logged, never fatal (it only weakens future replay detection, it
/// cannot grant trust).
fn record_serial_floor(dir: &Path, serial: u64) {
    if serial <= read_serial_floor(dir) {
        return;
    }
    if let Err(e) = std::fs::write(dir.join(MANIFEST_SERIAL_FILENAME), serial.to_string()) {
        tracing::warn!(
            error = %e,
            "could not record the model-manifest serial high-water mark; manifest replay detection is degraded"
        );
    }
}

/// Load + verify the manifest from `dir` using the given trust root. On
/// success the serial is recorded as the new high-water mark.
fn load_manifest_with(
    dir: &Path,
    public_key_b64: Option<&str>,
) -> Result<ModelManifest, ManifestError> {
    let path = dir.join(MANIFEST_FILENAME);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(ManifestError::Missing(path.display().to_string()))
        }
        Err(e) => return Err(ManifestError::Io(e.to_string())),
    };
    let manifest = verify_manifest_json(&text, public_key_b64, read_serial_floor(dir))?;
    record_serial_floor(dir, manifest.serial);
    Ok(manifest)
}

/// Load + verify the manifest from the app storage base directory against the
/// compiled-in trust root.
pub fn load_manifest(storage_base: &Path) -> Result<ModelManifest, ManifestError> {
    load_manifest_with(storage_base, MANIFEST_PUBLIC_KEY_B64)
}

/// The fail-closed outcome of resolving one model against the manifest. Only
/// [`ManifestState::Verified`] yields [`Provenance::Curated`] and a pinned
/// revision; the other variants carry the reason so the UI can be honest
/// instead of silently degrading.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ManifestState {
    /// Manifest loaded, signature verified, model listed at this immutable commit.
    Verified { revision: String },
    /// Manifest loaded and verified, but it does not list this model.
    NotListed,
    /// There is no manifest file at all.
    Absent,
    /// A manifest exists but could not be trusted — bad signature, malformed,
    /// unsupported schema, replayed, or no key compiled into this build.
    Invalid { reason: String },
}

/// A resolved model: its [`ManifestState`] and, only when verified, the entry
/// that pins it. The entry is private so it cannot be obtained without going
/// through the verified path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestResolution {
    state: ManifestState,
    entry: Option<ManifestEntry>,
}

impl ManifestResolution {
    #[must_use]
    pub fn state(&self) -> &ManifestState {
        &self.state
    }

    /// The verified entry, if any. `Some` ⇔ [`ManifestState::Verified`].
    #[must_use]
    pub fn entry(&self) -> Option<&ManifestEntry> {
        self.entry.as_ref()
    }

    #[must_use]
    pub fn is_verified(&self) -> bool {
        matches!(self.state, ManifestState::Verified { .. })
    }

    /// The trust label. The ONLY place [`Provenance::Curated`] is produced.
    #[must_use]
    pub fn provenance(&self) -> Provenance {
        if self.is_verified() {
            Provenance::Curated
        } else {
            Provenance::Community
        }
    }

    /// The revision to build URLs from: the pinned commit when verified, the
    /// mutable tip otherwise (which is only ever paired with
    /// [`Provenance::Community`], i.e. consent-gated).
    #[must_use]
    pub fn revision(&self) -> &str {
        match &self.state {
            ManifestState::Verified { revision } => revision,
            _ => UNPINNED_REVISION,
        }
    }
}

/// Resolve one model against the manifest in `dir`, using the given trust root.
fn resolve_manifest_with(
    dir: &Path,
    public_key_b64: Option<&str>,
    model_id: &str,
) -> ManifestResolution {
    match load_manifest_with(dir, public_key_b64) {
        Ok(manifest) => match manifest.lookup(model_id) {
            // Entries were validated during verification, so `revision` here is
            // guaranteed to be an immutable 40-hex commit.
            Some(entry) => ManifestResolution {
                state: ManifestState::Verified {
                    revision: entry.revision.clone(),
                },
                entry: Some(entry.clone()),
            },
            None => ManifestResolution {
                state: ManifestState::NotListed,
                entry: None,
            },
        },
        Err(ManifestError::Missing(_)) => ManifestResolution {
            state: ManifestState::Absent,
            entry: None,
        },
        Err(e) => ManifestResolution {
            state: ManifestState::Invalid {
                reason: e.to_string(),
            },
            entry: None,
        },
    }
}

/// Resolve one model against the signed manifest in the app storage base
/// directory. FAIL-CLOSED — see [`ManifestState`].
#[must_use]
pub fn resolve_manifest(storage_base: &Path, model_id: &str) -> ManifestResolution {
    resolve_manifest_with(storage_base, MANIFEST_PUBLIC_KEY_B64, model_id)
}

/// Is this an immutable pinned revision (a full 40-hex git commit sha)? Mutable
/// refs (`main`, tags, short shas) are refused, so a manifest cannot pin to
/// something the content host can move under us.
#[must_use]
pub fn is_pinned_revision(revision: &str) -> bool {
    revision.len() == 40 && revision.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Is a revision safe to splice into a URL path? Either the literal mutable tip
/// or a pinned commit sha — nothing else, so a manifest- or caller-supplied
/// revision can never smuggle a path segment, query, or scheme into a request.
fn revision_ok_for_url(revision: &str) -> bool {
    revision == UNPINNED_REVISION || is_pinned_revision(revision)
}

/// Reduce a live tree listing to what a VERIFIED manifest entry authorises: a
/// file the manifest does not list is DROPPED (never offered with a
/// host-supplied hash under a `Curated` label), and a listed file's expected
/// digest is replaced by the manifest's.
fn apply_manifest_entry(files: Vec<QuantOption>, entry: &ManifestEntry) -> Vec<QuantOption> {
    files
        .into_iter()
        .filter_map(|mut f| {
            let sha = entry.files.get(&f.filename)?;
            f.sha256 = sha.clone();
            Some(f)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Pure helpers (fixture-tested; no I/O)
// ---------------------------------------------------------------------------

/// The publisher segment of a repo id (before the first `/`). Empty if none.
pub fn publisher_of(id: &str) -> &str {
    id.split('/').next().unwrap_or("")
}

/// Is this publisher on the [`CURATED_PUBLISHERS`] **discovery** allowlist
/// (case-insensitive)? Replaces the pre-P09 `provenance_of`, which returned a
/// trust label from a name — the exact conflation H-08 flagged. This answers a
/// list-filtering question only; it can never produce [`Provenance::Curated`].
#[must_use]
pub fn is_curated_publisher(publisher: &str) -> bool {
    let p = publisher.trim().to_ascii_lowercase();
    !p.is_empty() && CURATED_PUBLISHERS.iter().any(|t| *t == p)
}

/// Is this a well-formed HF repo id (`org/repo`, both segments limited to
/// `[A-Za-z0-9._-]`)? Enforced BEFORE a model id is spliced into any URL, so a
/// crafted id can never smuggle a path traversal, query, or scheme into an
/// outbound request — the host allowlist then re-checks the final URL anyway
/// (defense in depth).
pub fn valid_model_id(id: &str) -> bool {
    fn ok_seg(s: &str) -> bool {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
            && s != "."
            && s != ".."
    }
    let mut parts = id.split('/');
    matches!(
        (parts.next(), parts.next(), parts.next()),
        (Some(org), Some(repo), None) if ok_seg(org) && ok_seg(repo)
    )
}

/// Is a repo-tree file path safe to splice into a resolve URL? Server-supplied,
/// so treated as untrusted: no leading slash, no traversal, no query/fragment
/// metacharacters, no scheme. A file failing this is skipped (defensive — a
/// benign repo never trips it).
fn safe_tree_path(p: &str) -> bool {
    !p.is_empty()
        && !p.starts_with('/')
        && !p.contains("..")
        && !p.contains('\\')
        && !p.contains('?')
        && !p.contains('#')
        && !p.contains("://")
}

/// Extract the quant token from a GGUF filename. Recognises the llama.cpp
/// convention: a `Q<n>...` / `IQ<n>...` / `F16`/`F32`/`BF16` token, matched
/// case-insensitively and returned in the canonical upper-case form. `None`
/// when no such token is present.
///
/// Examples: `"Qwen3-0.6B-Q4_K_M.gguf"` → `Some("Q4_K_M")`,
/// `"model.IQ4_XS.gguf"` → `Some("IQ4_XS")`, `"model-f16.gguf"` →
/// `Some("F16")`, `"model.gguf"` → `None`.
pub fn parse_quant_from_filename(filename: &str) -> Option<String> {
    // Strip the extension and any split-part suffix, then scan tokens split on
    // the usual separators.
    let stem = filename.strip_suffix(".gguf").unwrap_or(filename);
    for raw in stem.split(['-', '.', '_', ' ']) {
        let tok = raw.to_ascii_uppercase();
        let bytes = tok.as_bytes();
        // Q<digit>... or IQ<digit>...  (K/M/S/L/XS/XXS variants follow, but the
        // quant "family" token is the one starting Q/IQ + a digit; we want the
        // WHOLE quant descriptor, so reconstruct it from the stem instead).
        let is_q = bytes.first() == Some(&b'Q') && bytes.get(1).is_some_and(|c| c.is_ascii_digit());
        let is_iq = tok.starts_with("IQ") && bytes.get(2).is_some_and(|c| c.is_ascii_digit());
        if is_q || is_iq {
            return Some(reconstruct_quant(stem, raw));
        }
        if matches!(tok.as_str(), "F16" | "F32" | "BF16" | "FP16") {
            return Some(tok);
        }
    }
    None
}

/// Given the filename stem and the raw token that started the quant (e.g.
/// `"Q4"`), reconstruct the full quant descriptor `"Q4_K_M"` by consuming the
/// contiguous quant sub-tokens (`K`, `M`, `S`, `L`, `XS`, `0`, `1`, digits) that
/// follow it, joined by `_`. Falls back to the raw token upper-cased.
fn reconstruct_quant(stem: &str, start_tok: &str) -> String {
    // Find the start token, then walk forward over `_`-joined quant qualifiers.
    let upper = stem.to_ascii_uppercase();
    let start_up = start_tok.to_ascii_uppercase();
    let Some(pos) = upper.find(&start_up) else {
        return start_up;
    };
    let tail = &upper[pos..];
    // The quant descriptor is the leading run of [A-Z0-9_] characters.
    let end = tail
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(tail.len());
    let mut desc = tail[..end].to_string();
    // Trim a trailing split-part join if it leaked in (defensive).
    while desc.ends_with('_') {
        desc.pop();
    }
    desc
}

/// Parse a `-NNNNN-of-MMMMM` split-file token from a filename, if present.
/// Returns the part info and the byte span of the token within the stem (used
/// by [`logical_stem`] to strip it for grouping).
fn find_part_token(stem: &str) -> Option<(PartInfo, std::ops::Range<usize>)> {
    let lower = stem.to_ascii_lowercase();
    let of_idx = lower.rfind("-of-")?;
    let before = &stem[..of_idx];
    let index_str = before.rsplit('-').next()?;
    if index_str.is_empty() || !index_str.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let after = &stem[of_idx + 4..];
    let total_str: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
    let index: u32 = index_str.parse().ok()?;
    let total: u32 = total_str.parse().ok()?;
    // Token span: the '-' before the index through the end of the total digits.
    let start = of_idx - index_str.len() - 1;
    let end = of_idx + 4 + total_str.len();
    Some((PartInfo { index, total }, start..end))
}

/// Parse a `-NNNNN-of-MMMMM` split-file token from a filename, if present.
fn parse_part_info(filename: &str) -> Option<PartInfo> {
    let stem = filename.strip_suffix(".gguf").unwrap_or(filename);
    find_part_token(stem).map(|(p, _)| p)
}

/// The filename with any split-part token removed — the grouping key that maps
/// every part of one quant to the same logical entry.
fn logical_stem(filename: &str) -> String {
    let stem = filename.strip_suffix(".gguf").unwrap_or(filename);
    match find_part_token(stem) {
        Some((_, span)) => format!("{}{}", &stem[..span.start], &stem[span.end..]),
        None => stem.to_string(),
    }
}

/// Group a flat file list into LOGICAL quants: single files stand alone;
/// multi-part sets are grouped by their part-token-stripped filename, sizes
/// summed, and marked incomplete when parts are missing or inconsistent (an
/// incomplete set must never be offered as downloadable — its size would lie
/// and the merged file could never verify). Pure; preserves first-seen order.
pub fn group_quants(files: Vec<QuantOption>) -> Vec<QuantGroup> {
    let mut order: Vec<String> = Vec::new();
    let mut map: HashMap<String, Vec<QuantOption>> = HashMap::new();
    for f in files {
        let key = logical_stem(&f.filename);
        if !map.contains_key(&key) {
            order.push(key.clone());
        }
        map.entry(key).or_default().push(f);
    }
    order
        .into_iter()
        .map(|k| {
            let mut files = map.remove(&k).expect("key came from the map");
            files.sort_by_key(|f| f.part.map(|p| p.index).unwrap_or(0));
            let total_size_bytes = files.iter().map(|f| f.size_bytes).sum();
            let quant = files[0].quant.clone();
            let complete = match files[0].part {
                // A lone file is complete; two same-named non-part files would
                // be a server anomaly — not downloadable.
                None => files.len() == 1,
                Some(first) => {
                    let declared = first.total;
                    files.len() == declared as usize
                        && files
                            .iter()
                            .all(|f| f.part.is_some_and(|p| p.total == declared))
                        && (1..=declared)
                            .all(|i| files.iter().any(|f| f.part.is_some_and(|p| p.index == i)))
                }
            };
            QuantGroup {
                quant,
                total_size_bytes,
                files,
                complete,
            }
        })
        .collect()
}

/// Build the resolve URL for a file in a repo at a specific revision. Callers
/// must have validated `revision` with [`revision_ok_for_url`] first
/// ([`parse_tree`] and [`list_quants`] both do, and are the only paths in).
fn resolve_url(model_id: &str, path: &str, revision: &str) -> String {
    format!("https://huggingface.co/{model_id}/resolve/{revision}/{path}")
}

// --- search results ---

#[derive(Debug, Deserialize)]
struct RawSearchRow {
    // The live HF API returns BOTH `id` and `modelId` (identical values). A
    // serde `alias` makes them collide ("duplicate field `id`") when both are
    // present — the real-API bug the A5 live run caught — so read them as
    // SEPARATE optional fields and prefer `id` (canonical in the current API;
    // `modelId` is the older name — accepting either is future/back-proof).
    #[serde(default)]
    id: Option<String>,
    #[serde(default, rename = "modelId")]
    model_id: Option<String>,
    // Option, not bare u64: HF can emit `null` for counts it can't compute, and
    // `#[serde(default)]` alone only covers a MISSING field — an explicit null
    // would otherwise fail the whole page. Absent stays absent (honest), never 0.
    #[serde(default)]
    downloads: Option<u64>,
    #[serde(default)]
    likes: Option<u64>,
    #[serde(default)]
    tags: Option<Vec<String>>,
}

/// Parse the JSON body of a `/api/models` search response into summaries. A row
/// with no usable id at all is skipped (never a blank entry). Pure.
///
/// `manifest` is the VERIFIED manifest (`None` when this installation has none,
/// which is the fail-closed default). A row is labelled
/// [`Provenance::Curated`] only when that manifest lists its id — the publisher
/// allowlist sets `curated_publisher` for the Staff-picks filter and nothing else.
pub fn parse_search_results(
    json: &str,
    manifest: Option<&ModelManifest>,
) -> anyhow::Result<Vec<HfModelSummary>> {
    let rows: Vec<RawSearchRow> = serde_json::from_str(json)?;
    Ok(rows
        .into_iter()
        .filter_map(|r| {
            let id = r.id.or(r.model_id).filter(|s| !s.is_empty())?;
            let publisher = publisher_of(&id).to_string();
            let provenance = match manifest.and_then(|m| m.lookup(&id)) {
                Some(_) => Provenance::Curated,
                None => Provenance::Community,
            };
            Some(HfModelSummary {
                curated_publisher: is_curated_publisher(&publisher),
                id,
                publisher,
                downloads: r.downloads,
                likes: r.likes,
                tags: r.tags.unwrap_or_default(),
                provenance,
            })
        })
        .collect())
}

// --- tree (files/quants) ---

#[derive(Debug, Deserialize)]
struct RawTreeEntry {
    #[serde(default, rename = "type")]
    entry_type: String,
    path: String,
    #[serde(default)]
    lfs: Option<RawLfs>,
}

#[derive(Debug, Deserialize)]
struct RawLfs {
    oid: String,
    size: u64,
}

/// Parse a `/api/models/{id}/tree/{revision}` response into the model's
/// downloadable files. Only `*.gguf` LFS files with a usable 64-hex oid and
/// a safe path are surfaced (a GGUF small enough to not be LFS-tracked, or
/// one whose oid isn't a sha256, can't be verify-installed — we drop it
/// rather than offer an un-verifiable download). Pure over
/// `(json, model_id, revision)`; refuses a malformed model id loudly.
pub fn parse_tree(json: &str, model_id: &str, revision: &str) -> anyhow::Result<Vec<QuantOption>> {
    if !valid_model_id(model_id) {
        anyhow::bail!("malformed model id: {model_id:?}");
    }
    if !revision_ok_for_url(revision) {
        anyhow::bail!(
            "refusing to build urls for revision {revision:?}: expected an immutable 40-hex commit sha or {UNPINNED_REVISION:?}"
        );
    }
    let entries: Vec<RawTreeEntry> = serde_json::from_str(json)?;
    let mut out = Vec::new();
    for e in entries {
        if e.entry_type != "file" && !e.entry_type.is_empty() {
            continue;
        }
        if !e.path.to_ascii_lowercase().ends_with(".gguf") {
            continue;
        }
        if !safe_tree_path(&e.path) {
            continue; // server-supplied path with URL metacharacters — skip
        }
        // GGUF weights are LFS-tracked; the `oid` there is the real sha256. A
        // .gguf without an LFS block (or with a non-sha oid) can't be
        // verify-installed, so it's not a real download option.
        let Some(lfs) = e.lfs else { continue };
        if !is_real_sha256(&lfs.oid) {
            continue;
        }
        out.push(QuantOption {
            quant: parse_quant_from_filename(&e.path),
            url: resolve_url(model_id, &e.path, revision),
            part: parse_part_info(&e.path),
            filename: e.path,
            sha256: lfs.oid,
            size_bytes: lfs.size,
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Network I/O (thin; exercised by the env-gated live test)
// ---------------------------------------------------------------------------

fn hf_client() -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent("lost-harness/0.1 (model-search)")
        // Small API responses — a hung request must not wedge the caller.
        .timeout(Duration::from_secs(30))
        .connect_timeout(Duration::from_secs(10))
        // Re-check the host allowlist on every redirect hop.
        .redirect(allowlisted_redirect_policy())
        .build()?)
}

/// GET a host-allowlisted HF URL as text, failing loudly on an off-allowlist
/// host (the SSRF/allowlist discipline the whole subsystem shares).
async fn get_allowlisted_text(client: &reqwest::Client, url: &str) -> anyhow::Result<String> {
    if !host_allowed(url) {
        anyhow::bail!("refusing to fetch a non-allowlisted host: {url}");
    }
    let body = client
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    Ok(body)
}

/// Build the search URL (host-allowlisted by construction). `query` is empty for
/// the Staff-picks default view.
fn search_url(query: &str, sort: SearchSort, limit: u32) -> String {
    let mut url = url::Url::parse("https://huggingface.co/api/models").expect("static base");
    {
        let mut q = url.query_pairs_mut();
        if !query.trim().is_empty() {
            q.append_pair("search", query.trim());
        }
        q.append_pair("filter", "gguf");
        q.append_pair("sort", sort.as_api_param());
        q.append_pair("direction", "-1");
        q.append_pair("limit", &limit.to_string());
    }
    url.into()
}

/// Search HuggingFace for GGUF models. `query` empty → the Staff-picks default
/// (top by the chosen sort). `storage_base` is where the signed manifest lives;
/// rows it lists are labelled [`Provenance::Curated`], everything else is
/// `Community`. Network I/O — live-tested only.
pub async fn search(
    query: &str,
    sort: SearchSort,
    limit: u32,
    storage_base: &Path,
) -> anyhow::Result<Vec<HfModelSummary>> {
    let client = hf_client()?;
    let url = search_url(query, sort, limit.clamp(1, 100));
    let body = get_allowlisted_text(&client, &url).await?;
    // Fail-closed: an unverifiable manifest is simply no manifest, which means
    // no `Curated` labels — never a fallback to publisher-name trust.
    let manifest = load_manifest(storage_base).ok();
    parse_search_results(&body, manifest.as_ref())
}

/// The Staff-picks default view: top allowlisted-publisher GGUF models by
/// downloads. Filters on [`HfModelSummary::curated_publisher`] — a
/// **discovery** filter. It does not imply the rows are verified: their
/// `provenance` is still `Community` unless the signed manifest lists them.
pub async fn staff_picks(limit: u32, storage_base: &Path) -> anyhow::Result<Vec<HfModelSummary>> {
    // Over-fetch, then keep only allowlisted publishers, up to `limit`.
    let limit = limit.clamp(1, 25);
    let all = search("", SearchSort::Downloads, limit * 4, storage_base).await?;
    Ok(all
        .into_iter()
        .filter(|m| m.curated_publisher)
        .take(limit as usize)
        .collect())
}

/// List a model's downloadable GGUF files (ungrouped) at the given revision.
/// `revision` must be [`UNPINNED_REVISION`] or an immutable 40-hex commit sha.
/// Network I/O.
pub async fn list_quants(model_id: &str, revision: &str) -> anyhow::Result<Vec<QuantOption>> {
    if !valid_model_id(model_id) {
        anyhow::bail!("malformed model id: {model_id:?}");
    }
    if !revision_ok_for_url(revision) {
        anyhow::bail!(
            "refusing to fetch revision {revision:?}: expected an immutable 40-hex commit sha or {UNPINNED_REVISION:?}"
        );
    }
    let client = hf_client()?;
    let url = format!("https://huggingface.co/api/models/{model_id}/tree/{revision}");
    let body = get_allowlisted_text(&client, &url).await?;
    parse_tree(&body, model_id, revision)
}

/// The full detail view for a model: publisher/provenance + every quant grouped
/// into logical (multi-part-aware) download units.
///
/// FAIL-CLOSED provenance (P09 / H-08). The signed manifest in `storage_base` is
/// resolved first ([`resolve_manifest`]):
///
/// - **Verified**: URLs are pinned to the manifest's immutable commit, the
///   expected hashes are the manifest's, files the manifest does not list are
///   dropped, and the label is [`Provenance::Curated`].
/// - **Anything else** (absent / not listed / bad signature / replayed / no key
///   compiled in): the label is [`Provenance::Community`] — which the download
///   command consent-gates — the listing uses the mutable tip, and
///   [`HfModelDetail::manifest`] states the reason. There is no path here in
///   which a publisher name produces `Curated`.
pub async fn model_detail(model_id: &str, storage_base: &Path) -> anyhow::Result<HfModelDetail> {
    let resolution = resolve_manifest(storage_base, model_id);
    let revision = resolution.revision().to_string();

    let mut files = list_quants(model_id, &revision).await?;

    // Only a VERIFIED entry may touch hashes — and when one does, it is
    // authoritative in both directions: it replaces the host's digest AND
    // removes files it does not vouch for.
    if let Some(entry) = resolution.entry() {
        files = apply_manifest_entry(files, entry);
    }

    let publisher = publisher_of(model_id).to_string();
    Ok(HfModelDetail {
        id: model_id.to_string(),
        curated_publisher: is_curated_publisher(&publisher),
        publisher,
        provenance: resolution.provenance(),
        manifest: resolution.state().clone(),
        quants: group_quants(files),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publisher_allowlist_is_discovery_only() {
        assert_eq!(publisher_of("Qwen/Qwen3-0.6B-GGUF"), "Qwen");
        assert_eq!(publisher_of("no-slash-id"), "no-slash-id");
        assert_eq!(publisher_of(""), "");
        // Allowlisted: official orgs + the named requantizers, case-insensitive.
        assert!(is_curated_publisher("Qwen"));
        assert!(is_curated_publisher("lmstudio-community"));
        assert!(is_curated_publisher("BARTOWSKI"));
        assert!(is_curated_publisher("unsloth"));
        // Anyone else is off the list.
        assert!(!is_curated_publisher("some-random-user"));
        assert!(!is_curated_publisher(""));
    }

    #[test]
    fn model_id_validation_blocks_url_smuggling() {
        assert!(valid_model_id("Qwen/Qwen3-0.6B-GGUF"));
        assert!(valid_model_id("lmstudio-community/some_model.v2"));
        // Traversal, extra segments, schemes, metacharacters: all refused.
        assert!(!valid_model_id("../../evil.com"));
        assert!(!valid_model_id("a/b/c"));
        assert!(!valid_model_id("https://evil.com/x"));
        assert!(!valid_model_id("org/repo?x=1"));
        assert!(!valid_model_id("org/repo#frag"));
        assert!(!valid_model_id("org/.."));
        assert!(!valid_model_id("/repo"));
        assert!(!valid_model_id(""));
    }

    #[test]
    fn quant_token_is_parsed_from_the_filename_convention() {
        let cases = [
            ("Qwen3-0.6B-Q4_K_M.gguf", Some("Q4_K_M")),
            ("Qwen3-0.6B-Q8_0.gguf", Some("Q8_0")),
            ("model-Q4_0.gguf", Some("Q4_0")),
            ("some.model.IQ4_XS.gguf", Some("IQ4_XS")),
            ("Meta-Llama-3-8B-Q6_K.gguf", Some("Q6_K")),
            ("model-f16.gguf", Some("F16")),
            ("model.BF16.gguf", Some("BF16")),
            ("tinystories.gguf", None),
            ("model-Q4_K_M-00001-of-00002.gguf", Some("Q4_K_M")),
        ];
        for (name, want) in cases {
            assert_eq!(
                parse_quant_from_filename(name).as_deref(),
                want,
                "filename {name}"
            );
        }
    }

    #[test]
    fn split_part_info_is_parsed_when_present() {
        assert_eq!(
            parse_part_info("model-Q4_K_M-00001-of-00003.gguf"),
            Some(PartInfo { index: 1, total: 3 })
        );
        assert_eq!(parse_part_info("model-Q4_K_M.gguf"), None);
        // The grouping key strips the part token; non-part names pass through.
        assert_eq!(
            logical_stem("model-Q4_K_M-00001-of-00003.gguf"),
            "model-Q4_K_M"
        );
        assert_eq!(logical_stem("model-Q4_K_M.gguf"), "model-Q4_K_M");
    }

    fn qopt(filename: &str, size: u64) -> QuantOption {
        QuantOption {
            quant: parse_quant_from_filename(filename),
            filename: filename.to_string(),
            url: resolve_url("org/repo", filename, "main"),
            sha256: "a".repeat(64),
            size_bytes: size,
            part: parse_part_info(filename),
        }
    }

    #[test]
    fn multi_part_quants_group_with_summed_sizes() {
        let files = vec![
            qopt("big-Q4_K_M-00002-of-00002.gguf", 40),
            qopt("big-Q4_K_M-00001-of-00002.gguf", 60),
            qopt("small-Q8_0.gguf", 7),
        ];
        let groups = group_quants(files);
        assert_eq!(groups.len(), 2, "two logical quants");
        let big = groups
            .iter()
            .find(|g| g.quant.as_deref() == Some("Q4_K_M"))
            .unwrap();
        assert_eq!(big.total_size_bytes, 100, "size is the SUM across parts");
        assert!(big.complete, "1..=2 all present");
        assert_eq!(big.files.len(), 2);
        assert_eq!(big.files[0].part.unwrap().index, 1, "parts sorted by index");
        let small = groups
            .iter()
            .find(|g| g.quant.as_deref() == Some("Q8_0"))
            .unwrap();
        assert!(small.complete);
        assert_eq!(small.total_size_bytes, 7);
    }

    #[test]
    fn an_incomplete_part_set_is_marked_not_downloadable() {
        // Part 2-of-3 missing → complete=false (its size would lie).
        let files = vec![
            qopt("big-Q4_K_M-00001-of-00003.gguf", 10),
            qopt("big-Q4_K_M-00003-of-00003.gguf", 10),
        ];
        let groups = group_quants(files);
        assert_eq!(groups.len(), 1);
        assert!(
            !groups[0].complete,
            "a missing part must not be downloadable"
        );
    }

    #[test]
    fn search_results_parse_and_carry_provenance() {
        // Mirrors the REAL HF API: a row carries BOTH `id` and `modelId`
        // (identical) — the A5 live run proved this, and a naive serde alias
        // errors "duplicate field `id`" on it. Plus explicit nulls, a
        // `modelId`-only (older-API) row, and a no-id row (must be skipped).
        let json = r#"[
            {"_id":"x","id":"Qwen/Qwen3-0.6B-GGUF","modelId":"Qwen/Qwen3-0.6B-GGUF","downloads":123456,"likes":42,"tags":["gguf","conversational"]},
            {"id":"randomuser/mystery-gguf","downloads":null,"likes":null,"tags":null},
            {"modelId":"unsloth/Qwen3-4B-GGUF","tags":["gguf","moe"]},
            {"likes":3,"tags":["gguf"]}
        ]"#;
        // No manifest on this installation — the fail-closed default.
        let out = parse_search_results(json, None).unwrap();
        assert_eq!(out.len(), 3, "the no-id row is skipped");
        assert_eq!(out[0].id, "Qwen/Qwen3-0.6B-GGUF");
        assert_eq!(out[0].publisher, "Qwen");
        assert_eq!(out[0].downloads, Some(123456));
        // H-08: an allowlisted publisher is a DISCOVERY hit, never a trust label.
        assert!(
            out[0].curated_publisher,
            "Qwen is on the discovery allowlist"
        );
        assert_eq!(
            out[0].provenance,
            Provenance::Community,
            "without a signed manifest even an allowlisted publisher is community"
        );
        // Explicit nulls parse as honest absence — never a fabricated 0.
        assert_eq!(out[1].provenance, Provenance::Community);
        assert!(!out[1].curated_publisher, "unknown publisher, off the list");
        assert_eq!(out[1].downloads, None);
        assert!(out[1].tags.is_empty());
        // `modelId` alias + missing counts still parse.
        assert_eq!(out[2].id, "unsloth/Qwen3-4B-GGUF");
        assert!(out[2].curated_publisher);
        assert_eq!(out[2].provenance, Provenance::Community);
        assert_eq!(out[2].downloads, None);

        // With a VERIFIED manifest that lists exactly one of them, only that row
        // is Curated — the allowlisted-but-unlisted rows stay Community.
        let manifest = signed_test_manifest(1, 1, &[("Qwen/Qwen3-0.6B-GGUF", PINNED_REV)]);
        let verified = verify_manifest_json(
            &serde_json::to_string(&manifest).unwrap(),
            Some(&test_public_key_b64()),
            0,
        )
        .expect("test-key manifest verifies");
        let out = parse_search_results(json, Some(&verified)).unwrap();
        assert_eq!(out[0].provenance, Provenance::Curated, "listed → curated");
        assert_eq!(
            out[2].provenance,
            Provenance::Community,
            "allowlisted publisher, not in the manifest → still community"
        );
    }

    #[test]
    fn tree_parse_extracts_only_verifiable_gguf_files() {
        // A realistic tree: README (not gguf), a config (not lfs), two GGUF
        // quants (lfs with a real oid), a GGUF with a bogus non-sha oid, and a
        // GGUF with an unsafe path.
        let json = r#"[
            {"type":"file","path":"README.md","size":1000},
            {"type":"file","path":"config.json","size":500},
            {"type":"file","path":"Qwen3-0.6B-Q8_0.gguf",
             "lfs":{"oid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","size":650000000}},
            {"type":"file","path":"Qwen3-0.6B-Q4_K_M.gguf",
             "lfs":{"oid":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","size":400000000}},
            {"type":"file","path":"broken.gguf","lfs":{"oid":"not-a-sha","size":10}},
            {"type":"file","path":"../escape.gguf",
             "lfs":{"oid":"cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc","size":10}}
        ]"#;
        let quants = parse_tree(json, "Qwen/Qwen3-0.6B-GGUF", "main").unwrap();
        assert_eq!(quants.len(), 2, "only the two verifiable, safe-path GGUFs");
        let q8 = quants
            .iter()
            .find(|q| q.quant.as_deref() == Some("Q8_0"))
            .unwrap();
        assert_eq!(q8.size_bytes, 650000000);
        assert_eq!(q8.filename, "Qwen3-0.6B-Q8_0.gguf");
        assert_eq!(
            q8.url,
            "https://huggingface.co/Qwen/Qwen3-0.6B-GGUF/resolve/main/Qwen3-0.6B-Q8_0.gguf"
        );
        assert!(
            host_allowed(&q8.url),
            "every surfaced url is host-allowlisted"
        );
        assert_eq!(q8.sha256.len(), 64);
        // A malformed model id refuses loudly before any URL is built.
        assert!(parse_tree(json, "../../evil.com", "main").is_err());
    }

    #[test]
    fn search_url_is_well_formed_and_allowlisted() {
        let u = search_url("qwen 3", SearchSort::Downloads, 20);
        assert!(
            host_allowed(&u),
            "constructed search url must be allowlisted"
        );
        assert!(u.contains("filter=gguf"));
        assert!(u.contains("sort=downloads"));
        assert!(u.contains("limit=20"));
        assert!(u.contains("search=qwen"), "query is included + encoded");
        // Empty query (staff picks) omits the search param.
        let empty = search_url("", SearchSort::Trending, 10);
        assert!(!empty.contains("search="));
        assert!(empty.contains("sort=trendingScore"));
    }

    // -----------------------------------------------------------------------
    // Signed-manifest fixtures (P09 / H-08). Everything below signs with a
    // THROWAWAY TEST KEY that exists only in this module. It is deliberately
    // NOT a production key, and
    // `a_manifest_signed_with_the_test_key_is_refused_by_the_shipped_build`
    // asserts the shipped binary refuses anything it signs.
    // -----------------------------------------------------------------------

    /// Deterministic test-only Ed25519 seed. Never use for anything real.
    const TEST_SEED: [u8; 32] = [0x2a; 32];
    /// A syntactically valid immutable commit sha for fixtures.
    const PINNED_REV: &str = "0123456789abcdef0123456789abcdef01234567";
    const OLDER_REV: &str = "fedcba9876543210fedcba9876543210fedcba98";
    const FIXTURE_ID: &str = "Qwen/Qwen3-0.6B-GGUF";
    const FIXTURE_FILE: &str = "model-Q4_K_M.gguf";

    fn test_signing_key() -> ed25519_dalek::SigningKey {
        ed25519_dalek::SigningKey::from_bytes(&TEST_SEED)
    }

    fn test_public_key_b64() -> String {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD
            .encode(test_signing_key().verifying_key().to_bytes())
    }

    /// Build a manifest over `models` and sign it with the TEST key.
    fn signed_test_manifest(version: u32, serial: u64, models: &[(&str, &str)]) -> ModelManifest {
        use ed25519_dalek::Signer;
        let mut m = ModelManifest {
            version,
            serial,
            signed_at: "2026-07-29T00:00:00Z".to_string(),
            signature: String::new(),
            models: models
                .iter()
                .map(|(id, rev)| {
                    let mut files = BTreeMap::new();
                    files.insert(FIXTURE_FILE.to_string(), "a".repeat(64));
                    (
                        (*id).to_string(),
                        ManifestEntry {
                            revision: (*rev).to_string(),
                            files,
                        },
                    )
                })
                .collect(),
        };
        let payload = m.signing_payload().expect("fixture payload");
        m.signature = hex::encode(test_signing_key().sign(&payload).to_bytes());
        m
    }

    fn fixture_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("lhp-manifest-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).expect("fixture dir");
        d
    }

    fn write_manifest(dir: &Path, m: &ModelManifest) {
        std::fs::write(
            dir.join(MANIFEST_FILENAME),
            serde_json::to_string_pretty(m).expect("serialise fixture"),
        )
        .expect("write fixture manifest");
    }

    #[test]
    fn a_test_key_signed_manifest_verifies_and_pins_an_immutable_revision() {
        let dir = fixture_dir("valid");
        let key = test_public_key_b64();
        write_manifest(
            &dir,
            &signed_test_manifest(1, 1, &[(FIXTURE_ID, PINNED_REV)]),
        );

        let res = resolve_manifest_with(&dir, Some(&key), FIXTURE_ID);
        assert!(res.is_verified(), "a valid signature must verify");
        assert_eq!(res.provenance(), Provenance::Curated);
        assert_eq!(res.revision(), PINNED_REV, "the pinned commit, not `main`");
        assert_eq!(
            res.state(),
            &ManifestState::Verified {
                revision: PINNED_REV.to_string()
            }
        );
        assert_eq!(
            res.entry().map(|e| e.files[FIXTURE_FILE].clone()),
            Some("a".repeat(64)),
            "the verified entry carries the manifest's own digests"
        );

        // A model the (valid) manifest does not list gets nothing.
        let other = resolve_manifest_with(&dir, Some(&key), "unsloth/Qwen3-4B-GGUF");
        assert_eq!(other.state(), &ManifestState::NotListed);
        assert_eq!(other.provenance(), Provenance::Community);
        assert_eq!(other.revision(), UNPINNED_REVISION);
        assert!(other.entry().is_none());
    }

    #[test]
    fn a_tampered_manifest_is_refused() {
        let key = test_public_key_b64();
        let sign_then = |mutate: fn(&mut ModelManifest)| {
            let mut m = signed_test_manifest(1, 5, &[(FIXTURE_ID, PINNED_REV)]);
            mutate(&mut m);
            serde_json::to_string(&m).expect("serialise")
        };

        // (a) a file digest swapped after signing
        let json = sign_then(|m| {
            m.models
                .get_mut(FIXTURE_ID)
                .unwrap()
                .files
                .insert(FIXTURE_FILE.to_string(), "b".repeat(64));
        });
        assert_eq!(
            verify_manifest_json(&json, Some(&key), 0).unwrap_err(),
            ManifestError::BadSignature,
            "a swapped digest must not verify"
        );

        // (b) the revision repointed after signing
        let json = sign_then(|m| {
            m.models.get_mut(FIXTURE_ID).unwrap().revision = OLDER_REV.to_string();
        });
        assert_eq!(
            verify_manifest_json(&json, Some(&key), 0).unwrap_err(),
            ManifestError::BadSignature,
            "a repointed revision must not verify"
        );

        // (c) an extra model spliced in after signing
        let json = sign_then(|m| {
            let mut files = BTreeMap::new();
            files.insert("evil-Q4_K_M.gguf".to_string(), "c".repeat(64));
            m.models.insert(
                "attacker/evil-GGUF".to_string(),
                ManifestEntry {
                    revision: OLDER_REV.to_string(),
                    files,
                },
            );
        });
        assert_eq!(
            verify_manifest_json(&json, Some(&key), 0).unwrap_err(),
            ManifestError::BadSignature,
            "an added model must not verify"
        );

        // (d) the anti-rollback serial bumped after signing — proves the serial
        //     is INSIDE the signed payload and cannot be forged to defeat the
        //     high-water mark.
        let json = sign_then(|m| m.serial = 9_999);
        assert_eq!(
            verify_manifest_json(&json, Some(&key), 0).unwrap_err(),
            ManifestError::BadSignature,
            "the serial must be covered by the signature"
        );

        // (e) resolution surfaces the failure instead of degrading silently.
        let dir = fixture_dir("tampered");
        std::fs::write(dir.join(MANIFEST_FILENAME), &json).expect("write");
        let res = resolve_manifest_with(&dir, Some(&key), FIXTURE_ID);
        assert_eq!(res.provenance(), Provenance::Community);
        assert_eq!(res.revision(), UNPINNED_REVISION);
        match res.state() {
            ManifestState::Invalid { reason } => {
                assert!(reason.contains("signature"), "reason was {reason:?}")
            }
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn a_missing_manifest_fails_closed_instead_of_trusting_main() {
        // The exact H-08 regression: the pre-fix code did
        // `ModelManifest::load(..).unwrap_or(None)` and then fell back to the
        // publisher allowlist, so an absent manifest still produced "Curated"
        // against the mutable `main` tip.
        let dir = fixture_dir("missing");
        let key = test_public_key_b64();
        assert!(matches!(
            load_manifest_with(&dir, Some(&key)).unwrap_err(),
            ManifestError::Missing(_)
        ));

        // `Qwen` IS on the discovery allowlist — the old code's Curated path.
        assert!(is_curated_publisher(publisher_of(FIXTURE_ID)));
        let res = resolve_manifest_with(&dir, Some(&key), FIXTURE_ID);
        assert_eq!(res.state(), &ManifestState::Absent);
        assert_eq!(
            res.provenance(),
            Provenance::Community,
            "no manifest ⇒ no Curated label, even for an allowlisted publisher"
        );
        assert!(!res.is_verified());
        assert!(
            res.entry().is_none(),
            "nothing may be pinned or hash-overridden without a manifest"
        );
    }

    #[test]
    fn a_replayed_older_manifest_is_refused() {
        let dir = fixture_dir("rollback");
        let key = test_public_key_b64();

        // Accept serial 7; the high-water mark is recorded.
        write_manifest(
            &dir,
            &signed_test_manifest(1, 7, &[(FIXTURE_ID, PINNED_REV)]),
        );
        assert!(load_manifest_with(&dir, Some(&key)).is_ok());
        assert_eq!(read_serial_floor(&dir), 7, "serial recorded on acceptance");

        // An attacker replays a genuinely-signed OLDER manifest (e.g. one that
        // still vouches for a revision since pulled for a bad hash).
        let old = signed_test_manifest(1, 3, &[(FIXTURE_ID, OLDER_REV)]);
        let old_json = serde_json::to_string(&old).expect("serialise");
        // Its signature is perfectly valid — only the serial floor stops it.
        assert!(
            verify_manifest_json(&old_json, Some(&key), 0).is_ok(),
            "the replayed manifest is authentically signed"
        );
        write_manifest(&dir, &old);
        assert_eq!(
            load_manifest_with(&dir, Some(&key)).unwrap_err(),
            ManifestError::Rollback { found: 3, floor: 7 }
        );
        let res = resolve_manifest_with(&dir, Some(&key), FIXTURE_ID);
        assert!(!res.is_verified(), "a rolled-back manifest grants nothing");
        assert_eq!(res.provenance(), Provenance::Community);
        assert_ne!(res.revision(), OLDER_REV, "the stale pin is never used");

        // Re-presenting the current serial still loads (idempotent reload).
        write_manifest(
            &dir,
            &signed_test_manifest(1, 7, &[(FIXTURE_ID, PINNED_REV)]),
        );
        assert!(load_manifest_with(&dir, Some(&key)).is_ok());
        // And a newer serial advances the mark.
        write_manifest(
            &dir,
            &signed_test_manifest(1, 8, &[(FIXTURE_ID, PINNED_REV)]),
        );
        assert!(load_manifest_with(&dir, Some(&key)).is_ok());
        assert_eq!(read_serial_floor(&dir), 8);
    }

    #[test]
    fn a_manifest_signed_with_the_test_key_is_refused_by_the_shipped_build() {
        // Guards against the test key (or any dev key) ever becoming the
        // compiled-in trust root. Today `MANIFEST_PUBLIC_KEY_B64` is `None`
        // (NoKeyConfigured); after the human key ceremony it is a real key and
        // this becomes BadSignature. Both are refusals — a success here would
        // mean the shipped binary trusts a key that lives in the test module.
        let json = serde_json::to_string(&signed_test_manifest(1, 1, &[(FIXTURE_ID, PINNED_REV)]))
            .expect("serialise");
        let err = verify_manifest_json(&json, MANIFEST_PUBLIC_KEY_B64, 0)
            .expect_err("the shipped build must never trust the test key");
        assert!(
            matches!(
                err,
                ManifestError::NoKeyConfigured | ManifestError::BadSignature
            ),
            "unexpected refusal reason: {err:?}"
        );
    }

    #[test]
    fn a_build_with_no_signing_key_cannot_label_anything_curated() {
        let dir = fixture_dir("nokey");
        write_manifest(
            &dir,
            &signed_test_manifest(1, 1, &[(FIXTURE_ID, PINNED_REV)]),
        );

        // No trust root compiled in ⇒ a perfectly-formed manifest buys nothing.
        let res = resolve_manifest_with(&dir, None, FIXTURE_ID);
        assert_eq!(res.provenance(), Provenance::Community);
        assert_eq!(res.revision(), UNPINNED_REVISION);
        match res.state() {
            ManifestState::Invalid { reason } => {
                assert!(reason.contains("signing key"), "reason was {reason:?}")
            }
            other => panic!("expected Invalid, got {other:?}"),
        }

        // The shipped resolver must agree while the ceremony is outstanding.
        assert!(
            !resolve_manifest(&dir, FIXTURE_ID).is_verified(),
            "the shipped build must not verify a test-key manifest"
        );
    }

    #[test]
    fn a_manifest_that_pins_a_mutable_ref_or_an_unknown_schema_is_refused() {
        let key = test_public_key_b64();
        // Correctly signed, but pins the MUTABLE tip — the whole point of the
        // manifest is an immutable pin, so this is refused outright.
        let json = serde_json::to_string(&signed_test_manifest(
            1,
            1,
            &[(FIXTURE_ID, UNPINNED_REVISION)],
        ))
        .expect("serialise");
        match verify_manifest_json(&json, Some(&key), 0).unwrap_err() {
            ManifestError::Malformed(r) => assert!(r.contains("immutable"), "reason {r:?}"),
            other => panic!("expected Malformed, got {other:?}"),
        }

        // A short sha is not an immutable pin either.
        let json =
            serde_json::to_string(&signed_test_manifest(1, 1, &[(FIXTURE_ID, "0123456")])).unwrap();
        assert!(matches!(
            verify_manifest_json(&json, Some(&key), 0).unwrap_err(),
            ManifestError::Malformed(_)
        ));

        // A future schema version is refused rather than half-understood.
        let json = serde_json::to_string(&signed_test_manifest(2, 1, &[(FIXTURE_ID, PINNED_REV)]))
            .unwrap();
        assert_eq!(
            verify_manifest_json(&json, Some(&key), 0).unwrap_err(),
            ManifestError::UnsupportedSchema {
                found: 2,
                expected: MANIFEST_SCHEMA_VERSION
            }
        );

        // Truncated / non-JSON content is Malformed, never accepted.
        assert!(matches!(
            verify_manifest_json("{ not json", Some(&key), 0).unwrap_err(),
            ManifestError::Malformed(_)
        ));

        // A malformed model id inside an otherwise valid manifest is refused.
        let json =
            serde_json::to_string(&signed_test_manifest(1, 1, &[("../evil", PINNED_REV)])).unwrap();
        assert!(matches!(
            verify_manifest_json(&json, Some(&key), 0).unwrap_err(),
            ManifestError::Malformed(_)
        ));
    }

    #[test]
    fn manifest_state_wire_shape_is_stable_for_the_ui() {
        // The renderer keys its provenance copy off this shape, so pin it.
        assert_eq!(
            serde_json::to_value(ManifestState::Verified {
                revision: PINNED_REV.to_string()
            })
            .unwrap(),
            serde_json::json!({ "state": "verified", "revision": PINNED_REV })
        );
        assert_eq!(
            serde_json::to_value(ManifestState::Absent).unwrap(),
            serde_json::json!({ "state": "absent" })
        );
        assert_eq!(
            serde_json::to_value(ManifestState::NotListed).unwrap(),
            serde_json::json!({ "state": "not_listed" })
        );
        assert_eq!(
            serde_json::to_value(ManifestState::Invalid {
                reason: "boom".to_string()
            })
            .unwrap(),
            serde_json::json!({ "state": "invalid", "reason": "boom" })
        );
        // And the label enum the UI branches on.
        assert_eq!(
            serde_json::to_value(Provenance::Curated).unwrap(),
            serde_json::json!("curated")
        );
        assert_eq!(
            serde_json::to_value(Provenance::Community).unwrap(),
            serde_json::json!("community")
        );
    }

    #[test]
    fn a_verified_entry_replaces_hashes_and_drops_unlisted_files() {
        let mut files = BTreeMap::new();
        files.insert("listed-Q4_K_M.gguf".to_string(), "c".repeat(64));
        let entry = ManifestEntry {
            revision: PINNED_REV.to_string(),
            files,
        };
        // `qopt` gives every file the live API's "aaa…" oid.
        let live = vec![
            qopt("listed-Q4_K_M.gguf", 10),
            qopt("unlisted-Q8_0.gguf", 20),
        ];
        let out = apply_manifest_entry(live, &entry);
        assert_eq!(
            out.len(),
            1,
            "a file the manifest does not vouch for is not downloadable"
        );
        assert_eq!(out[0].filename, "listed-Q4_K_M.gguf");
        assert_eq!(
            out[0].sha256,
            "c".repeat(64),
            "the manifest digest replaces the host-reported one"
        );
    }

    #[test]
    fn a_revision_is_validated_before_it_reaches_a_url() {
        assert!(is_pinned_revision(PINNED_REV));
        assert!(!is_pinned_revision(UNPINNED_REVISION));
        assert!(!is_pinned_revision(&PINNED_REV[..39]), "short sha");
        assert!(
            !is_pinned_revision(&format!("{}g", &PINNED_REV[..39])),
            "non-hex"
        );

        // Only the tip or a pinned sha may be spliced into a URL — a crafted
        // revision cannot smuggle extra path segments into the request.
        assert!(parse_tree("[]", "org/repo", UNPINNED_REVISION).is_ok());
        assert!(parse_tree("[]", "org/repo", PINNED_REV).is_ok());
        for bad in [
            "../../../etc/passwd",
            "refs/heads/main",
            "main/../../evil",
            "",
        ] {
            let err = parse_tree("[]", "org/repo", bad)
                .expect_err("a non-pinned revision must be refused")
                .to_string();
            assert!(
                err.contains("revision"),
                "revision {bad:?} refused for the wrong reason: {err}"
            );
        }

        // The pinned sha is what actually lands in the resolve URL.
        let tree = format!(
            r#"[{{"type":"file","path":"m-Q8_0.gguf","lfs":{{"oid":"{}","size":1}}}}]"#,
            "a".repeat(64)
        );
        let q = parse_tree(&tree, "org/repo", PINNED_REV).unwrap();
        assert_eq!(
            q[0].url,
            format!("https://huggingface.co/org/repo/resolve/{PINNED_REV}/m-Q8_0.gguf")
        );
    }

    #[tokio::test]
    async fn list_quants_refuses_a_non_pinned_revision_before_any_request() {
        let err = list_quants("org/repo", "refs/heads/main")
            .await
            .expect_err("a non-pinned revision must be refused")
            .to_string();
        assert!(
            err.contains("refusing to fetch revision"),
            "expected the pre-flight revision guard, got: {err}"
        );
    }

    /// Ceremony helper, not an assertion: writes the exact bytes a manifest
    /// signature must cover, so the human key ceremony never re-derives the
    /// canonical framing by hand. The input's `signature` field may be `""`.
    ///
    /// ```text
    /// LHP_MANIFEST_IN=model-manifest.json LHP_MANIFEST_PAYLOAD_OUT=payload.bin \
    ///   cargo test --lib models::hf_search::tests::emit_manifest_signing_payload \
    ///   -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "ceremony helper — needs LHP_MANIFEST_IN / LHP_MANIFEST_PAYLOAD_OUT"]
    fn emit_manifest_signing_payload() {
        let in_path = std::env::var("LHP_MANIFEST_IN").expect("set LHP_MANIFEST_IN");
        let out_path =
            std::env::var("LHP_MANIFEST_PAYLOAD_OUT").expect("set LHP_MANIFEST_PAYLOAD_OUT");
        let text = std::fs::read_to_string(&in_path).expect("read the manifest");
        let manifest: ModelManifest = serde_json::from_str(&text).expect("parse the manifest");
        manifest
            .validate()
            .expect("fix the manifest content before signing it");
        let payload = manifest
            .signing_payload()
            .expect("build the signing payload");
        std::fs::write(&out_path, &payload).expect("write the payload");
        eprintln!("wrote {} payload bytes to {out_path}", payload.len());
    }

    /// Live HF search + tree round-trip. Opt-in — set `LHP_HF_LIVE=1` to run
    /// (self-skips offline / in CI). Mirrors the `LHP_NATIVE_ENDPOINT` pattern.
    #[tokio::test]
    async fn live_hf_search_and_tree() {
        if std::env::var_os("LHP_HF_LIVE").is_none() {
            eprintln!("skipping live HF search test — set LHP_HF_LIVE=1 to run");
            return;
        }
        // No manifest in this empty dir → the fail-closed path, which is what a
        // pre-ceremony build does in production.
        let dir = fixture_dir("live");
        let results = search("qwen3", SearchSort::Downloads, 10, &dir)
            .await
            .expect("search");
        assert!(!results.is_empty(), "a real search returns rows");
        // Every row's publisher/provenance is derived, and ids are non-empty.
        for r in &results {
            assert!(!r.id.is_empty());
            assert_eq!(
                r.provenance,
                Provenance::Community,
                "with no manifest present, no live row may claim Curated"
            );
        }
        // The tiny live-test model must list at least one verifiable quant.
        let detail = model_detail("Qwen/Qwen3-0.6B-GGUF", &dir)
            .await
            .expect("detail");
        assert_eq!(detail.manifest, ManifestState::Absent);
        assert_eq!(detail.provenance, Provenance::Community);
        assert!(!detail.quants.is_empty(), "Qwen3-0.6B-GGUF lists quants");
        for g in &detail.quants {
            assert!(g.total_size_bytes > 0);
            for q in &g.files {
                assert_eq!(q.sha256.len(), 64);
                assert!(host_allowed(&q.url));
            }
        }
    }
}
