//! Wave 5.3 / M8 — hardware detection for the model-lifecycle onboarding. The
//! curated model catalog is sized to what the machine can actually run.
//!
//! **Probe v2 (2026-07-22)** extends the original RAM+cores probe beyond memory
//! *capacity* into the facts an EDUCATED model decision needs: Apple-Silicon
//! memory *bandwidth* (estimated by chip family — macOS exposes no bandwidth
//! API), GPU enumeration/topology, and a unified-vs-discrete memory flag. See
//! `docs/plans/2026-07-18-m8-model-lifecycle-design.md` → "REVISION 2026-07-22"
//! §A for the design + the §0 shared-type contract the recommendation engine
//! builds against.
//!
//! Honesty is the house rule: every new field is an explicit `Option`/`bool`
//! with a `None`/`false` "we don't know" state — the probe NEVER guesses a
//! number (an unmapped chip → `mem_bandwidth_gbps: None`, not "close enough to
//! the nearest neighbour"). Every bandwidth figure in the lookup table is an
//! ESTIMATE. `fits()` keeps its exact RAM-only signature + math (a profile with
//! every new field `None` yields exactly today's verdict — the conservative
//! default is free).
//!
//! Deliberately conservative + pure where it matters: [`probe`] does the impure
//! syscall/subprocess reads (behind `#[cfg(target_os = "macos")]`, with a
//! non-macOS stub so `--no-default-features` and non-mac CI stay green); the
//! sizing/parsing helpers ([`fits`], [`parse_brand_string`],
//! [`bandwidth_gbps_estimate`]) are pure and unit-testable without real
//! hardware. The [`HardwareSource`] trait lets the recommendation engine + tests
//! inject a synthetic profile.

use serde::{Deserialize, Serialize};

/// A snapshot of what this machine can run. The four original fields keep their
/// exact names/types (read by [`fits`] and every existing fixture) — Probe v2 is
/// a purely ADDITIVE revision, not a rename. `Eq` is intentionally NOT derived
/// (the `f64` bandwidth field precludes it; nothing depends on it), and
/// `Default` IS, so fixtures can spread `..Default::default()`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct HardwareProfile {
    /// Total physical RAM in bytes.
    pub total_ram_bytes: u64,
    /// Logical CPU cores.
    pub cpu_cores: u32,
    /// "macos" | "windows" | "linux" | other (std::env::consts::OS).
    pub os: String,
    /// CPU architecture (e.g. "aarch64", "x86_64").
    pub arch: String,

    // ---- Probe v2, all additive ----
    /// Raw `machdep.cpu.brand_string` (e.g. "Apple M3 Max"). `None` on non-macOS
    /// or if the read failed. Kept even when unparseable, for display/support.
    #[serde(default)]
    pub cpu_brand: Option<String>,
    /// `cpu_brand` parsed into a known Apple-Silicon family. `None` when not
    /// Apple Silicon OR when the brand string is a chip this table doesn't
    /// recognise yet — an unmapped brand NEVER silently matches a neighbour.
    #[serde(default)]
    pub apple_chip_family: Option<AppleChipFamily>,
    /// True iff RAM and GPU share one on-package pool (Apple Silicon). Computed
    /// from `(os, arch)` — no syscall.
    #[serde(default)]
    pub unified_memory: bool,
    /// Estimated unified-memory bandwidth in GB/s, from `apple_chip_family` (+
    /// GPU core count for binned variants). `None` when the family is unknown —
    /// never a guessed number. Every table value is an ESTIMATE.
    #[serde(default)]
    pub mem_bandwidth_gbps: Option<f64>,
    /// GPUs found, best-effort. `None` = enumeration wasn't attempted or failed
    /// — this is NOT "confirmed zero GPUs" (that would be `Some(vec![])`). Use
    /// [`gpu_enumeration_known`] to distinguish the two; nothing may treat
    /// `None` as "definitely no GPU".
    #[serde(default)]
    pub gpus: Option<Vec<GpuInfo>>,
}

/// One enumerated GPU. `core_count`/`vram_bytes` are best-effort — `None` when
/// the OS didn't report them, never a placeholder zero.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct GpuInfo {
    /// e.g. "Apple M3 Max", or "AMD Radeon Pro 5500M".
    pub name: String,
    /// Shares system RAM (Apple Silicon) vs has its own VRAM.
    pub is_unified: bool,
    /// `None` on a unified GPU; best-effort on discrete.
    pub vram_bytes: Option<u64>,
    /// Best-effort Apple-GPU core count, when reported (disambiguates binned
    /// bandwidth).
    pub core_count: Option<u32>,
}

/// Known Apple-Silicon chip families. Deliberately does NOT encode every binned
/// SKU as its own variant (e.g. M4 Max ships in two GPU-core bins at different
/// bandwidths) — binning is resolved by pairing this with `GpuInfo.core_count`
/// at lookup time. No `M4Ultra`: Apple hasn't shipped one as of 2026-07-22; an
/// unrecognised brand string parses to `None`, never a nearest match.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum AppleChipFamily {
    M1,
    M1Pro,
    M1Max,
    M1Ultra,
    M2,
    M2Pro,
    M2Max,
    M2Ultra,
    M3,
    M3Pro,
    M3Max,
    M3Ultra,
    M4,
    M4Pro,
    M4Max,
}

/// Parse a `machdep.cpu.brand_string` into a known Apple-Silicon family. Returns
/// `None` for anything that isn't an exactly-recognised `"Apple M<n>[ Pro|Max|
/// Ultra]"` — an Intel Mac, a future chip, or a garbled string. Pure.
pub fn parse_brand_string(brand: &str) -> Option<AppleChipFamily> {
    use AppleChipFamily::*;
    // Normalise internal whitespace; strip the leading "Apple ".
    let normalised = brand.split_whitespace().collect::<Vec<_>>().join(" ");
    let rest = normalised.strip_prefix("Apple ")?;
    Some(match rest {
        "M1" => M1,
        "M1 Pro" => M1Pro,
        "M1 Max" => M1Max,
        "M1 Ultra" => M1Ultra,
        "M2" => M2,
        "M2 Pro" => M2Pro,
        "M2 Max" => M2Max,
        "M2 Ultra" => M2Ultra,
        "M3" => M3,
        "M3 Pro" => M3Pro,
        "M3 Max" => M3Max,
        "M3 Ultra" => M3Ultra,
        "M4" => M4,
        "M4 Pro" => M4Pro,
        "M4 Max" => M4Max,
        _ => return None,
    })
}

/// Estimated unified-memory bandwidth in GB/s for an Apple-Silicon family. For
/// binned families (same name, fewer GPU cores → lower bandwidth) `gpu_cores`
/// disambiguates; when it's `None` we deliberately return the LOWER bin (never
/// over-promise). Every value is an ESTIMATE; the "verified" ones were
/// cross-checked against Apple-published figures (2026-07-22), the rest are
/// order-of-magnitude. Total (never fallible) — every variant has a row; the
/// fallibility lives one level up in whether `apple_chip_family` is `Some`.
pub fn bandwidth_gbps_estimate(family: AppleChipFamily, gpu_cores: Option<u32>) -> f64 {
    use AppleChipFamily::*;
    match family {
        M1 => 68.0,
        M1Pro => 200.0,
        // M1 Max: binned ≤24-core ≈200, full 32-core ≈400.
        M1Max => match gpu_cores {
            Some(c) if c >= 32 => 400.0,
            _ => 200.0,
        },
        M1Ultra => 800.0,
        M2 => 100.0,
        M2Pro => 200.0,
        M2Max => 400.0,
        M2Ultra => 800.0,
        M3 => 100.0,
        M3Pro => 150.0,
        // M3 Max: binned 30-core ≈300, full 40-core ≈400.
        M3Max => match gpu_cores {
            Some(c) if c >= 40 => 400.0,
            _ => 300.0,
        },
        M3Ultra => 800.0,
        M4 => 120.0,
        M4Pro => 273.0,
        // M4 Max: binned 32-core ≈410, full 40-core ≈546.
        M4Max => match gpu_cores {
            Some(c) if c >= 40 => 546.0,
            _ => 410.0,
        },
    }
}

/// Probe the current machine. Cheap on the RAM/CPU path; the macOS GPU
/// enumeration shells out to `system_profiler` (can be hundreds of ms), so
/// callers on a hot path should cache the result rather than re-probe.
pub fn probe() -> HardwareProfile {
    let mut sys = sysinfo::System::new();
    sys.refresh_memory();
    let total_ram_bytes = sys.total_memory(); // bytes in sysinfo 0.31
    let cpu_cores = std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(1);
    let os = std::env::consts::OS.to_string();
    let arch = std::env::consts::ARCH.to_string();

    let cpu_brand = macos::cpu_brand_string();
    let apple_chip_family = cpu_brand.as_deref().and_then(parse_brand_string);
    // Every Apple-Silicon Mac is unified memory by construction; Intel Macs and
    // (v1) Windows/Linux are not — the bandwidth table only covers Apple Silicon.
    let unified_memory = os == "macos" && arch == "aarch64";
    let gpus = macos::enumerate_gpus();
    // GPU core count of the primary GPU, used to disambiguate binned bandwidth.
    let gpu_cores = gpus
        .as_ref()
        .and_then(|g| g.first())
        .and_then(|g| g.core_count);
    let mem_bandwidth_gbps = apple_chip_family.map(|f| bandwidth_gbps_estimate(f, gpu_cores));

    HardwareProfile {
        total_ram_bytes,
        cpu_cores,
        os,
        arch,
        cpu_brand,
        apple_chip_family,
        unified_memory,
        mem_bandwidth_gbps,
        gpus,
    }
}

/// Did GPU enumeration actually run and report a definitive answer? `false` when
/// `gpus` is `None` (not attempted / failed). Callers must NOT treat `None` as
/// "no GPU" — that would wrongly force CPU-only sizing on an un-probed machine.
pub fn gpu_enumeration_known(profile: &HardwareProfile) -> bool {
    profile.gpus.is_some()
}

/// macOS-only syscall/subprocess helpers, behind `cfg` with non-macOS stubs so
/// `--no-default-features` and non-mac CI stay green. These are the only impure
/// parts of the probe.
mod macos {
    use super::GpuInfo;

    #[cfg(target_os = "macos")]
    pub fn cpu_brand_string() -> Option<String> {
        let out = std::process::Command::new("sysctl")
            .args(["-n", "machdep.cpu.brand_string"])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    }

    #[cfg(not(target_os = "macos"))]
    pub fn cpu_brand_string() -> Option<String> {
        None
    }

    /// Enumerate GPUs via `system_profiler SPDisplaysDataType -json`. Best-effort:
    /// any failure (spawn error, non-zero exit, unparseable JSON) → `None`
    /// ("we don't know"), never a fabricated empty list. One top-level
    /// `SPDisplaysDataType` array entry = one GPU (the nested `spdisplays_ndrvs`
    /// is connected monitors, a different concept — never counted here).
    #[cfg(target_os = "macos")]
    pub fn enumerate_gpus() -> Option<Vec<GpuInfo>> {
        let out = std::process::Command::new("system_profiler")
            .args(["SPDisplaysDataType", "-json"])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let json: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
        let arr = json.get("SPDisplaysDataType")?.as_array()?;
        let gpus = arr.iter().map(parse_gpu_entry).collect::<Vec<_>>();
        Some(gpus)
    }

    #[cfg(not(target_os = "macos"))]
    pub fn enumerate_gpus() -> Option<Vec<GpuInfo>> {
        None
    }

    /// Parse one `SPDisplaysDataType` entry. `pub(super)` + macOS-gated so the
    /// unit test can drive it with a captured-real fixture without spawning
    /// `system_profiler`.
    #[cfg(target_os = "macos")]
    pub(super) fn parse_gpu_entry(entry: &serde_json::Value) -> GpuInfo {
        let name = entry
            .get("sppci_model")
            .or_else(|| entry.get("_name"))
            .and_then(|v| v.as_str())
            .unwrap_or("Unknown GPU")
            .to_string();
        let core_count = entry
            .get("sppci_cores")
            .and_then(|v| v.as_str())
            .and_then(|s| s.trim().parse::<u32>().ok());
        // Only `spdisplays_vram` signals DEDICATED VRAM (a discrete card, e.g.
        // "8192 MB") — this is the verified-live schema (design §A). We do NOT
        // treat `spdisplays_vram_shared` as discrete: that key is a *shared*
        // slice of system RAM (a unified/integrated GPU reports it), so folding
        // it in here would flip a unified GPU to "discrete" and fabricate a VRAM
        // number for a machine that has none. Absence of `spdisplays_vram` ⇒
        // unified, `vram_bytes: None`. A present-but-unparseable value fails
        // closed to `None` (never a guessed number).
        let vram_str = entry.get("spdisplays_vram").and_then(|v| v.as_str());
        let is_unified = vram_str.is_none();
        let vram_bytes = vram_str.and_then(parse_vram_mb);
        GpuInfo {
            name,
            is_unified,
            vram_bytes,
            core_count,
        }
    }

    /// Parse an Apple `"<N> MB"` / `"<N> GB"` VRAM string into bytes. Returns
    /// `None` on any parse failure (fail closed — never a fabricated number).
    #[cfg(target_os = "macos")]
    pub(super) fn parse_vram_mb(s: &str) -> Option<u64> {
        let s = s.trim();
        let (num, mult): (&str, u64) = if let Some(n) = s.strip_suffix("GB") {
            (n, 1024 * 1024 * 1024)
        } else if let Some(n) = s.strip_suffix("MB") {
            (n, 1024 * 1024)
        } else {
            return None;
        };
        num.trim().parse::<u64>().ok().map(|n| n * mult)
    }
}

/// Anything that can produce a [`HardwareProfile`]: the real probe, or a
/// synthetic fixture. `&self` (object-safe) so it's usable as
/// `Box<dyn HardwareSource>`/`Arc<dyn HardwareSource>` for dependency injection.
pub trait HardwareSource {
    fn snapshot(&self) -> HardwareProfile;
}

/// The real probe — owns the actual syscalls/subprocess reads.
pub struct RealHardwareSource;
impl HardwareSource for RealHardwareSource {
    fn snapshot(&self) -> HardwareProfile {
        probe()
    }
}

/// A fixed, synthetic profile for tests and the recommendation engine's own
/// test suite. Never touches the OS.
#[derive(Clone)]
pub struct FakeHardwareSource(pub HardwareProfile);
impl HardwareSource for FakeHardwareSource {
    fn snapshot(&self) -> HardwareProfile {
        self.0.clone()
    }
}

/// How well a model of `model_bytes` fits `profile`'s RAM. A GGUF's runtime
/// working set is roughly its file size plus overhead (KV cache, context,
/// runtime); we use a headroom multiplier rather than pretending to be exact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Fit {
    /// Comfortable — model + overhead leaves the OS room to breathe.
    Fits,
    /// Runnable but tight — likely to swap / be slow; offered with a warning.
    Tight,
    /// Won't fit — not offered (or offered disabled).
    TooLarge,
}

/// Overhead factor over the raw model bytes for the working set (KV cache,
/// context window, the runtime itself). Conservative on purpose.
pub(crate) const WORKING_SET_OVERHEAD: f64 = 1.3;
/// Below this fraction of total RAM used by the working set → `Fits`; between
/// this and 1.0 → `Tight`; at/above total RAM → `TooLarge`. `pub(crate)` so the
/// recommendation engine can reuse the same comfort boundary instead of
/// duplicating the constant.
pub(crate) const COMFORTABLE_FRACTION: f64 = 0.7;

/// Pure sizing decision — testable without real hardware.
pub fn fits(model_bytes: u64, profile: &HardwareProfile) -> Fit {
    if profile.total_ram_bytes == 0 {
        // Unknown RAM (a probe failure) — never claim it fits.
        return Fit::TooLarge;
    }
    let working_set = model_bytes as f64 * WORKING_SET_OVERHEAD;
    let total = profile.total_ram_bytes as f64;
    if working_set <= total * COMFORTABLE_FRACTION {
        Fit::Fits
    } else if working_set < total {
        Fit::Tight
    } else {
        Fit::TooLarge
    }
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
    fn probe_returns_sane_values_on_this_machine() {
        let p = probe();
        assert!(p.total_ram_bytes > GB, "a dev machine has > 1 GB RAM");
        assert!(p.cpu_cores >= 1);
        assert!(!p.os.is_empty() && !p.arch.is_empty());
        // New Probe v2 fields must be EITHER None (non-macOS CI) OR sane
        // (macOS) — never required to be populated, so this stays green across
        // CI runner OSes (the Unknown-handling contract in action).
        if let Some(brand) = &p.cpu_brand {
            assert!(!brand.is_empty());
        }
        if let Some(gpus) = &p.gpus {
            for g in gpus {
                assert!(!g.name.is_empty());
            }
        }
    }

    #[test]
    fn fits_sizes_models_against_ram() {
        // A ~4 GB model on 32 GB: comfortable.
        assert_eq!(fits(4 * GB, &profile(32)), Fit::Fits);
        // A 40 GB model on 32 GB: 40*1.3 = 52 GB working set > 32 → TooLarge.
        assert_eq!(fits(40 * GB, &profile(32)), Fit::TooLarge);
        // The same 40 GB model on 64 GB: 52 GB < 64*0.7=44.8? no → 52 < 64 → Tight.
        assert_eq!(fits(40 * GB, &profile(64)), Fit::Tight);
        // A 20 GB model on 64 GB: 26 GB < 44.8 → Fits.
        assert_eq!(fits(20 * GB, &profile(64)), Fit::Fits);
        // Right at the comfortable edge.
        assert_eq!(fits(30 * GB, &profile(64)), Fit::Fits); // 39 < 44.8
    }

    #[test]
    fn fits_fails_closed_on_unknown_ram() {
        let unknown = HardwareProfile {
            total_ram_bytes: 0,
            cpu_cores: 1,
            os: "linux".into(),
            arch: "x86_64".into(),
            ..Default::default()
        };
        assert_eq!(
            fits(GB, &unknown),
            Fit::TooLarge,
            "unknown RAM never claims a fit"
        );
    }

    #[test]
    fn fake_hardware_source_returns_injected_profile() {
        let p = profile(48);
        let src = FakeHardwareSource(p.clone());
        assert_eq!(
            src.snapshot(),
            p,
            "DI returns the injected profile bit-for-bit"
        );
    }

    #[test]
    fn bandwidth_lookup_disambiguates_binned_variants_by_core_count() {
        assert_eq!(
            bandwidth_gbps_estimate(AppleChipFamily::M3Max, Some(30)),
            300.0
        );
        assert_eq!(
            bandwidth_gbps_estimate(AppleChipFamily::M3Max, Some(40)),
            400.0
        );
        assert_eq!(
            bandwidth_gbps_estimate(AppleChipFamily::M4Max, Some(40)),
            546.0
        );
    }

    #[test]
    fn bandwidth_lookup_unknown_cores_defaults_to_lower_bin() {
        // Never over-promise: unknown core count picks the conservative bin.
        assert_eq!(bandwidth_gbps_estimate(AppleChipFamily::M4Max, None), 410.0);
        assert_eq!(bandwidth_gbps_estimate(AppleChipFamily::M3Max, None), 300.0);
        assert_eq!(bandwidth_gbps_estimate(AppleChipFamily::M1Max, None), 200.0);
    }

    #[test]
    fn chip_family_parse_handles_all_known_brand_strings() {
        use AppleChipFamily::*;
        let cases = [
            ("Apple M1", M1),
            ("Apple M1 Pro", M1Pro),
            ("Apple M1 Max", M1Max),
            ("Apple M1 Ultra", M1Ultra),
            ("Apple M2", M2),
            ("Apple M2 Pro", M2Pro),
            ("Apple M2 Max", M2Max),
            ("Apple M2 Ultra", M2Ultra),
            ("Apple M3", M3),
            ("Apple M3 Pro", M3Pro),
            ("Apple M3 Max", M3Max),
            ("Apple M3 Ultra", M3Ultra),
            ("Apple M4", M4),
            ("Apple M4 Pro", M4Pro),
            ("Apple M4 Max", M4Max),
        ];
        for (brand, expect) in cases {
            assert_eq!(parse_brand_string(brand), Some(expect), "{brand}");
        }
        // Tolerant of extra internal whitespace.
        assert_eq!(parse_brand_string("Apple  M3   Max"), Some(M3Max));
    }

    #[test]
    fn chip_family_parse_returns_none_for_unmapped_or_non_apple() {
        assert_eq!(
            parse_brand_string("Apple M5 Max"),
            None,
            "future chip → None, not a guess"
        );
        assert_eq!(
            parse_brand_string("Apple M4 Ultra"),
            None,
            "unshipped SKU → None"
        );
        assert_eq!(parse_brand_string("Intel(R) Core(TM) i9-9980HK"), None);
        assert_eq!(parse_brand_string("M3 Max"), None, "no Apple prefix → None");
        assert_eq!(parse_brand_string(""), None);
    }

    #[test]
    fn unified_memory_is_computed_only_for_apple_silicon() {
        // probe() sets unified_memory from (os, arch); mirror that logic here
        // over the table of cases (probe itself is exercised on the real machine
        // above).
        let cases = [
            ("macos", "aarch64", true),
            ("macos", "x86_64", false),
            ("linux", "aarch64", false),
            ("windows", "x86_64", false),
        ];
        for (os, arch, expect) in cases {
            let unified = os == "macos" && arch == "aarch64";
            assert_eq!(unified, expect, "{os}/{arch}");
        }
    }

    #[test]
    fn gpu_enumeration_none_is_distinct_from_confirmed_zero() {
        let mut unknown = profile(16);
        unknown.gpus = None;
        let mut confirmed_none = profile(16);
        confirmed_none.gpus = Some(vec![]);
        assert!(
            !gpu_enumeration_known(&unknown),
            "None = not-probed, not 'no GPU'"
        );
        assert!(
            gpu_enumeration_known(&confirmed_none),
            "Some(vec![]) = probed, reported nothing"
        );
    }

    #[test]
    fn serde_round_trip_accepts_old_four_field_shape() {
        // A blob shaped like the pre-v2 struct must deserialize, with every new
        // field at its serde default — forward compat for any cached blob.
        let old = r#"{"total_ram_bytes":34359738368,"cpu_cores":8,"os":"macos","arch":"aarch64"}"#;
        let p: HardwareProfile = serde_json::from_str(old).unwrap();
        assert_eq!(p.total_ram_bytes, 34359738368);
        assert!(p.cpu_brand.is_none());
        assert!(p.apple_chip_family.is_none());
        assert!(p.gpus.is_none());
        assert!(p.mem_bandwidth_gbps.is_none());
        assert!(!p.unified_memory);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn parse_gpu_entry_reads_a_real_unified_apple_entry() {
        // Captured-real SPDisplaysDataType entry (M3 Max, 30-core, unified).
        let entry: serde_json::Value = serde_json::from_str(
            r#"{"_name":"Apple M3 Max","sppci_model":"Apple M3 Max","sppci_cores":"30",
                "spdisplays_vendor":"sppci_vendor_Apple","sppci_device_type":"spdisplays_gpu"}"#,
        )
        .unwrap();
        let g = macos::parse_gpu_entry(&entry);
        assert_eq!(g.name, "Apple M3 Max");
        assert_eq!(g.core_count, Some(30));
        assert!(g.is_unified, "no VRAM key → unified");
        assert_eq!(g.vram_bytes, None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn shared_vram_key_does_not_flip_a_unified_gpu_to_discrete() {
        // `spdisplays_vram_shared` is a SHARED-RAM figure a unified/integrated
        // GPU can report — it must NEVER be read as dedicated VRAM (that would
        // fabricate a discrete number for a machine with none). Only
        // `spdisplays_vram` marks a discrete card.
        let entry: serde_json::Value = serde_json::from_str(
            r#"{"sppci_model":"Apple M2","sppci_cores":"10",
                "spdisplays_vram_shared":"1536 MB","spdisplays_vendor":"sppci_vendor_Apple"}"#,
        )
        .unwrap();
        let g = macos::parse_gpu_entry(&entry);
        assert!(
            g.is_unified,
            "a shared-VRAM key must not mark the GPU discrete"
        );
        assert_eq!(g.vram_bytes, None, "no dedicated VRAM must be fabricated");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn parse_gpu_entry_reads_a_discrete_vram_entry_best_effort() {
        let entry: serde_json::Value = serde_json::from_str(
            r#"{"sppci_model":"AMD Radeon Pro 5500M","spdisplays_vram":"8192 MB",
                "spdisplays_vendor":"sppci_vendor_amd"}"#,
        )
        .unwrap();
        let g = macos::parse_gpu_entry(&entry);
        assert_eq!(g.name, "AMD Radeon Pro 5500M");
        assert!(!g.is_unified, "a VRAM key → discrete");
        assert_eq!(g.vram_bytes, Some(8192 * 1024 * 1024));
        // A garbled VRAM string fails closed to None (never a guessed number).
        assert_eq!(macos::parse_vram_mb("lots"), None);
    }
}
