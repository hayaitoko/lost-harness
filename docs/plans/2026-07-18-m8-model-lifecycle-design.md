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


---

# REVISION 2026-07-22 — "make it REAL": Probe v2 · Recommendation engine · Catalog v2 · Sidecar

> **Directive (Lukas, 2026-07-21, ultracode):** complete M8 as a first-class capability — the app
> detects your hardware and makes an EDUCATED model decision (memory *bandwidth*, dense-vs-MoE,
> single-vs-multi GPU, quant choice), for users **without** their own inference infra; users **with**
> one (LM Studio on a LAN box) keep external OpenAI-compatible endpoints as an equally-first-class
> path. A fresh user on a bare Mac goes from first launch to a working local model with zero expertise.
>
> **Decisions already made (not re-litigated):** bundled sidecar = YES (llama.cpp `llama-server`);
> HuggingFace direct download via the existing host allowlist; macOS/Metal sidecar FIRST (Win/Linux
> sidecar backends = Wave 7.4, out of scope); catalog = curated mainstream open models across
> hardware tiers; BYO-GGUF import ships as the escape hatch; sidecar starts **on-demand** (first
> message needing it) with keep-alive, not at boot.
>
> This revision was produced by a 4-agent design fan-out + a skeptical staff-engineer review (all
> file:line citations spot-checked against the real tree; the llama.cpp release facts and the HF-API
> curation mechanism were verified against live GitHub/HF fetches). It **supersedes** the aspirational
> `models/runner.rs`/`HardwareProfile`/`CatalogModel` sketches earlier in this doc (those predate the
> shipped code and don't match it). Where this revision and the earlier draft disagree, **this wins.**

## 0. The reconciled shared data model (READ FIRST — this is the load-bearing contract)

The independent design sections were each internally sound but were written against **incompatible
versions of the shared types** (the review's headline blocking finding: the recommendation engine
referenced `HardwareProfile` fields the probe doesn't expose and `CatalogEntry` fields the catalog
doesn't carry — as literally written, the recommender would not compile against the probe + catalog).
This section is the **single authoritative type contract**; §A/§B/§C/§D all build against exactly
these. When building, treat this as the source of truth over any struct literal in the sub-sections.

**`HardwareProfile` (authoritative — §A owns it).** Purely ADDITIVE over today's shipped 4-field
struct (`hardware.rs:17-27`); every new field is `Option`/`bool` with `#[serde(default)]`, and
`#[derive(Default)]` is added so existing fixtures keep compiling with `..Default::default()`.
`derive(Eq)` is dropped (unavoidable once `f64` fields exist; nothing in the tree depends on
`HardwareProfile: Eq` — verified).

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct HardwareProfile {
    // existing, unchanged:
    pub total_ram_bytes: u64,
    pub cpu_cores: u32,
    pub os: String,       // std::env::consts::OS
    pub arch: String,     // std::env::consts::ARCH
    // NEW (all additive, all honest-Option):
    #[serde(default)] pub cpu_brand: Option<String>,                 // macOS: machdep.cpu.brand_string
    #[serde(default)] pub apple_chip_family: Option<AppleChipFamily>,// None if not-Apple-Silicon OR unmapped
    #[serde(default)] pub unified_memory: bool,                      // computed: os==macos && arch==aarch64
    #[serde(default)] pub mem_bandwidth_gbps: Option<f64>,           // ESTIMATE table; None if family unknown
    #[serde(default)] pub gpus: Option<Vec<GpuInfo>>,                // None = not-probed/failed (≠ "zero GPUs")
}
```

There is deliberately **no `available_ram_bytes`** field (no live-free-memory syscall is designed for
v1) and **no top-level `vram_bytes`** (discrete VRAM lives per-GPU inside `gpus`). The recommender
must derive its memory pool from these fields only — see §B's reconciliation.

**`CatalogEntry` / `QuantArtifact` (authoritative — §C owns it, sequenced before §B builds).** §C's
per-quant schema, PLUS the two fields the recommender structurally needs (the review's blocking #2):
`quality_tier` and an explicit `default_quant` (replacing the fragile per-quant `is_default` bool +
its runtime `.expect()` landmine with a single named default validated at parse time).

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Architecture { Dense, Moe }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QualityTier { Small, Balanced, Flagship }  // curator-assigned; weight() = 1/2/3

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QuantArtifact {
    pub quant: String,        // "Q4_K_M" — matches the GGUF filename convention
    pub url: String,          // pinned HF resolve URL for THIS file (host-allowlist checked)
    pub sha256: String,       // "TODO-CURATE" until curated; download.rs::is_real_sha256 gate unchanged
    pub size_bytes: u64,
}
impl QuantArtifact { pub fn is_curated(&self) -> bool { /* 64 lowercase-hex, same as today */ } }

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CatalogEntry {
    pub id: String,
    pub name: String,
    #[serde(default)] pub description: String,
    #[serde(default)] pub family: String,
    pub architecture: Architecture,
    pub total_params_billions: f64,
    pub active_params_billions: f64,     // == total for Dense (kept explicit for self-documenting JSON)
    pub quality_tier: QualityTier,       // ADDED for the recommender's scoring
    pub quants: Vec<QuantArtifact>,      // ≥1; replaces the flat url/sha256/size/quant quartet
    pub default_quant: String,           // ADDED; must name an existing quant (parse-time validated)
    pub min_ram_bytes: u64,              // fast pre-filter = ceil(smallest_curated_quant.size * 1.3)
    #[serde(default)] pub min_vram_bytes: Option<u64>,      // schema-ready, null in v1 (Wave 7.4)
    #[serde(default)] pub min_bandwidth_gbps: Option<f64>,  // schema-ready, null in v1
    pub license: String,
    pub context_len: u32,
}
impl CatalogEntry {
    // installable iff ≥1 quant is curated (partial curation is now expressible)
    pub fn is_curated(&self) -> bool { self.quants.iter().any(|q| q.is_curated()) }
}
```

**Parse-time validation (`parse_catalog`, alongside the existing `catalog_version` guard):** bump
`catalog_version` to `2` and the guard to `> 2`; for every entry assert (a) `quants` non-empty and
(b) `default_quant` names an existing `quants[_].quant` — a loud parse error, never a runtime
`.expect()` panic discovered when a curator forgets to set a flag. (`catalog.json` is `include_str!`'d,
so a malformed catalog is a build-time failure, which is exactly what we want.)

---

## A. Probe v2 — hardware detection beyond RAM (`models/hardware.rs`; outside `local-runner`)

**Scope:** data collection only. `fits()` keeps its exact RAM-only signature + math (a profile with
every new field `None` yields exactly today's verdict — the conservative default is free). Hardware
detection must work whether or not a sidecar ever runs, so it stays OUTSIDE the `local-runner` feature,
like `probe()`/`fits()` today. The new syscalls live behind `#[cfg(target_os="macos")]` helpers with a
`#[cfg(not(macos))]` all-`None`/`false` stub, so `--no-default-features` and non-mac CI stay green
(`hardware.rs` gains its first `#[cfg]` branching).

**Brand string → family (verified live on an M3 Max: `sysctl -n machdep.cpu.brand_string` → "Apple
M3 Max").** Strip `"Apple "`, match `^(M\d+)( Pro| Max| Ultra)?$` against the known table below; any
unmatched string → `apple_chip_family: None` but `cpu_brand: Some(raw)` (kept for display/support).
`hw.model` (e.g. `Mac15,10`) is deliberately NOT a lookup key — it's a machine id, not a chip id, and
would need an ever-growing table for zero decision-relevant gain.

**`AppleChipFamily`** enum: `M1 M1Pro M1Max M1Ultra M2 M2Pro M2Max M2Ultra M3 M3Pro M3Max M3Ultra M4
M4Pro M4Max` (no `M4Ultra` — Apple hasn't shipped one as of 2026-07-22; an unrecognized `"Apple M4
Ultra"`/`"Apple M5 …"` must return `None`, never nearest-match). Binned SKUs are NOT separate variants;
binning is resolved by pairing the family with `GpuInfo.core_count` at lookup time.

**Starter bandwidth table** — `bandwidth_gbps_estimate(family, gpu_cores: Option<u32>) -> f64`, keyed
by `(family, core-bin)`, **defaults to the LOWER bin when core count is unknown** (conservative). EVERY
value is an ESTIMATE; the ~ ones flagged "verified" were cross-checked against Apple-published figures
via web search on 2026-07-22, the rest are order-of-magnitude:

| Family | bin | Est. GB/s | | Family | bin | Est. GB/s |
|---|---|---|---|---|---|---|
| M1 | — | ~68 | | M3 | — | ~100 |
| M1 Pro | — | ~200 | | M3 Pro | — | ~150 (known regression vs M2 Pro) |
| M1 Max | ≤24 | ~200 | | M3 Max | 30 (binned) | ~300 |
| M1 Max | 32 | ~400 (verified) | | M3 Max | 40 (full) | ~400 |
| M1 Ultra | — | ~800 (verified) | | M3 Ultra | — | ~800 (verified 819) |
| M2 | — | ~100 | | M4 | — | ~120 |
| M2 Pro | — | ~200 | | M4 Pro | — | ~273 (verified) |
| M2 Max | — | ~400 | | M4 Max | 32 (binned) | ~410 (verified) |
| M2 Ultra | — | ~800 | | M4 Max | 40 (full) | ~546 (verified) |

The lookup is a total fn (every enum variant has a row); the *fallibility* is one level up — whether
`apple_chip_family` is `Some` at all. Unknown family ⇒ `mem_bandwidth_gbps: None`, never a guess.

**GPU enumeration** — `system_profiler SPDisplaysDataType -json` via `std::process::Command` (no new
crate, no unsafe FFI; verified-live schema). Rules: one top-level `SPDisplaysDataType` array entry =
one GPU (do NOT count nested `spdisplays_ndrvs` — those are connected monitors); `sppci_model`/`_name`
→ name; `sppci_cores` → `core_count`; no `spdisplays_vram` key ⇒ `is_unified: true, vram_bytes: None`.
Discrete-card parsing (`"4096 MB"` → bytes, non-Apple vendor ⇒ `is_unified:false`) is **best-effort,
unverified** (no Intel/discrete Mac to test) — parse failure ⇒ `vram_bytes: None` (fail closed, never
guess). Multiple entries ⇒ record `gpus.len()>1` now, decide nothing from it (multi-GPU serving is
Wave 7.4). **Perf note:** once `probe()` shells out to `system_profiler` (hundreds of ms), cache the
profile (`OnceLock<HardwareProfile>` or an `AppState` field populated at boot) — `list_model_catalog`
(`ipc/mod.rs:692-694`) currently re-probes every call.

**`unified_memory = (os=="macos") && (arch=="aarch64")`** — computed, no syscall. Intel Macs and
Win/Linux are `false` in v1 (even a shared-memory AMD APU gets conservative `false` — no bandwidth
table for it; explicitly Wave 7.4).

**Fake-probe seam** — replace the un-object-safe `HardwareProbe` sketch with:
```rust
pub trait HardwareSource { fn snapshot(&self) -> HardwareProfile; }
pub struct RealHardwareSource;  impl HardwareSource for RealHardwareSource { fn snapshot(&self)->HardwareProfile { probe() } }
pub struct FakeHardwareSource(pub HardwareProfile); impl HardwareSource for FakeHardwareSource { fn snapshot(&self)->HardwareProfile { self.0.clone() } }
```
`probe()` stays a free fn (unchanged shape); `RealHardwareSource` is the thin DI wrapper. Most new
logic (`parse_brand_string`, `bandwidth_gbps_estimate`) is already pure/`self`-free and directly
testable; the trait exists so the recommender + any future IPC can swap a *whole profile*.

**Unknown-handling contract** (binds §B): (1) unknown bandwidth never upgrades a speed verdict —
suppress the estimate, don't mid-range-guess; (2) `gpus: None` is NOT "no GPU" — a `gpu_enumeration_known()`
helper must distinguish it so nothing forces CPU-only on an un-probed machine; (3) unknown chip family
never guesses a neighbor; (4) multi-GPU is descriptive-only in v1.

**Tests (pure/fixture, ~9):** fake-source round-trip; bandwidth known-family (M3Max/30→300, /40→400);
unknown-cores→lower-bin (M4Max/None→410 not 546); brand-parse table-driven over all known strings;
unmapped-future-chip→`None`; `unified_memory` true only for (macos,aarch64); `gpus:None` vs `Some(vec![])`
distinguished; serde round-trip old-4-field JSON → new struct defaults; `..Default::default()` compiles
old fixtures. Loosen `probe_returns_sane_values_on_this_machine` so new fields may be `None` on non-mac CI.

---

## B. Recommendation engine (`models/recommend.rs`, pure; RECONCILED to §A + §C)

A **pure** `recommend(profile: &HardwareProfile, catalog: &[CatalogEntry]) -> Recommendation` — no I/O,
fully fixture-testable. **The section as originally drafted does not compile against §A/§C; these are
the reconciliations (review blocking #1–#4), now the build contract:**

- **Drop the `Bandwidth` enum;** read `profile.mem_bandwidth_gbps: Option<f64>` directly.
- **Drop `available_ram_bytes`;** the fraction-of-total path is the ONLY v1 path (there is no live
  free-memory probe). `memory_pool` computes from `total_ram_bytes * FALLBACK_USABLE_FRACTION`.
- **Derive the memory pool from `§A` fields, honoring the Unknown contract:**
  ```rust
  enum PoolKind { UnifiedMemory, DiscreteVram, CpuRamOnly }
  fn memory_pool(p: &HardwareProfile) -> (u64, PoolKind) {
      if p.unified_memory { (frac(p.total_ram_bytes), UnifiedMemory) }
      else if let Some(vram) = primary_discrete_vram(p) { (vram, DiscreteVram) } // §A gpus[], best-effort
      else { (frac(p.total_ram_bytes), CpuRamOnly) }   // unknown-GPU and no-GPU both size on RAM
  }
  ```
  `primary_discrete_vram` reads `p.gpus` (Blocking #4: never a flattened top-level field); on Apple
  Silicon (`unified_memory`) we never reach the discrete branch. When GPU enumeration is unknown on a
  non-unified box the pool is RAM-based (conservative, identical math to CpuRamOnly) — the label is
  best-effort but the sizing never over-claims a phantom VRAM pool.
- **Catalog fields:** `CatalogEntry`/`QuantArtifact` per §0 (not `CatalogModel`/`QuantVariant`/
  `display_name`/`source_url`/`is_default`). `select_quant`'s default = `quants.iter().find(|q| q.quant
  == entry.default_quant)` (parse-time-guaranteed to exist — no `.expect()` landmine).
- **`refusal()` must filter to CURATED quants before `min_by_key`** (Blocking #3) — otherwise it can
  claim "even the smallest curated model…" about an uncurated placeholder. Uses `entry.name`.

**The real tradeoffs (unchanged logic, correct):**
- *Sizing* is architecture-blind: `required = chosen_quant.size_bytes + kv_cache_bytes(active_params,
  context_len) + os_headroom(kind)`. An MoE's file already contains every expert, so its RAM need
  tracks TOTAL params; a MoE and a dense model of equal file size need equal RAM.
- *Speed* is where they diverge: `predicted_tok_s = (bandwidth_gbps·1e9) / (quant.size_bytes ·
  active/total)`. Dense collapses to `bandwidth/file_size` (the memory-bandwidth roofline — each token
  streams the full weights once). A 30B-A3B MoE (`active/total ≈ 0.1`) is ~10× faster than a dense
  model of identical file size — this is exactly "big-unified-memory Macs favor MoE," and it falls out
  of the formula, not a special case. This is a **roofline upper bound** (real llama.cpp ≈ 60–85% of
  it); use it only to bucket Fast/Usable/Slow in the why-string, never as a literal promise.
- *Quant ladder:* pick the `default_quant` as the ceiling; step DOWN to a smaller curated quant under
  memory pressure; **never auto-step-UP** past the curated default (bigger quant = manual informed
  choice in the model manager later). `TooLarge` at every curated quant ⇒ the model is excluded.
- *Scoring:* `score = quality_tier.weight()(1/2/3) · fit_multiplier(Fits=1.0/Tight=0.6) ·
  speed_dampened([0.7,1.0])`. Speed is dampened into a narrow band so it only ever tie-breaks WITHIN a
  quality/fit tier (a tiny-but-fast model can't leapfrog a meaningfully more capable one); `TooLarge`
  is a hard pre-filter, never a multiplier. Ties → `id` ascending (deterministic).
- *Unknown-tolerant:* `mem_bandwidth_gbps: None` ⇒ `predicted_tok_s = None` ⇒ `SpeedTier::Unknown`,
  neutral speed component, and the why-string says "based on memory fit only." Degrade + say so, never
  silently.
- *Refuse loudly:* empty picks ⇒ `Recommendation::Refused { reason, external_endpoint_hint }` — the
  hint points at Settings → Providers / an external OpenAI-compatible endpoint (e.g. LM Studio on the
  LAN). Onboarding renders the refusal + an "I have my own server" CTA, never an empty picker. **Never
  silently degrade** (house invariant).

**Worked outcomes to pin as tests:** 16 GB M2 (bw 100) → picks the Balanced 7B (Fits, ~fast) over a
Tight Flagship 14B, 14B ranked 2nd not excluded. Heavily-loaded 8 GB Intel → **Refused** + external
hint. 128 GB M3 Ultra (bw 800) → MoE 30B-A3B beats an equal-tier dense 32B purely on active-param
speed. 16 GB headless Linux, bw Unknown → ranks on fit, `SpeedTier::Unknown`, why-string says "memory
fit only." Plus: steps down a tight default to a smaller quant that Fits; never upsells above the
default; `TooLarge` never appears; an uncurated-only catalog Refuses (never install-blind); a
MIXED curated/uncurated catalog refusal names a real *curated* model (Blocking #3 regression).

**IPC:** `recommend_models(state) -> Recommendation` alongside `probe_hardware`/`list_model_catalog`;
Onboarding step 2 renders `Pick.why` as the card copy. `list_model_catalog` stays as the "browse
everything, incl. what doesn't fit" Settings surface.

---

## C. Catalog v2 schema + proposed model list + cheap hash curation (`models/catalog.rs` + `.json`)

**Schema:** per §0 (the authoritative `CatalogEntry`/`QuantArtifact` + `quality_tier`/`default_quant`).
`CatalogEntryView`/`view_catalog` go **per-quant**: on 16 GB, Qwen3-14B's Q4_K_M (9 GB) is `Tight`
while its Q8_0 (15.7 GB) is `TooLarge` — the same entry needs a `Fit` per quant so the UI can say "pick
a smaller quant, that one fits." `best_fit` = best `Fit` across *curated* quants; `installable` =
`is_curated()`. `min_ram_bytes = ceil(smallest_curated_quant.size · WORKING_SET_OVERHEAD(1.3))`, reusing
the existing constant. `fits()` itself is untouched (takes a bare `u64`).

**Ripple points the build must hit** (cite so nothing's missed): `DownloadModelArgs` (`ipc/mod.rs:743-747`)
gains `quant: String` (a model is no longer one file); `download_model` (`ipc/mod.rs:771-810`) looks up
the specific `QuantArtifact` by `args.quant`, checks *that* artifact's `is_curated()`, passes *its*
`url`/`sha256` to `download_to_partial`/`verify_and_install` (was `entry.url`/`entry.sha256` at
`:795`/`:805`); `list_model_catalog`'s return shape nests `quants: Vec<QuantView>`; the 3 existing
`catalog.rs` tests + `Onboarding.svelte` consumer update to the nested shape.

**Proposed model list (OWNER DECISION — see the ask). All repos/filenames/sizes fetched LIVE from the
HF tree API on 2026-07-22; param counts + context lengths are family-doc recollection to re-verify at
curation.** Sizes are decimal GB.

| Tier (~RAM) | Role | HF repo | quant file | arch · params (total/active) | size | license |
|---|---|---|---|---|---|---|
| **Tiny ~8 GB / live-test** | primary | `Qwen/Qwen3-0.6B-GGUF` | `Qwen3-0.6B-Q8_0.gguf` | Dense · 0.6/0.6 | 0.64 GB | Apache-2.0 |
| | alt | `bartowski/Llama-3.2-1B-Instruct-GGUF` | `…-Q4_K_M.gguf` | Dense · ~1.2 | 0.81 GB | Llama-3.2 |
| | alt | `ggml-org/gemma-3-1b-it-GGUF` | `…-Q4_K_M.gguf` | Dense · ~1.0 | 0.81 GB | Gemma |
| **Small ~16 GB** | primary | `Qwen/Qwen3-8B-GGUF` | `Qwen3-8B-Q4_K_M.gguf` | Dense · ~8.2 | 5.03 GB | Apache-2.0 |
| | alt | `bartowski/Meta-Llama-3.1-8B-Instruct-GGUF` | `…-Q4_K_M.gguf` | Dense · ~8.0 | 4.92 GB | Llama-3.1 |
| | alt | `bartowski/Mistral-7B-Instruct-v0.3-GGUF` | `…-Q4_K_M.gguf` | Dense · ~7.3 | 4.37 GB | Apache-2.0 |
| **Medium ~32 GB** | primary | `Qwen/Qwen3-14B-GGUF` | `Qwen3-14B-Q4_K_M.gguf` | Dense · ~14.8 | 9.00 GB | Apache-2.0 |
| | alt | `unsloth/gemma-3-27b-it-GGUF` | `…-Q4_K_M.gguf` | Dense · ~27 | 16.55 GB | Gemma |
| | alt | `bartowski/phi-4-GGUF` | `phi-4-Q4_K_M.gguf` | Dense · 14 | 9.05 GB | MIT |
| **Large ~64 GB+ (MoE)** | primary (matches Lukas's LM Studio) | `ggml-org/Qwen3.6-35B-A3B-GGUF` | `…-Q4_K_M.gguf` | MoE · ~35/~3 | 20.42 GB | Apache-2.0 (verify) |
| | alt (more battle-tested MoE) | `Qwen/Qwen3-30B-A3B-GGUF` | `Qwen3-30B-A3B-Q4_K_M.gguf` | MoE · 30.5/3.3 | 18.56 GB | Apache-2.0 |
| | alt (dense fallback) | `Qwen/Qwen3-32B-GGUF` | `Qwen3-32B-Q4_K_M.gguf` | Dense · ~32.8 | 19.76 GB | Apache-2.0 |

Each proposed model ships its Q4_K_M default plus a couple of siblings (Q5_K_M/Q8_0) as `quants` so the
ladder has room. Notes for curation: `ggml-org/Qwen3.6-35B-A3B-GGUF` is real and llama.cpp's own team
endorses `llama-server -hf ggml-org/Qwen3.6-35B-A3B-GGUF` — but HF tags the base repo
`image-text-to-text`, so **S2 must confirm the base (non-`mmproj`) GGUF serves as a plain causal-LM via
`llama-server --model` with no vision requirement** before shipping it; its 262K/1M context claim is
unverified. `Qwen3-30B-A3B` (kept as alt) is the safer established MoE default. The `mmproj-*`/`dflash-*`
side files are NOT catalog entries.

**Cheap hash curation (verified live, the efficiency win):** `GET
https://huggingface.co/api/models/{owner}/{repo}/tree/main` (anonymous, a few KB regardless of file
size) returns per-file LFS metadata. **The gotcha:** there are two hashes — top-level `oid` (40 hex) is
the git-blob SHA-1 of the LFS *pointer stub* (WRONG), and `lfs.oid` (**64 hex, no `sha256:` prefix in
the current API**) is the SHA-256 of the actual file content — this is the value `download.rs::file_sha256`
computes and the one that belongs in `QuantArtifact.sha256`. `lfs.size` gives `size_bytes` in the same
response. So **10 of the 11 entries can be curated from the API alone, no multi-GB download**; the ONE
that must still be downloaded and independently re-hashed end-to-end is the tiny live-test model
(`Qwen3-0.6B-Q8_0.gguf`, 0.64 GB) — proving the whole fetch→verify→rename→register pipeline for real
rather than trusting the API's self-report for everything. (Curation script fails loudly if `lfs` is
absent on a multi-GB `.gguf` — that would be surprising and must not silently fall back to the git oid.)

*(Two real, live-verified sha256 examples already pulled this session, ready to seed the catalog:
`Qwen/Qwen3-30B-A3B-GGUF` Q4_K_M = `0d003f6662faee786ed5da3e31b29c978de5ae5d275c8794c606a7f3c01aa8f5`
(18,556,685,824 B); `Qwen/Qwen2.5-0.5B-Instruct-GGUF` q2_k =
`9ee36184e616dfc76df4f5dd66f908dbde6979524ae36e6cefb67f532f798cb8`.)*

---

## D. Sidecar — llama-server acquisition + supervision (`models/runner.rs`, behind `local-runner`)

**Binary facts (verified 2026-07-22 by actually downloading + hashing the real artifact):** latest
`ggml-org/llama.cpp` tag `b10088`; asset is `llama-b10088-bin-macos-arm64.tar.gz` (a `.tar.gz`, not
`.zip`; build-number not semver; 10,615,347 B, sha256 `e39658aa0af5acac893b2fdf58dc6480faf6254cfbc89b3a6c5f6ce71db9442e`
matching the release API's own `digest` field). It is **NOT a single binary** — `llama-server`
dynamically links **9 ggml/llama dylibs** (`otool -L` confirmed), so **Tauri `externalBin` (one file per
target-triple) is the wrong mechanism.** Metal shaders are **embedded** at build time
(`-DGGML_METAL_EMBED_LIBRARY=ON` in the CI release workflow) — no runtime `.metal`/`.metallib` to
resolve. The prebuilt is **ad-hoc signed only** (no Developer ID).

**Acquisition decision: (a) bundle at build time** (not download-on-demand, not build-from-source). It's
the only option that keeps "one-click" literally true AND gives the code-signing problem a real answer:
the sidecar rides inside our own signed+notarized `.app`, so `tauri build`'s codesign pass re-signs it
under our Developer ID (replacing the ad-hoc sig), and a subprocess spawned via `posix_spawn`/`execve`
by an already-approved parent doesn't re-trigger Gatekeeper. Download-on-demand is kept only as a
documented future "Advanced → newer llama-server build" fallback.

**Layout:** use `tauri.conf.json` `bundle.resources` (not `externalBin`) — today's `bundle` block
(`tauri.conf.json:29-39`) has no `resources` key. Vendor a PRUNED tree
`src-tauri/vendor/llama-cpp/macos-arm64/` = `llama-server` + the 11 dylibs `otool -L` actually needs
(~22.3 MiB measured), dropping the ~20 unused CLI tools. **Flatten the 2-hop dylib symlink chains to one
real file per `.0.dylib` rpath name** (symlinks break through tar/codesign/notarization). Add
`vendor/llama-cpp/VERSION` (`b10088`) + `MANIFEST.sha256` re-checked by a `cargo test` assertion (an
on-brand "verified-before-runnable" echo for the first-party binary; the .app signature is the real
guarantee). Bundle the MIT `LICENSE` for attribution. Resolve at runtime via
`app.path().resolve("llama-cpp", BaseDirectory::Resource)`. **Flag for S4 build-verify:** confirm
`codesign --deep`/notarization actually covers `Resources/`; if not, add one explicit `codesign` loop
over the vendor dir pre-notarization.

**Feature:** `local-runner` gates every symbol in `models/runner.rs`; NO new crates (`tokio::process`
from the existing `tokio "full"`, `reqwest` already used by `download.rs`). `default =
["onnx-classifier","local-runner"]`; `--no-default-features` drops it and must still compile.

**Supervision seams (fake-testable, no real binary):** `trait ProcessSpawner{spawn}` / `trait
SpawnedProcess{id,try_wait,kill}` / `trait HealthCheck{is_healthy}` + `SidecarCommand{bin,args}` kept as
data so tests assert exact argv. `LocalRunnerSupervisor{spawner,health,running: RwLock<HashMap<catalog_id,RunnerHandle>>}`.

**`build_args` (macOS/Metal v1 — hardcoded, no backend branching yet):** `--model <gguf> --host
127.0.0.1 --port <p> -ngl 999 --threads <cpu_cores> --ctx-size <catalog.context_len, else 4096>
--parallel 2`. **`--host 127.0.0.1` is load-bearing, NEVER `0.0.0.0`** — the privacy story rests on
`is_private_endpoint` treating `127.0.0.0/8` private; a sidecar on all interfaces is LAN-reachable
regardless of our `base_url` string (routing only vets our *own* outbound call, not who else can reach
the port). **This must get its own pinned unit test** (review nit): `build_args()` output always
contains `--host 127.0.0.1` and never `0.0.0.0`.

**Lifecycle:** free port via `TcpListener::bind(("127.0.0.1",0))` then drop+spawn (tiny TOCTOU, health
check is the net); `wait_healthy` polls `GET /v1/models` up to ~30s (validate on real model at S4) —
**a model is never registered as a Provider until healthy** (runtime analog of verified-before-runnable);
restart-with-backoff 1/2/4/8/16s cap 5 → distinct `runner_failed` state (≠ `quarantined`: process won't
stay up vs file integrity failed — different Settings copy, different remedy); idle shutdown after ~10
min guarded UNCONDITIONALLY by an `in_flight` counter (a 5-min generation is never killed);
`--parallel 2` lets a background `complete()` call and an interactive stream not block each other.

**Teardown — never a zombie:** three triggers → one `stop()` (`start_kill` + ~2s grace `try_wait` →
hard-kill): idle; explicit (`remove_local_model` must `stop()` before deleting the file); app-exit
(`RunEvent::ExitRequested/Exit` → best-effort `stop_all()`). The gap clean handlers can't close — a hard
crash of *our* process (macOS has no Linux `PR_SET_PDEATHSIG`) — is closed by a **pidfile**
(`<storage>/models/local/<id>.pid` with PID+start-time), reaped at boot in the same pass as the integrity
sweep, right after `crash_recovery::run_boot_pass` (`lib.rs:69`), verifying the PID is alive AND looks
like our sidecar before killing (avoid PID reuse).

**Provider registration through existing plumbing (zero routing changes):** provider id = pure
`format!("local-runner:{catalog_id}")` — **no `provider_id` schema column** (the one the old draft
proposed never landed; a pure fn can't drift). `ensure_running(mm, storage, catalog_id)` is the ONE
lazy-spawn seam: warm-path returns the already-registered provider; else load the `ready` `model_catalog`
row (refuse `quarantined`/`runner_failed`), pick a port, spawn, `wait_healthy`, then
`mm.add_provider(Provider::new("local-runner:{id}", name, "http://127.0.0.1:{port}", None,
ProviderKind::Local))`. That provider satisfies `is_local() && is_private()` → `find_local_provider()`
(`loop_mod.rs:1131-1136`) + `enforce_local_routing` LocalRequired (`routing.rs:50,60-68`) with no new
code, and the usage ledger books **$0** (local). **Call-site change** at the two `find_local_provider`-
adjacent branches (`loop_mod.rs:388,427` and the `enforce_local_routing` path `~1752-1763`): when the
in-memory snapshot is empty, before concluding "no local provider," bring up a `ready` row — the
seat/caller's bound local model if any, else the most-recently-added `ready` row (`list_models()` is
already `ORDER BY added_at DESC`) — `.await ensure_running`, retry the lookup once. The ephemeral
`base_url` (port re-picked per launch) is **never** persisted to `endpoints` (that table is
user-configured cloud/custom only); local-runner providers are pure derived session state.
`AppState` gains `#[cfg(feature="local-runner")] local_runner: Arc<LocalRunnerSupervisor>`.

**Boot integrity re-check — wires the callerless `set_model_status` (`global.rs:623`, zero callers
today):** `sweep_local_model_integrity_at_boot(storage, rehash)` called once after
`hydrate_providers_from_storage` (`lib.rs:90`), best-effort/logged/never-brick (crash-recovery
discipline). For each non-quarantined row: missing/unreadable OR size-mismatch (cheap, catches
truncation) OR (if `rehash`) hash-mismatch ⇒ `set_model_status(id,"quarantined")` (fail-closed,
Invariant 2). Full re-hash is opt-in (a "verify all models now" Settings action) — hashing multi-GB
files every boot is too costly by default; existence+size catches the common cases. Renamed from the
old `start_local_runners_from_catalog` because it does NOT spawn — spawn stays lazy; this is a pure
file sweep + the orphan-sidecar reap. A quarantined row is never handed to `ensure_running` (the
`status != "ready"` guard); `list_local_models` already surfaces `status` for re-download.

**Testability:** `FakeSpawner` + `FakeBehavior{HealthyImmediately, NeverHealthy, CrashesAfter,
RefusesToDie}` + tokio `test-util` `pause/advance` (dev-dep) → assert health-timeout→no-registration,
exact backoff schedule→`runner_failed`, idle-stop only when `in_flight==0`, kill-once-per-handle,
`--host 127.0.0.1` argv, quarantine on tampered/missing file (pure, like `verify_and_install`'s tests).
Plus the ONE env-gated live test (`live_local_runner_roundtrip`, mirroring `live_native_tool_call_roundtrip`):
`LHP_LLAMA_SERVER_BIN` + `LHP_TEST_GGUF` → real spawn → poll `/v1/models` → one real
`/v1/chat/completions` via `ModelClient` → `stop()` → assert the child is gone. Self-skips when env unset.

---

## Build slices (each committable; gate = `cargo test --lib` + `clippy` 0-err + `build --lib
--no-default-features` + `npm run build`/`check`, adversarial review before commit; update ROADMAP+HANDOFF)

- **S1 — Probe v2** (§A). Pure/fixture-testable; the `system_profiler`/`sysctl` calls behind macOS cfg.
- **S2 — Catalog v2 schema + REAL sha256 curation** (§C). Schema first (with parse-time validation +
  per-quant view), then curate the Lukas-approved list via the HF API (download+re-hash only the tiny
  live-test model). Closes ROADMAP current-list item 8. **Gated on the model-list decision.**
- **S3 — Recommendation engine** (§B). Pure; builds on the S1 struct + S2 schema (§0 contract).
- **S4 — Sidecar** (§D). Behind `local-runner`. Vendor `b10088`, supervision, `ensure_running` +
  call-site wiring, `sweep_local_model_integrity_at_boot`, fake-spawner tests + the env-gated live test.
- **S5 — Onboarding + Settings** (base-doc S5, extended). Wire `Onboarding.svelte`: probe → **recommend**
  → download (progress/resume off `model:download-progress`) → verify → serve → default seat assignment;
  first-run gate (via `list_providers` IPC, not the private `find_local_provider`); `complete_onboarding`
  flag in `app_settings`; **BYO-GGUF import** (a verified local-file path → `model_catalog` row); the
  **"I have my own server" external-endpoint path equally prominent on the same screen** (reuses
  `add_provider`). The refusal path renders a real CTA, never an empty picker.

## Review fixes folded in (the skeptical pass — all resolved above)

**Blocking:** #1 `HardwareProfile` field mismatch → §0 authoritative struct, §B reconciled (drop
`Bandwidth` enum + `available_ram_bytes`). #2 catalog struct mismatch + missing `quality_tier`/`is_default`
→ §0 adds `quality_tier` + `default_quant` (parse-time validated, no `.expect()`). #3 `refusal()`
uncurated mislabel → filter curated before `min_by_key` + mixed-catalog test. #4 flattened `vram_bytes`
re-conflates unknown-vs-no GPU → `memory_pool` reads `p.gpus` + `gpu_enumeration_known`. **Nits:** fix the
"10 tests" miscount (it's 3 fns); add the per-quant `Fit` catalog test (14B Q4 Tight / Q8 TooLarge on
16 GB); S2 must confirm the `Qwen3.6-35B-A3B` base GGUF serves as plain causal-LM (HF tags it multimodal);
pin the `--host 127.0.0.1` argv test; note `derive(Eq)` removal (safe) and the `COMFORTABLE_FRACTION`
`pub(crate)` bump ownership (lands with whichever of S1/S3 comes first).

**Invariant sweep (all upheld):** verified-before-runnable + fail-closed (per-quant `is_curated`,
boot quarantine, health-gate-before-register); `--no-default-features` builds (probe cfg-stubbed, sidecar
feature-gated, no new deps); privacy gate covers the sidecar (`127.0.0.1` Local ⇒ `is_private`,
`--host` locked to loopback); probe Unknown fails closed (never "fits"/never over-claims speed);
on-demand-spawn vs boot-recheck coherent (sweep is file-only, spawn is lazy).

## OWNER DECISION (before S2 hash curation)

The one product call: **the concrete catalog model list** (§C table). Sub-questions: (1) approve the 11
repo/quant picks as-is? (2) ship `Qwen3.6-35B-A3B` as the large-tier primary (matches your LM Studio
daily driver) given its context claim is unverified + HF tags it multimodal, or lead with the
better-established `Qwen3-30B-A3B`? (3) green-light S2 curation on the approved list. Everything else
(probe v2, recommender, catalog v2 schema, sidecar) proceeds without a decision.


---

# REVISION 2026-07-22b — PRODUCT REDIRECT (Lukas): HuggingFace model search + an interactive hardware calculator (supersedes the curated bundled catalog)

> **Lukas, 2026-07-22 (with an LM Studio screenshot):** *"I want it to work like the one in LM
> Studio where you can search for a model — I think they just use Hugging Face for it. … It should
> do the calculations on its own regardless of what we say here; I'm not looking for a suggestions
> page that will get outdated, I'm looking for a **calculator** to help a user configure the best
> output for **tps** for the hardware they have. It should be **interactive**, take into account
> **quants for KV and model weights**, it should think of **context size**, the whole thing."*

**What this changes.** The "curated bundled catalog + pinned per-model hashes" framing of REVISION
2026-07-22 §C is **superseded**. A hardcoded model list goes stale and is not what Lukas wants. The
discovery half of M8 becomes, mirroring the LM Studio screenshot:

1. **A HuggingFace model SEARCH** (search box by name/author + a format filter + a sort + a
   **Staff picks** default list + capability badges + a per-model detail pane with selectable
   quants) — LM Studio does exactly this against HF, and so do we.
2. **An interactive hardware CALCULATOR** — for the model + quant the user is looking at, compute,
   live, whether it fits *their* machine and roughly how fast it will run (tokens/sec), as a
   function of **weight quant**, **KV-cache quant**, and **context size**. LM Studio's green "Full
   GPU Offload Possible" badge is the minimal version of this; Lukas wants the richer TPS/KV/context
   calculator.

**What SURVIVES unchanged from the prior revision** (this is a re-scope of the *discovery/catalog*
slices, not a teardown): **Probe v2 (§A / S1) — DONE and directly needed** (the calculator's whole
input is the hardware profile it produces). The **verified downloader + HF host allowlist** (§C's
`download.rs`, already shipped). The **sidecar (§D / S4)** — still runs the chosen GGUF (now at the
user-chosen context size). And the **recommendation MATH** (§B) is not thrown away — it is
*repurposed* as the calculator's pure core (the MoE-vs-dense / bandwidth / quant / fit logic is
exactly the calculator; it just gains KV-quant + context as first-class interactive inputs).

**What is DEPRECATED:** the bundled `catalog.json` as *the* model list, and **pre-curating a fixed
set of sha256 hashes** (S2 as written). The verified-before-runnable invariant is UNCHANGED — the
sha256 now comes from **HF's tree API `lfs.oid` at selection/download time** (the mechanism verified
in §C, still 64-hex, still what `download.rs::file_sha256` checks the downloaded bytes against), so
nothing is trusted-before-verified; there is just no stale hardcoded list. **The curated list I
proposed in §C is repurposed as the "Staff picks" seed** (the default rows shown before the user
searches — the screenshot has exactly this), not the whole story.

## New components

### 1. HF model search service (`models/hf_search.rs`, new; outside `local-runner`)
Query the public, anonymous HF API (same host allowlist as downloads — `huggingface.co`):
- **Search:** `GET https://huggingface.co/api/models?search=<q>&filter=gguf&sort=<downloads|trendingScore|lastModified>&limit=N` — verified live this session: returns per model `id`, `downloads`, `likes`, `tags`, `library_name`. Tags carry the capability/label signal the screenshot's badges need (`image-text-to-text`/vision, `moe`, `conversational`, `license:*`) and the publisher (`lmstudio-community`, `unsloth`, `bartowski`, official `Qwen`/`google`). Map to a `HfModelSummary { id, downloads, likes, tags, publisher }`.
- **Staff picks default** (no query yet): a small curated seed (the repurposed §C list) filtered/sorted for quality — OR simply `sort=downloads&filter=gguf` top-N from trusted publishers (`lmstudio-community`, `ggml-org`, `unsloth`, official orgs). The seed is a *starting view*, refreshed by search, so it never "goes outdated" the way a hardcoded install list would.
- **Per-model files + quants:** `GET .../api/models/{id}/tree/main` (verified in §C) → the list of `*.gguf` files, each with `lfs.oid` (sha256, 64-hex) + `lfs.size` (bytes). Group by quant (parse `Q4_K_M`/`Q8_0`/… from the filename) → the selectable "Download Options" dropdown in the detail pane, each row showing size — exactly the screenshot's `Q4_0 · 7.15 GB` control. **This is also where the sha256 for verified download comes from — no pre-curation.**
- Host-allowlist reuse: every HF URL runs through `download::host_allowed` before a request (SSRF/allowlist discipline unchanged).

### 2. GGUF metadata reader (`models/gguf_meta.rs`, new) — the calculator's model inputs
The calculator's KV-cache term needs architecture params the file itself carries. Two tiers,
honest-fallback:
- **Cheap repo summary** (verified live): `GET .../api/models/{id}?blobs=false` returns a `gguf`
  object = `{ architecture, context_length (native max), total (parameter count), totalFileSize }`.
  Enough for weights sizing, the native context ceiling, and dense-vs-MoE hinting — but it LACKS
  `block_count`/`head_count_kv`/`embedding_length`, so KV-cache sizing from this alone is not exact.
- **Exact header read** (for the real KV number): GGUF stores its metadata KV block at the FRONT of
  the file (before tensor data), so a **ranged `GET` of the first ~1–4 MB** of the chosen `.gguf`
  URL yields the full header without downloading the weights. Parse the GGUF metadata KVs we need:
  `general.architecture`, `{arch}.block_count` (n_layers), `{arch}.attention.head_count` (n_heads),
  `{arch}.attention.head_count_kv` (n_kv_heads — GQA), `{arch}.embedding_length` (d_model),
  `{arch}.attention.key_length`/`value_length` (head_dim, when present), `{arch}.context_length`,
  `general.parameter_count`. This is precisely how LM Studio and the online GGUF VRAM calculators
  work. **Honest fallback:** if the ranged read fails or a key is absent, fall back to the repo
  summary + a documented KV estimate and **mark the KV figure "approximate"** in the UI (never a
  silent guess — the house rule).

### 3. The interactive calculator (`models/calculator.rs`, pure — the heart of the redirect)
A pure, unit-testable engine — no I/O; the search/metadata layers feed it data, it computes.

```rust
pub enum KvCacheQuant { F16, Q8_0, Q4_0 }   // llama.cpp --cache-type-k/v
impl KvCacheQuant { pub fn bytes_per_elem(self) -> f64 { match self { F16 => 2.0, Q8_0 => 1.0, Q4_0 => 0.5 } } }

pub struct ModelSpec {          // from the GGUF reader
    pub architecture: String,
    pub total_params_b: f64,
    pub active_params_b: f64,   // == total for dense; < total for MoE
    pub n_layers: u32,
    pub n_kv_heads: u32,
    pub head_dim: u32,
    pub native_context_len: u32,
}
pub struct CalcInput {
    pub weight_quant_file_bytes: u64,   // EXACT selected-quant size from HF lfs.size (not estimated)
    pub kv_quant: KvCacheQuant,
    pub context_len: u32,               // user-chosen, ≤ native (or YaRN-extended, flagged)
    pub active_fraction: f64,           // active_params_b / total_params_b
}
pub struct CalcOutput {
    pub weights_bytes: u64,
    pub kv_cache_bytes: u64,
    pub overhead_bytes: u64,
    pub total_required_bytes: u64,
    pub fit: Fit,                       // reuse §A: Fits/Tight/TooLarge vs the hardware pool
    pub full_gpu_offload: bool,         // total ≤ pool (LM Studio's badge)
    pub predicted_tokens_per_sec: Option<f64>,   // None when bandwidth Unknown
    pub notes: Vec<String>,             // honest caveats (approx KV, roofline, YaRN context, etc.)
}
```

**Formulas (each an explicit, caveated model):**
- **KV cache** (the term Lukas called out): `kv_bytes = 2 · n_layers · n_kv_heads · head_dim ·
  context_len · kv_quant.bytes_per_elem()`. The `2` = K and V; GQA (`n_kv_heads` ≪ `n_heads`) is
  captured exactly, which is why it matters — a 128K-context model's KV can dwarf a small model's
  weights, and halving it via `Q8_0`/`Q4_0` cache is a real lever the user should see move.
- **Weights** = the EXACT selected quant file's `lfs.size` (from HF) — better than any per-param
  estimate, since it's the real artifact byte count.
- **Overhead** = OS headroom (reuse §B's `OS_HEADROOM_*` estimates) + a modest compute/activation
  buffer (estimate, scales mildly with context) — clearly an estimate.
- **Pool / fit** = §A's unified-RAM-fraction (Apple) or VRAM-else-RAM, compared via §B's
  `fit_in_pool` → `Fits/Tight/TooLarge`. `full_gpu_offload = total_required ≤ pool`.
- **TPS (roofline, bandwidth-bound decode):** `tps ≈ (mem_bandwidth_gbps · 1e9) / (active_weight_bytes
  + kv_cache_bytes)`, where `active_weight_bytes = weights_bytes · active_fraction` (MoE reads only
  active experts per token; dense = full weights). Bucket **Fast/Usable/Slow**; label it an upper
  bound (real ≈ 60–85%); suppress the number entirely when `mem_bandwidth_gbps` is `None` (Unknown
  → "based on fit only," never a fabricated speed). This is the interactive payoff: as the user drags
  context up, `kv_cache_bytes` rises, `tps` falls, and `fit` can flip — visibly.
- **Refuse-loudly** still holds: if nothing fits at the smallest quant / shortest context, the
  calculator says so and points at the external-endpoint path (never a silent empty state).

### 4. Interactive UI (S5, replaces the static onboarding catalog step)
Mirror the screenshot: a search field + format/sort controls + Staff-picks list (rows: icon, name,
one-line desc, updated-ago, capability badges) + a detail pane (params/arch/format/capabilities,
downloads/likes, a **quant dropdown** with sizes, and — richer than LM Studio — **live KV-quant +
context-size controls** driving a memory bar + `Fits/Tight/TooLarge` badge + a `~N tok/s` estimate
that updates as the user changes any knob). The "Download" button runs the existing verified
download (HF `lfs.oid` sha256) → sidecar serve → seat. The **external-endpoint path stays equally
prominent** on the same screen (an "I run my own server / LM Studio on my network" tab → the existing
`add_provider`).

## Revised build slices (supersede REVISION 2026-07-22's S2/S3; S1/S4 stand)

- **S1 — Probe v2** — **DONE** (`1c8bca0`). The calculator's hardware input. No change.
- **S2′ — HF search + GGUF metadata reader** (replaces "catalog v2 curation"). `models/hf_search.rs`
  (search + tree→quants+sha256) + `models/gguf_meta.rs` (repo summary + ranged header parse, honest
  fallback). Reuses `download::host_allowed`. Gate: search returns real results (env-gated live test
  hitting HF, self-skipping offline, mirroring the existing live-test pattern) + pure parsers
  (tag→capabilities, filename→quant, GGUF header bytes→`ModelSpec`) unit-tested with fixtures.
- **S3′ — the calculator engine** (repurposes §B's math). `models/calculator.rs` pure
  `calculate(hw, model, input) -> CalcOutput` + the KV/weights/overhead/TPS/fit formulas above.
  Gate: pure fixture tests — KV scales with context×n_kv_heads×kv_quant; Q4_0 cache halves KV vs
  F16; a long context flips `Fits`→`TooLarge`; MoE active-fraction makes TPS ~10× a same-size dense;
  Unknown bandwidth → `predicted_tokens_per_sec: None` + a "fit only" note; refuse-loudly when
  nothing fits. (`bundled_catalog` becomes the small Staff-picks seed, not the install list.)
- **S4 — Sidecar** — unchanged (§D), plus: pass the user-chosen `--ctx-size` from the calculator
  through `build_args` (the §D coordination point already flagged), and `--cache-type-k/v` from the
  chosen KV quant.
- **S5 — Search + calculator UI + first-run + BYO-GGUF + external-endpoint path** (replaces the
  static onboarding catalog step). The interactive screen above.

## Invariants — all still upheld
verified-before-runnable (HF `lfs.oid` sha256 verified against downloaded bytes — now at
selection/download time, no pre-curation) · host allowlist (HF only, incl. search + ranged header
reads) · privacy gate (sidecar Local+Private, `--host 127.0.0.1`) · `--no-default-features` (search/
calc/metadata are pure/std+reqwest, no new features; sidecar stays behind `local-runner`) ·
refuse-loudly (calculator says so + points external when nothing fits) · **honest Unknown** (Unknown
bandwidth → no TPS number; failed GGUF header read → KV marked approximate, never silently guessed).

**⚠ TRUST-ROOT HONESTY (added by the 2026-07-22 independent review — the paragraph above
originally overclaimed).** "Verified-before-runnable is unchanged" is true only in the narrow
bytes-match-a-hash sense. Pre-redirect, the expected sha256 was maintainer-curated and pinned in
`catalog.json` — an out-of-band trust root independent of the download source. Post-redirect it is
**self-reported by the same host at the same time** as the file (`lfs.oid`). That still catches
transport/CDN corruption and partial downloads; it can NO longer catch a malicious or compromised
HF repo (whoever controls the repo controls both bytes and hash). This is the same posture LM
Studio accepts, and it's a reasonable tradeoff for a live search — but it must be taken knowingly,
so **S2′ carries a compensating REQUIREMENT: Staff-picks / default rows are limited to a
trusted-publisher allowlist (official orgs + `lmstudio-community`/`ggml-org`/`unsloth`/`bartowski`),
and an arbitrary-search result outside that list is visibly labeled ("community model — provenance
is the publisher's") before download.** The repo-trust decision then sits with the user per model,
never silently.

## Open (for Lukas / the UX pass)
- Staff-picks seed source: the repurposed §C list vs. a live "top GGUF from trusted publishers" query
  (or both). Default recommendation: a tiny trusted seed + live search as the primary path.
- Which capability badges to surface (vision / tool-use / reasoning) and from what signal (HF tags
  are coarse; the GGUF `general.*`/chat-template can refine) — the screenshot shows all three.
- Whether the calculator also exposes an "auto-pick best quant for my machine" one-click (the §B
  ranking, now over search results) alongside the manual interactive knobs — likely yes.
