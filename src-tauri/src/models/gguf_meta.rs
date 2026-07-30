//! Wave 5.3 / M8 (REVISION 2026-07-22b) — the GGUF metadata reader that supplies
//! the calculator's model inputs ([`crate::models::calculator::ModelSpec`]).
//!
//! The calculator's KV-cache term needs architecture params the model file
//! carries (`block_count`, `head_count_kv`, `head_dim`, `context_length`). Two
//! tiers, honest-fallback (the house rule: never a silent guess):
//!
//!   1. **Exact header read** — GGUF stores its metadata KV block at the FRONT
//!      of the file, before the tensor data, so a **ranged `GET` of the first
//!      few MB** yields the full header without downloading the weights (exactly
//!      how LM Studio and the online GGUF VRAM calculators work). We parse the
//!      keys we need and produce an EXACT [`ModelSpec`] (`kv_exact: true`).
//!   2. **Cheap repo summary** — `GET /api/models/{id}?blobs=false` returns a
//!      `gguf` object (`architecture`/`context_length`/`total` param count).
//!      Enough for weights + native-context sizing, but it LACKS the layer/head
//!      geometry, so if the header read fails we fall back to this plus a
//!      **documented, conservative geometry estimate** and mark the KV figure
//!      APPROXIMATE (`kv_exact: false`) so the UI can say so. If we have neither
//!      the geometry NOR a parameter count, we **refuse loudly** rather than
//!      invent a model.
//!
//! Every HF URL runs through [`crate::models::download::host_allowed`] first
//! (SSRF/allowlist discipline unchanged). The binary parser + the merge logic
//! are **pure** and fixture-tested; only the two fetch functions do I/O.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::models::calculator::ModelSpec;
use crate::models::download::host_allowed;

/// How many bytes of the GGUF file to range-GET for the header. The metadata KV
/// block (incl. the architecture keys we need) precedes the tensor data and the
/// large tokenizer arrays; a few MB comfortably covers the architecture keys for
/// real models. Capped so a server that ignores `Range` can't make us download
/// the whole weights file.
const HEADER_READ_BYTES: u64 = 4 * 1024 * 1024;

const GGUF_MAGIC: &[u8; 4] = b"GGUF";

// GGUF metadata value types.
const T_UINT8: u32 = 0;
const T_INT8: u32 = 1;
const T_UINT16: u32 = 2;
const T_INT16: u32 = 3;
const T_UINT32: u32 = 4;
const T_INT32: u32 = 5;
const T_FLOAT32: u32 = 6;
const T_BOOL: u32 = 7;
const T_STRING: u32 = 8;
const T_ARRAY: u32 = 9;
const T_UINT64: u32 = 10;
const T_INT64: u32 = 11;
const T_FLOAT64: u32 = 12;

/// The subset of GGUF header metadata the calculator needs. Every field is
/// `Option` — a key absent from the (possibly truncated) ranged read is `None`,
/// never a fabricated value.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GgufHeaderMeta {
    pub architecture: Option<String>,
    pub block_count: Option<u32>,
    pub head_count: Option<u32>,
    pub head_count_kv: Option<u32>,
    pub key_length: Option<u32>,
    pub embedding_length: Option<u32>,
    pub context_length: Option<u32>,
    pub parameter_count: Option<u64>,
    pub expert_count: Option<u32>,
    pub expert_used_count: Option<u32>,
}

impl GgufHeaderMeta {
    /// The exact KV geometry `(n_layers, n_kv_heads, head_dim)` when the header
    /// carries enough to size the KV cache EXACTLY: layer count, a KV-head
    /// count (GQA `head_count_kv`, or `head_count` for MHA), and a head
    /// dimension (explicit `key_length`, or derived `embedding_length /
    /// head_count`). Every value must be **> 0** — a zero (a malformed or
    /// hostile header; the trust-root note puts a compromised repo squarely in
    /// this file's threat model) is NOT exact geometry, it's garbage, and
    /// treating it as exact would present a fabricated zero-KV figure as fact.
    /// A derived head_dim that truncates to 0 fails the same way.
    fn exact_geometry(&self) -> Option<(u32, u32, u32)> {
        let n_layers = self.block_count.filter(|v| *v > 0)?;
        let n_kv_heads = self.head_count_kv.or(self.head_count).filter(|v| *v > 0)?;
        let head_dim = match self.key_length.filter(|v| *v > 0) {
            Some(k) => k,
            None => {
                let d = self.embedding_length.filter(|v| *v > 0)?;
                let heads = self.head_count.filter(|v| *v > 0)?;
                let derived = d / heads;
                if derived == 0 {
                    return None; // embedding < heads — nonsense geometry
                }
                derived
            }
        };
        Some((n_layers, n_kv_heads, head_dim))
    }

    /// True when [`Self::exact_geometry`] yields a usable exact geometry.
    pub fn has_exact_geometry(&self) -> bool {
        self.exact_geometry().is_some()
    }
}

/// The cheap repo-summary fields (`/api/models/{id}?blobs=false` → `gguf`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RepoSummary {
    pub architecture: Option<String>,
    pub context_length: Option<u32>,
    /// Total parameter count (the `gguf.total` field).
    pub total_params: Option<u64>,
}

// ---------------------------------------------------------------------------
// Pure binary parser
// ---------------------------------------------------------------------------

/// A bounds-checked little-endian cursor over the ranged-read prefix. Every read
/// returns `None` (rather than panicking) when it would run past the buffer —
/// which is how a truncated header stops parsing gracefully.
struct Cursor<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.p.checked_add(n)?;
        if end > self.b.len() {
            return None;
        }
        let s = &self.b[self.p..end];
        self.p = end;
        Some(s)
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    /// A GGUF string: `u64` length + UTF-8 bytes.
    fn gstr(&mut self) -> Option<String> {
        let len = self.u64()? as usize;
        Some(String::from_utf8_lossy(self.take(len)?).into_owned())
    }
}

/// Byte size of a fixed-width primitive GGUF value type. `None` for
/// variable-width (string/array) or unknown types.
fn prim_size(vtype: u32) -> Option<usize> {
    Some(match vtype {
        T_UINT8 | T_INT8 | T_BOOL => 1,
        T_UINT16 | T_INT16 => 2,
        T_UINT32 | T_INT32 | T_FLOAT32 => 4,
        T_UINT64 | T_INT64 | T_FLOAT64 => 8,
        _ => return None,
    })
}

/// A scalar value we keep (ints widened to `u64`; strings). Floats, bools and
/// arrays are skipped (not needed — the big tokenizer arrays are exactly what we
/// don't want to load).
enum MetaVal {
    U(u64),
    S(String),
}

/// Read one metadata value of `vtype`, advancing the cursor past it. Returns
/// `Some(Some(v))` for a kept scalar, `Some(None)` for a value we skipped over,
/// and `None` when the value runs past the buffer (truncated read → stop).
fn read_value(c: &mut Cursor, vtype: u32) -> Option<Option<MetaVal>> {
    match vtype {
        T_UINT8 => Some(Some(MetaVal::U(c.take(1)?[0] as u64))),
        T_UINT16 => Some(Some(MetaVal::U(
            u16::from_le_bytes(c.take(2)?.try_into().ok()?) as u64,
        ))),
        T_UINT32 => Some(Some(MetaVal::U(c.u32()? as u64))),
        T_UINT64 => Some(Some(MetaVal::U(c.u64()?))),
        // Signed ints: kept as u64 (the keys we actually read are all unsigned;
        // a negative unrelated key is never looked up, so widening is harmless).
        T_INT8 => Some(Some(MetaVal::U(c.take(1)?[0] as u64))),
        T_INT16 => Some(Some(MetaVal::U(
            u16::from_le_bytes(c.take(2)?.try_into().ok()?) as u64,
        ))),
        T_INT32 => Some(Some(MetaVal::U(c.u32()? as u64))),
        T_INT64 => Some(Some(MetaVal::U(c.u64()?))),
        T_STRING => Some(Some(MetaVal::S(c.gstr()?))),
        T_FLOAT32 => {
            c.take(4)?;
            Some(None)
        }
        T_FLOAT64 => {
            c.take(8)?;
            Some(None)
        }
        T_BOOL => {
            c.take(1)?;
            Some(None)
        }
        T_ARRAY => {
            let elem = c.u32()?;
            let count = c.u64()?;
            if count == 0 {
                // An empty array spans zero bytes regardless of its element
                // type — including element types we can't otherwise size
                // (e.g. a nested array). Skipping it keeps later keys readable.
                return Some(None);
            }
            if elem == T_STRING {
                // Walk each string (variable width). Exhaustion → truncated.
                for _ in 0..count {
                    c.gstr()?;
                }
            } else if let Some(sz) = prim_size(elem) {
                let total = (count as usize).checked_mul(sz)?;
                c.take(total)?;
            } else {
                // Nested/unknown array element — can't compute its span, stop.
                return None;
            }
            Some(None)
        }
        _ => None, // unknown value type — can't advance safely
    }
}

/// Parse the GGUF header metadata out of a (possibly truncated) file prefix.
/// Fails loudly only on a bad magic / unsupported container version; a truncated
/// KV block simply yields whatever keys were reachable (the caller decides
/// whether that's enough for an exact spec). Pure.
pub fn parse_gguf_header(bytes: &[u8]) -> anyhow::Result<GgufHeaderMeta> {
    let mut c = Cursor { b: bytes, p: 0 };
    let magic = c
        .take(4)
        .ok_or_else(|| anyhow::anyhow!("buffer too small for GGUF magic"))?;
    if magic != GGUF_MAGIC {
        anyhow::bail!("not a GGUF file (bad magic)");
    }
    let version = c
        .u32()
        .ok_or_else(|| anyhow::anyhow!("truncated before version"))?;
    if !(2..=3).contains(&version) {
        anyhow::bail!("unsupported GGUF version {version} (expected 2 or 3)");
    }
    let _tensor_count = c
        .u64()
        .ok_or_else(|| anyhow::anyhow!("truncated before tensor count"))?;
    let kv_count = c
        .u64()
        .ok_or_else(|| anyhow::anyhow!("truncated before kv count"))?;

    let mut map: HashMap<String, MetaVal> = HashMap::new();
    for _ in 0..kv_count {
        let Some(key) = c.gstr() else { break };
        let Some(vtype) = c.u32() else { break };
        match read_value(&mut c, vtype) {
            Some(Some(v)) => {
                map.insert(key, v);
            }
            Some(None) => {} // skipped (array/float/bool)
            None => break,   // truncated — keep what we have
        }
    }

    let get_u = |k: &str| match map.get(k) {
        Some(MetaVal::U(v)) => Some(*v),
        _ => None,
    };
    let get_u32 = |k: &str| get_u(k).and_then(|v| u32::try_from(v).ok());
    let get_s = |k: &str| match map.get(k) {
        Some(MetaVal::S(s)) => Some(s.clone()),
        _ => None,
    };

    let architecture = get_s("general.architecture");
    // Arch-prefixed keys use the architecture string as their namespace.
    let arch_key = |suffix: &str| architecture.as_ref().map(|a| format!("{a}.{suffix}"));
    let ag_u32 = |suffix: &str| arch_key(suffix).and_then(|k| get_u32(&k));

    Ok(GgufHeaderMeta {
        block_count: ag_u32("block_count"),
        head_count: ag_u32("attention.head_count"),
        head_count_kv: ag_u32("attention.head_count_kv"),
        key_length: ag_u32("attention.key_length"),
        embedding_length: ag_u32("embedding_length"),
        context_length: ag_u32("context_length"),
        expert_count: ag_u32("expert_count"),
        expert_used_count: ag_u32("expert_used_count"),
        parameter_count: get_u("general.parameter_count"),
        architecture,
    })
}

/// Parse the `/api/models/{id}?blobs=false` body into the summary fields we use.
/// Pure. Tolerant of the `gguf` object being absent (all fields → `None`).
pub fn parse_repo_summary(json: &str) -> anyhow::Result<RepoSummary> {
    let v: serde_json::Value = serde_json::from_str(json)?;
    let g = v.get("gguf");
    let architecture = g
        .and_then(|g| g.get("architecture"))
        .and_then(|a| a.as_str())
        .map(|s| s.to_string());
    let context_length = g
        .and_then(|g| g.get("context_length"))
        .and_then(|c| c.as_u64())
        .and_then(|c| u32::try_from(c).ok());
    let total_params = g.and_then(|g| g.get("total")).and_then(|t| t.as_u64());
    Ok(RepoSummary {
        architecture,
        context_length,
        total_params,
    })
}

// ---------------------------------------------------------------------------
// Merge → ModelSpec (pure, honest fallback)
// ---------------------------------------------------------------------------

/// A conservative geometry estimate from a raw parameter count, for the FALLBACK
/// path only (header geometry unavailable). Returns `(n_layers, n_kv_heads,
/// head_dim)`. Deliberately assumes a modern GQA-typical `n_kv_heads` and a
/// 128-wide head — a documented ballpark, clearly flagged approximate by the
/// caller; NEVER presented as exact. Bucketed by well-known dense-model shapes.
fn estimate_geometry(params: u64) -> (u32, u32, u32) {
    let b = params as f64 / 1e9;
    // (n_layers, n_kv_heads, head_dim) — head_dim fixed at 128 (the near-universal
    // modern choice); n_kv_heads ≈ 8 (typical GQA) so we don't wildly over-count.
    let n_layers = if b <= 1.0 {
        24
    } else if b <= 4.0 {
        32
    } else if b <= 9.0 {
        32
    } else if b <= 16.0 {
        40
    } else if b <= 35.0 {
        48
    } else if b <= 80.0 {
        64
    } else {
        80
    };
    (n_layers, 8, 128)
}

/// Build a [`ModelSpec`] from the (optional) exact header and the (optional)
/// cheap summary. Returns the spec plus honest caveat notes. Refuses loudly when
/// neither source gives even a parameter count to work from.
pub fn build_model_spec(
    header: Option<&GgufHeaderMeta>,
    summary: Option<&RepoSummary>,
) -> anyhow::Result<(ModelSpec, Vec<String>)> {
    let mut notes = Vec::new();

    let architecture = header
        .and_then(|h| h.architecture.clone())
        .or_else(|| summary.and_then(|s| s.architecture.clone()))
        .unwrap_or_else(|| "unknown".to_string());

    let params = header
        .and_then(|h| h.parameter_count)
        .or_else(|| summary.and_then(|s| s.total_params));

    let native_context_len = header
        .and_then(|h| h.context_length)
        .or_else(|| summary.and_then(|s| s.context_length))
        .unwrap_or(0);

    // Exact geometry path. `exact_geometry()` guarantees every value is > 0 —
    // a zero-valued (malformed/hostile) header falls through to the estimate/
    // refuse path instead of presenting a fabricated zero-KV figure as exact.
    if let Some(h) = header {
        if let Some((n_layers, n_kv_heads, head_dim)) = h.exact_geometry() {
            let total_params_b = match params {
                Some(p) => p as f64 / 1e9,
                None => {
                    // No parameter count anywhere (`general.parameter_count` is
                    // optional in real GGUFs, and the repo-summary fetch can
                    // independently fail). The KV/weights sizing is still exact
                    // (weights come from the file's byte size, KV from the
                    // geometry) — only the MoE-vs-dense speed split needs
                    // params. 0.0 is the calculator's documented "unknown"
                    // sentinel (its `total_params_b > 0.0` guard then treats
                    // ALL weights as active — the conservative choice), and
                    // this note makes the absence non-silent (honest-Unknown,
                    // never a silently fabricated figure).
                    notes.push(
                        "Parameter count unknown (not in the file header or repo summary) — \
                         the speed estimate conservatively treats all weights as active."
                            .to_string(),
                    );
                    0.0
                }
            };
            // MoE active-parameter data isn't derivable from the header alone:
            // scaling by used/total experts would IGNORE the always-active
            // shared weights and so overestimate speed — the wrong direction.
            // Treat as dense (active == total), CONSERVATIVE for the speed
            // estimate (a real MoE runs faster than predicted). The note
            // carries the expert ratio when the header exposes it, so the UI
            // can say precisely why the real model will beat the estimate.
            if h.expert_count.is_some_and(|e| e > 1) {
                let ratio = match (h.expert_used_count, h.expert_count) {
                    (Some(u), Some(t)) if u > 0 => {
                        format!(" ({u} of {t} experts active per token)")
                    }
                    _ => String::new(),
                };
                notes.push(format!(
                    "Mixture-of-Experts model{ratio}: the speed estimate treats it as dense \
                     (conservative — the real model typically runs faster) unless \
                     active-parameter data is supplied."
                ));
            }
            return Ok((
                ModelSpec {
                    architecture,
                    total_params_b,
                    active_params_b: total_params_b,
                    n_layers,
                    n_kv_heads,
                    head_dim,
                    native_context_len,
                    kv_exact: true,
                },
                notes,
            ));
        }
    }

    // Fallback: no exact geometry. We need at least a parameter count.
    let Some(params) = params else {
        anyhow::bail!(
            "could not read the model's architecture: the GGUF header was unreadable and the \
             repo summary carries no parameter count — cannot size this model"
        );
    };
    let (n_layers, n_kv_heads, head_dim) = estimate_geometry(params);
    let total_params_b = params as f64 / 1e9;
    notes.push(
        "Model architecture was estimated from the parameter count (the exact GGUF header \
         wasn't read) — KV-cache size and fit are APPROXIMATE; the app reads exact geometry \
         when the model is downloaded."
            .to_string(),
    );
    Ok((
        ModelSpec {
            architecture,
            total_params_b,
            active_params_b: total_params_b, // dense assumption (conservative)
            n_layers,
            n_kv_heads,
            head_dim,
            native_context_len,
            kv_exact: false,
        },
        notes,
    ))
}

// ---------------------------------------------------------------------------
// Network I/O (thin; exercised by the live test)
// ---------------------------------------------------------------------------

/// A hardened HTTP client for the metadata reads: bounded wall-clock (a hung
/// request must not wedge the caller) and the shared per-hop redirect
/// allowlist re-check.
fn meta_client() -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent("lost-harness/0.1 (gguf-meta)")
        .timeout(std::time::Duration::from_secs(30))
        .connect_timeout(std::time::Duration::from_secs(10))
        .redirect(crate::models::download::allowlisted_redirect_policy())
        .build()?)
}

/// Fetch the cheap repo summary. Best-effort — an error is propagated so the
/// caller can decide whether the header alone suffices. Refuses a malformed
/// model id before building any URL.
pub async fn fetch_repo_summary(model_id: &str) -> anyhow::Result<RepoSummary> {
    if !crate::models::hf_search::valid_model_id(model_id) {
        anyhow::bail!("malformed model id: {model_id:?}");
    }
    let url = format!("https://huggingface.co/api/models/{model_id}?blobs=false");
    if !host_allowed(&url) {
        anyhow::bail!("refusing to fetch a non-allowlisted host: {url}");
    }
    let client = meta_client()?;
    let body = client
        .get(&url)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    parse_repo_summary(&body)
}

/// Range-GET the first [`HEADER_READ_BYTES`] of a GGUF file and parse its header.
/// Streams and STOPS at the cap so a server that ignores `Range` (returns `200`)
/// can never make us pull the whole weights file. Host-allowlisted.
pub async fn fetch_gguf_header(gguf_url: &str) -> anyhow::Result<GgufHeaderMeta> {
    use tokio_stream::StreamExt;
    if !host_allowed(gguf_url) {
        anyhow::bail!("refusing to fetch a non-allowlisted host: {gguf_url}");
    }
    let client = meta_client()?;
    let resp = client
        .get(gguf_url)
        .header(
            reqwest::header::RANGE,
            format!("bytes=0-{}", HEADER_READ_BYTES - 1),
        )
        .send()
        .await?
        .error_for_status()?;
    let mut buf: Vec<u8> = Vec::with_capacity(HEADER_READ_BYTES as usize);
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        buf.extend_from_slice(&chunk?);
        if buf.len() as u64 >= HEADER_READ_BYTES {
            break; // cap reached — drop the rest of the connection
        }
    }
    parse_gguf_header(&buf)
}

/// The full read: try the exact header, fall back to the summary, merge into a
/// [`ModelSpec`] + honest notes. The header attempt failing is not fatal (that's
/// the whole point of the fallback) — but if BOTH the header and the summary are
/// unusable, [`build_model_spec`] refuses loudly.
pub async fn read_model_spec(
    model_id: &str,
    gguf_url: &str,
) -> anyhow::Result<(ModelSpec, Vec<String>)> {
    let header = fetch_gguf_header(gguf_url).await.ok();
    let summary = fetch_repo_summary(model_id).await.ok();
    build_model_spec(header.as_ref(), summary.as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- a minimal GGUF header writer, so the parser is tested on real bytes ---

    enum TV<'a> {
        U32(u32),
        U64(u64),
        Str(&'a str),
        StrArray(Vec<&'a str>),
        /// An empty array whose ELEMENT type is itself T_ARRAY — a shape the
        /// parser can't size element-wise but must still skip (zero elements =
        /// zero bytes).
        EmptyNestedArray,
    }

    fn wr_str(out: &mut Vec<u8>, s: &str) {
        out.extend_from_slice(&(s.len() as u64).to_le_bytes());
        out.extend_from_slice(s.as_bytes());
    }

    /// Serialize a valid GGUF (v3) header with the given KV entries.
    fn gguf_bytes(kvs: &[(&str, TV)]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(GGUF_MAGIC);
        out.extend_from_slice(&3u32.to_le_bytes()); // version
        out.extend_from_slice(&0u64.to_le_bytes()); // tensor_count
        out.extend_from_slice(&(kvs.len() as u64).to_le_bytes()); // kv_count
        for (k, v) in kvs {
            wr_str(&mut out, k);
            match v {
                TV::U32(n) => {
                    out.extend_from_slice(&T_UINT32.to_le_bytes());
                    out.extend_from_slice(&n.to_le_bytes());
                }
                TV::U64(n) => {
                    out.extend_from_slice(&T_UINT64.to_le_bytes());
                    out.extend_from_slice(&n.to_le_bytes());
                }
                TV::Str(s) => {
                    out.extend_from_slice(&T_STRING.to_le_bytes());
                    wr_str(&mut out, s);
                }
                TV::StrArray(items) => {
                    out.extend_from_slice(&T_ARRAY.to_le_bytes());
                    out.extend_from_slice(&T_STRING.to_le_bytes());
                    out.extend_from_slice(&(items.len() as u64).to_le_bytes());
                    for it in items {
                        wr_str(&mut out, it);
                    }
                }
                TV::EmptyNestedArray => {
                    out.extend_from_slice(&T_ARRAY.to_le_bytes());
                    out.extend_from_slice(&T_ARRAY.to_le_bytes()); // element type: array
                    out.extend_from_slice(&0u64.to_le_bytes()); // zero elements
                }
            }
        }
        out
    }

    fn full_llama_header() -> Vec<u8> {
        gguf_bytes(&[
            ("general.architecture", TV::Str("llama")),
            ("general.name", TV::Str("Test Llama 8B")),
            ("general.parameter_count", TV::U64(8_000_000_000)),
            ("llama.block_count", TV::U32(32)),
            ("llama.embedding_length", TV::U32(4096)),
            ("llama.attention.head_count", TV::U32(32)),
            ("llama.attention.head_count_kv", TV::U32(8)),
            ("llama.attention.key_length", TV::U32(128)),
            ("llama.context_length", TV::U32(8192)),
            // A big-ish tokenizer array AFTER the keys we need — must be skipped
            // without derailing the (already-collected) architecture keys.
            (
                "tokenizer.ggml.tokens",
                TV::StrArray(vec!["<s>", "</s>", "the", "and", "cat"]),
            ),
        ])
    }

    #[test]
    fn parses_exact_geometry_from_a_real_header_and_skips_arrays() {
        let meta = parse_gguf_header(&full_llama_header()).unwrap();
        assert_eq!(meta.architecture.as_deref(), Some("llama"));
        assert_eq!(meta.block_count, Some(32));
        assert_eq!(meta.head_count, Some(32));
        assert_eq!(meta.head_count_kv, Some(8));
        assert_eq!(meta.key_length, Some(128));
        assert_eq!(meta.embedding_length, Some(4096));
        assert_eq!(meta.context_length, Some(8192));
        assert_eq!(meta.parameter_count, Some(8_000_000_000));
        assert!(meta.has_exact_geometry());
    }

    #[test]
    fn rejects_a_bad_magic_and_bad_version() {
        assert!(parse_gguf_header(b"NOPExxxxxxxx").is_err());
        let mut v = full_llama_header();
        v[4] = 9; // clobber version to 9
        assert!(parse_gguf_header(&v).is_err());
    }

    #[test]
    fn a_truncated_header_keeps_the_keys_it_reached() {
        // Cut the buffer partway through the tokenizer array (after all the
        // architecture keys). The reachable keys must still parse; the parser
        // must not error.
        let full = full_llama_header();
        // Find a cut point comfortably past the arch keys but inside the array.
        let cut = full.len() - 12;
        let meta = parse_gguf_header(&full[..cut]).unwrap();
        assert_eq!(meta.block_count, Some(32), "arch keys survived truncation");
        assert_eq!(meta.head_count_kv, Some(8));
        assert!(meta.has_exact_geometry());
    }

    #[test]
    fn build_spec_exact_uses_header_geometry() {
        let meta = parse_gguf_header(&full_llama_header()).unwrap();
        let (spec, notes) = build_model_spec(Some(&meta), None).unwrap();
        assert!(spec.kv_exact, "header geometry → exact");
        assert_eq!(spec.n_layers, 32);
        assert_eq!(spec.n_kv_heads, 8);
        assert_eq!(spec.head_dim, 128);
        assert_eq!(spec.native_context_len, 8192);
        assert!((spec.total_params_b - 8.0).abs() < 1e-6);
        assert_eq!(
            spec.active_params_b, spec.total_params_b,
            "dense assumption"
        );
        assert!(notes.is_empty());
    }

    #[test]
    fn head_dim_falls_back_to_embedding_over_heads_when_key_length_absent() {
        let bytes = gguf_bytes(&[
            ("general.architecture", TV::Str("qwen3")),
            ("general.parameter_count", TV::U64(4_000_000_000)),
            ("qwen3.block_count", TV::U32(36)),
            ("qwen3.embedding_length", TV::U32(2560)),
            ("qwen3.attention.head_count", TV::U32(20)),
            ("qwen3.attention.head_count_kv", TV::U32(4)),
            ("qwen3.context_length", TV::U32(32768)),
        ]);
        let meta = parse_gguf_header(&bytes).unwrap();
        assert_eq!(meta.key_length, None);
        let (spec, _) = build_model_spec(Some(&meta), None).unwrap();
        assert!(spec.kv_exact);
        assert_eq!(
            spec.head_dim,
            2560 / 20,
            "derived from embedding/head_count"
        );
        assert_eq!(spec.n_kv_heads, 4);
    }

    #[test]
    fn moe_header_notes_conservative_dense_treatment() {
        let bytes = gguf_bytes(&[
            ("general.architecture", TV::Str("qwen3moe")),
            ("general.parameter_count", TV::U64(30_000_000_000)),
            ("qwen3moe.block_count", TV::U32(48)),
            ("qwen3moe.embedding_length", TV::U32(2048)),
            ("qwen3moe.attention.head_count", TV::U32(32)),
            ("qwen3moe.attention.head_count_kv", TV::U32(4)),
            ("qwen3moe.attention.key_length", TV::U32(128)),
            ("qwen3moe.context_length", TV::U32(32768)),
            ("qwen3moe.expert_count", TV::U32(128)),
            ("qwen3moe.expert_used_count", TV::U32(8)),
        ]);
        let meta = parse_gguf_header(&bytes).unwrap();
        assert_eq!(meta.expert_count, Some(128));
        let (spec, notes) = build_model_spec(Some(&meta), None).unwrap();
        assert!(spec.kv_exact);
        assert_eq!(
            spec.active_params_b, spec.total_params_b,
            "conservative dense"
        );
        assert!(notes.iter().any(|n| n.contains("Mixture-of-Experts")));
        // The expert ratio the header exposes is surfaced honestly in the note
        // (never used to scale active params — that would overestimate speed).
        assert!(
            notes.iter().any(|n| n.contains("8 of 128 experts")),
            "expert ratio surfaces in the note: {notes:?}"
        );
    }

    #[test]
    fn zero_valued_geometry_is_not_exact() {
        // A hostile/malformed header with block_count=0 (the trust-root note
        // puts a compromised repo in this file's threat model) must NOT be
        // presented as exact zero-KV geometry — it falls to the estimate path
        // (params present → approximate) instead.
        let bytes = gguf_bytes(&[
            ("general.architecture", TV::Str("llama")),
            ("general.parameter_count", TV::U64(8_000_000_000)),
            ("llama.block_count", TV::U32(0)), // garbage
            ("llama.attention.head_count_kv", TV::U32(8)),
            ("llama.attention.key_length", TV::U32(128)),
            ("llama.context_length", TV::U32(8192)),
        ]);
        let meta = parse_gguf_header(&bytes).unwrap();
        assert!(
            !meta.has_exact_geometry(),
            "zero n_layers is not exact geometry"
        );
        let (spec, notes) = build_model_spec(Some(&meta), None).unwrap();
        assert!(
            !spec.kv_exact,
            "degrades to the labeled estimate, never fake-exact"
        );
        assert!(spec.n_layers > 0, "the estimate supplies sane geometry");
        assert!(notes.iter().any(|n| n.contains("APPROXIMATE")));
    }

    #[test]
    fn truncating_head_dim_derivation_is_not_exact() {
        // embedding_length < head_count → integer division would yield
        // head_dim 0; that must disqualify the exact path, not ship a zero.
        let bytes = gguf_bytes(&[
            ("general.architecture", TV::Str("llama")),
            ("general.parameter_count", TV::U64(1_000_000_000)),
            ("llama.block_count", TV::U32(24)),
            ("llama.embedding_length", TV::U32(16)), // absurd: < head_count
            ("llama.attention.head_count", TV::U32(32)),
            ("llama.attention.head_count_kv", TV::U32(8)),
        ]);
        let meta = parse_gguf_header(&bytes).unwrap();
        assert!(
            !meta.has_exact_geometry(),
            "derived head_dim 0 is not exact"
        );
        let (spec, _) = build_model_spec(Some(&meta), None).unwrap();
        assert!(!spec.kv_exact);
        assert!(spec.head_dim > 0);
    }

    #[test]
    fn an_empty_nested_array_does_not_stop_the_parse() {
        // An empty array with an un-sizable element type spans zero bytes —
        // keys after it must still be reachable.
        let bytes = gguf_bytes(&[
            ("general.architecture", TV::Str("llama")),
            ("weird.empty", TV::EmptyNestedArray),
            ("llama.block_count", TV::U32(32)), // AFTER the weird key
            ("llama.attention.head_count_kv", TV::U32(8)),
            ("llama.attention.key_length", TV::U32(128)),
        ]);
        let meta = parse_gguf_header(&bytes).unwrap();
        assert_eq!(
            meta.block_count,
            Some(32),
            "keys after the empty array parse"
        );
        assert!(meta.has_exact_geometry());
    }

    #[test]
    fn unknown_parameter_count_is_noted_never_silent() {
        // Exact geometry but NO parameter count anywhere (header key absent,
        // summary unavailable): the spec is still exact for sizing, but the
        // absence is flagged in a note — never a silently fabricated figure.
        let bytes = gguf_bytes(&[
            ("general.architecture", TV::Str("llama")),
            ("llama.block_count", TV::U32(32)),
            ("llama.attention.head_count", TV::U32(32)),
            ("llama.attention.head_count_kv", TV::U32(8)),
            ("llama.attention.key_length", TV::U32(128)),
            ("llama.context_length", TV::U32(8192)),
        ]);
        let meta = parse_gguf_header(&bytes).unwrap();
        let (spec, notes) = build_model_spec(Some(&meta), None).unwrap();
        assert!(spec.kv_exact, "geometry is still exact");
        assert_eq!(
            spec.total_params_b, 0.0,
            "the calculator's unknown sentinel"
        );
        assert!(
            notes.iter().any(|n| n.contains("Parameter count unknown")),
            "the absence is non-silent: {notes:?}"
        );
    }

    #[test]
    fn build_spec_falls_back_to_summary_with_approximate_flag() {
        // No header; only a summary with a param count → estimated geometry,
        // kv_exact=false, an explicit approximate note.
        let summary = RepoSummary {
            architecture: Some("llama".into()),
            context_length: Some(8192),
            total_params: Some(7_000_000_000),
        };
        let (spec, notes) = build_model_spec(None, Some(&summary)).unwrap();
        assert!(!spec.kv_exact, "no header geometry → approximate");
        assert_eq!(spec.native_context_len, 8192);
        assert!(spec.n_layers > 0 && spec.n_kv_heads > 0 && spec.head_dim > 0);
        assert!(notes.iter().any(|n| n.contains("APPROXIMATE")));
    }

    #[test]
    fn build_spec_refuses_loudly_with_no_geometry_and_no_params() {
        // A header with only an architecture string (no geometry, no params) and
        // no summary → refuse, never invent.
        let bytes = gguf_bytes(&[("general.architecture", TV::Str("llama"))]);
        let meta = parse_gguf_header(&bytes).unwrap();
        let err = build_model_spec(Some(&meta), None).unwrap_err();
        assert!(err.to_string().contains("cannot size this model"));
    }

    #[test]
    fn repo_summary_parses_the_gguf_object() {
        let json = r#"{"id":"Qwen/Qwen3-0.6B-GGUF","gguf":{"architecture":"qwen3",
            "context_length":40960,"total":751000000}}"#;
        let s = parse_repo_summary(json).unwrap();
        assert_eq!(s.architecture.as_deref(), Some("qwen3"));
        assert_eq!(s.context_length, Some(40960));
        assert_eq!(s.total_params, Some(751000000));
        // Missing gguf object → all None, no error.
        let empty = parse_repo_summary(r#"{"id":"x"}"#).unwrap();
        assert_eq!(empty, RepoSummary::default());
    }

    /// Live ranged header read of the tiny model. Opt-in via `LHP_HF_LIVE=1`.
    #[tokio::test]
    async fn live_gguf_header_read() {
        if std::env::var_os("LHP_HF_LIVE").is_none() {
            eprintln!("skipping live GGUF header test — set LHP_HF_LIVE=1 to run");
            return;
        }
        let url = "https://huggingface.co/Qwen/Qwen3-0.6B-GGUF/resolve/main/Qwen3-0.6B-Q8_0.gguf";
        let meta = fetch_gguf_header(url).await.expect("ranged header read");
        assert!(
            meta.architecture.is_some(),
            "real GGUF exposes an architecture"
        );
        assert!(meta.block_count.is_some(), "real GGUF exposes block_count");
        assert!(
            meta.has_exact_geometry(),
            "the header read yields exact geometry"
        );
    }
}
