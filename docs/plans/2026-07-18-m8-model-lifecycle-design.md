# M8 / 5.3 — Local-model lifecycle + onboarding: design

> **STATUS: design-pass draft (2026-07-18). Skeptical review verdict: SOLID (design-pass quality; three revisions to fold in before/during build, none a redesign).** Read the **Design Review** at the bottom before building — it flags concrete architecture gaps to fold in during the build phase.


**Status:** design pass (Tier-B, BUILD-MANIFEST Wave 5 item 5.3 / PLAN §6 "Local-model
lifecycle" + §8 M8). Not yet built. Read alongside PLAN §6 (the from-scratch flagships), §8
M8, and §12 (invariants). Every "add" below cites where it plugs into the existing spine.

## The one idea

Today `find_local_provider()` returns `None` on a fresh machine — the app can *route to*
local but there is nothing local to route to. M8 is the subsystem that **makes a local model
exist**: probe the hardware → show a curated catalog sized to it → download + **verify** →
stand up a local OpenAI-compatible endpoint → register it as a `Provider` → bind seats. It is
mostly *composition of existing parts* (the `ModelManager`, the `model_catalog` table, the
`seat_bindings` table, the `is_local() && is_private()` predicate) plus three genuinely new
pieces: a **hardware probe**, a **verified downloader**, and a **local runner** that turns a
GGUF on disk into a callable `127.0.0.1` endpoint.

## Goal / scope / non-goals

**Goal.** First-run (and later, Settings) flow that takes a user from zero local models to a
working, seat-bound, on-device model — "local-first made real."

**In scope.** (1) cross-platform hardware detection; (2) a curated, hardware-filtered model
catalog; (3) resumable download + checksum verification; (4) a managed local inference
runtime that exposes each verified model as an OpenAI-compat endpoint; (5) registering it as a
`Provider` + `model_catalog` row; (6) seat binding; (7) wiring the visual-only
`Onboarding.svelte` and a first-run gate; (8) a Settings model-manager surface + boot-time
integrity re-check.

**Non-goals.** Fine-tuning / quantizing models ourselves; a general model marketplace; making
model download an *agent* tool (it stays IPC/first-run driven — see Invariant 4); cloud-model
onboarding (cloud providers are added the existing way via `add_provider`, `ipc/mod.rs:343`);
GPU-backend autotuning beyond "pick the right sidecar for this OS."

## Where it slots into the existing spine (code-grounded)

The whole point: a downloaded model becomes *indistinguishable from any other local provider*,
so the privacy/routing invariants apply to it for free.

- **`ModelManager`** (`models/manager.rs:46` `add_provider`, `:83` `get_client`) — the runner
  registers a `Provider` here; the agent loop then treats it like LM Studio.
- **`Provider` / `ProviderKind::Local`** (`models/provider.rs:20`, `:83` `is_local`, `:92`
  `is_private`) — a runner endpoint at `http://127.0.0.1:PORT` is `Local` **and**
  `is_private_endpoint` → true (`agent/egress.rs:23`, `127.0.0.0/8` branch). So it satisfies
  `find_local_provider()`'s `is_local() && is_private()` predicate (`agent/loop_mod.rs:1131`)
  **and** `enforce_local_routing`'s `LocalRequired` branch (`hooks/routing.rs:51`) with no new
  routing code. This is the linchpin — M8 does not touch the privacy gate; it *feeds* it.
- **`model_catalog` table** (`storage/schema.rs:74`, methods `storage/global.rs:558`–`623`) —
  the persistent record of downloaded models. **Needs a schema bump** (see below).
- **`seat_bindings`** (per-profile; `resolve_seat` `models/seat.rs:26`) — 3.1 is DONE. Seat
  binding IPC **already exists**: `set_seat_binding` (`ipc/mod.rs:839`), `list_seat_bindings`
  (`:817`). Onboarding's "Assign seats" step just calls these. No new seat code.
- **`ModelClient`** (`models/client.rs`) already speaks the OpenAI `/v1/chat/completions` +
  `/v1/models` surface llama.cpp's server exposes — so a runner endpoint needs zero
  client-side translation.
- **Storage layout** — models already live under `<storage>/models/{classifier,embedder}`
  (`lib.rs:100`,`:129`); downloaded GGUFs go to `<storage>/models/local/<model-id>/`.
- **Boot seeding** — `hydrate_providers_from_storage` (`lib.rs:299`) hydrates cloud/custom
  endpoints from `endpoints`. M8 adds a parallel `start_local_runners_from_catalog` pass that,
  for each verified `model_catalog` row, (re)starts its runner and registers its provider.

## The NEW flagship invariant: **verified-before-runnable**

PLAN §6 calls out that each flagship introduces a new privacy/security invariant. For M8 it is
the trust boundary on **third-party model weights fetched over the network** — the binary
analog of guard-wrapping untrusted text (`tools/calling.rs` `guard_wrap`).

> **A model artifact is inert until its bytes match the catalog's pinned SHA-256. Only a
> verified artifact may be registered as a `Provider` / `model_catalog` row and thus become
> reachable by `find_local_provider()` / `resolve_seat`.**

Three fail-closed corollaries the build must honor:

1. **Verify before register, atomically.** Download to `…/<id>/model.gguf.partial`; compute
   SHA-256 over the finished file; only on a match `rename` to `model.gguf` and insert the
   `model_catalog` row. A mismatch deletes the partial and inserts **nothing** — no row, no
   provider, no runner. (Mirror of the existing "no half-durability" rule, PLAN §3.)
2. **Download egress is not chat egress.** Fetching public weights is a distinct path from the
   §7-gated chat egress. It must (a) only hit hosts **pinned in the curated catalog** (never an
   arbitrary URL), (b) carry **no** user/profile data (anonymous `GET`; privacy rule: no
   personal data in URLs), (c) never be confused with `enforce_local_routing`. Validate the
   URL host against the catalog's allowlist *before* the first byte.
3. **Boot-time integrity re-check.** A file verified at download can be tampered/corrupted
   *afterward* on disk. The boot runner-start pass existence-checks (and, cheap-enough,
   size-checks; optionally re-hashes on a flag) each row's backing file and **quarantines**
   (`status='quarantined'`, provider not registered) any that fail — so a swapped file can
   never be silently served. A quarantined model surfaces in Settings for re-download.

## Architecture — the new modules

All new code lives under `src-tauri/src/models/` (the natural home; keeps `--no-default-features`
independent of the `onnx-classifier` feature).

### `models/hardware.rs` — the probe
```rust
pub struct HardwareProfile {
    pub ram_bytes: u64,
    pub vram_bytes: Option<u64>,   // None = unified memory (Apple) or unknown
    pub unified_memory: bool,
    pub gpu_name: Option<String>,
    pub cpu_brand: String,
    pub logical_cores: usize,
    pub backend: RunnerBackend,    // Metal | Cuda | Vulkan | Cpu
}
pub trait HardwareProbe { fn probe() -> HardwareProfile; }
```
Per-OS impls behind `#[cfg(target_os = …)]` (see Cross-platform). A pure
`fn fits(model: &CatalogModel, hw: &HardwareProfile) -> Fit` (`Fits | Tight | TooLarge`)
implements the sizing heuristic: required footprint ≈ `size_bytes` (quantized weights) + KV/
overhead headroom (~20–30%), compared against **unified RAM** on Apple or **VRAM else RAM**
elsewhere. This is a *capability* estimate, not a routing claim (matches the Onboarding
copy at `Onboarding.svelte:31`).

### `models/catalog.rs` — the curated catalog
```rust
pub struct CatalogModel {
    pub id: String, pub name: String, pub params: String, pub quant: String,
    pub size_bytes: u64, pub min_ram_bytes: u64,
    pub sha256: String,          // pinned digest — the verification anchor
    pub url: String,             // pinned host, allowlist-checked
    pub license: String,
    pub context_len: u32,
}
pub fn load_catalog() -> Vec<CatalogModel>;                 // bundled, offline
pub fn allowed_hosts(&[CatalogModel]) -> HashSet<String>;   // egress allowlist
```
v1: a bundled `catalog.json` embedded via `include_str!` (offline-first). See Open Q1 for
provenance/refresh. Filtering = `catalog × fits(hw)` → the UI's "fits your machine" / "too
large" flags (already rendered at `Onboarding.svelte:146`).

### `models/download.rs` — verified, resumable download
- Streaming `reqwest` `GET` with `Range: bytes=<partial_len>-` resume; writes to
  `…/<id>/model.gguf.partial`; emits `stream:model-download` progress (bytes/total/speed),
  same Tauri-event pattern as `stream:token`.
- On completion: SHA-256 over the file (streamed, `sha2` crate), compare to `CatalogModel.sha256`.
- **Enforces the verified-before-runnable invariant** (corollary 1). Cancellable
  (`cancel_download`) — a cancel leaves only the `.partial` for later resume, never a row.

### `models/runner.rs` — the local inference runtime (the real new dependency)
Turns a verified GGUF into an OpenAI-compat endpoint. `ModelClient` speaks HTTP, so a served
GGUF must be *behind* HTTP.
```rust
pub struct RunnerHandle { pub model_id: String, pub port: u16, child: Child, ... }
pub trait LocalRunner {
    fn start(&self, gguf: &Path, hw: &HardwareProfile) -> Result<RunnerHandle>;
    fn health(&self, h: &RunnerHandle) -> bool;   // GET /v1/models
    fn stop(&self, h: RunnerHandle);
}
```
**Recommended default:** a bundled `llama-server` (llama.cpp) **sidecar** (Tauri external
binary), spawned per model on a free `127.0.0.1` port, GPU-offload flags chosen from
`HardwareProfile.backend`, health-checked before the provider is registered, restarted if it
dies. On success: `mm.add_provider(Provider::new(id, name, "http://127.0.0.1:PORT", None,
Local))` + upsert the `model_catalog` row. **This is the biggest architectural commitment —
see Open Q2** (bundle a sidecar vs. drive an external LM Studio/Ollama the user installs). Gate
it behind a `local-runner` cargo feature so a minimal `--no-default-features` build can omit
sidecar management and fall back to "you have the file; point a runner at it."

### Schema change (GLOBAL v6 → v7, migration)
`model_catalog` (`storage/schema.rs:74`) currently: `id, name, path, size_bytes, quantization,
added_at`. Add:
```sql
sha256      TEXT,                       -- pinned digest verified at download (integrity re-check anchor)
source_url  TEXT,                       -- provenance
provider_id TEXT,                       -- the registered local Provider id (link into ModelManager)
status      TEXT NOT NULL DEFAULT 'ready'  -- 'ready' | 'quarantined'
```
Additive columns → a v6→v7 migration in `storage/migrations.rs` (bump
`GLOBAL_SCHEMA_VERSION`, `schema.rs:12`). Extend `ModelEntry` (`global.rs:41`) + its
insert/list/get.

### IPC surface (`ipc/mod.rs`, mirror the `args`-struct convention; add to `tauri.ts`)
| command | shape | notes |
|---|---|---|
| `probe_hardware` | `() -> HardwareInfo` | read-only; drives Onboarding step 1 |
| `list_model_catalog` | `() -> Vec<CatalogEntry>` | bundled catalog × fits(hw); step 2 |
| `download_model` | `(args:{model_id}) -> ()` | streams `stream:model-download`; verify+register on complete |
| `cancel_download` | `(args:{model_id}) -> bool` | |
| `list_local_models` | `() -> Vec<LocalModelInfo>` | Settings surface; includes `status` |
| `remove_local_model` | `(args:{id}) -> bool` | stop runner, drop provider, delete files, rebind orphaned seats → `inherit` |
| `complete_onboarding` | `() -> ()` | sets `app_settings.onboarding_complete` (`global.rs:1387`) |

Seat assignment reuses the **existing** `set_seat_binding` / `list_seat_bindings`.

### Frontend wiring (`Onboarding.svelte`, currently hardcoded)
Replace the three `const` fixtures (`HARDWARE:26`, `CATALOG:35`, `seats:53`) with store calls:
step 1 → `probe_hardware`; step 2 → `list_model_catalog` + `download_model` with a real
progress bar off `stream:model-download` (the "Download → Ready" button state at `:153` maps to
idle→downloading→ready); step 3 → `set_seat_binding` per seat (only over `status='ready'`
models). "Finish" → `complete_onboarding`. **First-run gate** (App.svelte, `:36` already maps
the screen): on boot, if `!onboarding_complete && find_local_provider()==None && no cloud
provider`, `nav.go('onboarding')`. "Skip setup" (`:217`) sets the flag too — the app must run
with zero local models (local-first ≠ mandatory download).

## Cross-platform strategy

| | Hardware probe | Runner sidecar / backend |
|---|---|---|
| **macOS** | `sysctl hw.memsize`; unified memory; GPU via IOKit/`system_profiler` | `llama-server` + **Metal** offload. Primary dev target. |
| **Windows** | `GlobalMemoryStatusEx`; GPU + VRAM via DXGI `DedicatedVideoMemory` | `llama-server` + **CUDA** (NVIDIA) / **Vulkan** / CPU. PLAN §6 "Windows depth." |
| **Linux** | `/proc/meminfo`; GPU via `/sys/class/drm` / vendor query | `llama-server` + **CUDA/Vulkan/CPU**. |

The probe is the only place with `#[cfg(target_os)]` branching; `fits()`, catalog, download,
and registration are OS-agnostic. The sidecar is a per-(OS × backend) prebuilt binary shipped
as a Tauri external binary (Open Q2 covers the distribution/signing weight). Windows should be
smoke-tested from the first slice, not discovered late (PLAN §6).

## Build slices (each committable, each with its gate)

**Verify per slice:** `cargo test --lib` green + `cargo build --lib --no-default-features` clean
+ `cargo clippy --lib` 0 err + `npm run check` clean; adversarial multi-lens review before
commit (per manifest §5).

- **S1 — Hardware probe.** `models/hardware.rs`: `HardwareProfile`, per-OS `probe()`, pure
  `fits()`. IPC `probe_hardware`. *Gate:* returns real values on the build Mac; `fits()` unit
  tests over fixtures (a 40 GB model `TooLarge` on 32 GB, `Fits` on 64 GB); builds on
  `--no-default-features`.
- **S2 — Curated catalog + filtering.** `models/catalog.rs` + bundled `catalog.json`
  (`include_str!`); `list_model_catalog` = catalog × `fits(probe)`. *Gate:* catalog parses,
  each entry carries a `sha256` + pinned `url`; fits/too-large flags correct; works fully
  **offline**.
- **S3 — Download + verify + resume (the invariant lands here).** `models/download.rs`:
  ranged resume, `stream:model-download` progress, SHA-256 verify, atomic
  `.partial`→final, host-allowlist check. Schema v6→v7 + `ModelEntry` extension. *Gate:* a
  digest-mismatch download registers **nothing** and leaves no artifact; an interrupted
  download resumes; a URL off the allowlist is refused; verified download inserts a `ready`
  row. (No runner yet — S3 proves the trust boundary in isolation.)
- **S4 — Local runner + provider registration.** `models/runner.rs` (behind `local-runner`
  feature): spawn/health/stop the sidecar, register the `Provider` + link `provider_id`;
  `start_local_runners_from_catalog` boot pass with the **integrity re-check / quarantine**
  (invariant corollary 3). *Gate:* a verified model becomes a callable `Local` provider;
  `find_local_provider()` now returns it and `is_local() && is_private()` holds; a killed
  sidecar restarts; a tampered/missing file quarantines instead of registering.
- **S5 — Onboarding wiring + first-run gate + seats.** Replace `Onboarding.svelte` fixtures
  with the S1–S4 IPC + `set_seat_binding`; `complete_onboarding` flag; App.svelte first-run
  route. *Gate:* end-to-end first run downloads a model, binds a seat, and a subsequent chat
  turn resolves that seat to the local model (`resolve_seat` → the new provider); "Skip" leaves
  a usable app.
- **S6 — Settings model-manager surface.** `list_local_models` / `remove_local_model` +
  re-download of a quarantined model; orphaned-seat rebind → `inherit`. *Gate:* removing a
  model drops its provider and rebinds seats; a quarantined model is re-downloadable; no
  dangling `provider_id`.

## Tool-spine placement (RiskClass / Capability)

For M8, model management is **IPC/first-run driven, not an agent tool** — the agent must not
autonomously pull GB files (Invariant 4). So it does **not** register in the `ToolRegistry`
and does **not** pass the PreToolUse chain; onboarding is a human-initiated UI flow. *If* a
future `manage_models` agent tool is wanted, it would need a **new `Capability::ModelManagement`**
(`tools/mod.rs:49`) and `RiskClass::External` (network + large write; a *delete* is
`Dangerous`) — noted as future work, deliberately out of M8 scope. The download's network
reach is instead constrained by the catalog host-allowlist (invariant corollary 2), not the
tool gate.

## `--no-default-features` / local-first impact

- M8 is independent of the `onnx-classifier` feature — the whole subsystem must build with
  `--no-default-features` (manifest §Scope). Add a separate **`local-runner`** feature for the
  sidecar-management code so a minimal build omits it cleanly.
- **Catalog browsing works offline** (bundled `catalog.json`); only the actual download needs
  network — inherent and acceptable.
- **The app never *requires* a download.** Onboarding has "Skip"; `find_local_provider()`
  returning `None` is a supported state (cloud providers, or none). Local-first means local is
  the *default polarity*, not a mandatory step.
- The runner sidecar is optional: absent it, a verified GGUF still registers its `model_catalog`
  row and Settings can surface "downloaded, needs a runner," degrading loudly, never silently.

## Open questions (need Lukas — product/security decisions, not in the specs)

1. **Where does the curated catalog live, and who signs it?** Options: (a) **bundled** in the
   binary (offline-first, but a new model needs an app update); (b) a **signed remote JSON**
   fetched at onboarding (fresh, needs a trust anchor + hosting); (c) **hybrid** — bundled
   default + optional signed refresh. Default recommendation: **(c)**, bundled for v1, refresh
   later. The real decision is the **catalog's own authenticity** (the `sha256`s are only
   trustworthy if the catalog is) — a signed-manifest scheme + where it's hosted. *Flagged in
   the task as the likely open Q.*
2. **The local inference runtime: bundle a `llama-server` sidecar, or drive an external runner
   (LM Studio / Ollama) the user installs?** This is the largest commitment. **Sidecar** =
   true one-click local-first, but adds a per-(OS × GPU-backend) prebuilt binary to
   distribution, code-signing/notarization weight (ties into 7.3), and llama.cpp
   license/attribution. **External runner** = far lighter to ship, but breaks "it just works"
   and couples us to another app's model dir. Recommendation: **bundled sidecar** to honor the
   §6 promise, but this needs Lukas's explicit sign-off on the distribution/signing cost.
3. **Model download source of truth.** Direct Hugging Face URLs (host allowlist =
   `huggingface.co` + its CDN), a self-hosted mirror, or both? Affects reliability, the
   allowlist, and licensing surfaced in the UI (`CatalogModel.license`). Sensible default:
   pinned HF URLs + CDN in the allowlist; confirm the hosting posture.

**Sensible defaults taken without asking (not open questions):** verified-before-runnable +
fail-closed verification; SHA-256 as the digest; per-model sidecar on a loopback port;
`<storage>/models/local/<id>/`; models global (shared) + seats per-profile; download is IPC not
an agent tool; boot integrity re-check + quarantine.


---

## Design Review (skeptical pass, 2026-07-18)

*An independent staff-engineer critique of the design above, grounded in the actual codebase. Address these in the build phase.*

I have everything I need. All major code references verified against the actual source. Here is my review.

---

## VERDICT: **SOLID** (design-pass quality; three revisions to fold in before/during build, none a redesign)

This is an unusually well-grounded design doc. I verified nearly every cited symbol against source — the architecture is real, and critically **it violates none of the load-bearing invariants**. The linchpin claim checks out exactly: `Provider::is_private()` (`models/provider.rs:92`) delegates to `is_private_endpoint(&self.base_url)` (`agent/egress.rs:24`), which returns true for `127.0.0.0/8`; combined with `ProviderKind::Local` (`provider.rs:20`) → `is_local()`, a `127.0.0.1:PORT` runner satisfies `find_local_provider()`'s `is_local() && is_private()` predicate (`agent/loop_mod.rs:1131`) and `enforce_local_routing`'s `LocalRequired` arm (`hooks/routing.rs:50/60`) **with zero new routing code**. The doc feeds the privacy gate; it does not touch it.

Invariant sweep (against the real "Locked invariants" in `tool-system-build-plan.md:64-78`, not "PLAN §12" which is actually the parity check — a mis-cite): privacy gate — fed, not weakened; `RouteLocal` never→cloud (#2) — explicitly kept separate from download egress; danger-floor / per-call gating (#1,#8) — download is IPC/first-run, not a tool, mirroring how `add_provider` (`ipc/mod.rs:343`) already lives outside the gate, so no chain interaction to weaken; guard-wrap (#6) — SHA-verify is offered as a binary *analog*, correctly rhetorical; local-first / `--no-default-features` — handled well (new `local-runner` feature, offline catalog, download never required, "Skip" preserved). The NEW invariant (**verified-before-runnable**) is genuinely designed, not hand-waved: crisp statement + three fail-closed corollaries + schema support (`sha256`/`status` columns) + an isolated build slice (S3) that proves it *before* the runner exists, with concrete gates (digest-mismatch registers nothing, off-allowlist refused, resume works). It deliberately mirrors existing invariant #7 (`atomic_write` no-half-file).

Grounding is strong and does **not** invent parallel machinery: `model_catalog` (`storage/schema.rs:74`) has *exactly* the six columns the doc lists; `insert_model`/`list_models`/`get_model`/`delete_model` exist at `global.rs:558-623`; seat reuse is real (`set_seat_binding` `ipc:839`, `list_seat_bindings` `ipc:817`, `resolve_seat` `seat.rs:26` with `inherit` fallback at `:36/:46`); the doc correctly notes `hydrate_providers_from_storage` (`lib.rs:299`) reads the **`endpoints`** table and that M8 needs a *parallel* `start_local_runners_from_catalog` pass keyed off `model_catalog` — the right seam, not a duplicate. Build slices are committable and testable, and the S3-before-S4 ordering (invariant provable before the risky runtime) is smart.

## Top 3 gaps / risks (most severe first)

**1. The S4 sidecar is the entire "one-click" payoff, is greenfield, and its gate is not `cargo test --lib`-able.** Everything the product has shipped to date runs against an **external** LM Studio endpoint (ROADMAP: "verified live with LM Studio qwen3.6-35b-a3b"). A bundled per-(OS × GPU-backend) `llama-server` with process supervision, health-check, and restart is an unproven bet — exactly what Open Q2 flags. But S4's gates ("a verified model becomes a callable `Local` provider," "a killed sidecar restarts," "a tampered file quarantines") require a real sidecar binary + a real GGUF fixture and a running loopback server; the doc's per-slice "`cargo test --lib` green" mantra can't reach any of that. **Fix:** name the S4 integration harness explicitly (a stub OpenAI `/v1/models` server, or a tiny real GGUF fixture) and — since the doc's own `--no-default-features` section already describes the fallback ("point a runner at it") — ship **S1–S3 + external-runner registration as v1** and treat the sidecar as a fast-follow, so the invariant value doesn't ride on the Q2 bet.

**2. The catalog's own authenticity is the real trust root of the new invariant, and the doc over-flags it as blocking while under-designing the one path that would need design.** `verified-before-runnable` pins bytes to `catalog.sha256` — but a SHA is only trustworthy if the catalog is. For the **bundled v1** path this is *already answered*: `catalog.json` via `include_str!` inherits the app's code signature (Wave 7.1/7.3 bundle models into the signed `tauri build`). So Open Q1 should **not block v1**. The gap is the opposite: the moment Q1's "signed remote refresh" option is taken, you need a pinned-public-key manifest-verification scheme that the doc doesn't design (it just says "needs a trust anchor"). **Fix:** state that bundled-v1 authenticity = app signing (resolved, non-blocking), and scope the signature-verification design as the actual work *if/when* remote refresh is chosen.

**3. Cross-platform GPU matrix is undersized and `fits()` rides on VRAM numbers the probe can't reliably get.** "Pick the right sidecar for this OS" hides that CUDA cannot be a single bundled binary (driver / CUDA-version / GPU-arch fragmentation), that each macOS sidecar needs notarization, and that Linux VRAM via `/sys/class/drm` is vendor-inconsistent (NVIDIA proprietary vs amdgpu vs Intel). Since `fits()` → the UI's "fits your machine" flag (`Onboarding.svelte:146`) keys off VRAM, an unreliable probe produces misleading capability claims. Unmentioned and locally relevant: the build Mac's Rust toolchain is x86_64-under-Rosetta (per the M1 env note), so a Rosetta build spawning an arm64-Metal sidecar is real friction. **Fix:** treat VRAM as best-effort with a conservative `Tight` fallback, and enumerate the sidecar build/sign matrix as a distinct cost line under Q2.

## Open questions — which are already answered (shouldn't block)

- **Q1 (catalog signing) is answered for v1** by the bundled-in-a-signed-binary path (7.1/7.3). Only the optional remote-refresh variant is genuinely open. Don't let Q1 gate S1–S3.
- **Q2's "external runner" arm is the already-validated status quo** — LM Studio is how M1–M4 were proven, and `ModelClient` (`models/client.rs`) already speaks the `/v1/...` surface. So the invariant-bearing slices ship without resolving Q2; only the *bundled-sidecar* promise waits on Lukas's sign-off. Q2 blocks the "one-click" polish, not the subsystem.
- **Q3 (HF vs mirror)** is genuinely open but low-stakes; the pinned-HF default doesn't block anything.

## Minor citation fixes (not load-bearing, but tighten before build)
- `RiskClass` actually has **four** variants — `Safe, Write, External, Dangerous` (`tools/mod.rs:252-261`). The doc's future-tool note ("network + large write → `External`; delete → `Dangerous`") silently drops `Write`; a GB local write is most naturally `Write`.
- `onboarding_complete` **does not exist yet.** The doc cites it as `app_settings.onboarding_complete` (`global.rs:1387`) as if pre-existing; only the generic `app_settings` K/V mechanism is there (`set_setting` ~`:1390`). Reword to "add an `onboarding_complete` key to `app_settings`."
- The first-run gate prose uses `find_local_provider()==None` — that's a **private Rust method on the agent loop**, not callable from `App.svelte`; the frontend gate must use the `list_providers` IPC (`ipc/mod.rs:333`).
- Path nit: the screen is `src/lib/design/screens/Onboarding.svelte` (line numbers cited are accurate); `App.svelte:36` is a screen-map entry, and there is **no** existing boot gate — the doc is honest that it's new, just calls the file by a short name.
- "PLAN §12 (invariants)" is wrong — §12 is the Claude Code parity check; the load-bearing invariants live in `tool-system-build-plan.md:64-78`.
