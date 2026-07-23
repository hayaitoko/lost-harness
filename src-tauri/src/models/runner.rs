//! Wave 5.3 / M8 S4 — the bundled **llama.cpp sidecar**: acquisition is
//! bundle-at-build (`vendor/llama-cpp` → `bundle.resources`, re-signed under
//! our Developer ID by `tauri build`), supervision + lazy spawn live here.
//! Design: `docs/plans/2026-07-18-m8-model-lifecycle-design.md` §D.
//!
//! The shape, in one breath: a verified (`ready`) `model_catalog` row becomes a
//! callable **Local** provider the FIRST time something needs a local model —
//! [`ensure_running`] is the ONE lazy-spawn seam (wired into the agent loop's
//! `find_local_provider` empty-snapshot branches). The spawned `llama-server`
//! binds **`127.0.0.1` only** (load-bearing: the privacy story rests on the
//! port not being LAN-reachable — routing only vets our own outbound call, not
//! who else can reach the socket), is health-checked before registration (a
//! model is NEVER a provider until `/v1/models` answers — the runtime analog of
//! verified-before-runnable), restarts with backoff when it dies (1/2/4/8/16 s,
//! then the distinct `runner_failed` state — "won't stay up" ≠ `quarantined`
//! "file integrity failed"), idles out after ~10 min guarded UNCONDITIONALLY by
//! an in-flight counter (a 5-minute generation is never killed), and is torn
//! down on idle / explicit remove / app exit. The gap clean handlers can't
//! close — a hard crash of OUR process — is closed by a **pidfile** reaped at
//! the next boot (PID + start-time recorded; the reaper verifies the PID is
//! alive AND still looks like our sidecar before killing, to survive PID
//! reuse).
//!
//! Everything is seam-injected ([`ProcessSpawner`] / [`HealthCheck`]) so the
//! whole lifecycle is unit-tested with a fake spawner + tokio's paused clock —
//! no real binary, no real sleeping. The ONE live test
//! (`live_local_runner_roundtrip`, env-gated) exercises the real vendored
//! binary end-to-end.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use parking_lot::{Mutex, RwLock};
use tokio::time::{Duration, Instant};

use crate::models::calculator::KvCacheQuant;
use crate::models::manager::ModelManager;
use crate::models::provider::{Provider, ProviderKind};
use crate::storage::Storage;

/// Default `--ctx-size` when the caller didn't carry a calculator choice
/// through (design §D: "catalog.context_len, else 4096").
const DEFAULT_CTX_SIZE: u32 = 4096;

// ---------------------------------------------------------------------------
// The command (kept as DATA so tests assert exact argv)
// ---------------------------------------------------------------------------

/// A fully-resolved sidecar invocation. Pure data — [`build_args`] constructs
/// it, the spawner executes it, tests assert on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidecarCommand {
    pub bin: PathBuf,
    pub args: Vec<String>,
}

/// llama.cpp `--cache-type-k/v` argument for a KV-cache quant.
fn cache_type_arg(q: KvCacheQuant) -> &'static str {
    match q {
        KvCacheQuant::F16 => "f16",
        KvCacheQuant::Q8_0 => "q8_0",
        KvCacheQuant::Q4_0 => "q4_0",
    }
}

/// Build the sidecar argv (macOS/Metal v1 — hardcoded backend, no branching).
/// **`--host 127.0.0.1` is load-bearing and pinned by test** — NEVER `0.0.0.0`
/// (a sidecar on all interfaces is LAN-reachable regardless of how we route our
/// own calls). The calculator's chosen context size and KV-cache quant pass
/// through here (design §D + the 22b redirect's S4 note).
pub fn build_args(
    bin: &Path,
    model_path: &Path,
    port: u16,
    threads: u32,
    ctx_size: Option<u32>,
    kv_quant: Option<KvCacheQuant>,
) -> SidecarCommand {
    let mut args = vec![
        "--model".to_string(),
        model_path.to_string_lossy().into_owned(),
        "--host".to_string(),
        "127.0.0.1".to_string(),
        "--port".to_string(),
        port.to_string(),
        "-ngl".to_string(),
        "999".to_string(),
        "--threads".to_string(),
        threads.to_string(),
        "--ctx-size".to_string(),
        ctx_size.unwrap_or(DEFAULT_CTX_SIZE).to_string(),
        "--parallel".to_string(),
        "2".to_string(),
    ];
    if let Some(q) = kv_quant {
        args.push("--cache-type-k".to_string());
        args.push(cache_type_arg(q).to_string());
        args.push("--cache-type-v".to_string());
        args.push(cache_type_arg(q).to_string());
    }
    SidecarCommand { bin: bin.to_path_buf(), args }
}

/// A free loopback port, picked by binding port 0 and reading what the OS
/// assigned. Tiny TOCTOU between drop and spawn — the health check is the net.
pub fn pick_free_port() -> Result<u16> {
    let l = std::net::TcpListener::bind(("127.0.0.1", 0))?;
    Ok(l.local_addr()?.port())
}

// ---------------------------------------------------------------------------
// Seams: process + health (fake-testable, no real binary)
// ---------------------------------------------------------------------------

/// A spawned sidecar process — the minimal surface supervision needs.
pub trait SpawnedProcess: Send {
    fn id(&self) -> Option<u32>;
    /// `Ok(Some(_))` once the process has exited; `Ok(None)` while running.
    fn try_wait(&mut self) -> Result<Option<i32>>;
    /// Begin killing (SIGKILL). Idempotence is the SUPERVISOR's job — this may
    /// error if called twice.
    fn start_kill(&mut self) -> Result<()>;
}

/// Spawns sidecar processes. The real impl shells `tokio::process`; tests
/// inject a fake.
pub trait ProcessSpawner: Send + Sync {
    fn spawn(&self, cmd: &SidecarCommand) -> Result<Box<dyn SpawnedProcess>>;
}

/// Answers "is the server at `base_url` serving?" — the real impl GETs
/// `{base_url}/models` (llama-server's `/v1/models`). Boxed-future shape so the
/// trait stays object-safe.
pub trait HealthCheck: Send + Sync {
    fn is_healthy<'a>(&'a self, base_url: &'a str) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>>;
}

/// The real spawner: `tokio::process::Command`, stdio nulled, `kill_on_drop`
/// as the last-resort teardown net.
pub struct TokioSpawner;

struct TokioChild(tokio::process::Child);

impl SpawnedProcess for TokioChild {
    fn id(&self) -> Option<u32> {
        self.0.id()
    }
    fn try_wait(&mut self) -> Result<Option<i32>> {
        Ok(self.0.try_wait()?.map(|s| s.code().unwrap_or(-1)))
    }
    fn start_kill(&mut self) -> Result<()> {
        Ok(self.0.start_kill()?)
    }
}

impl ProcessSpawner for TokioSpawner {
    fn spawn(&self, cmd: &SidecarCommand) -> Result<Box<dyn SpawnedProcess>> {
        let child = tokio::process::Command::new(&cmd.bin)
            .args(&cmd.args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("spawning sidecar {:?}", cmd.bin))?;
        Ok(Box::new(TokioChild(child)))
    }
}

/// The real health check: `GET {base_url}/models` with a short per-poll
/// timeout. Loopback-only traffic (the sidecar binds 127.0.0.1).
pub struct HttpHealthCheck {
    client: reqwest::Client,
}

impl HttpHealthCheck {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(2))
                .build()
                .expect("static reqwest builder"),
        }
    }
}

impl Default for HttpHealthCheck {
    fn default() -> Self {
        Self::new()
    }
}

impl HealthCheck for HttpHealthCheck {
    fn is_healthy<'a>(&'a self, base_url: &'a str) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
        Box::pin(async move {
            let url = format!("{base_url}/models");
            matches!(self.client.get(&url).send().await, Ok(r) if r.status().is_success())
        })
    }
}

// ---------------------------------------------------------------------------
// Supervisor
// ---------------------------------------------------------------------------

/// Tunable timings — injected so tests run on tokio's paused clock with tiny
/// values. Defaults are the design's production values.
#[derive(Debug, Clone)]
pub struct SupervisorConfig {
    /// How long a fresh spawn gets to answer the health check before the
    /// attempt is failed (design: ~30 s — model load can be slow).
    pub health_timeout: Duration,
    /// Poll interval within the health window.
    pub health_poll: Duration,
    /// Backoff between failed attempts. Length = retry cap (design: 5).
    pub backoff: Vec<Duration>,
    /// Idle shutdown after this long with zero in-flight requests.
    pub idle_shutdown: Duration,
    /// Grace between `start_kill` and giving up on reaping the exit.
    pub kill_grace: Duration,
    /// Crash-monitor poll interval.
    pub monitor_poll: Duration,
    /// Where pidfiles are written (`<storage>/models/local`). `None` = don't
    /// write (tests).
    pub pidfile_dir: Option<PathBuf>,
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        Self {
            health_timeout: Duration::from_secs(30),
            health_poll: Duration::from_millis(500),
            backoff: [1u64, 2, 4, 8, 16].map(Duration::from_secs).to_vec(),
            idle_shutdown: Duration::from_secs(600),
            kill_grace: Duration::from_secs(2),
            monitor_poll: Duration::from_secs(1),
            pidfile_dir: None,
        }
    }
}

/// Why a runner isn't running. `Failed` is the design's distinct
/// `runner_failed` state: the PROCESS won't stay up (vs `quarantined` = the
/// FILE failed integrity) — different Settings copy, different remedy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunnerState {
    Starting,
    Healthy,
    /// Exhausted the restart schedule. The message is operator-facing.
    Failed(String),
}

/// One live runner. `in_flight` guards idle shutdown unconditionally; the
/// `killed` flag makes teardown kill-once.
pub struct RunnerHandle {
    pub catalog_id: String,
    pub port: u16,
    pub base_url: String,
    cmd: SidecarCommand,
    process: Mutex<Box<dyn SpawnedProcess>>,
    in_flight: AtomicU32,
    last_used: Mutex<Instant>,
    killed: AtomicBool,
}

/// RAII in-flight marker: hold it across a model call; dropping it decrements
/// the counter and refreshes the idle clock.
pub struct InFlightGuard {
    handle: Arc<RunnerHandle>,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.handle.in_flight.fetch_sub(1, Ordering::SeqCst);
        *self.handle.last_used.lock() = Instant::now();
    }
}

/// The supervisor: owns every live sidecar, their states, and the lifecycle
/// rules. One per app (in `AppState`).
pub struct LocalRunnerSupervisor {
    spawner: Arc<dyn ProcessSpawner>,
    health: Arc<dyn HealthCheck>,
    config: SupervisorConfig,
    running: RwLock<HashMap<String, Arc<RunnerHandle>>>,
    states: RwLock<HashMap<String, RunnerState>>,
    /// Per-`catalog_id` async start locks — serialize concurrent
    /// [`Self::ensure_started`] for the same id so two callers can't both pass
    /// the "not running" check and each spawn a sidecar (review finding #1). One
    /// lock per id ever started; bounded by the (tiny) model count.
    start_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl LocalRunnerSupervisor {
    pub fn new(
        spawner: Arc<dyn ProcessSpawner>,
        health: Arc<dyn HealthCheck>,
        config: SupervisorConfig,
    ) -> Self {
        Self {
            spawner,
            health,
            config,
            running: RwLock::new(HashMap::new()),
            states: RwLock::new(HashMap::new()),
            start_locks: Mutex::new(HashMap::new()),
        }
    }

    /// The production supervisor: real spawner, real HTTP health check.
    pub fn real(pidfile_dir: PathBuf) -> Self {
        let config = SupervisorConfig { pidfile_dir: Some(pidfile_dir), ..Default::default() };
        Self::new(Arc::new(TokioSpawner), Arc::new(HttpHealthCheck::new()), config)
    }

    pub fn is_running(&self, catalog_id: &str) -> bool {
        self.running.read().contains_key(catalog_id)
    }

    pub fn state(&self, catalog_id: &str) -> Option<RunnerState> {
        self.states.read().get(catalog_id).cloned()
    }

    /// Clear a `Failed` state so a later `ensure_started` may retry (the
    /// Settings "try again" affordance).
    pub fn clear_failed(&self, catalog_id: &str) {
        let mut states = self.states.write();
        if matches!(states.get(catalog_id), Some(RunnerState::Failed(_))) {
            states.remove(catalog_id);
        }
    }

    /// Mark a request in flight against a running sidecar (idle-shutdown
    /// guard). `None` if the runner isn't up.
    pub fn begin_request(&self, catalog_id: &str) -> Option<InFlightGuard> {
        // The `fetch_add` happens WHILE the `running` read lock is held (finding
        // #3): `stop_if_idle` takes the `running` WRITE lock to check-and-remove,
        // so read/write exclusion guarantees either this increment lands before
        // an idle sweep can remove the handle (→ the sweep sees in_flight ≥ 1 and
        // skips it), or the handle is already gone (→ we return `None` and the
        // caller re-spawns). A request can never be handed a doomed handle.
        let running = self.running.read();
        let handle = running.get(catalog_id).cloned()?;
        handle.in_flight.fetch_add(1, Ordering::SeqCst);
        drop(running);
        *handle.last_used.lock() = Instant::now();
        Some(InFlightGuard { handle })
    }

    /// Bring the sidecar for `catalog_id` up (warm path: return the running
    /// one). Retries through the backoff schedule; exhaustion records the
    /// distinct `runner_failed` state and errors. On success a crash monitor
    /// keeps it restarted until [`stop`]ped.
    pub async fn ensure_started(
        self: &Arc<Self>,
        catalog_id: &str,
        cmd: SidecarCommand,
        base_url: String,
    ) -> Result<String> {
        // Fast warm path (no lock): already running → return it.
        if let Some(h) = self.running.read().get(catalog_id) {
            return Ok(h.base_url.clone());
        }
        // Finding #1: serialize the start for THIS id so two concurrent callers
        // can't both spawn. A second caller blocks here, then finds the runner
        // registered by the first at the re-check below.
        let start_lock = {
            let mut locks = self.start_locks.lock();
            Arc::clone(
                locks
                    .entry(catalog_id.to_string())
                    .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
            )
        };
        let _start_guard = start_lock.lock().await;

        // Re-check under the start lock — a racing caller may have started it
        // while we waited.
        if let Some(h) = self.running.read().get(catalog_id) {
            return Ok(h.base_url.clone());
        }
        if let Some(RunnerState::Failed(msg)) = self.state(catalog_id) {
            bail!("local runner previously failed ({msg}) — clear the failure to retry");
        }
        self.states
            .write()
            .insert(catalog_id.to_string(), RunnerState::Starting);

        match self.attempt_with_backoff(catalog_id, &cmd, &base_url).await {
            Ok(handle) => {
                self.states
                    .write()
                    .insert(catalog_id.to_string(), RunnerState::Healthy);
                self.running
                    .write()
                    .insert(catalog_id.to_string(), Arc::clone(&handle));
                self.write_pidfile(&handle);
                // Crash monitor: restart-with-backoff until stopped.
                let sup = Arc::clone(self);
                let id = catalog_id.to_string();
                tokio::spawn(async move { sup.monitor_loop(id).await });
                Ok(handle.base_url.clone())
            }
            Err(e) => {
                self.states.write().insert(
                    catalog_id.to_string(),
                    RunnerState::Failed(e.to_string()),
                );
                Err(e.context("sidecar would not become healthy (runner_failed)"))
            }
        }
    }

    /// One spawn + health-wait attempt per backoff slot: initial attempt, then
    /// `backoff.len()` retries. An attempt fails on process exit or health
    /// timeout (the child is killed before the next attempt).
    async fn attempt_with_backoff(
        &self,
        catalog_id: &str,
        cmd: &SidecarCommand,
        base_url: &str,
    ) -> Result<Arc<RunnerHandle>> {
        let mut last_err = None;
        for attempt in 0..=self.config.backoff.len() {
            if attempt > 0 {
                tokio::time::sleep(self.config.backoff[attempt - 1]).await;
            }
            match self.one_attempt(catalog_id, cmd, base_url).await {
                Ok(h) => return Ok(h),
                Err(e) => {
                    tracing::warn!(
                        target: "lhp::runner",
                        catalog_id,
                        attempt,
                        error = %e,
                        "sidecar start attempt failed"
                    );
                    last_err = Some(e);
                }
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow::anyhow!("no attempts ran")))
    }

    async fn one_attempt(
        &self,
        catalog_id: &str,
        cmd: &SidecarCommand,
        base_url: &str,
    ) -> Result<Arc<RunnerHandle>> {
        let mut process = self.spawner.spawn(cmd)?;
        let deadline = Instant::now() + self.config.health_timeout;
        loop {
            // Early-exit: a dead child never becomes healthy.
            if let Ok(Some(code)) = process.try_wait() {
                bail!("sidecar exited (code {code}) before becoming healthy");
            }
            if self.health.is_healthy(base_url).await {
                let port = base_url
                    .rsplit(':')
                    .next()
                    .and_then(|p| p.split('/').next())
                    .and_then(|p| p.parse().ok())
                    .unwrap_or(0);
                return Ok(Arc::new(RunnerHandle {
                    catalog_id: catalog_id.to_string(),
                    port,
                    base_url: base_url.to_string(),
                    cmd: cmd.clone(),
                    process: Mutex::new(process),
                    in_flight: AtomicU32::new(0),
                    last_used: Mutex::new(Instant::now()),
                    killed: AtomicBool::new(false),
                }));
            }
            if Instant::now() >= deadline {
                // Health window exhausted — kill this child before retrying.
                let _ = process.start_kill();
                bail!("health check did not pass within {:?}", self.config.health_timeout);
            }
            tokio::time::sleep(self.config.health_poll).await;
        }
    }

    /// The crash monitor for one runner: poll for unexpected exit; restart
    /// through the backoff schedule; exhaustion → `runner_failed` + removal.
    /// Ends when the runner is stopped (killed flag) or permanently failed.
    async fn monitor_loop(self: Arc<Self>, catalog_id: String) {
        loop {
            tokio::time::sleep(self.config.monitor_poll).await;
            let Some(handle) = self.running.read().get(&catalog_id).cloned() else {
                return; // stopped/removed — monitor retires
            };
            if handle.killed.load(Ordering::SeqCst) {
                return; // deliberate teardown
            }
            let exited = { handle.process.lock().try_wait().ok().flatten() };
            if let Some(code) = exited {
                // Finding #2 — do NOT resurrect a deliberately-stopped runner.
                // `stop()`/`stop_if_idle()` set `killed` (SeqCst) BEFORE killing
                // the process, so any exit we observe as a result of a teardown
                // is preceded by `killed == true`. The `killed` check at the top
                // of the loop can be sampled stale (before the stop), so we
                // re-check HERE, after seeing the exit, right before restarting.
                if handle.killed.load(Ordering::SeqCst) {
                    return;
                }
                tracing::warn!(
                    target: "lhp::runner",
                    catalog_id,
                    code,
                    "sidecar exited unexpectedly — restarting with backoff"
                );
                self.remove_pidfile(&catalog_id);
                self.running.write().remove(&catalog_id);
                match self
                    .attempt_with_backoff(&catalog_id, &handle.cmd, &handle.base_url)
                    .await
                {
                    Ok(new_handle) => {
                        self.states
                            .write()
                            .insert(catalog_id.clone(), RunnerState::Healthy);
                        self.running
                            .write()
                            .insert(catalog_id.clone(), Arc::clone(&new_handle));
                        self.write_pidfile(&new_handle);
                    }
                    Err(e) => {
                        tracing::error!(
                            target: "lhp::runner",
                            catalog_id,
                            error = %e,
                            "sidecar restart schedule exhausted — runner_failed"
                        );
                        self.states
                            .write()
                            .insert(catalog_id.clone(), RunnerState::Failed(e.to_string()));
                        return;
                    }
                }
            }
        }
    }

    /// Stop one runner: kill-once (`start_kill` guarded by the `killed` flag),
    /// wait up to `kill_grace` for the exit, remove from the registry either
    /// way (`kill_on_drop` is the last-resort net for a refuses-to-die child).
    pub async fn stop(&self, catalog_id: &str) {
        let Some(handle) = self.running.write().remove(catalog_id) else {
            return;
        };
        self.finish_stop(catalog_id, handle).await;
    }

    /// Stop a runner ONLY if it has no in-flight requests — the idle-sweep path.
    /// The in-flight check and the removal happen under ONE `running` write lock
    /// (finding #3), and [`Self::begin_request`] increments in-flight under the
    /// paired read lock, so a request that began after the sweep's snapshot
    /// aborts the kill (the "unconditional in-flight guard" is now actually
    /// unconditional). Returns whether it stopped one.
    pub async fn stop_if_idle(&self, catalog_id: &str) -> bool {
        let handle = {
            let mut running = self.running.write();
            match running.get(catalog_id) {
                Some(h) if h.in_flight.load(Ordering::SeqCst) == 0 => running.remove(catalog_id),
                _ => None, // gone, or a request landed after the snapshot — leave it
            }
        };
        match handle {
            Some(h) => {
                self.finish_stop(catalog_id, h).await;
                true
            }
            None => false,
        }
    }

    /// Kill-once + grace-reap for a handle already removed from `running`. Shared
    /// by [`Self::stop`] and [`Self::stop_if_idle`].
    async fn finish_stop(&self, catalog_id: &str, handle: Arc<RunnerHandle>) {
        self.states.write().remove(catalog_id);
        self.remove_pidfile(catalog_id);
        if !handle.killed.swap(true, Ordering::SeqCst) {
            let _ = handle.process.lock().start_kill();
        }
        let deadline = Instant::now() + self.config.kill_grace;
        loop {
            if handle.process.lock().try_wait().ok().flatten().is_some() {
                return; // reaped
            }
            if Instant::now() >= deadline {
                tracing::warn!(
                    target: "lhp::runner",
                    catalog_id,
                    "sidecar did not exit within grace — relying on kill_on_drop"
                );
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Stop everything (app exit / `remove_local_model`).
    pub async fn stop_all(&self) {
        let ids: Vec<String> = self.running.read().keys().cloned().collect();
        for id in ids {
            self.stop(&id).await;
        }
    }

    /// Idle sweep: stop any runner with zero in-flight requests whose last use
    /// is older than `idle_shutdown`. The in-flight guard is UNCONDITIONAL — a
    /// long generation is never killed no matter how stale the clock: the
    /// snapshot below only PROPOSES candidates; [`Self::stop_if_idle`] re-checks
    /// in-flight atomically before killing, so a request that starts between the
    /// snapshot and the kill is never cut off.
    pub async fn idle_sweep(&self) {
        let now = Instant::now();
        let idle: Vec<String> = self
            .running
            .read()
            .iter()
            .filter(|(_, h)| {
                h.in_flight.load(Ordering::SeqCst) == 0
                    && now.duration_since(*h.last_used.lock()) >= self.config.idle_shutdown
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in idle {
            if self.stop_if_idle(&id).await {
                tracing::info!(target: "lhp::runner", catalog_id = %id, "idle sidecar shutdown");
            }
        }
    }

    // ── pidfiles ────────────────────────────────────────────────────────

    fn pidfile_path(&self, catalog_id: &str) -> Option<PathBuf> {
        self.config
            .pidfile_dir
            .as_ref()
            .map(|d| d.join(format!("{catalog_id}.pid")))
    }

    fn write_pidfile(&self, handle: &RunnerHandle) {
        let Some(path) = self.pidfile_path(&handle.catalog_id) else { return };
        let Some(pid) = handle.process.lock().id() else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        // "PID START_EPOCH BOOT_ID" — the reaper verifies liveness + process
        // identity (comm name) AND that the boot id still matches before killing.
        // The boot id closes cross-reboot PID reuse (finding #5): after a reboot
        // the PID space resets, so a matching PID belongs to an unrelated
        // process and must never be killed.
        let _ = std::fs::write(
            &path,
            format!("{pid} {} {}", chrono::Utc::now().timestamp(), current_boot_id().unwrap_or_default()),
        );
    }

    fn remove_pidfile(&self, catalog_id: &str) {
        if let Some(path) = self.pidfile_path(catalog_id) {
            let _ = std::fs::remove_file(path);
        }
    }
}

// ---------------------------------------------------------------------------
// Boot passes: orphan reap + integrity sweep
// ---------------------------------------------------------------------------

/// Reap sidecars orphaned by a hard crash of OUR process (macOS has no
/// `PR_SET_PDEATHSIG`): for each recorded pidfile, if the PID is alive AND
/// still looks like our sidecar (`ps -o comm=` names `llama-server` — the
/// PID-reuse guard), kill it; remove the pidfile either way. Best-effort,
/// never bricks boot. Returns how many processes were killed.
pub fn reap_orphan_sidecars(pidfile_dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(pidfile_dir) else { return 0 };
    let current_boot = current_boot_id();
    let mut killed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("pid") {
            continue;
        }
        if let Ok(content) = std::fs::read_to_string(&path) {
            let mut fields = content.split_whitespace();
            let pid = fields.next().and_then(|p| p.parse::<i32>().ok());
            let _epoch = fields.next();
            let file_boot = fields.next(); // Option<&str>; absent in legacy pidfiles
            // Cross-reboot PID-reuse guard (finding #5): if the pidfile recorded
            // a boot id and it differs from the current boot, the PID space has
            // since reset — a process at that PID is NOT ours. Never kill; just
            // clean up. A legacy (no-boot-id) pidfile falls back to the
            // comm-name check alone (prior behavior).
            let stale_boot = matches!(
                (file_boot, current_boot.as_deref()),
                (Some(fb), Some(cb)) if fb != cb
            );
            if let Some(pid) = pid {
                if pid > 0 && !stale_boot && process_is_our_sidecar(pid) {
                    #[cfg(unix)]
                    unsafe {
                        libc::kill(pid, libc::SIGKILL);
                    }
                    killed += 1;
                    tracing::warn!(target: "lhp::runner", pid, "reaped orphaned sidecar from a previous run");
                }
            }
        }
        let _ = std::fs::remove_file(&path);
    }
    killed
}

/// A per-boot identifier (macOS `kern.boottime` seconds). Detects that a pidfile
/// was written in a PREVIOUS boot: after a reboot the PID space resets, so a
/// matching PID belongs to an unrelated process and must never be killed. `None`
/// when unavailable — the reaper then falls back to the comm-name check alone.
#[cfg(target_os = "macos")]
fn current_boot_id() -> Option<String> {
    let out = std::process::Command::new("sysctl")
        .args(["-n", "kern.boottime"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    // e.g. "{ sec = 1699999999, usec = 0 } Mon Nov ..." → the `sec` value.
    let s = String::from_utf8_lossy(&out.stdout);
    let sec = s.split("sec =").nth(1)?.split(',').next()?.trim().to_string();
    if sec.is_empty() {
        None
    } else {
        Some(sec)
    }
}

#[cfg(not(target_os = "macos"))]
fn current_boot_id() -> Option<String> {
    None
}

/// Is `pid` alive and running our sidecar binary? Checks the process comm name
/// via `ps` (macOS/unix). A dead PID, a reused PID running something else, or
/// any lookup failure → `false` (never kill on uncertainty).
fn process_is_our_sidecar(pid: i32) -> bool {
    let Ok(out) = std::process::Command::new("ps")
        .args(["-o", "comm=", "-p", &pid.to_string()])
        .output()
    else {
        return false;
    };
    if !out.status.success() {
        return false;
    }
    String::from_utf8_lossy(&out.stdout).trim().ends_with("llama-server")
}

/// Report from the boot integrity sweep.
#[derive(Debug, Default)]
pub struct SweepReport {
    pub checked: usize,
    pub quarantined: Vec<String>,
}

/// Boot-time integrity re-check — finally wires the callerless
/// `set_model_status` (design §D). For each non-quarantined `model_catalog`
/// row: a missing/unreadable file OR a size mismatch (cheap, catches
/// truncation) OR — when `rehash` — a hash mismatch ⇒ `quarantined`
/// (fail-closed, invariant corollary 3: never silently served). Full re-hash
/// is opt-in (multi-GB hashing every boot is too costly); existence+size
/// catches the common cases. Best-effort/logged/never-bricks (crash-recovery
/// discipline). Does NOT spawn anything — spawn stays lazy.
pub fn sweep_local_model_integrity_at_boot(storage: &Storage, rehash: bool) -> SweepReport {
    let mut report = SweepReport::default();
    let rows = match storage.global().list_models() {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(target: "lhp::runner", error = %e, "integrity sweep: list failed");
            return report;
        }
    };
    for row in rows {
        if row.status == "quarantined" {
            continue;
        }
        report.checked += 1;
        let path = Path::new(&row.path);
        let verdict: Option<&str> = match std::fs::metadata(path) {
            Err(_) => Some("missing or unreadable"),
            Ok(meta) if meta.len() != row.size_bytes as u64 => Some("size mismatch"),
            Ok(_) if rehash => match crate::models::download::file_sha256(path) {
                Ok(actual) if actual == row.sha256.to_ascii_lowercase() => None,
                Ok(_) => Some("hash mismatch"),
                Err(_) => Some("unreadable while hashing"),
            },
            Ok(_) => None,
        };
        if let Some(reason) = verdict {
            tracing::warn!(
                target: "lhp::runner",
                model = %row.id,
                reason,
                "integrity sweep: quarantining model"
            );
            if let Err(e) = storage.global().set_model_status(&row.id, "quarantined") {
                tracing::error!(target: "lhp::runner", model = %row.id, error = %e, "quarantine write failed");
            } else {
                report.quarantined.push(row.id);
            }
        }
    }
    report
}

// ---------------------------------------------------------------------------
// The ONE lazy-spawn seam
// ---------------------------------------------------------------------------

/// Where the vendored sidecar lives at runtime (resolved from the app bundle's
/// resources, an env override, or the dev tree — see `lib.rs`).
#[derive(Debug, Clone)]
pub struct SidecarPaths {
    pub bin: PathBuf,
}

/// The supervisor + its resolved binary, bundled for injection into the agent
/// loop and `AppState`.
pub struct LocalRunnerContext {
    pub supervisor: Arc<LocalRunnerSupervisor>,
    pub paths: SidecarPaths,
}

/// Provider id for a catalog model — a pure function, no schema column
/// (design §D: "a pure fn can't drift").
pub fn provider_id_for(catalog_id: &str) -> String {
    format!("local-runner:{catalog_id}")
}

/// **The lazy-spawn seam** (design §D): bring up a `ready` local model and
/// register it as a Local provider, or return the already-running one. Called
/// from the agent loop's `find_local_provider` empty-snapshot branches — so
/// the FIRST turn that needs a local model starts one, and `RouteLocal` stops
/// failing on a machine that has a downloaded model but no external runner.
///
/// Refuses (fail-closed): no `ready` row; a `runner_failed` state (distinct
/// from quarantine — won't stay up vs integrity); a missing model file
/// (quarantines it on the spot). The ephemeral `base_url` (port re-picked per
/// launch) is NEVER persisted to `endpoints` — a local-runner provider is pure
/// derived session state.
pub async fn ensure_running(
    supervisor: &Arc<LocalRunnerSupervisor>,
    mm: &ModelManager,
    storage: &Storage,
    paths: &SidecarPaths,
    preferred_model: Option<&str>,
    ctx_size: Option<u32>,
    kv_quant: Option<KvCacheQuant>,
) -> Result<Provider> {
    // Pick the row: the caller's preference, else the most recently added
    // `ready` model (list_models is ORDER BY added_at DESC).
    let rows = storage.global().list_models()?;
    let row = match preferred_model {
        Some(id) => rows.into_iter().find(|r| r.id == id),
        None => rows.into_iter().find(|r| r.status == "ready"),
    }
    .ok_or_else(|| anyhow::anyhow!("no ready local model is downloaded"))?;
    if row.status != "ready" {
        bail!(
            "local model \"{}\" is {} — re-download it from Settings → Models",
            row.id,
            row.status
        );
    }
    if let Some(RunnerState::Failed(msg)) = supervisor.state(&row.id) {
        bail!(
            "local model \"{}\" runner keeps failing ({msg}) — its process won't stay up; \
             check Settings → Models",
            row.id
        );
    }

    let provider_id = provider_id_for(&row.id);
    // Warm path: registered AND actually running.
    if let Some(p) = mm.get_provider(&provider_id) {
        if supervisor.is_running(&row.id) {
            return Ok(p);
        }
    }

    // The file must exist before we spawn anything at it (fail closed +
    // quarantine on the spot — same verdict the boot sweep would reach).
    let model_path = Path::new(&row.path);
    if !model_path.is_file() {
        let _ = storage.global().set_model_status(&row.id, "quarantined");
        bail!(
            "local model \"{}\" file is missing — quarantined; re-download it",
            row.id
        );
    }

    let port = pick_free_port()?;
    let base_url = format!("http://127.0.0.1:{port}/v1");
    let threads = std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(4);
    let cmd = build_args(&paths.bin, model_path, port, threads, ctx_size, kv_quant);
    supervisor
        .ensure_started(&row.id, cmd, base_url.clone())
        .await?;

    let provider = Provider::new(&provider_id, &row.name, &base_url, None, ProviderKind::Local);
    mm.add_provider(provider.clone());
    tracing::info!(
        target: "lhp::runner",
        model = %row.id,
        %base_url,
        "local sidecar up + registered as provider"
    );
    Ok(provider)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::atomic::AtomicUsize;

    // ── the fake world (spawner + health share it) ─────────────────────

    #[derive(Clone, Copy, Debug)]
    enum FakeBehavior {
        HealthyImmediately,
        NeverHealthy,
        /// Healthy at first, then the process "crashes" after this many
        /// virtual milliseconds.
        CrashesAfterMs(u64),
        RefusesToDie,
    }

    struct FakeWorld {
        behaviors: Mutex<VecDeque<FakeBehavior>>,
        /// (command, virtual spawn time) per spawn — the backoff assertions.
        spawns: Mutex<Vec<(SidecarCommand, Instant)>>,
        healthy: AtomicBool,
        kill_calls: AtomicUsize,
    }

    impl FakeWorld {
        fn new(behaviors: Vec<FakeBehavior>) -> Arc<Self> {
            Arc::new(Self {
                behaviors: Mutex::new(behaviors.into()),
                spawns: Mutex::new(Vec::new()),
                healthy: AtomicBool::new(false),
                kill_calls: AtomicUsize::new(0),
            })
        }
        fn spawn_count(&self) -> usize {
            self.spawns.lock().len()
        }
    }

    struct FakeProcess {
        world: Arc<FakeWorld>,
        behavior: FakeBehavior,
        spawned_at: Instant,
        killed: bool,
    }

    impl SpawnedProcess for FakeProcess {
        fn id(&self) -> Option<u32> {
            Some(4242)
        }
        fn try_wait(&mut self) -> Result<Option<i32>> {
            if self.killed && !matches!(self.behavior, FakeBehavior::RefusesToDie) {
                return Ok(Some(9));
            }
            if let FakeBehavior::CrashesAfterMs(ms) = self.behavior {
                if Instant::now().duration_since(self.spawned_at) >= Duration::from_millis(ms) {
                    self.world.healthy.store(false, Ordering::SeqCst);
                    return Ok(Some(1));
                }
            }
            Ok(None)
        }
        fn start_kill(&mut self) -> Result<()> {
            self.killed = true;
            self.world.kill_calls.fetch_add(1, Ordering::SeqCst);
            if !matches!(self.behavior, FakeBehavior::RefusesToDie) {
                self.world.healthy.store(false, Ordering::SeqCst);
            }
            Ok(())
        }
    }

    struct FakeSpawner(Arc<FakeWorld>);
    impl ProcessSpawner for FakeSpawner {
        fn spawn(&self, cmd: &SidecarCommand) -> Result<Box<dyn SpawnedProcess>> {
            let behavior = {
                let mut b = self.0.behaviors.lock();
                if b.len() > 1 { b.pop_front().unwrap() } else { *b.front().expect("behavior") }
            };
            self.0.spawns.lock().push((cmd.clone(), Instant::now()));
            self.0.healthy.store(
                matches!(
                    behavior,
                    FakeBehavior::HealthyImmediately
                        | FakeBehavior::CrashesAfterMs(_)
                        | FakeBehavior::RefusesToDie
                ),
                Ordering::SeqCst,
            );
            Ok(Box::new(FakeProcess {
                world: Arc::clone(&self.0),
                behavior,
                spawned_at: Instant::now(),
                killed: false,
            }))
        }
    }

    struct FakeHealth(Arc<FakeWorld>);
    impl HealthCheck for FakeHealth {
        fn is_healthy<'a>(&'a self, _base_url: &'a str) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
            let v = self.0.healthy.load(Ordering::SeqCst);
            Box::pin(async move { v })
        }
    }

    fn test_config() -> SupervisorConfig {
        SupervisorConfig {
            health_timeout: Duration::from_secs(1),
            health_poll: Duration::from_millis(100),
            backoff: [1u64, 2, 4, 8, 16].map(Duration::from_secs).to_vec(),
            idle_shutdown: Duration::from_secs(600),
            kill_grace: Duration::from_millis(300),
            monitor_poll: Duration::from_millis(200),
            pidfile_dir: None,
        }
    }

    fn supervisor_with(world: &Arc<FakeWorld>) -> Arc<LocalRunnerSupervisor> {
        Arc::new(LocalRunnerSupervisor::new(
            Arc::new(FakeSpawner(Arc::clone(world))),
            Arc::new(FakeHealth(Arc::clone(world))),
            test_config(),
        ))
    }

    fn cmd() -> SidecarCommand {
        build_args(
            Path::new("/fake/llama-server"),
            Path::new("/fake/model.gguf"),
            8080,
            8,
            Some(8192),
            Some(KvCacheQuant::Q8_0),
        )
    }

    // ── build_args: the pinned loopback test ───────────────────────────

    #[test]
    fn build_args_pins_loopback_host_and_carries_calculator_choices() {
        let c = cmd();
        let joined = c.args.join(" ");
        // THE pinned assertion (design §D): loopback only, never all-interfaces.
        assert!(joined.contains("--host 127.0.0.1"), "must bind loopback: {joined}");
        assert!(!joined.contains("0.0.0.0"), "must NEVER bind all interfaces");
        // The calculator's knobs pass through.
        assert!(joined.contains("--ctx-size 8192"));
        assert!(joined.contains("--cache-type-k q8_0"));
        assert!(joined.contains("--cache-type-v q8_0"));
        assert!(joined.contains("--parallel 2"));
        assert!(joined.contains("-ngl 999"));
        // Defaults: no kv flag when unspecified; ctx falls back to 4096.
        let d = build_args(Path::new("/b"), Path::new("/m"), 1, 4, None, None);
        let dj = d.args.join(" ");
        assert!(dj.contains("--ctx-size 4096"));
        assert!(!dj.contains("--cache-type-k"));
    }

    // ── lifecycle ──────────────────────────────────────────────────────

    #[tokio::test(start_paused = true)]
    async fn healthy_spawn_registers_and_warm_path_reuses() {
        let world = FakeWorld::new(vec![FakeBehavior::HealthyImmediately]);
        let sup = supervisor_with(&world);
        let url = sup
            .ensure_started("m1", cmd(), "http://127.0.0.1:8080/v1".into())
            .await
            .unwrap();
        assert_eq!(url, "http://127.0.0.1:8080/v1");
        assert!(sup.is_running("m1"));
        assert_eq!(sup.state("m1"), Some(RunnerState::Healthy));
        assert_eq!(world.spawn_count(), 1);
        // Warm path: no second spawn.
        let again = sup
            .ensure_started("m1", cmd(), "http://127.0.0.1:9999/v1".into())
            .await
            .unwrap();
        assert_eq!(again, url, "warm path returns the RUNNING url");
        assert_eq!(world.spawn_count(), 1, "no respawn on warm path");
    }

    #[tokio::test(start_paused = true)]
    async fn never_healthy_walks_the_exact_backoff_schedule_then_fails() {
        let world = FakeWorld::new(vec![FakeBehavior::NeverHealthy]);
        let sup = supervisor_with(&world);
        let err = sup
            .ensure_started("m1", cmd(), "http://127.0.0.1:8080/v1".into())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("runner_failed"), "distinct failure state: {err}");
        // No registration ever happened (a model is never a provider until healthy).
        assert!(!sup.is_running("m1"));
        assert!(matches!(sup.state("m1"), Some(RunnerState::Failed(_))));
        // Initial attempt + the full 5-slot retry schedule.
        let spawns = world.spawns.lock();
        assert_eq!(spawns.len(), 6, "initial + 5 backoff retries");
        // EXACT schedule on the virtual clock: each delta = health_timeout (1s,
        // the failed window) + the backoff slot.
        let expect = [2.0f64, 3.0, 5.0, 9.0, 17.0];
        for (i, want) in expect.iter().enumerate() {
            let delta = spawns[i + 1].1.duration_since(spawns[i].1).as_secs_f64();
            assert!(
                (delta - want).abs() < 0.35,
                "spawn {} → {}: delta {delta}s, want ~{want}s",
                i,
                i + 1
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_crashed_sidecar_restarts_via_the_monitor() {
        // First process crashes 300ms in; the restart is healthy immediately.
        let world = FakeWorld::new(vec![
            FakeBehavior::CrashesAfterMs(300),
            FakeBehavior::HealthyImmediately,
        ]);
        let sup = supervisor_with(&world);
        sup.ensure_started("m1", cmd(), "http://127.0.0.1:8080/v1".into())
            .await
            .unwrap();
        assert_eq!(world.spawn_count(), 1);
        // Let the crash land and the monitor restart it: crash at +300ms,
        // monitor polls every 200ms, restart backoff slot 0 = 1s.
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert_eq!(world.spawn_count(), 2, "the monitor respawned the crashed sidecar");
        assert!(sup.is_running("m1"), "back up after restart");
        assert_eq!(sup.state("m1"), Some(RunnerState::Healthy));
    }

    #[tokio::test(start_paused = true)]
    async fn stop_kills_exactly_once_and_removes() {
        let world = FakeWorld::new(vec![FakeBehavior::HealthyImmediately]);
        let sup = supervisor_with(&world);
        sup.ensure_started("m1", cmd(), "http://127.0.0.1:8080/v1".into())
            .await
            .unwrap();
        sup.stop("m1").await;
        assert!(!sup.is_running("m1"));
        assert_eq!(world.kill_calls.load(Ordering::SeqCst), 1);
        // A second stop is a no-op — kill-once semantics.
        sup.stop("m1").await;
        assert_eq!(world.kill_calls.load(Ordering::SeqCst), 1, "never double-kills");
    }

    #[tokio::test(start_paused = true)]
    async fn refuses_to_die_exits_stop_after_grace_without_hanging() {
        let world = FakeWorld::new(vec![FakeBehavior::RefusesToDie]);
        let sup = supervisor_with(&world);
        sup.ensure_started("m1", cmd(), "http://127.0.0.1:8080/v1".into())
            .await
            .unwrap();
        let before = Instant::now();
        sup.stop("m1").await; // must return after kill_grace, not hang
        let took = Instant::now().duration_since(before);
        assert!(took >= Duration::from_millis(300), "waited the grace window");
        assert!(!sup.is_running("m1"), "removed from registry regardless");
    }

    #[tokio::test(start_paused = true)]
    async fn idle_sweep_stops_only_idle_runners_never_in_flight_ones() {
        let world = FakeWorld::new(vec![FakeBehavior::HealthyImmediately]);
        let sup = supervisor_with(&world);
        sup.ensure_started("m1", cmd(), "http://127.0.0.1:8080/v1".into())
            .await
            .unwrap();
        // Hold an in-flight guard, age WAY past the idle window: must survive.
        let guard = sup.begin_request("m1").expect("running");
        tokio::time::sleep(Duration::from_secs(3600)).await;
        sup.idle_sweep().await;
        assert!(sup.is_running("m1"), "an in-flight runner is NEVER idle-killed");
        // Drop the guard (refreshes last_used), then age past idle: now stops.
        drop(guard);
        sup.idle_sweep().await;
        assert!(sup.is_running("m1"), "just-used runner is not yet idle");
        tokio::time::sleep(Duration::from_secs(601)).await;
        sup.idle_sweep().await;
        assert!(!sup.is_running("m1"), "idle past the window → stopped");
    }

    #[tokio::test]
    async fn stop_if_idle_atomically_spares_an_in_flight_runner() {
        // Finding #3: stop_if_idle re-checks in_flight under the write lock, so a
        // request that lands (via begin_request) is never cut off even if an idle
        // sweep already decided to stop this id.
        let world = FakeWorld::new(vec![FakeBehavior::HealthyImmediately]);
        let sup = supervisor_with(&world);
        sup.ensure_started("m1", cmd(), "http://127.0.0.1:8080/v1".into())
            .await
            .unwrap();
        let guard = sup.begin_request("m1").expect("running");
        assert!(!sup.stop_if_idle("m1").await, "an in-flight runner is spared");
        assert!(sup.is_running("m1"), "still running");
        drop(guard);
        assert!(sup.stop_if_idle("m1").await, "an idle runner is stopped");
        assert!(!sup.is_running("m1"));
    }

    #[tokio::test]
    async fn ensure_started_is_idempotent_and_spawns_once_per_id() {
        // Finding #1: repeated ensure_started for one id reuses the running
        // sidecar (the per-id start lock serializes concurrent callers so they
        // can't double-spawn; the warm path proves single-spawn here).
        let world = FakeWorld::new(vec![FakeBehavior::HealthyImmediately]);
        let sup = supervisor_with(&world);
        let a = sup
            .ensure_started("m1", cmd(), "http://127.0.0.1:8080/v1".into())
            .await
            .unwrap();
        let b = sup
            .ensure_started("m1", cmd(), "http://127.0.0.1:8080/v1".into())
            .await
            .unwrap();
        assert_eq!(a, b, "same base_url");
        assert_eq!(world.spawn_count(), 1, "no double-spawn for the same id");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn current_boot_id_is_available_on_macos() {
        assert!(
            super::current_boot_id().is_some(),
            "kern.boottime should be readable on macOS"
        );
    }

    #[test]
    fn reap_cleans_up_pidfiles_and_skips_a_stale_boot_entry() {
        // Finding #5: a pidfile from a DIFFERENT boot must never trigger a kill
        // (its PID belongs to an unrelated post-reboot process), and every
        // pidfile is cleaned up regardless. No real sidecars exist in a temp dir,
        // so nothing is reaped.
        let dir = std::env::temp_dir().join(format!("lhp-pid-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("m1.pid"), "1 1699999999 boot-from-a-previous-run").unwrap();
        std::fs::write(dir.join("m2.pid"), "2147483647").unwrap(); // legacy 1-field, dead pid
        let killed = super::reap_orphan_sidecars(&dir);
        assert_eq!(killed, 0, "nothing real to reap in a temp dir");
        assert!(!dir.join("m1.pid").exists(), "pidfiles are always cleaned up");
        assert!(!dir.join("m2.pid").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(start_paused = true)]
    async fn failed_state_blocks_restart_until_cleared() {
        let world = FakeWorld::new(vec![FakeBehavior::NeverHealthy]);
        let sup = supervisor_with(&world);
        let _ = sup
            .ensure_started("m1", cmd(), "http://127.0.0.1:8080/v1".into())
            .await;
        let spawns_after_fail = world.spawn_count();
        // While Failed, ensure_started refuses without spawning.
        let err = sup
            .ensure_started("m1", cmd(), "http://127.0.0.1:8080/v1".into())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("previously failed"));
        assert_eq!(world.spawn_count(), spawns_after_fail, "no spawn while Failed");
        // clear_failed re-arms the retry path.
        sup.clear_failed("m1");
        assert_eq!(sup.state("m1"), None);
    }

    // ── boot passes ────────────────────────────────────────────────────

    fn tmp_dir() -> PathBuf {
        let p = std::env::temp_dir().join(format!("lhp-runner-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn integrity_sweep_quarantines_missing_truncated_and_tampered() {
        let dir = tmp_dir();
        let storage = Storage::open(&dir).unwrap();
        let write_model = |id: &str, contents: Option<&[u8]>, size: i64, sha: &str| {
            let path = dir.join(format!("{id}.gguf"));
            if let Some(bytes) = contents {
                std::fs::write(&path, bytes).unwrap();
            }
            storage
                .global()
                .insert_model(&crate::storage::ModelEntry {
                    id: id.into(),
                    name: id.into(),
                    path: path.to_string_lossy().into_owned(),
                    size_bytes: size,
                    quantization: None,
                    added_at: 0,
                    sha256: sha.into(),
                    status: "ready".into(),
                })
                .unwrap();
        };
        // sha256("hello")
        let hello_sha = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        write_model("good", Some(b"hello"), 5, hello_sha);
        write_model("missing", None, 5, hello_sha);
        write_model("truncated", Some(b"hel"), 5, hello_sha);
        write_model("tampered", Some(b"jello"), 5, hello_sha);

        let report = sweep_local_model_integrity_at_boot(&storage, true);
        assert_eq!(report.checked, 4);
        let mut q = report.quarantined.clone();
        q.sort();
        assert_eq!(q, vec!["missing", "tampered", "truncated"]);
        let status = |id: &str| storage.global().get_model(id).unwrap().unwrap().status;
        assert_eq!(status("good"), "ready", "an intact model stays ready");
        assert_eq!(status("missing"), "quarantined");
        assert_eq!(status("truncated"), "quarantined", "size check catches truncation");
        assert_eq!(status("tampered"), "quarantined", "rehash catches tampering");
        // Without rehash, tampering (same size) is NOT caught — documented cost
        // tradeoff; size/existence still are.
        let storage2 = Storage::open(&tmp_dir()).unwrap();
        let dir2 = PathBuf::from(storage2.base_path());
        let path2 = dir2.join("t2.gguf");
        std::fs::write(&path2, b"jello").unwrap();
        storage2
            .global()
            .insert_model(&crate::storage::ModelEntry {
                id: "t2".into(),
                name: "t2".into(),
                path: path2.to_string_lossy().into_owned(),
                size_bytes: 5,
                quantization: None,
                added_at: 0,
                sha256: hello_sha.into(),
                status: "ready".into(),
            })
            .unwrap();
        let r2 = sweep_local_model_integrity_at_boot(&storage2, false);
        assert!(r2.quarantined.is_empty(), "cheap sweep passes same-size tampering");
    }

    #[test]
    fn orphan_reap_removes_stale_pidfiles_without_killing_strangers() {
        let dir = tmp_dir();
        // A pidfile for a PID that (a) doesn't exist or (b) is not llama-server
        // — must be cleaned up with nothing killed. PID 1 is launchd (never
        // ours); 999999 shouldn't exist.
        std::fs::write(dir.join("m1.pid"), "999999 1234").unwrap();
        std::fs::write(dir.join("m2.pid"), "1 1234").unwrap();
        std::fs::write(dir.join("not-a-pidfile.txt"), "ignore me").unwrap();
        let killed = reap_orphan_sidecars(&dir);
        assert_eq!(killed, 0, "no stranger processes were killed");
        assert!(!dir.join("m1.pid").exists(), "stale pidfile removed");
        assert!(!dir.join("m2.pid").exists(), "PID-reuse-guarded pidfile removed");
        assert!(dir.join("not-a-pidfile.txt").exists(), "non-pidfiles untouched");
    }

    // ── ensure_running (the seam) ──────────────────────────────────────

    fn seeded_storage(dir: &Path) -> Storage {
        let storage = Storage::open(dir).unwrap();
        let model_path = dir.join("tiny.gguf");
        std::fs::write(&model_path, b"GGUFfake").unwrap();
        storage
            .global()
            .insert_model(&crate::storage::ModelEntry {
                id: "tiny".into(),
                name: "Tiny Test Model".into(),
                path: model_path.to_string_lossy().into_owned(),
                size_bytes: 8,
                quantization: Some("Q8_0".into()),
                added_at: 1,
                sha256: "e".repeat(64),
                status: "ready".into(),
            })
            .unwrap();
        storage
    }

    #[tokio::test(start_paused = true)]
    async fn ensure_running_spawns_and_registers_a_private_local_provider() {
        let dir = tmp_dir();
        let storage = seeded_storage(&dir);
        let world = FakeWorld::new(vec![FakeBehavior::HealthyImmediately]);
        let sup = supervisor_with(&world);
        let mm = ModelManager::new();
        let paths = SidecarPaths { bin: PathBuf::from("/fake/llama-server") };

        let provider = ensure_running(&sup, &mm, &storage, &paths, None, Some(8192), None)
            .await
            .unwrap();
        assert_eq!(provider.id, "local-runner:tiny");
        assert!(provider.is_local(), "kind Local");
        assert!(provider.is_private(), "127.0.0.1 base_url is private");
        assert!(provider.base_url.starts_with("http://127.0.0.1:"));
        assert!(mm.get_provider("local-runner:tiny").is_some(), "registered in the manager");
        // The spawned argv carried the model path + loopback host.
        let spawns = world.spawns.lock();
        let joined = spawns[0].0.args.join(" ");
        assert!(joined.contains("tiny.gguf"));
        assert!(joined.contains("--host 127.0.0.1"));
        assert!(joined.contains("--ctx-size 8192"));
    }

    #[tokio::test(start_paused = true)]
    async fn ensure_running_refuses_quarantined_and_missing_files() {
        let dir = tmp_dir();
        let storage = seeded_storage(&dir);
        storage.global().set_model_status("tiny", "quarantined").unwrap();
        let world = FakeWorld::new(vec![FakeBehavior::HealthyImmediately]);
        let sup = supervisor_with(&world);
        let mm = ModelManager::new();
        let paths = SidecarPaths { bin: PathBuf::from("/fake/llama-server") };
        // Quarantined-only catalog → "no ready model" (the default pick skips it).
        let err = ensure_running(&sup, &mm, &storage, &paths, None, None, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no ready local model"));
        // Preferred-by-id quarantined → the explicit refusal.
        let err = ensure_running(&sup, &mm, &storage, &paths, Some("tiny"), None, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("quarantined"));
        assert_eq!(world.spawn_count(), 0, "nothing was ever spawned");

        // A ready row whose FILE is gone: quarantined on the spot, no spawn.
        storage.global().set_model_status("tiny", "ready").unwrap();
        std::fs::remove_file(dir.join("tiny.gguf")).unwrap();
        let err = ensure_running(&sup, &mm, &storage, &paths, None, None, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("missing"));
        assert_eq!(
            storage.global().get_model("tiny").unwrap().unwrap().status,
            "quarantined",
            "missing file quarantines immediately (fail closed)"
        );
        assert_eq!(world.spawn_count(), 0);
    }

    // ── vendor manifest (verified-before-runnable echo for our own binary) ──

    #[test]
    fn vendored_sidecar_matches_its_manifest() {
        let vendor = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("vendor/llama-cpp");
        let manifest = vendor.join("MANIFEST.sha256");
        let contents = std::fs::read_to_string(&manifest)
            .expect("vendor/llama-cpp/MANIFEST.sha256 must be committed");
        let mut checked = 0;
        for line in contents.lines() {
            let Some((hash, name)) = line.split_once("  ") else { continue };
            let file = vendor.join("macos-arm64").join(name.trim());
            let actual = crate::models::download::file_sha256(&file)
                .unwrap_or_else(|e| panic!("hashing {}: {e}", file.display()));
            assert_eq!(actual, hash.trim(), "vendored file drifted: {name}");
            checked += 1;
        }
        assert!(checked >= 11, "manifest covers the binary + its dylib closure ({checked})");
        assert_eq!(
            std::fs::read_to_string(vendor.join("VERSION")).unwrap().trim(),
            "b10088",
            "pinned llama.cpp release"
        );
    }

    /// The ONE live test (design §D): real vendored binary + a real GGUF →
    /// spawn → health → one real /v1/chat/completions → teardown. Opt-in via
    /// `LHP_TEST_GGUF=/path/to/model.gguf` (+ optional `LHP_LLAMA_SERVER_BIN`
    /// to override the vendored binary). Self-skips otherwise.
    #[tokio::test]
    async fn live_local_runner_roundtrip() {
        let Some(gguf) = std::env::var_os("LHP_TEST_GGUF") else {
            eprintln!("skipping live sidecar test — set LHP_TEST_GGUF to run");
            return;
        };
        let bin = std::env::var_os("LHP_LLAMA_SERVER_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("vendor/llama-cpp/macos-arm64/llama-server")
            });
        let sup = Arc::new(LocalRunnerSupervisor::new(
            Arc::new(TokioSpawner),
            Arc::new(HttpHealthCheck::new()),
            SupervisorConfig::default(),
        ));
        let port = pick_free_port().unwrap();
        let base_url = format!("http://127.0.0.1:{port}/v1");
        let cmd = build_args(&bin, Path::new(&gguf), port, 4, Some(2048), None);
        sup.ensure_started("live-test", cmd, base_url.clone())
            .await
            .expect("sidecar becomes healthy");

        // One real chat round-trip through the existing ModelClient.
        let provider = Provider::new("live-test", "Live", &base_url, None, ProviderKind::Local);
        let client = crate::models::ModelClient::new(provider).unwrap();
        let models = client.list_models().await.expect("GET /v1/models");
        assert!(!models.is_empty(), "the served model is listed");
        let reply = client
            .complete(
                &models[0],
                vec![crate::models::ChatMessage {
                    role: "user".into(),
                    content: "Reply with exactly the word: pong".into(),
                }],
            )
            .await
            .expect("chat completion");
        assert!(!reply.trim().is_empty(), "the model answered: {reply:?}");

        // Clean teardown: process gone.
        sup.stop("live-test").await;
        assert!(!sup.is_running("live-test"));
    }

    /// A5 — the FULL live E2E chain (design §D + the 22b redirect): HF search →
    /// pick the tiny model → download → sha-verify → sidecar spawn → real
    /// /v1/chat/completions → clean teardown. This is the one test that proves
    /// A1 (search/metadata/download) + A2 (sidecar) work together end-to-end on
    /// real bytes. Opt-in via `LHP_E2E_LIVE=1` (it downloads ~0.6 GB, so it must
    /// be requested explicitly); self-skips otherwise. `LHP_LLAMA_SERVER_BIN`
    /// overrides the vendored binary.
    #[tokio::test]
    async fn live_e2e_search_download_spawn_chat_teardown() {
        if std::env::var_os("LHP_E2E_LIVE").is_none() {
            eprintln!("skipping live E2E — set LHP_E2E_LIVE=1 to run (downloads ~0.6 GB)");
            return;
        }
        use crate::models::hf_search::{self, SearchSort};

        // 1. HF SEARCH — prove discovery returns real rows for the tiny model.
        let hits = hf_search::search("qwen3 0.6b", SearchSort::Downloads, 20)
            .await
            .expect("HF search");
        assert!(!hits.is_empty(), "search returns rows for qwen3-0.6b");

        // 2. Resolve the specific tiny repo's quants; pick the SMALLEST complete
        //    single-file quant (kindest download) — carries the real lfs.oid sha.
        const REPO: &str = "Qwen/Qwen3-0.6B-GGUF";
        let detail = hf_search::model_detail(REPO).await.expect("model detail");
        let quant = detail
            .quants
            .iter()
            .filter(|q| q.complete && q.files.len() == 1)
            .min_by_key(|q| q.total_size_bytes)
            .expect("a complete single-file quant");
        let file = &quant.files[0];
        assert_eq!(file.sha256.len(), 64, "a real 64-hex oid to verify against");

        // 3. DOWNLOAD → VERIFY (the verified-before-runnable invariant on real bytes).
        let dir = std::env::temp_dir().join(format!("lhp-e2e-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let partial = dir.join("model.gguf.partial");
        let gguf = dir.join("model.gguf");
        crate::models::download::download_to_partial(&file.url, &partial, |_, _| {})
            .await
            .expect("download");
        crate::models::download::verify_and_install(&partial, &gguf, &file.sha256)
            .expect("sha256 verifies — bytes match the HF-reported oid");
        assert!(gguf.exists() && !partial.exists());

        // 4. SPAWN the real vendored sidecar against the verified file.
        let bin = std::env::var_os("LHP_LLAMA_SERVER_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("vendor/llama-cpp/macos-arm64/llama-server")
            });
        let sup = Arc::new(LocalRunnerSupervisor::new(
            Arc::new(TokioSpawner),
            Arc::new(HttpHealthCheck::new()),
            SupervisorConfig::default(),
        ));
        let port = pick_free_port().unwrap();
        let base_url = format!("http://127.0.0.1:{port}/v1");
        let cmd = build_args(&bin, &gguf, port, 4, Some(2048), Some(KvCacheQuant::F16));
        sup.ensure_started("e2e", cmd, base_url.clone())
            .await
            .expect("sidecar becomes healthy");

        // 5. Real chat round-trip through the ModelClient.
        let provider = Provider::new("e2e", "E2E", &base_url, None, ProviderKind::Local);
        let client = crate::models::ModelClient::new(provider).unwrap();
        let models = client.list_models().await.expect("GET /v1/models");
        assert!(!models.is_empty());
        let reply = client
            .complete(
                &models[0],
                vec![crate::models::ChatMessage {
                    role: "user".into(),
                    content: "Reply with exactly the word: pong".into(),
                }],
            )
            .await
            .expect("chat completion");
        assert!(!reply.trim().is_empty(), "the model answered: {reply:?}");

        // 6. Clean teardown + cleanup.
        sup.stop("e2e").await;
        assert!(!sup.is_running("e2e"), "sidecar stopped");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
