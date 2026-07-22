//! Wave 5.3 / M8 — the interactive **hardware calculator** (the heart of the
//! 2026-07-22 product redirect: not a curated catalog, a calculator).
//!
//! For a model + a chosen quant, given THIS machine's [`HardwareProfile`], it
//! computes — live, as the user changes knobs — whether the model fits and
//! roughly how fast it will run (tokens/sec), as a function of:
//!   - **weight quant** (passed as the exact selected-file byte size — no
//!     estimate; it's the real artifact size from HuggingFace),
//!   - **KV-cache quant** ([`KvCacheQuant`] — `f16`/`q8_0`/`q4_0`, llama.cpp's
//!     `--cache-type-k/v`),
//!   - **context size** (the user-chosen window — the KV cache scales linearly
//!     with it, so this is the lever that makes a model flip from fitting to
//!     not).
//!
//! This is a **pure** engine (no I/O): the search + GGUF-metadata layers feed it
//! a [`ModelSpec`]; it does arithmetic. Every heuristic constant is an ESTIMATE,
//! flagged as such. It honours the house rules: **fail closed** (unknown RAM →
//! `TooLarge`, never "fits"), and **honest Unknown** (no memory-bandwidth
//! reading → `predicted_tokens_per_sec: None` + a plain note, never a fabricated
//! speed). See `docs/plans/2026-07-18-m8-model-lifecycle-design.md` → "REVISION
//! 2026-07-22b" §3.

use serde::{Deserialize, Serialize};

use crate::models::hardware::{gpu_enumeration_known, Fit, HardwareProfile, COMFORTABLE_FRACTION};

/// KV-cache element quantization — llama.cpp's `--cache-type-k` / `--cache-type-v`.
/// Halving this (f16 → q8_0 → q4_0) is a real lever the calculator surfaces:
/// at long context the KV cache can rival the weights, and a lighter cache buys
/// back memory (at some quality cost the UI should mention).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KvCacheQuant {
    F16,
    Q8_0,
    Q4_0,
}

impl KvCacheQuant {
    /// Bytes per stored KV element. `f16` = 2 bytes exactly; `q8_0`/`q4_0` are
    /// the practical llama.cpp cache-type sizes (~1 and ~0.5 bytes/elem incl.
    /// their small block scales — an ESTIMATE, close enough for sizing).
    pub fn bytes_per_elem(self) -> f64 {
        match self {
            KvCacheQuant::F16 => 2.0,
            KvCacheQuant::Q8_0 => 1.0,
            KvCacheQuant::Q4_0 => 0.5,
        }
    }
}

/// A model's architecture facts, as read from the GGUF header (exact) or the HF
/// repo summary (coarser — then `kv_exact` is false and the KV figure is
/// flagged approximate). See the GGUF-metadata reader (S2′).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelSpec {
    pub architecture: String,
    pub total_params_b: f64,
    /// For a dense model == `total_params_b`; for a MoE, the (smaller) params
    /// actually computed per token — drives the speed estimate.
    pub active_params_b: f64,
    pub n_layers: u32,
    /// Number of **KV** heads (GQA: usually ≪ attention heads — this is why KV
    /// cache is far smaller than a naïve n_heads×head_dim would suggest).
    pub n_kv_heads: u32,
    pub head_dim: u32,
    /// The model's advertised native context ceiling (a longer request window
    /// is flagged as extended/YaRN territory in the output notes).
    pub native_context_len: u32,
    /// True when `n_layers`/`n_kv_heads`/`head_dim` came from the GGUF file
    /// header (exact); false when derived/estimated from the repo summary.
    pub kv_exact: bool,
}

/// The user-chosen knobs for one calculation.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CalcInput {
    /// EXACT byte size of the selected quant's GGUF file (HF `lfs.size`) — not
    /// an estimate.
    pub weight_file_bytes: u64,
    pub kv_quant: KvCacheQuant,
    /// The context window the user wants to run at.
    pub context_len: u32,
}

/// A coarse speed bucket for the "why" copy. The raw tok/s figure is a
/// bandwidth roofline (upper bound); never surface it as a promise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpeedTier {
    Fast,
    Usable,
    Slow,
    /// No bandwidth reading for this hardware — speed genuinely unknown.
    Unknown,
}

/// Which memory pool the model has to fit in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PoolKind {
    /// Apple-Silicon: RAM and GPU share one pool.
    UnifiedMemory,
    /// A discrete GPU's dedicated VRAM.
    DiscreteVram,
    /// No usable GPU pool — sized against system RAM (CPU inference).
    CpuRam,
}

/// The full result — everything the interactive UI renders.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CalcOutput {
    pub weights_bytes: u64,
    pub kv_cache_bytes: u64,
    pub overhead_bytes: u64,
    pub total_required_bytes: u64,
    /// Usable memory the model is measured against (pool minus OS reserve).
    pub available_bytes: u64,
    pub pool_kind: PoolKind,
    pub fit: Fit,
    /// All layers fit in the fast memory pool (LM Studio's "Full GPU Offload
    /// Possible"). On unified memory this ≈ "fits with the OS's share left over".
    pub full_gpu_offload: bool,
    /// Bandwidth-roofline decode estimate. `None` when bandwidth is unknown —
    /// never a fabricated number.
    pub predicted_tokens_per_sec: Option<f64>,
    pub speed_tier: SpeedTier,
    /// Plain-language caveats to show verbatim (approximate KV, roofline speed,
    /// extended context, tight fit, …).
    pub notes: Vec<String>,
}

// ---- Estimated constants (all ESTIMATE — tune with real measurements) ----

/// Reserve for macOS + other apps sharing the UNIFIED pool. ESTIMATE.
const OS_RESERVE_UNIFIED_BYTES: u64 = 3 * GB;
/// Reserve for the compositor / other clients on a discrete GPU. ESTIMATE.
const OS_RESERVE_VRAM_BYTES: u64 = 768 * MB;
/// Reserve for the OS + background apps on the CPU-RAM path. ESTIMATE.
const OS_RESERVE_CPU_BYTES: u64 = 2 * GB;
/// A modest fixed compute/activation-buffer allowance on top of weights + KV.
/// llama.cpp's compute buffers are context/batch-driven, not weight-driven; a
/// flat allowance is a deliberate, conservative simplification. ESTIMATE.
const COMPUTE_OVERHEAD_BYTES: u64 = 512 * MB;

/// tok/s at/above which decode feels snappy. ESTIMATE (roofline units).
const SPEED_FAST_TOK_S: f64 = 20.0;
/// tok/s at/above which decode is usable-with-latency. ESTIMATE.
const SPEED_USABLE_TOK_S: f64 = 5.0;

const MB: u64 = 1024 * 1024;
const GB: u64 = 1024 * 1024 * 1024;

/// The one impure-free entry point: pure arithmetic over `(hardware, model,
/// input)`. No I/O, fully unit-testable with fixtures.
pub fn calculate(hw: &HardwareProfile, model: &ModelSpec, input: &CalcInput) -> CalcOutput {
    let (pool_bytes, pool_kind) = memory_pool(hw);
    let os_reserve = match pool_kind {
        PoolKind::UnifiedMemory => OS_RESERVE_UNIFIED_BYTES,
        PoolKind::DiscreteVram => OS_RESERVE_VRAM_BYTES,
        PoolKind::CpuRam => OS_RESERVE_CPU_BYTES,
    };
    let available_bytes = pool_bytes.saturating_sub(os_reserve);

    let weights_bytes = input.weight_file_bytes;
    let kv_cache_bytes = kv_cache_bytes(model, input.kv_quant, input.context_len);
    let overhead_bytes = COMPUTE_OVERHEAD_BYTES;
    let total_required_bytes = weights_bytes
        .saturating_add(kv_cache_bytes)
        .saturating_add(overhead_bytes);

    let fit = fit_in(total_required_bytes, available_bytes);
    let full_gpu_offload = available_bytes > 0 && total_required_bytes <= available_bytes;

    // Speed: bandwidth-bound decode roofline. Each token streams the ACTIVE
    // weights (all of them for a dense model; only the active experts for a
    // MoE) plus a read over the KV cache (worst case: context full).
    let active_fraction = if model.total_params_b > 0.0 {
        (model.active_params_b / model.total_params_b).clamp(0.0, 1.0)
    } else {
        1.0
    };
    let predicted_tokens_per_sec = hw.mem_bandwidth_gbps.filter(|g| *g > 0.0).and_then(|gbps| {
        let bytes_per_token =
            weights_bytes as f64 * active_fraction + kv_cache_bytes as f64;
        if bytes_per_token > 0.0 {
            Some(gbps * 1e9 / bytes_per_token)
        } else {
            None
        }
    });
    let speed_tier = match predicted_tokens_per_sec {
        None => SpeedTier::Unknown,
        Some(t) if t >= SPEED_FAST_TOK_S => SpeedTier::Fast,
        Some(t) if t >= SPEED_USABLE_TOK_S => SpeedTier::Usable,
        Some(_) => SpeedTier::Slow,
    };

    let mut notes = Vec::new();
    match fit {
        Fit::TooLarge => notes.push(
            "Too large for this machine at these settings — try a smaller quant, a shorter \
             context, or a lighter KV-cache type. If nothing fits, connect an external \
             OpenAI-compatible endpoint (e.g. LM Studio on another machine) instead."
                .to_string(),
        ),
        Fit::Tight => notes.push(
            "Fits, but it's tight — it will use most of your usable memory; close other heavy \
             apps for the best results."
                .to_string(),
        ),
        Fit::Fits => {}
    }
    if predicted_tokens_per_sec.is_none() {
        notes.push(
            "Speed estimate unavailable (no memory-bandwidth reading for this hardware) — this \
             is based on memory fit only."
                .to_string(),
        );
    } else {
        notes.push(
            "Speed is an approximate upper bound (memory-bandwidth roofline); real throughput \
             is typically lower."
                .to_string(),
        );
    }
    if !model.kv_exact {
        notes.push(
            "KV-cache size is approximate (model architecture read from the repo summary, not \
             the file header)."
                .to_string(),
        );
    }
    if model.native_context_len > 0 && input.context_len > model.native_context_len {
        notes.push(format!(
            "Requested context ({}) exceeds the model's native window ({}) — needs context \
             extension (e.g. YaRN) and may degrade quality.",
            input.context_len, model.native_context_len
        ));
    }
    if pool_kind == PoolKind::CpuRam {
        // Honesty split (gpu_enumeration_known exists precisely for this): an
        // un-probed machine (`gpus: None`) must NOT be told "no GPU" as fact —
        // only a machine we actually enumerated and found nothing usable on.
        if gpu_enumeration_known(hw) {
            notes.push(
                "No usable GPU was detected on this machine — sizing assumes CPU/system-RAM \
                 inference."
                    .to_string(),
            );
        } else {
            notes.push(
                "GPU detection didn't run on this machine — sizing conservatively assumes \
                 CPU/system-RAM inference."
                    .to_string(),
            );
        }
    }

    CalcOutput {
        weights_bytes,
        kv_cache_bytes,
        overhead_bytes,
        total_required_bytes,
        available_bytes,
        pool_kind,
        fit,
        full_gpu_offload,
        predicted_tokens_per_sec,
        speed_tier,
        notes,
    }
}

/// KV-cache bytes for `context_len` tokens: `2 (K and V) · n_layers ·
/// n_kv_heads · head_dim · context · bytes_per_elem`. GQA is captured exactly
/// via `n_kv_heads`. Sized for the FULL requested window (worst case), so the
/// number is conservative rather than optimistic.
pub fn kv_cache_bytes(model: &ModelSpec, kv_quant: KvCacheQuant, context_len: u32) -> u64 {
    let elems = 2.0
        * model.n_layers as f64
        * model.n_kv_heads as f64
        * model.head_dim as f64
        * context_len as f64;
    (elems * kv_quant.bytes_per_elem()) as u64
}

/// The memory pool this machine offers the model, and its kind. Reads the
/// Probe-v2 fields: unified (Apple) → the whole RAM pool; a discrete GPU with
/// known VRAM → that VRAM; otherwise system RAM (CPU path). An unknown GPU is
/// NOT treated as "no GPU" (see [`gpu_enumeration_known`]) — but with no VRAM
/// number to size against it degrades to the RAM pool either way, which is the
/// conservative choice (never invents a phantom VRAM pool).
fn memory_pool(hw: &HardwareProfile) -> (u64, PoolKind) {
    if hw.unified_memory {
        return (hw.total_ram_bytes, PoolKind::UnifiedMemory);
    }
    if let Some(vram) = primary_discrete_vram(hw) {
        return (vram, PoolKind::DiscreteVram);
    }
    (hw.total_ram_bytes, PoolKind::CpuRam)
}

/// The largest known dedicated-VRAM figure across enumerated GPUs, if any.
/// `None` when enumeration didn't run, found no discrete card, or reported no
/// VRAM number — all of which correctly fall through to the RAM pool.
fn primary_discrete_vram(hw: &HardwareProfile) -> Option<u64> {
    hw.gpus
        .as_ref()?
        .iter()
        .filter(|g| !g.is_unified)
        .filter_map(|g| g.vram_bytes)
        .max()
}

/// Three-way fit of `required` against `available`, mirroring `hardware::fits`'s
/// shape (reusing its comfort fraction). Fails closed: `available == 0` (unknown
/// RAM / probe failure) → `TooLarge`, never "fits".
fn fit_in(required: u64, available: u64) -> Fit {
    if available == 0 {
        return Fit::TooLarge;
    }
    let required = required as f64;
    let available = available as f64;
    if required <= available * COMFORTABLE_FRACTION {
        Fit::Fits
    } else if required < available {
        // `< available` (not `<=`) matches hardware::fits exactly: consuming
        // 100% of usable memory with zero slack is TooLarge, not Tight.
        Fit::Tight
    } else {
        Fit::TooLarge
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::hardware::{AppleChipFamily, GpuInfo};

    fn mac(ram_gb: u64, bw: Option<f64>) -> HardwareProfile {
        HardwareProfile {
            total_ram_bytes: ram_gb * GB,
            cpu_cores: 12,
            os: "macos".into(),
            arch: "aarch64".into(),
            apple_chip_family: Some(AppleChipFamily::M3Max),
            unified_memory: true,
            mem_bandwidth_gbps: bw,
            gpus: Some(vec![GpuInfo {
                name: "Apple M3 Max".into(),
                is_unified: true,
                vram_bytes: None,
                core_count: Some(30),
            }]),
            ..Default::default()
        }
    }

    // A dense 8B-ish model: 32 layers, GQA 8 KV heads, head_dim 128, 32k native.
    fn dense_8b(weight_gb: f64) -> (ModelSpec, u64) {
        (
            ModelSpec {
                architecture: "qwen3".into(),
                total_params_b: 8.2,
                active_params_b: 8.2,
                n_layers: 32,
                n_kv_heads: 8,
                head_dim: 128,
                native_context_len: 32768,
                kv_exact: true,
            },
            (weight_gb * GB as f64) as u64,
        )
    }

    // A MoE 30B/3B: same footprint class as a 30B dense, but ~3B active.
    fn moe_30b_a3b(weight_gb: f64) -> (ModelSpec, u64) {
        (
            ModelSpec {
                architecture: "qwen3moe".into(),
                total_params_b: 30.5,
                active_params_b: 3.3,
                n_layers: 48,
                n_kv_heads: 4,
                head_dim: 128,
                native_context_len: 32768,
                kv_exact: true,
            },
            (weight_gb * GB as f64) as u64,
        )
    }

    #[test]
    fn kv_scales_linearly_with_context_and_halves_with_lighter_cache() {
        let (m, _) = dense_8b(5.0);
        let at4k = kv_cache_bytes(&m, KvCacheQuant::F16, 4096);
        let at8k = kv_cache_bytes(&m, KvCacheQuant::F16, 8192);
        assert_eq!(at8k, at4k * 2, "KV is linear in context");
        let q8 = kv_cache_bytes(&m, KvCacheQuant::Q8_0, 4096);
        let q4 = kv_cache_bytes(&m, KvCacheQuant::Q4_0, 4096);
        assert_eq!(q8, at4k / 2, "q8_0 halves f16 KV");
        assert_eq!(q4, at4k / 4, "q4_0 quarters f16 KV");
        // Sanity on the absolute figure: 2*32*8*128*4096*2 bytes = 512 MiB.
        assert_eq!(at4k, 512 * MB);
    }

    #[test]
    fn a_comfortable_model_fits_and_full_offloads() {
        let hw = mac(36, Some(300.0));
        let (m, wbytes) = dense_8b(5.0);
        let out = calculate(&hw, &m, &CalcInput { weight_file_bytes: wbytes, kv_quant: KvCacheQuant::F16, context_len: 8192 });
        assert_eq!(out.fit, Fit::Fits);
        assert!(out.full_gpu_offload);
        assert_eq!(out.pool_kind, PoolKind::UnifiedMemory);
    }

    #[test]
    fn growing_context_flips_fit_from_fits_to_too_large() {
        // Small pool + a model whose KV balloons with context.
        let hw = mac(8, Some(100.0));
        let (m, wbytes) = dense_8b(5.0); // 5 GB weights on ~8 GB machine
        let short = calculate(&hw, &m, &CalcInput { weight_file_bytes: wbytes, kv_quant: KvCacheQuant::F16, context_len: 4096 });
        let long = calculate(&hw, &m, &CalcInput { weight_file_bytes: wbytes, kv_quant: KvCacheQuant::F16, context_len: 131072 });
        assert!(long.kv_cache_bytes > short.kv_cache_bytes);
        assert_eq!(long.fit, Fit::TooLarge, "a huge context must push it over");
        assert!(long.notes.iter().any(|n| n.contains("Too large")));
    }

    #[test]
    fn lighter_kv_cache_can_rescue_a_tight_fit() {
        let hw = mac(16, Some(200.0));
        // Pick a weight size where f16 KV at long context is over the comfort
        // line but q4_0 KV brings it back.
        let (m, wbytes) = dense_8b(9.0);
        let heavy = calculate(&hw, &m, &CalcInput { weight_file_bytes: wbytes, kv_quant: KvCacheQuant::F16, context_len: 131072 });
        let light = calculate(&hw, &m, &CalcInput { weight_file_bytes: wbytes, kv_quant: KvCacheQuant::Q4_0, context_len: 131072 });
        assert!(light.kv_cache_bytes < heavy.kv_cache_bytes);
        assert!(light.total_required_bytes < heavy.total_required_bytes);
    }

    #[test]
    fn moe_is_predicted_much_faster_than_a_same_size_dense() {
        let hw = mac(128, Some(400.0));
        let (moe, moe_w) = moe_30b_a3b(18.0);
        let dense_same = ModelSpec { active_params_b: 30.5, ..moe.clone() }; // force dense-equivalent active
        let a = calculate(&hw, &moe, &CalcInput { weight_file_bytes: moe_w, kv_quant: KvCacheQuant::F16, context_len: 8192 });
        let b = calculate(&hw, &dense_same, &CalcInput { weight_file_bytes: moe_w, kv_quant: KvCacheQuant::F16, context_len: 8192 });
        let (ta, tb) = (a.predicted_tokens_per_sec.unwrap(), b.predicted_tokens_per_sec.unwrap());
        assert!(ta > tb * 3.0, "MoE active-fraction should make it multiples faster ({ta} vs {tb})");
        assert_eq!(a.speed_tier, SpeedTier::Fast);
    }

    #[test]
    fn unknown_bandwidth_yields_no_speed_number_but_still_sizes() {
        let hw = mac(36, None); // bandwidth unknown
        let (m, wbytes) = dense_8b(5.0);
        let out = calculate(&hw, &m, &CalcInput { weight_file_bytes: wbytes, kv_quant: KvCacheQuant::F16, context_len: 8192 });
        assert!(out.predicted_tokens_per_sec.is_none());
        assert_eq!(out.speed_tier, SpeedTier::Unknown);
        assert_eq!(out.fit, Fit::Fits, "still sizes on memory");
        assert!(out.notes.iter().any(|n| n.to_lowercase().contains("memory fit only")));
    }

    #[test]
    fn fails_closed_on_unknown_ram() {
        let mut hw = mac(0, Some(300.0)); // total_ram 0 = probe failure
        hw.total_ram_bytes = 0;
        let (m, wbytes) = dense_8b(2.0);
        let out = calculate(&hw, &m, &CalcInput { weight_file_bytes: wbytes, kv_quant: KvCacheQuant::F16, context_len: 4096 });
        assert_eq!(out.fit, Fit::TooLarge, "unknown RAM never claims a fit");
        assert!(!out.full_gpu_offload);
    }

    #[test]
    fn approximate_kv_and_extended_context_are_flagged() {
        let hw = mac(64, Some(400.0));
        let mut m = dense_8b(5.0).0;
        m.kv_exact = false;
        let wbytes = (5.0 * GB as f64) as u64;
        let out = calculate(&hw, &m, &CalcInput { weight_file_bytes: wbytes, kv_quant: KvCacheQuant::F16, context_len: 131072 });
        assert!(out.notes.iter().any(|n| n.contains("approximate")));
        assert!(out.notes.iter().any(|n| n.contains("exceeds the model's native window")));
    }

    #[test]
    fn unprobed_gpu_is_not_reported_as_no_gpu() {
        // A non-macOS machine (gpus: None = detection didn't run) must NOT be
        // told "No GPU was detected" — that's the honesty invariant
        // gpu_enumeration_known protects.
        let hw = HardwareProfile {
            total_ram_bytes: 32 * GB,
            cpu_cores: 16,
            os: "windows".into(),
            arch: "x86_64".into(),
            unified_memory: false,
            mem_bandwidth_gbps: None,
            gpus: None, // not probed
            ..Default::default()
        };
        let (m, wbytes) = dense_8b(5.0);
        let out = calculate(&hw, &m, &CalcInput { weight_file_bytes: wbytes, kv_quant: KvCacheQuant::F16, context_len: 4096 });
        assert_eq!(out.pool_kind, PoolKind::CpuRam);
        assert!(out.notes.iter().any(|n| n.contains("didn't run")));
        assert!(!out.notes.iter().any(|n| n.contains("No usable GPU was detected")));

        // Confirmed-empty (Some(vec![])) IS allowed to say no GPU was found.
        let mut hw2 = hw.clone();
        hw2.gpus = Some(vec![]);
        let out2 = calculate(&hw2, &m, &CalcInput { weight_file_bytes: wbytes, kv_quant: KvCacheQuant::F16, context_len: 4096 });
        assert!(out2.notes.iter().any(|n| n.contains("No usable GPU was detected")));
    }

    #[test]
    fn non_positive_bandwidth_yields_no_fabricated_speed() {
        let hw = mac(36, Some(0.0)); // a bogus/zero reading must not fabricate a number
        let (m, wbytes) = dense_8b(5.0);
        let out = calculate(&hw, &m, &CalcInput { weight_file_bytes: wbytes, kv_quant: KvCacheQuant::F16, context_len: 4096 });
        assert!(out.predicted_tokens_per_sec.is_none(), "zero bandwidth → Unknown, not Some(0.0)");
        assert_eq!(out.speed_tier, SpeedTier::Unknown);
    }

    #[test]
    fn discrete_vram_pool_is_used_when_present() {
        let hw = HardwareProfile {
            total_ram_bytes: 64 * GB,
            cpu_cores: 16,
            os: "windows".into(),
            arch: "x86_64".into(),
            unified_memory: false,
            mem_bandwidth_gbps: None,
            gpus: Some(vec![GpuInfo {
                name: "NVIDIA RTX 4090".into(),
                is_unified: false,
                vram_bytes: Some(24 * GB),
                core_count: None,
            }]),
            ..Default::default()
        };
        let (m, wbytes) = dense_8b(5.0);
        let out = calculate(&hw, &m, &CalcInput { weight_file_bytes: wbytes, kv_quant: KvCacheQuant::F16, context_len: 8192 });
        assert_eq!(out.pool_kind, PoolKind::DiscreteVram);
        // available = 24 GB VRAM - 0.75 GB reserve; a 5 GB model fits.
        assert_eq!(out.fit, Fit::Fits);
    }
}
