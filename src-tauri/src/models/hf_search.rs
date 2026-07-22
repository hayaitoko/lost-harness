//! Wave 5.3 / M8 (REVISION 2026-07-22b) — HuggingFace model **search** + the
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
//!   - **tree** `GET /api/models/{id}/tree/main` → the `*.gguf` files, each
//!     carrying its `lfs.oid` (the real sha256, 64-hex) + `lfs.size`
//!     ([`QuantOption`]). **This is where the verified-before-runnable sha256
//!     comes from** — no pre-curation, but also (see the trust-root note below)
//!     no longer an out-of-band trust root.
//!
//! ## Trust-root honesty (2026-07-22c review requirement — LOAD-BEARING)
//!
//! Post-redirect the expected sha256 is self-reported by the same host at the
//! same moment as the bytes (`lfs.oid`). That still catches transport/CDN
//! corruption and partial downloads; it can NOT catch a compromised HF repo.
//! The compensating control lives HERE: [`Provenance`]. Staff-picks / default
//! rows are limited to a **trusted-publisher allowlist**; any other result is
//! labelled [`Provenance::Community`] in the returned data so the UI can render
//! a visible "community model — provenance is the publisher's" warning before
//! download. The repo-trust decision then sits with the user, per model, never
//! silently.
//!
//! The network functions are thin; every parse/classify step is a **pure**
//! helper unit-tested with fixtures (the live endpoints are exercised only by an
//! env-gated, self-skipping integration test, mirroring
//! `live_native_tool_call_roundtrip`).

use std::collections::HashMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::models::download::{allowlisted_redirect_policy, host_allowed, is_real_sha256};

/// Publishers whose GGUF repos we treat as trusted for the Staff-picks default
/// view and for suppressing the community-provenance warning. Two groups:
/// well-known official model orgs, and the trusted community requantizers the
/// design names explicitly (`lmstudio-community`/`ggml-org`/`unsloth`/
/// `bartowski`). Anything not on this list is [`Provenance::Community`] — the
/// conservative default (more warnings, never fewer). Matched case-insensitively
/// against the publisher (the segment before `/` in a repo id).
const TRUSTED_PUBLISHERS: &[&str] = &[
    // Trusted community requantizers (design §22b + 22c note).
    "lmstudio-community",
    "ggml-org",
    "unsloth",
    "bartowski",
    // Well-known official model orgs that publish (or whose GGUFs are mirrored
    // under) these names. Not exhaustive by design — an unlisted publisher is
    // Community, which only ever ADDS a provenance warning.
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

/// How much we vouch for a model's bytes. This is the compensating control for
/// the post-redirect trust-root shift (the sha256 now comes from the same host
/// as the bytes) — see the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    /// Publisher is on the trusted allowlist — eligible for Staff-picks, no
    /// community warning.
    Trusted,
    /// Any other publisher — the UI must show a "community model — provenance is
    /// the publisher's" label before download.
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
    /// Trusted vs community — the compensating trust-root control.
    pub provenance: Provenance,
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
    pub quants: Vec<QuantGroup>,
}

// ---------------------------------------------------------------------------
// Pure helpers (fixture-tested; no I/O)
// ---------------------------------------------------------------------------

/// The publisher segment of a repo id (before the first `/`). Empty if none.
pub fn publisher_of(id: &str) -> &str {
    id.split('/').next().unwrap_or("")
}

/// Classify a publisher against the trusted allowlist (case-insensitive).
pub fn provenance_of(publisher: &str) -> Provenance {
    let p = publisher.trim().to_ascii_lowercase();
    if !p.is_empty() && TRUSTED_PUBLISHERS.iter().any(|t| *t == p) {
        Provenance::Trusted
    } else {
        Provenance::Community
    }
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
        let is_iq = tok.starts_with("IQ")
            && bytes.get(2).is_some_and(|c| c.is_ascii_digit());
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
                        && (1..=declared).all(|i| {
                            files.iter().any(|f| f.part.is_some_and(|p| p.index == i))
                        })
                }
            };
            QuantGroup { quant, total_size_bytes, files, complete }
        })
        .collect()
}

/// Build the pinned resolve URL for a file in a repo (`main` revision).
fn resolve_url(model_id: &str, path: &str) -> String {
    format!("https://huggingface.co/{model_id}/resolve/main/{path}")
}

// --- search results ---

#[derive(Debug, Deserialize)]
struct RawSearchRow {
    #[serde(alias = "modelId")]
    id: String,
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

/// Parse the JSON body of a `/api/models` search response into summaries. Pure.
pub fn parse_search_results(json: &str) -> anyhow::Result<Vec<HfModelSummary>> {
    let rows: Vec<RawSearchRow> = serde_json::from_str(json)?;
    Ok(rows
        .into_iter()
        .map(|r| {
            let publisher = publisher_of(&r.id).to_string();
            let provenance = provenance_of(&publisher);
            HfModelSummary {
                id: r.id,
                publisher,
                downloads: r.downloads,
                likes: r.likes,
                tags: r.tags.unwrap_or_default(),
                provenance,
            }
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

/// Parse a `/api/models/{id}/tree/main` response into the model's downloadable
/// files. Only `*.gguf` LFS files with a usable 64-hex oid and a safe path are
/// surfaced (a GGUF small enough to not be LFS-tracked, or one whose oid isn't
/// a sha256, can't be verify-installed — we drop it rather than offer an
/// un-verifiable download). Pure over `(json, model_id)`; refuses a malformed
/// model id loudly.
pub fn parse_tree(json: &str, model_id: &str) -> anyhow::Result<Vec<QuantOption>> {
    if !valid_model_id(model_id) {
        anyhow::bail!("malformed model id: {model_id:?}");
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
            url: resolve_url(model_id, &e.path),
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
    let body = client.get(url).send().await?.error_for_status()?.text().await?;
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
/// (top by the chosen sort). Network I/O — live-tested only.
pub async fn search(query: &str, sort: SearchSort, limit: u32) -> anyhow::Result<Vec<HfModelSummary>> {
    let client = hf_client()?;
    let url = search_url(query, sort, limit.clamp(1, 100));
    let body = get_allowlisted_text(&client, &url).await?;
    parse_search_results(&body)
}

/// The Staff-picks default view: top trusted-publisher GGUF models by downloads.
/// Filters the live top-N to the trusted allowlist (the 22c requirement — the
/// default rows are trusted-only; arbitrary search surfaces community results
/// with a label).
pub async fn staff_picks(limit: u32) -> anyhow::Result<Vec<HfModelSummary>> {
    // Over-fetch, then keep only trusted publishers, up to `limit`.
    let limit = limit.clamp(1, 25);
    let all = search("", SearchSort::Downloads, limit * 4).await?;
    Ok(all
        .into_iter()
        .filter(|m| m.provenance == Provenance::Trusted)
        .take(limit as usize)
        .collect())
}

/// List a model's downloadable GGUF files (ungrouped). Network I/O.
pub async fn list_quants(model_id: &str) -> anyhow::Result<Vec<QuantOption>> {
    if !valid_model_id(model_id) {
        anyhow::bail!("malformed model id: {model_id:?}");
    }
    let client = hf_client()?;
    let url = format!("https://huggingface.co/api/models/{model_id}/tree/main");
    let body = get_allowlisted_text(&client, &url).await?;
    parse_tree(&body, model_id)
}

/// The full detail view for a model: publisher/provenance + every quant grouped
/// into logical (multi-part-aware) download units.
pub async fn model_detail(model_id: &str) -> anyhow::Result<HfModelDetail> {
    let files = list_quants(model_id).await?;
    let publisher = publisher_of(model_id).to_string();
    let provenance = provenance_of(&publisher);
    Ok(HfModelDetail {
        id: model_id.to_string(),
        publisher,
        provenance,
        quants: group_quants(files),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publisher_and_provenance_classify_trusted_vs_community() {
        assert_eq!(publisher_of("Qwen/Qwen3-0.6B-GGUF"), "Qwen");
        assert_eq!(publisher_of("no-slash-id"), "no-slash-id");
        assert_eq!(publisher_of(""), "");
        // Trusted: official org + community requantizers, case-insensitive.
        assert_eq!(provenance_of("Qwen"), Provenance::Trusted);
        assert_eq!(provenance_of("lmstudio-community"), Provenance::Trusted);
        assert_eq!(provenance_of("BARTOWSKI"), Provenance::Trusted);
        assert_eq!(provenance_of("unsloth"), Provenance::Trusted);
        // Anyone else is community (the conservative default).
        assert_eq!(provenance_of("some-random-user"), Provenance::Community);
        assert_eq!(provenance_of(""), Provenance::Community);
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
            url: resolve_url("org/repo", filename),
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
        let big = groups.iter().find(|g| g.quant.as_deref() == Some("Q4_K_M")).unwrap();
        assert_eq!(big.total_size_bytes, 100, "size is the SUM across parts");
        assert!(big.complete, "1..=2 all present");
        assert_eq!(big.files.len(), 2);
        assert_eq!(big.files[0].part.unwrap().index, 1, "parts sorted by index");
        let small = groups.iter().find(|g| g.quant.as_deref() == Some("Q8_0")).unwrap();
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
        assert!(!groups[0].complete, "a missing part must not be downloadable");
    }

    #[test]
    fn search_results_parse_and_carry_provenance() {
        // Shape verified live in the design doc: id/downloads/likes/tags —
        // including explicit nulls, which real APIs emit.
        let json = r#"[
            {"id":"Qwen/Qwen3-0.6B-GGUF","downloads":123456,"likes":42,"tags":["gguf","conversational"]},
            {"id":"randomuser/mystery-gguf","downloads":null,"likes":null,"tags":null},
            {"modelId":"unsloth/Qwen3-4B-GGUF","tags":["gguf","moe"]}
        ]"#;
        let out = parse_search_results(json).unwrap();
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].id, "Qwen/Qwen3-0.6B-GGUF");
        assert_eq!(out[0].publisher, "Qwen");
        assert_eq!(out[0].provenance, Provenance::Trusted);
        assert_eq!(out[0].downloads, Some(123456));
        // Explicit nulls parse as honest absence — never a fabricated 0.
        assert_eq!(out[1].provenance, Provenance::Community, "unknown publisher → community");
        assert_eq!(out[1].downloads, None);
        assert!(out[1].tags.is_empty());
        // `modelId` alias + missing counts still parse.
        assert_eq!(out[2].id, "unsloth/Qwen3-4B-GGUF");
        assert_eq!(out[2].provenance, Provenance::Trusted);
        assert_eq!(out[2].downloads, None);
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
        let quants = parse_tree(json, "Qwen/Qwen3-0.6B-GGUF").unwrap();
        assert_eq!(quants.len(), 2, "only the two verifiable, safe-path GGUFs");
        let q8 = quants.iter().find(|q| q.quant.as_deref() == Some("Q8_0")).unwrap();
        assert_eq!(q8.size_bytes, 650000000);
        assert_eq!(q8.filename, "Qwen3-0.6B-Q8_0.gguf");
        assert_eq!(
            q8.url,
            "https://huggingface.co/Qwen/Qwen3-0.6B-GGUF/resolve/main/Qwen3-0.6B-Q8_0.gguf"
        );
        assert!(host_allowed(&q8.url), "every surfaced url is host-allowlisted");
        assert_eq!(q8.sha256.len(), 64);
        // A malformed model id refuses loudly before any URL is built.
        assert!(parse_tree(json, "../../evil.com").is_err());
    }

    #[test]
    fn search_url_is_well_formed_and_allowlisted() {
        let u = search_url("qwen 3", SearchSort::Downloads, 20);
        assert!(host_allowed(&u), "constructed search url must be allowlisted");
        assert!(u.contains("filter=gguf"));
        assert!(u.contains("sort=downloads"));
        assert!(u.contains("limit=20"));
        assert!(u.contains("search=qwen"), "query is included + encoded");
        // Empty query (staff picks) omits the search param.
        let empty = search_url("", SearchSort::Trending, 10);
        assert!(!empty.contains("search="));
        assert!(empty.contains("sort=trendingScore"));
    }

    /// Live HF search + tree round-trip. Opt-in — set `LHP_HF_LIVE=1` to run
    /// (self-skips offline / in CI). Mirrors the `LHP_NATIVE_ENDPOINT` pattern.
    #[tokio::test]
    async fn live_hf_search_and_tree() {
        if std::env::var_os("LHP_HF_LIVE").is_none() {
            eprintln!("skipping live HF search test — set LHP_HF_LIVE=1 to run");
            return;
        }
        let results = search("qwen3", SearchSort::Downloads, 10).await.expect("search");
        assert!(!results.is_empty(), "a real search returns rows");
        // Every row's publisher/provenance is derived, and ids are non-empty.
        for r in &results {
            assert!(!r.id.is_empty());
        }
        // The tiny live-test model must list at least one verifiable quant.
        let detail = model_detail("Qwen/Qwen3-0.6B-GGUF").await.expect("detail");
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
