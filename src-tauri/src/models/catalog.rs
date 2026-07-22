//! Wave 5.3 / M8 — the curated model catalog. A small, human-curated list of
//! local GGUF models (bundled via `include_str!`, so browsing works fully
//! OFFLINE), each pinned to a specific Hugging Face URL + its `sha256`. The
//! catalog is filtered against the machine's [`HardwareProfile`] so onboarding
//! only offers models the user can actually run.
//!
//! The catalog's own authenticity rides on the app signature: `catalog.json` is
//! compiled into the binary, so a bundled entry is as trustworthy as the signed
//! build (Wave 7.1/7.3). A future *signed remote refresh* would need its own
//! pinned-key manifest scheme — deliberately out of v1 scope.
//!
//! **`sha256` is the trust root of the verified-before-runnable invariant**
//! ([`crate::models::download`]): a download whose bytes don't hash to the
//! catalog value installs NOTHING. Entries shipped with `sha256 = "TODO-CURATE"`
//! therefore fail closed — a placeholder can never install an unverified model.

use serde::{Deserialize, Serialize};

use crate::models::hardware::{fits, Fit, HardwareProfile};

const CATALOG_JSON: &str = include_str!("catalog.json");

/// One curated model. Durable, portable fields only.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CatalogEntry {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub family: String,
    #[serde(default)]
    pub quantization: String,
    #[serde(default)]
    pub params_billions: f64,
    /// Pinned download URL (a specific HF revision).
    pub url: String,
    /// The expected SHA-256 of the file — the download verifies against this.
    pub sha256: String,
    pub size_bytes: u64,
}

impl CatalogEntry {
    /// Is this entry actually downloadable, or still a placeholder awaiting
    /// release curation? A missing/placeholder `sha256` can never verify, so we
    /// surface it as not-yet-installable rather than letting a download start
    /// and fail opaquely later.
    pub fn is_curated(&self) -> bool {
        let s = self.sha256.trim();
        s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
    }
}

#[derive(Debug, Clone, Deserialize)]
struct CatalogFile {
    #[serde(default)]
    catalog_version: u32,
    models: Vec<CatalogEntry>,
}

/// An entry plus how it fits the current machine — the shape the onboarding UI
/// consumes.
#[derive(Debug, Clone, Serialize)]
pub struct CatalogEntryView {
    #[serde(flatten)]
    pub entry: CatalogEntry,
    pub fit: Fit,
    /// False when `sha256` is still a placeholder (not release-curated).
    pub installable: bool,
}

/// Parse a catalog JSON blob into entries. Public so tests can drive it with a
/// fixture rather than the bundled file.
pub fn parse_catalog(json: &str) -> anyhow::Result<Vec<CatalogEntry>> {
    let file: CatalogFile = serde_json::from_str(json)?;
    if file.catalog_version > 1 {
        anyhow::bail!(
            "model catalog version {} is newer than this app supports",
            file.catalog_version
        );
    }
    Ok(file.models)
}

/// The bundled catalog (compiled in). Panics only on a corrupt bundled file,
/// which is a build-time error, not a runtime one.
pub fn bundled_catalog() -> Vec<CatalogEntry> {
    parse_catalog(CATALOG_JSON).expect("bundled catalog.json must be valid")
}

/// The bundled catalog annotated with each entry's fit against `profile`, for
/// the onboarding picker. Pure over `(catalog, profile)`.
pub fn catalog_for(profile: &HardwareProfile) -> Vec<CatalogEntryView> {
    view_catalog(bundled_catalog(), profile)
}

fn view_catalog(entries: Vec<CatalogEntry>, profile: &HardwareProfile) -> Vec<CatalogEntryView> {
    entries
        .into_iter()
        .map(|entry| {
            let fit = fits(entry.size_bytes, profile);
            let installable = entry.is_curated();
            CatalogEntryView { entry, fit, installable }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const GB: u64 = 1024 * 1024 * 1024;

    fn profile(ram_gb: u64) -> HardwareProfile {
        HardwareProfile {
            total_ram_bytes: ram_gb * GB,
            cpu_cores: 8,
            os: "macos".into(),
            arch: "aarch64".into(),
            ..Default::default()
        }
    }

    #[test]
    fn bundled_catalog_parses_and_every_entry_pins_a_url() {
        let cat = bundled_catalog();
        assert!(!cat.is_empty(), "the bundled catalog ships some models");
        for e in &cat {
            assert!(e.url.starts_with("https://"), "{} must pin an https URL", e.id);
            assert!(e.size_bytes > 0, "{} must declare a size", e.id);
        }
    }

    #[test]
    fn view_filters_by_fit_against_the_machine() {
        let cat = bundled_catalog();
        // On a tiny 4 GB machine, the 14B (~9 GB) is TooLarge; the 0.5B fits.
        let views = view_catalog(cat.clone(), &profile(4));
        let by_id = |id: &str| views.iter().find(|v| v.entry.id == id).unwrap().fit;
        assert_eq!(by_id("qwen2.5-14b-instruct-q4"), Fit::TooLarge);
        assert_eq!(by_id("qwen2.5-0.5b-instruct-q4"), Fit::Fits);
        // On a 64 GB machine, the 14B fits comfortably.
        let big = view_catalog(cat, &profile(64));
        assert_eq!(big.iter().find(|v| v.entry.id == "qwen2.5-14b-instruct-q4").unwrap().fit, Fit::Fits);
    }

    #[test]
    fn placeholder_sha256_is_not_installable_curated_is() {
        // The bundled catalog ships with TODO-CURATE placeholders → not installable.
        for v in catalog_for(&profile(64)) {
            assert!(!v.installable, "{} ships a placeholder sha256", v.entry.id);
        }
        // A real 64-hex sha256 is installable.
        let real = parse_catalog(
            r#"{"catalog_version":1,"models":[{"id":"x","name":"X","url":"https://h/x.gguf",
                "sha256":"e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
                "size_bytes":100}]}"#,
        )
        .unwrap();
        assert!(real[0].is_curated());
    }

    #[test]
    fn parse_rejects_a_future_catalog_version() {
        assert!(parse_catalog(r#"{"catalog_version":99,"models":[]}"#).is_err());
    }
}
