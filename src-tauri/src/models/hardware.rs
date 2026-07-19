//! Wave 5.3 / M8 — hardware detection for the model-lifecycle onboarding. The
//! curated model catalog is sized to what the machine can actually run: we probe
//! total RAM + CPU cores, then [`fits`] each catalog entry's memory footprint
//! against the profile so onboarding only offers models the user can run (PLAN
//! §6 "local-first made real").
//!
//! Deliberately conservative + pure where it matters: [`probe`] does the one
//! impure syscall-ish read (via `sysinfo`), and [`fits`] is a pure function of
//! `(model_bytes, profile)` so the sizing logic is unit-testable without touching
//! real hardware.

use serde::{Deserialize, Serialize};

/// A snapshot of what this machine can run. `gpu` is best-effort/absent for now
/// (RAM is the load-bearing constraint for a GGUF's working set); a real GPU
/// probe is a later refinement.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HardwareProfile {
    /// Total physical RAM in bytes.
    pub total_ram_bytes: u64,
    /// Logical CPU cores.
    pub cpu_cores: u32,
    /// "macos" | "windows" | "linux" | other (std::env::consts::OS).
    pub os: String,
    /// CPU architecture (e.g. "aarch64", "x86_64").
    pub arch: String,
}

/// Probe the current machine. Cheap; called at onboarding + on the Settings
/// model-manager surface.
pub fn probe() -> HardwareProfile {
    let mut sys = sysinfo::System::new();
    sys.refresh_memory();
    let total_ram_bytes = sys.total_memory(); // bytes in sysinfo 0.31
    let cpu_cores = std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(1);
    HardwareProfile {
        total_ram_bytes,
        cpu_cores,
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
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
const WORKING_SET_OVERHEAD: f64 = 1.3;
/// Below this fraction of total RAM used by the working set → `Fits`; between
/// this and 1.0 → `Tight`; at/above total RAM → `TooLarge`.
const COMFORTABLE_FRACTION: f64 = 0.7;

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
        }
    }

    #[test]
    fn probe_returns_sane_values_on_this_machine() {
        let p = probe();
        assert!(p.total_ram_bytes > GB, "a dev machine has > 1 GB RAM");
        assert!(p.cpu_cores >= 1);
        assert!(!p.os.is_empty() && !p.arch.is_empty());
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
        };
        assert_eq!(fits(GB, &unknown), Fit::TooLarge, "unknown RAM never claims a fit");
    }
}
