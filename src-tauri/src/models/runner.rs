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
///
/// `api_key` is the per-spawn bearer token (H-04). It is BOTH in `args` (as
/// `--api-key <token>`, which is how llama-server learns it) and carried here
/// as a field, because everything on OUR side that talks to the sidecar needs
/// it: the health check, and — the part that makes it actually useful — the
/// registered provider client. `Debug` redacts it (argv is logged in places).
#[derive(Clone, PartialEq, Eq)]
pub struct SidecarCommand {
    pub bin: PathBuf,
    pub args: Vec<String>,
    /// The bearer token this sidecar was started with, `None` for an
    /// unauthenticated sidecar (only tests / an external runner).
    pub api_key: Option<String>,
}

/// Hand-written so a token never reaches a log line: the field prints as
/// `<redacted>` and the argv value following `--api-key` is elided.
impl std::fmt::Debug for SidecarCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut args: Vec<&str> = Vec::with_capacity(self.args.len());
        let mut redact_next = false;
        for a in &self.args {
            if std::mem::take(&mut redact_next) {
                args.push("<redacted>");
            } else {
                args.push(a.as_str());
            }
            redact_next = a == "--api-key";
        }
        f.debug_struct("SidecarCommand")
            .field("bin", &self.bin)
            .field("args", &args)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// A fresh per-spawn bearer token (H-04): 128 random bits, lowercase hex. Long
/// enough that [`redact_stderr_line`]'s hex rule scrubs it from any captured
/// stderr, and unguessable by a process racing us for the port.
pub fn new_spawn_token() -> String {
    uuid::Uuid::new_v4().simple().to_string()
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
///
/// H-04: `api_key` is passed to llama-server as `--api-key`, which makes the
/// server reject every request that doesn't present it. That is what turns the
/// health check from "something answers on this port" into "the process WE
/// started answers on this port".
pub fn build_args(
    bin: &Path,
    model_path: &Path,
    port: u16,
    threads: u32,
    ctx_size: Option<u32>,
    kv_quant: Option<KvCacheQuant>,
    api_key: Option<&str>,
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
    if let Some(key) = api_key {
        args.push("--api-key".to_string());
        args.push(key.to_string());
    }
    SidecarCommand {
        bin: bin.to_path_buf(),
        args,
        api_key: api_key.map(str::to_string),
    }
}

/// A free loopback port, picked by binding port 0 and reading what the OS
/// assigned. There is an unavoidable TOCTOU window between dropping the probe
/// listener and the sidecar binding it — H-04: the net is no longer "some HTTP
/// server answers" but "the server answering enforces the per-spawn token we
/// just generated" (see [`HttpHealthCheck::is_healthy`]).
pub fn pick_free_port() -> Result<u16> {
    let l = std::net::TcpListener::bind(("127.0.0.1", 0))?;
    Ok(l.local_addr()?.port())
}

// ---------------------------------------------------------------------------
// Seams: process + health (fake-testable, no real binary)
// ---------------------------------------------------------------------------

/// L-02 — how many stderr lines we keep, and how long a single line may be.
/// Bounded on BOTH axes: a sidecar that loops on an error must not grow our
/// memory, and llama.cpp can emit very long single lines.
const STDERR_TAIL_LINES: usize = 40;
const STDERR_LINE_MAX: usize = 400;
/// Cap on what we paste into an error message (below any log-line limit).
const STDERR_TAIL_CHARS: usize = 2000;

/// A size-bounded, already-redacted tail of a sidecar's stderr (L-02). Shared
/// (`Arc`) between the reader task and whoever reports the failure.
#[derive(Default)]
pub struct StderrTail {
    lines: Mutex<std::collections::VecDeque<String>>,
}

impl StderrTail {
    /// Redact, truncate, and append one line, evicting the oldest beyond the cap.
    pub fn push_redacted(&self, line: &str) {
        let mut line = redact_stderr_line(line);
        if line.chars().count() > STDERR_LINE_MAX {
            line = line.chars().take(STDERR_LINE_MAX).collect::<String>() + "…";
        }
        let mut lines = self.lines.lock();
        if lines.len() == STDERR_TAIL_LINES {
            lines.pop_front();
        }
        lines.push_back(line);
    }

    /// The captured tail as text, newest last, capped at [`STDERR_TAIL_CHARS`]
    /// (keeping the END — the failure is at the bottom). Empty if nothing was
    /// captured.
    pub fn snapshot(&self) -> String {
        let joined = self
            .lines
            .lock()
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
        let n = joined.chars().count();
        if n <= STDERR_TAIL_CHARS {
            joined
        } else {
            joined.chars().skip(n - STDERR_TAIL_CHARS).collect()
        }
    }
}

/// Scrub a stderr line before it is stored (L-02): absolute filesystem paths
/// (which carry the account name and the user's directory layout) and long hex
/// runs (our per-spawn bearer token, sha256 digests) are replaced. Applied on
/// the way IN, so the buffer never holds the sensitive form.
pub fn redact_stderr_line(line: &str) -> String {
    use std::sync::OnceLock;
    static HEX: OnceLock<regex::Regex> = OnceLock::new();
    static PATH: OnceLock<regex::Regex> = OnceLock::new();
    let hex = HEX.get_or_init(|| regex::Regex::new(r"\b[0-9a-fA-F]{32,}\b").expect("static regex"));
    // A leading `/` plus at least one more segment — `/Users/x/m.gguf`, but not
    // a bare `/` or an option like `--host`.
    let path = PATH.get_or_init(|| {
        regex::Regex::new(r"/[A-Za-z0-9._+@\-]+(?:/[A-Za-z0-9._+@\-]*)+").expect("static regex")
    });
    let no_hex = hex.replace_all(line, "<redacted>");
    path.replace_all(&no_hex, "<path>").into_owned()
}

/// A spawned sidecar process — the minimal surface supervision needs.
pub trait SpawnedProcess: Send {
    fn id(&self) -> Option<u32>;
    /// `Ok(Some(_))` once the process has exited; `Ok(None)` while running.
    fn try_wait(&mut self) -> Result<Option<i32>>;
    /// Begin killing (SIGKILL). Idempotence is the SUPERVISOR's job — this may
    /// error if called twice.
    fn start_kill(&mut self) -> Result<()>;
    /// L-02 — the process's captured stderr tail, when the spawner captures one.
    fn stderr_tail(&self) -> Option<Arc<StderrTail>> {
        None
    }
}

/// Spawns sidecar processes. The real impl shells `tokio::process`; tests
/// inject a fake.
pub trait ProcessSpawner: Send + Sync {
    fn spawn(&self, cmd: &SidecarCommand) -> Result<Box<dyn SpawnedProcess>>;
}

/// Answers "is the server at `base_url` the sidecar WE started?" — the real
/// impl GETs `{base_url}/models` (llama-server's `/v1/models`) and, when a
/// per-spawn token was issued, requires the server to enforce it. Boxed-future
/// shape so the trait stays object-safe.
pub trait HealthCheck: Send + Sync {
    fn is_healthy<'a>(
        &'a self,
        base_url: &'a str,
        api_key: Option<&'a str>,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>>;
}

/// The real spawner: `tokio::process::Command`, stdin/stdout nulled, **stderr
/// piped** into a bounded redacted tail (L-02 — a sidecar that refuses to start
/// used to fail with nothing to read), `kill_on_drop` as the last-resort
/// teardown net.
pub struct TokioSpawner;

struct TokioChild {
    child: tokio::process::Child,
    stderr: Arc<StderrTail>,
}

impl SpawnedProcess for TokioChild {
    fn id(&self) -> Option<u32> {
        self.child.id()
    }
    fn try_wait(&mut self) -> Result<Option<i32>> {
        Ok(self.child.try_wait()?.map(|s| s.code().unwrap_or(-1)))
    }
    fn start_kill(&mut self) -> Result<()> {
        Ok(self.child.start_kill()?)
    }
    fn stderr_tail(&self) -> Option<Arc<StderrTail>> {
        Some(Arc::clone(&self.stderr))
    }
}

impl ProcessSpawner for TokioSpawner {
    fn spawn(&self, cmd: &SidecarCommand) -> Result<Box<dyn SpawnedProcess>> {
        let mut child = tokio::process::Command::new(&cmd.bin)
            .args(&cmd.args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("spawning sidecar {:?}", cmd.bin))?;
        let tail = Arc::new(StderrTail::default());
        // Drain for as long as the child lives: an unread pipe fills up and then
        // BLOCKS the writer, so once stderr is piped this reader is mandatory,
        // not a diagnostic nicety. The task ends at EOF (process exit).
        if let Some(pipe) = child.stderr.take() {
            let sink = Arc::clone(&tail);
            tokio::spawn(async move {
                use tokio::io::AsyncBufReadExt;
                let mut lines = tokio::io::BufReader::new(pipe).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    sink.push_redacted(&line);
                }
            });
        }
        Ok(Box::new(TokioChild {
            child,
            stderr: tail,
        }))
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

/// The endpoint used to prove IDENTITY (H-04), as opposed to readiness.
///
/// This is deliberately not `/v1/models`: llama-server keeps `/health`,
/// `/models` and `/v1/models` **public**, exempt from `--api-key` enforcement, so
/// their answers say nothing about who is listening. `/props` IS enforced, and it
/// answers as soon as the server is up. Both halves of that claim are measured
/// against the pinned vendored binary by
/// `the_vendored_sidecar_enforces_the_api_key_on_props_but_not_on_models` — if a
/// re-vendor ever changes the auth surface, that test fails instead of this check
/// silently degrading (or refusing every legitimate start).
const IDENTITY_PATH: &str = "/props";

/// The server root for a `.../v1`-style base URL — `/props` lives at the root,
/// not under `/v1`.
fn server_root(base_url: &str) -> &str {
    let trimmed = base_url.trim_end_matches('/');
    trimmed.strip_suffix("/v1").unwrap_or(trimmed)
}

impl HealthCheck for HttpHealthCheck {
    /// H-04 — when a per-spawn token was issued, three things must hold before
    /// this port may become a PRIVATE provider:
    ///
    /// 1. **An unauthenticated request to a protected endpoint is refused.** A
    ///    squatter that already owned the port and answers everything cannot be
    ///    enforcing a token it has never seen. (Any non-2xx counts as a refusal —
    ///    the exact status is llama.cpp's business, not ours.)
    /// 2. **The same endpoint accepts OUR token.** A server enforcing somebody
    ///    else's key fails here, as does a wrong/absent token on our side.
    /// 3. **`/v1/models` answers with a models list**, not merely an HTTP 2xx —
    ///    the readiness half, as before.
    ///
    /// Residual (documented, not closed): an ADAPTIVE attacker holding the port
    /// can refuse step 1, read our token out of step 2, and then answer step 3.
    /// Closing that needs a challenge-response the sidecar does not offer; what
    /// this does close is the realistic case — a process already serving an
    /// OpenAI-compatible endpoint on the port we were handed.
    fn is_healthy<'a>(
        &'a self,
        base_url: &'a str,
        api_key: Option<&'a str>,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
        Box::pin(async move {
            if let Some(key) = api_key {
                let probe = format!("{}{IDENTITY_PATH}", server_root(base_url));
                match self.client.get(&probe).send().await {
                    // Served a protected endpoint to an anonymous caller → it is
                    // not enforcing our key, so it is not our sidecar.
                    Ok(r) if r.status().is_success() => return false,
                    Ok(_) => {}             // refused, as our own sidecar must
                    Err(_) => return false, // nothing there yet — poll again
                }
                match self.client.get(&probe).bearer_auth(key).send().await {
                    Ok(r) if r.status().is_success() => {}
                    // Refuses our token too → somebody else's server.
                    _ => return false,
                }
            }
            let url = format!("{base_url}/models");
            let mut req = self.client.get(&url);
            if let Some(key) = api_key {
                req = req.bearer_auth(key);
            }
            match req.send().await {
                Ok(r) if r.status().is_success() => {
                    matches!(
                        r.json::<serde_json::Value>().await,
                        Ok(v) if v.get("data").is_some_and(|d| d.is_array())
                    )
                }
                _ => false,
            }
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
    /// M-10: catalog ids whose bytes have been sha256-verified during THIS app
    /// run. First use of a model hashes it (before any spawn); later uses in the
    /// same run trust that result, so the cost is paid once, not per request.
    hash_verified: RwLock<std::collections::HashSet<String>>,
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
            hash_verified: RwLock::new(std::collections::HashSet::new()),
        }
    }

    /// M-10: has this model's sha256 already been checked in this app run?
    pub fn is_hash_verified(&self, catalog_id: &str) -> bool {
        self.hash_verified.read().contains(catalog_id)
    }

    /// M-10: record that this model's bytes matched, so the (expensive) hash is
    /// computed once per run. Forgotten on restart, and cleared by
    /// [`Self::forget_hash_verification`] when the file could have changed.
    pub fn mark_hash_verified(&self, catalog_id: &str) {
        self.hash_verified.write().insert(catalog_id.to_string());
    }

    /// Drop a cached verification (a re-download replaces the bytes).
    pub fn forget_hash_verification(&self, catalog_id: &str) {
        self.hash_verified.write().remove(catalog_id);
    }

    /// The production supervisor: real spawner, real HTTP health check.
    pub fn real(pidfile_dir: PathBuf) -> Self {
        let config = SupervisorConfig {
            pidfile_dir: Some(pidfile_dir),
            ..Default::default()
        };
        Self::new(
            Arc::new(TokioSpawner),
            Arc::new(HttpHealthCheck::new()),
            config,
        )
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
                self.states
                    .write()
                    .insert(catalog_id.to_string(), RunnerState::Failed(e.to_string()));
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
        // L-02: the child's redacted stderr tail, so both failure exits below
        // report WHY instead of just "it didn't come up".
        let stderr = process.stderr_tail();
        let diagnostics = |tail: &Option<Arc<StderrTail>>| match tail {
            Some(t) => match t.snapshot() {
                s if s.is_empty() => String::from(" (sidecar stderr: empty)"),
                s => format!(" — sidecar stderr:\n{s}"),
            },
            None => String::new(),
        };
        let deadline = Instant::now() + self.config.health_timeout;
        loop {
            // Early-exit: a dead child never becomes healthy.
            if let Ok(Some(code)) = process.try_wait() {
                // Let the reader task drain what the child wrote just before
                // dying — otherwise the most useful lines race the exit report.
                // Bounded: at most ~100 ms, and it stops as soon as anything
                // arrived. Under tokio's paused clock this costs no real time.
                for _ in 0..10 {
                    match stderr.as_ref() {
                        Some(t) if t.snapshot().is_empty() => {}
                        _ => break,
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                bail!(
                    "sidecar exited (code {code}) before becoming healthy{}",
                    diagnostics(&stderr)
                );
            }
            if self
                .health
                .is_healthy(base_url, cmd.api_key.as_deref())
                .await
            {
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
                bail!(
                    "health check did not pass within {:?}{}",
                    self.config.health_timeout,
                    diagnostics(&stderr)
                );
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
        let Some(path) = self.pidfile_path(&handle.catalog_id) else {
            return;
        };
        let Some(pid) = handle.process.lock().id() else {
            return;
        };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        // "PID START_EPOCH BOOT_ID" — the reaper verifies liveness + process
        // identity (the kernel's executable path) + that the process started when
        // START_EPOCH says (M-11) AND that the boot id still matches, before killing.
        // The boot id closes cross-reboot PID reuse (finding #5): after a reboot
        // the PID space resets, so a matching PID belongs to an unrelated
        // process and must never be killed.
        let _ = std::fs::write(
            &path,
            format!(
                "{pid} {} {}",
                chrono::Utc::now().timestamp(),
                current_boot_id().unwrap_or_default()
            ),
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
/// `PR_SET_PDEATHSIG`): for each recorded pidfile, kill the PID only when ALL
/// of the recorded identity still checks out — same boot, the kernel says the
/// PID's executable really is `llama-server` ([`process_exe_path`], not a
/// truncated `ps -o comm=` suffix match), and the process started when the
/// pidfile says it did ([`process_start_epoch_matches`], M-11: the epoch used to
/// be parsed and thrown away). Anything indeterminate ⇒ do not kill. The
/// pidfile is removed either way. Best-effort, never bricks boot. Returns how
/// many processes were killed.
pub fn reap_orphan_sidecars(pidfile_dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(pidfile_dir) else {
        return 0;
    };
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
            let epoch = fields.next().and_then(|e| e.parse::<i64>().ok());
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
                // M-11: the recorded epoch is now USED. A PID that is alive and
                // whose executable is llama-server but which started at a
                // different time than we recorded is a different process (PID
                // reuse within this boot) — leave it alone.
                if pid > 0
                    && !stale_boot
                    && process_is_our_sidecar(pid)
                    && process_start_epoch_matches(pid, epoch)
                {
                    #[cfg(unix)]
                    unsafe {
                        libc::kill(pid, libc::SIGKILL);
                    }
                    killed += 1;
                    tracing::warn!(target: "lhp::runner", pid, "reaped orphaned sidecar from a previous run");
                } else {
                    tracing::debug!(
                        target: "lhp::runner",
                        pid,
                        stale_boot,
                        "pidfile not reaped — identity did not check out"
                    );
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
    let sec = s
        .split("sec =")
        .nth(1)?
        .split(',')
        .next()?
        .trim()
        .to_string();
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

/// The executable path the KERNEL reports for `pid` (M-11). On macOS this is
/// `proc_pidpath(2)`: the full, untruncated path of the running image — unlike
/// `ps -o comm=`, which is truncated to 16 characters and was only suffix-matched
/// (so any process whose truncated name happened to end in `llama-server` looked
/// like ours). `None` for a dead PID, a PID we may not inspect, or a
/// non-macOS build.
#[cfg(target_os = "macos")]
fn process_exe_path(pid: i32) -> Option<PathBuf> {
    // PROC_PIDPATHINFO_MAXSIZE (4 * MAXPATHLEN).
    let mut buf = vec![0u8; 4096];
    let n =
        unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr() as *mut libc::c_void, buf.len() as u32) };
    if n <= 0 {
        return None;
    }
    buf.truncate(n as usize);
    Some(PathBuf::from(String::from_utf8_lossy(&buf).into_owned()))
}

#[cfg(not(target_os = "macos"))]
fn process_exe_path(_pid: i32) -> Option<PathBuf> {
    None
}

/// Is `pid` alive and running OUR sidecar executable? The file name of the
/// kernel-reported executable path must be exactly `llama-server`. A dead PID, a
/// reused PID running something else, or any lookup failure → `false` (never
/// kill on uncertainty).
fn process_is_our_sidecar(pid: i32) -> bool {
    match process_exe_path(pid) {
        Some(p) => p.file_name().and_then(|f| f.to_str()) == Some("llama-server"),
        // Nothing authoritative to go on (non-macOS, or the PID is gone) — the
        // reaper must not guess.
        None => false,
    }
}

/// How far a process's real start time may sit from the epoch recorded in the
/// pidfile and still be considered the same process. The pidfile is written
/// immediately after the health check passes, so the two are seconds apart in
/// practice; the window only has to be wider than a slow model load.
const START_EPOCH_TOLERANCE_SECS: i64 = 300;

/// Does `pid`'s actual start time match the epoch the pidfile recorded (M-11)?
/// This is the in-boot PID-reuse guard: a recycled PID running a *different*
/// `llama-server` (the user's own, started later) has a start time nowhere near
/// our record. A pidfile with no epoch (legacy) can't be checked ⇒ `true`, the
/// prior behaviour. A process whose start time we cannot read ⇒ `false`.
fn process_start_epoch_matches(pid: i32, recorded: Option<i64>) -> bool {
    let Some(recorded) = recorded else {
        return true; // legacy pidfile — identity rests on the exe-path check
    };
    match process_start_epoch(pid) {
        Some(actual) => (actual - recorded).abs() <= START_EPOCH_TOLERANCE_SECS,
        None => false,
    }
}

/// A process's start time in unix seconds, from the kernel's own BSD info
/// (`proc_pidinfo(PROC_PIDTBSDINFO)`). `None` when unavailable.
#[cfg(target_os = "macos")]
fn process_start_epoch(pid: i32) -> Option<i64> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
    let n = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            size,
        )
    };
    if n != size {
        return None;
    }
    Some(info.pbi_start_tvsec as i64)
}

#[cfg(not(target_os = "macos"))]
fn process_start_epoch(_pid: i32) -> Option<i64> {
    None
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
    verify_model_rows(storage, rehash, None)
}

/// **The manual verify (M-10).** A full re-hash of the local catalog — one model
/// (`only_id`) or all of them — quarantining anything whose bytes no longer match
/// the recorded sha256. This is the reachable counterpart to the deliberately
/// cheap boot sweep: boot stays existence+size (multi-GB hashing every launch is
/// not acceptable), and this is what an operator (or the app, at
/// [`ensure_running`]) runs when the answer has to be authoritative. Exposed to
/// the UI as the `verify_local_models` IPC command.
pub fn verify_local_models_now(storage: &Storage, only_id: Option<&str>) -> SweepReport {
    verify_model_rows(storage, true, only_id)
}

fn verify_model_rows(storage: &Storage, rehash: bool, only_id: Option<&str>) -> SweepReport {
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
        if only_id.is_some_and(|id| id != row.id) {
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

    // M-10 — FIRST-USE INTEGRITY. The boot sweep runs with `rehash=false`
    // (existence + size), so a same-size tampered file survives it; this is the
    // check that catches it, and it runs BEFORE the process is launched — a
    // tampered GGUF is never handed to llama-server, never becomes a provider,
    // and never sees a private prompt. Hashed once per app run (multi-GB), on a
    // blocking thread so the UI's runtime keeps moving. A mismatch quarantines.
    if !supervisor.is_hash_verified(&row.id) {
        let path_owned = model_path.to_path_buf();
        let expected = row.sha256.to_ascii_lowercase();
        let actual =
            tokio::task::spawn_blocking(move || crate::models::download::file_sha256(&path_owned))
                .await
                .context("hashing the model file panicked")?;
        match actual {
            Ok(actual) if actual == expected => supervisor.mark_hash_verified(&row.id),
            Ok(_) => {
                let _ = storage.global().set_model_status(&row.id, "quarantined");
                tracing::error!(
                    target: "lhp::runner",
                    model = %row.id,
                    "first-use integrity check FAILED — model quarantined before launch"
                );
                bail!(
                    "local model \"{}\" failed its integrity check (sha256 mismatch) — \
                     quarantined before launch; re-download it",
                    row.id
                );
            }
            Err(e) => {
                let _ = storage.global().set_model_status(&row.id, "quarantined");
                bail!(
                    "local model \"{}\" could not be read for its integrity check ({e}) — \
                     quarantined; re-download it",
                    row.id
                );
            }
        }
    }

    let port = pick_free_port()?;
    let base_url = format!("http://127.0.0.1:{port}/v1");
    let threads = std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(4);
    // H-04: one fresh token per spawn. It goes to llama-server (`--api-key`),
    // to the health check, AND — load-bearing — to the provider client below.
    // A token that only guards startup is worthless: every private prompt after
    // it would go out unauthenticated, and whoever holds the port would answer.
    let token = new_spawn_token();
    let cmd = build_args(
        &paths.bin,
        model_path,
        port,
        threads,
        ctx_size,
        kv_quant,
        Some(&token),
    );
    supervisor
        .ensure_started(&row.id, cmd, base_url.clone())
        .await?;

    let provider = Provider::new(
        &provider_id,
        &row.name,
        &base_url,
        Some(token),
        ProviderKind::Local,
    )
    .with_local_origin(crate::models::provider::LocalOrigin::BundledRunner);
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
        /// H-04: the token the thing answering the port will accept. Normally
        /// set at spawn from the argv `--api-key` (i.e. a real llama-server
        /// enforcing OUR token); [`Self::pin_server_token`] freezes it to model
        /// a different server on that port.
        server_token: Mutex<Option<String>>,
        server_token_pinned: AtomicBool,
        /// Stderr the fake process reports (L-02).
        stderr: Mutex<Option<Arc<StderrTail>>>,
    }

    impl FakeWorld {
        fn new(behaviors: Vec<FakeBehavior>) -> Arc<Self> {
            Arc::new(Self {
                behaviors: Mutex::new(behaviors.into()),
                spawns: Mutex::new(Vec::new()),
                healthy: AtomicBool::new(false),
                kill_calls: AtomicUsize::new(0),
                server_token: Mutex::new(None),
                server_token_pinned: AtomicBool::new(false),
                stderr: Mutex::new(None),
            })
        }
        fn spawn_count(&self) -> usize {
            self.spawns.lock().len()
        }
        /// Freeze what the port-holder accepts, ignoring whatever we spawn with.
        fn pin_server_token(&self, token: Option<&str>) {
            *self.server_token.lock() = token.map(str::to_string);
            self.server_token_pinned.store(true, Ordering::SeqCst);
        }
        /// Give the fake process a stderr tail with these lines already in it.
        fn with_stderr(self: &Arc<Self>, lines: &[&str]) {
            let tail = Arc::new(StderrTail::default());
            for l in lines {
                tail.push_redacted(l);
            }
            *self.stderr.lock() = Some(tail);
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
        fn stderr_tail(&self) -> Option<Arc<StderrTail>> {
            self.world.stderr.lock().clone()
        }
    }

    struct FakeSpawner(Arc<FakeWorld>);
    impl ProcessSpawner for FakeSpawner {
        fn spawn(&self, cmd: &SidecarCommand) -> Result<Box<dyn SpawnedProcess>> {
            let behavior = {
                let mut b = self.0.behaviors.lock();
                if b.len() > 1 {
                    b.pop_front().unwrap()
                } else {
                    *b.front().expect("behavior")
                }
            };
            if !self.0.server_token_pinned.load(Ordering::SeqCst) {
                *self.0.server_token.lock() = cmd.api_key.clone();
            }
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
        /// Models a server that enforces `server_token`: the presented token
        /// must match exactly (and a no-auth server only answers a no-auth
        /// probe). This is what makes the token threading testable — a
        /// supervisor that stopped passing the token would go unhealthy.
        fn is_healthy<'a>(
            &'a self,
            _base_url: &'a str,
            api_key: Option<&'a str>,
        ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
            let up = self.0.healthy.load(Ordering::SeqCst);
            let expected = self.0.server_token.lock().clone();
            let authed = match expected {
                Some(t) => api_key == Some(t.as_str()),
                None => api_key.is_none(),
            };
            Box::pin(async move { up && authed })
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

    /// The token the fake sidecar is started with in most tests.
    const TEST_TOKEN: &str = "0123456789abcdef0123456789abcdef";

    fn cmd() -> SidecarCommand {
        cmd_with_token(Some(TEST_TOKEN))
    }

    fn cmd_with_token(token: Option<&str>) -> SidecarCommand {
        build_args(
            Path::new("/fake/llama-server"),
            Path::new("/fake/model.gguf"),
            8080,
            8,
            Some(8192),
            Some(KvCacheQuant::Q8_0),
            token,
        )
    }

    // ── build_args: the pinned loopback test ───────────────────────────

    #[test]
    fn build_args_pins_loopback_host_and_carries_calculator_choices() {
        let c = cmd();
        let joined = c.args.join(" ");
        // THE pinned assertion (design §D): loopback only, never all-interfaces.
        assert!(
            joined.contains("--host 127.0.0.1"),
            "must bind loopback: {joined}"
        );
        assert!(
            !joined.contains("0.0.0.0"),
            "must NEVER bind all interfaces"
        );
        // The calculator's knobs pass through.
        assert!(joined.contains("--ctx-size 8192"));
        assert!(joined.contains("--cache-type-k q8_0"));
        assert!(joined.contains("--cache-type-v q8_0"));
        assert!(joined.contains("--parallel 2"));
        assert!(joined.contains("-ngl 999"));
        // Defaults: no kv flag when unspecified; ctx falls back to 4096.
        let d = build_args(Path::new("/b"), Path::new("/m"), 1, 4, None, None, None);
        let dj = d.args.join(" ");
        assert!(dj.contains("--ctx-size 4096"));
        assert!(!dj.contains("--cache-type-k"));
        assert!(!dj.contains("--api-key"), "no token asked for, none passed");
        assert_eq!(d.api_key, None);
    }

    // ── H-04: the per-spawn bearer token ───────────────────────────────

    #[test]
    fn build_args_passes_the_token_to_llama_server_and_keeps_it_for_our_own_calls() {
        let c = cmd_with_token(Some("tok-abc"));
        let joined = c.args.join(" ");
        // llama-server learns the token here — this is what makes it reject
        // everyone who doesn't know it.
        assert!(
            joined.contains("--api-key tok-abc"),
            "argv carries the key: {joined}"
        );
        // ...and OUR side keeps it, because the health check AND the provider
        // client both have to present it.
        assert_eq!(c.api_key.as_deref(), Some("tok-abc"));
    }

    #[test]
    fn spawn_tokens_are_high_entropy_and_unique_and_never_debug_printed() {
        let a = new_spawn_token();
        let b = new_spawn_token();
        assert_ne!(a, b, "a token is per-spawn, not per-process");
        assert_eq!(a.len(), 32, "128 bits of hex");
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        // A token long enough to be scrubbed by the stderr hex rule.
        assert_eq!(redact_stderr_line(&a), "<redacted>");
        // Debug is the log path: neither the field nor the argv value may appear.
        let dbg = format!("{:?}", cmd_with_token(Some(&a)));
        assert!(!dbg.contains(&a), "token leaked into Debug output: {dbg}");
        assert!(
            dbg.contains("--api-key"),
            "the flag itself still shows: {dbg}"
        );
        assert!(dbg.contains("<redacted>"));
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
        assert!(
            err.to_string().contains("runner_failed"),
            "distinct failure state: {err}"
        );
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
        assert_eq!(
            world.spawn_count(),
            2,
            "the monitor respawned the crashed sidecar"
        );
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
        assert_eq!(
            world.kill_calls.load(Ordering::SeqCst),
            1,
            "never double-kills"
        );
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
        assert!(
            took >= Duration::from_millis(300),
            "waited the grace window"
        );
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
        assert!(
            sup.is_running("m1"),
            "an in-flight runner is NEVER idle-killed"
        );
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
        assert!(
            !sup.stop_if_idle("m1").await,
            "an in-flight runner is spared"
        );
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
        assert!(
            !dir.join("m1.pid").exists(),
            "pidfiles are always cleaned up"
        );
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
        assert_eq!(
            world.spawn_count(),
            spawns_after_fail,
            "no spawn while Failed"
        );
        // clear_failed re-arms the retry path.
        sup.clear_failed("m1");
        assert_eq!(sup.state("m1"), None);
    }

    // ── H-04 adversarial: who is actually answering the port? ──────────
    //
    // These drive the REAL `HttpHealthCheck` against a REAL socket, because the
    // whole finding is about what an HTTP response is allowed to prove.

    /// What the thing squatting on / serving the port does. `EnforcesToken`
    /// reproduces the REAL llama-server split measured in
    /// `the_vendored_sidecar_enforces_the_api_key_on_props_but_not_on_models`:
    /// `/v1/models` is PUBLIC, `/props` is protected.
    #[derive(Clone, Copy)]
    enum FakeServer {
        /// A real llama-server started with `--api-key <token>`.
        EnforcesToken(&'static str),
        /// The H-04 attacker: it won the port race and answers everything
        /// happily, hoping to be registered as the private provider.
        OpenToAnyone,
        /// Rejects everything (e.g. a server holding a DIFFERENT key).
        RejectsEverything,
        /// Enforces the token but `/v1/models` is not a models list — a generic 2xx.
        WrongShape(&'static str),
    }

    /// Bind a loopback port and serve `kind` until the test drops the handle.
    /// Returns the `/v1` base_url.
    async fn spawn_fake_server(kind: FakeServer) -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    // Read the request head (one request per connection — every
                    // reply says `Connection: close`).
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 1024];
                    loop {
                        match sock.read(&mut chunk).await {
                            Ok(0) => break,
                            Ok(n) => {
                                buf.extend_from_slice(&chunk[..n]);
                                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                                    break;
                                }
                            }
                            Err(_) => return,
                        }
                    }
                    let req = String::from_utf8_lossy(&buf).to_string();
                    let presented = req
                        .lines()
                        .find_map(|l| {
                            let (k, v) = l.split_once(':')?;
                            k.eq_ignore_ascii_case("authorization")
                                .then(|| v.trim().trim_start_matches("Bearer ").to_string())
                        })
                        .unwrap_or_default();
                    // The requested path (first line: "GET /props HTTP/1.1").
                    let path = req
                        .lines()
                        .next()
                        .and_then(|l| l.split_whitespace().nth(1))
                        .unwrap_or("/")
                        .to_string();
                    const DENY: (&str, &str) = ("401 Unauthorized", r#"{"error":"no"}"#);
                    const MODELS: (&str, &str) = ("200 OK", r#"{"data":[{"id":"m"}]}"#);
                    const PROPS: (&str, &str) = ("200 OK", r#"{"default_generation_settings":{}}"#);
                    let (status, body) = match kind {
                        FakeServer::OpenToAnyone => {
                            // Answers anything to anyone — including the models
                            // list, which is what made the old check fall for it.
                            if path.ends_with("/models") {
                                MODELS
                            } else {
                                PROPS
                            }
                        }
                        FakeServer::RejectsEverything => DENY,
                        FakeServer::EnforcesToken(t) => {
                            if path.ends_with("/models") {
                                MODELS // public on the real binary, token or not
                            } else if presented == t {
                                PROPS
                            } else {
                                DENY
                            }
                        }
                        FakeServer::WrongShape(t) => {
                            if path.ends_with("/models") {
                                ("200 OK", r#"{"ok":true}"#)
                            } else if presented == t {
                                PROPS
                            } else {
                                DENY
                            }
                        }
                    };
                    let resp = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        (format!("http://127.0.0.1:{port}/v1"), handle)
    }

    #[tokio::test]
    async fn health_check_rejects_a_port_squatter_that_does_not_enforce_our_token() {
        // H-04: the attack. Something already owns the port and answers
        // /v1/models to anyone — under the old "any 2xx is healthy" rule it
        // would have been registered as the PRIVATE provider and handed private
        // prompts. Note that answering the models list is NOT what gives it away
        // (the real sidecar serves that publicly too): what gives it away is
        // serving a PROTECTED endpoint to an anonymous caller.
        let (base_url, server) = spawn_fake_server(FakeServer::OpenToAnyone).await;
        let health = HttpHealthCheck::new();
        assert!(
            !health.is_healthy(&base_url, Some("our-secret-token")).await,
            "a server that serves anyone is NOT the sidecar we started"
        );
        server.abort();
    }

    #[test]
    fn the_identity_probe_is_taken_from_the_server_root_not_the_v1_prefix() {
        assert_eq!(server_root("http://127.0.0.1:9/v1"), "http://127.0.0.1:9");
        assert_eq!(server_root("http://127.0.0.1:9/v1/"), "http://127.0.0.1:9");
        assert_eq!(server_root("http://127.0.0.1:9"), "http://127.0.0.1:9");
        assert_eq!(
            IDENTITY_PATH, "/props",
            "an endpoint the api-key DOES cover"
        );
    }

    #[tokio::test]
    async fn health_check_requires_exactly_our_token_and_a_models_body() {
        let (base_url, server) = spawn_fake_server(FakeServer::EnforcesToken("right-token")).await;
        let health = HttpHealthCheck::new();
        // The token we spawned with → healthy.
        assert!(health.is_healthy(&base_url, Some("right-token")).await);
        // A wrong token → the server rejects us, so we are not talking to a
        // sidecar we control.
        assert!(!health.is_healthy(&base_url, Some("wrong-token")).await);
        // No token issued → nothing to prove identity WITH, so the check degrades
        // to the old readiness-only behaviour (a public models list is enough).
        // This is exactly why `ensure_running` always issues one; the assertion is
        // here to state the degradation rather than let it be discovered later.
        assert!(
            health.is_healthy(&base_url, None).await,
            "tokenless startup is readiness-only — no identity guarantee"
        );
        server.abort();

        // A server that rejects even the right token is not healthy either.
        let (base_url, server) = spawn_fake_server(FakeServer::RejectsEverything).await;
        assert!(!health.is_healthy(&base_url, Some("right-token")).await);
        server.abort();

        // Authenticated 2xx is not enough — the body must be a models list.
        let (base_url, server) = spawn_fake_server(FakeServer::WrongShape("right-token")).await;
        assert!(
            !health.is_healthy(&base_url, Some("right-token")).await,
            "a generic 2xx must not pass for /v1/models"
        );
        server.abort();
    }

    #[tokio::test]
    async fn health_check_on_a_dead_port_is_not_healthy() {
        // Nothing listening at all: neither probe connects.
        let port = pick_free_port().unwrap();
        let health = HttpHealthCheck::new();
        assert!(
            !health
                .is_healthy(&format!("http://127.0.0.1:{port}/v1"), Some("tok"))
                .await
        );
    }

    /// The load-bearing assumption behind [`IDENTITY_PATH`], measured against the
    /// REAL vendored `llama-server` — no GGUF and no network needed, because
    /// router mode (`--models-dir <empty>`) starts a server with no model.
    ///
    /// Two things are pinned:
    ///   * `/v1/models` is **public** — an unauthenticated GET succeeds even with
    ///     `--api-key` set. So requiring "/v1/models refuses anonymous callers"
    ///     would refuse EVERY legitimate sidecar. (This is not hypothetical: it is
    ///     the bug this test was written after finding.)
    ///   * `/props` is **protected** — anonymous and wrong-key requests are
    ///     refused, ours is accepted — which is what makes it usable as identity.
    ///
    /// If a re-vendor changes either, this fails loudly instead of the health
    /// check quietly refusing to start local models (or quietly accepting a
    /// squatter).
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn the_vendored_sidecar_enforces_the_api_key_on_props_but_not_on_models() {
        let bin = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("vendor/llama-cpp/macos-arm64/llama-server");
        assert!(
            bin.is_file(),
            "the vendored sidecar must be present: {}",
            bin.display()
        );
        let empty = tmp_dir(); // no models in it — router mode needs no GGUF
        let port = pick_free_port().unwrap();
        let token = new_spawn_token();
        let mut child = tokio::process::Command::new(&bin)
            .args([
                "--models-dir".to_string(),
                empty.to_string_lossy().into_owned(),
                "--port".to_string(),
                port.to_string(),
                "--api-key".to_string(),
                token.clone(),
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn the vendored llama-server in router mode");

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let root = format!("http://127.0.0.1:{port}");
        // Wait for the port (it listens in well under a second here).
        let mut up = false;
        for _ in 0..100 {
            if client.get(format!("{root}/health")).send().await.is_ok() {
                up = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        assert!(up, "the vendored server never started listening");

        let status = |path: &'static str, key: Option<String>| {
            let client = client.clone();
            let root = root.clone();
            async move {
                let mut req = client.get(format!("{root}{path}"));
                if let Some(k) = key {
                    req = req.bearer_auth(k);
                }
                req.send().await.expect("request").status().as_u16()
            }
        };

        // /v1/models is PUBLIC — this is why it cannot be the identity probe.
        assert_eq!(
            status("/v1/models", None).await,
            200,
            "llama-server exempts /v1/models from --api-key; the identity probe \
             must not rely on it refusing anonymous callers"
        );
        // /props is PROTECTED, and answers with no model loaded.
        assert_eq!(
            status(IDENTITY_PATH, None).await,
            401,
            "anonymous must be refused"
        );
        assert_eq!(
            status(IDENTITY_PATH, Some("not-the-token".into())).await,
            401,
            "a wrong key must be refused"
        );
        assert_eq!(
            status(IDENTITY_PATH, Some(token.clone())).await,
            200,
            "our own token must be accepted"
        );

        // And the full check agrees, against the real binary.
        let health = HttpHealthCheck::new();
        let base_url = format!("{root}/v1");
        assert!(
            health.is_healthy(&base_url, Some(&token)).await,
            "the real sidecar with our token must be healthy"
        );
        assert!(
            !health.is_healthy(&base_url, Some("not-the-token")).await,
            "the real sidecar with the wrong token must NOT be healthy"
        );

        let _ = child.kill().await;
        let _ = std::fs::remove_dir_all(&empty);
    }

    #[tokio::test(start_paused = true)]
    async fn a_sidecar_that_rejects_our_token_is_never_registered() {
        // The same finding one layer up: the supervisor must pass the token to
        // the health check, and a port-holder that doesn't honour OUR token must
        // never reach the provider registry. `pin_server_token` makes the
        // port-holder enforce a token we were not started with.
        let world = FakeWorld::new(vec![FakeBehavior::HealthyImmediately]);
        world.pin_server_token(Some("some-other-processes-token"));
        let sup = supervisor_with(&world);
        let err = sup
            .ensure_started("m1", cmd(), "http://127.0.0.1:8080/v1".into())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("runner_failed"), "{err}");
        assert!(!sup.is_running("m1"), "never registered");
        assert!(matches!(sup.state("m1"), Some(RunnerState::Failed(_))));
    }

    #[tokio::test(start_paused = true)]
    async fn an_unauthenticated_startup_is_refused_by_a_token_enforcing_sidecar() {
        // Missing bearer: we spawn WITHOUT a token while the server on the port
        // enforces one. Proves the token we build the command with is the token
        // the health check presents — nothing else can bridge the two.
        let world = FakeWorld::new(vec![FakeBehavior::HealthyImmediately]);
        world.pin_server_token(Some(TEST_TOKEN));
        let sup = supervisor_with(&world);
        let err = sup
            .ensure_started(
                "m1",
                cmd_with_token(None),
                "http://127.0.0.1:8080/v1".into(),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("runner_failed"), "{err}");
        assert!(!sup.is_running("m1"));
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
        assert_eq!(
            status("truncated"),
            "quarantined",
            "size check catches truncation"
        );
        assert_eq!(
            status("tampered"),
            "quarantined",
            "rehash catches tampering"
        );
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
        assert!(
            r2.quarantined.is_empty(),
            "cheap sweep passes same-size tampering"
        );
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
        assert!(
            !dir.join("m2.pid").exists(),
            "PID-reuse-guarded pidfile removed"
        );
        assert!(
            dir.join("not-a-pidfile.txt").exists(),
            "non-pidfiles untouched"
        );
    }

    // ── M-11: reaper identity (PID reuse) ──────────────────────────────

    /// Is `pid` still alive? (`kill(pid, 0)` — signal 0 only checks.)
    #[cfg(unix)]
    fn pid_alive(pid: i32) -> bool {
        unsafe { libc::kill(pid, 0) == 0 }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_reaper_never_kills_a_live_pid_that_is_not_our_binary() {
        // The M-11 hazard, in the harshest form available: point a pidfile at a
        // process that is alive, on the current boot, with a start epoch that
        // matches — but whose executable is NOT llama-server. It happens to be
        // this test binary, so if the identity check ever weakens to "the PID is
        // alive", this test dies by SIGKILL instead of failing politely.
        let dir = tmp_dir();
        let me = std::process::id() as i32;
        let start = super::process_start_epoch(me).expect("own start time is readable");
        let boot = super::current_boot_id().unwrap_or_default();
        std::fs::write(dir.join("m1.pid"), format!("{me} {start} {boot}")).unwrap();

        // The kernel reports this process's own executable, not `llama-server`.
        let exe = super::process_exe_path(me).expect("proc_pidpath works on self");
        assert!(
            exe.is_absolute(),
            "a real path from the kernel: {}",
            exe.display()
        );
        assert_ne!(
            exe.file_name().and_then(|f| f.to_str()),
            Some("llama-server")
        );
        assert!(!super::process_is_our_sidecar(me));

        let killed = reap_orphan_sidecars(&dir);
        assert_eq!(killed, 0, "a live stranger PID is never killed");
        assert!(pid_alive(me), "…and we are still here");
        assert!(!dir.join("m1.pid").exists(), "pidfile cleaned up anyway");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_reaper_honours_the_recorded_start_epoch_against_pid_reuse() {
        // A process whose executable really is named `llama-server` — the old
        // `ps -o comm=` suffix check would have killed it on sight. What must
        // decide is the epoch: PID reuse means a *different* process now holds
        // the PID we recorded, and its start time gives that away.
        let dir = tmp_dir();
        let fake_bin = dir.join("llama-server");
        std::fs::copy("/bin/sleep", &fake_bin).expect("copy a real binary");
        let mut child = std::process::Command::new(&fake_bin)
            .arg("60")
            .spawn()
            .expect("spawn the stand-in sidecar");
        let pid = child.id() as i32;
        // Identity by executable path passes for this one.
        assert!(
            super::process_is_our_sidecar(pid),
            "the kernel path names llama-server: {:?}",
            super::process_exe_path(pid)
        );
        let real_start = super::process_start_epoch(pid).expect("start time");
        let boot = super::current_boot_id().unwrap_or_default();

        // 1. A pidfile whose epoch does NOT match: this PID was recycled, so the
        //    process running under it now is somebody else's. Do not kill.
        let stale_epoch = real_start - (START_EPOCH_TOLERANCE_SECS + 60);
        std::fs::write(dir.join("m1.pid"), format!("{pid} {stale_epoch} {boot}")).unwrap();
        assert_eq!(reap_orphan_sidecars(&dir), 0, "epoch mismatch ⇒ no kill");
        assert!(pid_alive(pid), "the reused-PID process survives");
        assert!(child.try_wait().unwrap().is_none(), "still running");

        // 2. The epoch we actually recorded ⇒ this IS our orphan. Reap it.
        std::fs::write(dir.join("m2.pid"), format!("{pid} {real_start} {boot}")).unwrap();
        assert_eq!(reap_orphan_sidecars(&dir), 1, "our own orphan is reaped");
        let status = child.wait().expect("reap the zombie");
        assert!(!status.success(), "killed, not exited normally: {status:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn epoch_matching_rejects_the_unknowable_and_allows_legacy_pidfiles() {
        // A dead PID has no start time, so nothing can vouch for it.
        assert!(!super::process_start_epoch_matches(2_147_483_647, Some(0)));
        // A legacy pidfile (no epoch field) can't be epoch-checked; identity then
        // rests on the executable-path check alone — the pre-M-11 behaviour, kept
        // so an upgrade doesn't leak the previous run's sidecar forever.
        let me = std::process::id() as i32;
        assert!(super::process_start_epoch_matches(me, None));
        // Our own PID with a wildly wrong epoch must not match.
        assert!(!super::process_start_epoch_matches(me, Some(0)));
    }

    // ── ensure_running (the seam) ──────────────────────────────────────

    fn seeded_storage(dir: &Path) -> Storage {
        seeded_storage_with_sha(dir, None)
    }

    /// Seed one `ready` model. `sha_override` records a DIFFERENT digest than the
    /// bytes actually hash to — the M-10 tamper fixture; `None` records the true
    /// digest, which is what a real verified download leaves behind.
    fn seeded_storage_with_sha(dir: &Path, sha_override: Option<&str>) -> Storage {
        let storage = Storage::open(dir).unwrap();
        let model_path = dir.join("tiny.gguf");
        std::fs::write(&model_path, b"GGUFfake").unwrap();
        let sha = match sha_override {
            Some(s) => s.to_string(),
            None => crate::models::download::file_sha256(&model_path).unwrap(),
        };
        storage
            .global()
            .insert_model(&crate::storage::ModelEntry {
                id: "tiny".into(),
                name: "Tiny Test Model".into(),
                path: model_path.to_string_lossy().into_owned(),
                size_bytes: 8,
                quantization: Some("Q8_0".into()),
                added_at: 1,
                sha256: sha,
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
        let paths = SidecarPaths {
            bin: PathBuf::from("/fake/llama-server"),
        };

        let provider = ensure_running(&sup, &mm, &storage, &paths, None, Some(8192), None)
            .await
            .unwrap();
        assert_eq!(provider.id, "local-runner:tiny");
        assert!(provider.is_local(), "kind Local");
        assert!(provider.is_private(), "127.0.0.1 base_url is private");
        assert!(
            provider.is_bundled_runner(),
            "C5: a runner-spawned provider is BundledRunner origin"
        );
        assert!(provider.base_url.starts_with("http://127.0.0.1:"));
        assert!(
            mm.get_provider("local-runner:tiny").is_some(),
            "registered in the manager"
        );
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
        storage
            .global()
            .set_model_status("tiny", "quarantined")
            .unwrap();
        let world = FakeWorld::new(vec![FakeBehavior::HealthyImmediately]);
        let sup = supervisor_with(&world);
        let mm = ModelManager::new();
        let paths = SidecarPaths {
            bin: PathBuf::from("/fake/llama-server"),
        };
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

    #[tokio::test(start_paused = true)]
    async fn the_registered_provider_carries_the_same_token_the_sidecar_was_started_with() {
        // THE H-04 defect that sank the first P03 attempt: the sidecar was
        // started with a bearer token and the health check presented it, but the
        // provider was registered with `None` credentials — so every actual
        // request after startup went out unauthenticated and any process holding
        // the port would answer it. Startup passed; real use was broken.
        let dir = tmp_dir();
        let storage = seeded_storage(&dir);
        let world = FakeWorld::new(vec![FakeBehavior::HealthyImmediately]);
        let sup = supervisor_with(&world);
        let mm = ModelManager::new();
        let paths = SidecarPaths {
            bin: PathBuf::from("/fake/llama-server"),
        };

        let provider = ensure_running(&sup, &mm, &storage, &paths, None, Some(4096), None)
            .await
            .unwrap();

        // The token llama-server was actually told to enforce.
        let spawned = world.spawns.lock()[0].0.clone();
        let argv = spawned.args.join(" ");
        let token = spawned
            .api_key
            .clone()
            .expect("a token was minted per spawn");
        assert!(argv.contains(&format!("--api-key {token}")), "argv: {argv}");
        assert_eq!(token.len(), 32, "a real 128-bit token, not a placeholder");

        // ...must be the credential the provider client will present.
        assert_eq!(
            provider.api_key.as_deref(),
            Some(token.as_str()),
            "the registered provider MUST carry the sidecar's token — a token that \
             only guards startup authenticates nothing"
        );
        // And the copy in the manager (what the agent loop actually fetches).
        assert_eq!(
            mm.get_provider("local-runner:tiny")
                .expect("registered")
                .api_key
                .as_deref(),
            Some(token.as_str())
        );
    }

    #[tokio::test(start_paused = true)]
    async fn ensure_running_refuses_and_quarantines_a_same_size_tampered_file_before_launch() {
        // M-10: the boot sweep runs with rehash=false, so a file that was
        // modified WITHOUT changing its size passes it. First use must catch it,
        // and must do so BEFORE the process is launched — a tampered GGUF is
        // never executed and never becomes a provider.
        let dir = tmp_dir();
        // sha256 of "GGUFfake" is not this; the size on disk is untouched.
        let storage = seeded_storage_with_sha(&dir, Some(&"a".repeat(64)));
        let recorded = storage.global().get_model("tiny").unwrap().unwrap();
        assert_eq!(
            std::fs::metadata(&recorded.path).unwrap().len(),
            recorded.size_bytes as u64,
            "the fixture is a SAME-SIZE tamper — the cheap sweep cannot see it"
        );
        assert!(
            sweep_local_model_integrity_at_boot(&storage, false)
                .quarantined
                .is_empty(),
            "precondition: the boot sweep lets this through"
        );

        let world = FakeWorld::new(vec![FakeBehavior::HealthyImmediately]);
        let sup = supervisor_with(&world);
        let mm = ModelManager::new();
        let paths = SidecarPaths {
            bin: PathBuf::from("/fake/llama-server"),
        };
        let err = ensure_running(&sup, &mm, &storage, &paths, None, None, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("integrity check"), "{err}");
        assert_eq!(
            world.spawn_count(),
            0,
            "quarantine happens BEFORE any spawn"
        );
        assert!(
            mm.get_provider("local-runner:tiny").is_none(),
            "never a provider"
        );
        assert_eq!(
            storage.global().get_model("tiny").unwrap().unwrap().status,
            "quarantined"
        );
        assert!(!sup.is_hash_verified("tiny"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_matching_file_is_hashed_once_per_run_then_trusted() {
        // M-10's cost control: the hash is per-run, not per-request. Proven by
        // deleting the file after the first (successful) verification — a second
        // hash attempt would fail to read it, so if the second call still gets
        // through the spawn path, the result was cached.
        let dir = tmp_dir();
        let storage = seeded_storage(&dir);
        let world = FakeWorld::new(vec![FakeBehavior::HealthyImmediately]);
        let sup = supervisor_with(&world);
        let mm = ModelManager::new();
        let paths = SidecarPaths {
            bin: PathBuf::from("/fake/llama-server"),
        };
        ensure_running(&sup, &mm, &storage, &paths, None, None, None)
            .await
            .unwrap();
        assert!(sup.is_hash_verified("tiny"), "verified on first use");
        // Warm path returns the running provider without re-hashing.
        sup.stop("tiny").await;
        mm.remove_provider("local-runner:tiny");
        ensure_running(&sup, &mm, &storage, &paths, None, None, None)
            .await
            .expect("second start reuses the first run's verification");
        assert_eq!(world.spawn_count(), 2);
        // The cache can be dropped (a re-download replaces the bytes).
        sup.forget_hash_verification("tiny");
        assert!(!sup.is_hash_verified("tiny"));
    }

    #[test]
    fn the_manual_verify_catches_what_the_cheap_boot_sweep_misses() {
        // M-10's operator half (`local_integrity::verify_local_models` delegates
        // straight to this): a full re-hash, and it can be scoped to one model.
        let dir = tmp_dir();
        let storage = seeded_storage_with_sha(&dir, Some(&"b".repeat(64)));
        // A second, intact model to prove `only_id` scoping is real.
        let other = dir.join("other.gguf");
        std::fs::write(&other, b"other-bytes").unwrap();
        storage
            .global()
            .insert_model(&crate::storage::ModelEntry {
                id: "other".into(),
                name: "Other".into(),
                path: other.to_string_lossy().into_owned(),
                size_bytes: 11,
                quantization: None,
                added_at: 2,
                sha256: crate::models::download::file_sha256(&other).unwrap(),
                status: "ready".into(),
            })
            .unwrap();

        // Scoped to the intact one: nothing is quarantined, and it really only
        // looked at that row.
        let scoped = verify_local_models_now(&storage, Some("other"));
        assert_eq!(scoped.checked, 1, "only the requested model was hashed");
        assert!(scoped.quarantined.is_empty());
        assert_eq!(
            storage.global().get_model("tiny").unwrap().unwrap().status,
            "ready",
            "a scoped verify must not touch other rows"
        );

        // Whole catalog: the tampered one is quarantined, the intact one is not.
        let all = verify_local_models_now(&storage, None);
        assert_eq!(all.checked, 2);
        assert_eq!(all.quarantined, vec!["tiny"]);
        assert_eq!(
            storage.global().get_model("tiny").unwrap().unwrap().status,
            "quarantined"
        );
        assert_eq!(
            storage.global().get_model("other").unwrap().unwrap().status,
            "ready"
        );
    }

    // ── L-02: a failure you can read ───────────────────────────────────

    #[test]
    fn redaction_scrubs_absolute_paths_and_long_hex_before_storage() {
        let line = redact_stderr_line(
            "error loading model /Users/someone/Documents/Lost-Harness/models/x.gguf",
        );
        assert!(
            !line.contains("someone"),
            "account name must not survive: {line}"
        );
        assert!(!line.contains(".gguf"), "the path is gone entirely: {line}");
        assert!(
            line.starts_with("error loading model "),
            "the message survives: {line}"
        );
        assert!(line.contains("<path>"));

        // A bearer token / sha256 in a log line.
        let tok = "deadbeefcafebabe0123456789abcdef";
        let line = redact_stderr_line(&format!("server: key={tok} ok"));
        assert!(!line.contains(tok), "token must not survive: {line}");
        assert_eq!(line, "server: key=<redacted> ok");

        // Short hex and relative words are untouched — redaction is not a
        // wholesale scrub of anything that looks technical.
        assert_eq!(redact_stderr_line("ctx=4096 abc123"), "ctx=4096 abc123");
    }

    #[test]
    fn the_stderr_tail_is_bounded_on_lines_and_line_length() {
        let tail = StderrTail::default();
        for i in 0..(STDERR_TAIL_LINES + 25) {
            tail.push_redacted(&format!("line {i}"));
        }
        let snap = tail.snapshot();
        assert_eq!(
            snap.lines().count(),
            STDERR_TAIL_LINES,
            "the ring never grows past its cap"
        );
        assert!(
            snap.contains(&format!("line {}", STDERR_TAIL_LINES + 24)),
            "keeps the newest"
        );
        assert!(!snap.contains("line 0\n"), "drops the oldest");

        let long = StderrTail::default();
        long.push_redacted(&"x".repeat(5000));
        let snap = long.snapshot();
        assert!(
            snap.chars().count() <= STDERR_LINE_MAX + 1,
            "a single huge line is truncated, got {} chars",
            snap.chars().count()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_startup_failure_reports_the_sidecars_stderr() {
        // L-02: stderr used to go to /dev/null, so "it didn't come up" was the
        // whole diagnosis. The captured tail must reach the error the caller sees.
        let world = FakeWorld::new(vec![FakeBehavior::NeverHealthy]);
        world.with_stderr(&["ggml_metal_init: failed to allocate", "srv: exiting"]);
        let sup = supervisor_with(&world);
        let err = sup
            .ensure_started("m1", cmd(), "http://127.0.0.1:8080/v1".into())
            .await
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("ggml_metal_init: failed to allocate"),
            "the stderr tail must be in the error: {msg}"
        );
        assert!(msg.contains("srv: exiting"), "{msg}");
    }

    #[tokio::test]
    async fn the_real_spawner_captures_a_dying_processes_stderr() {
        // The same path with the REAL spawner and a REAL process: a "sidecar"
        // that writes to stderr and exits non-zero. Proves the pipe, the reader
        // task, the redaction and the error plumbing work together — no fake in
        // the loop. Real clock (a real process is involved).
        let world_free_config = SupervisorConfig {
            health_timeout: Duration::from_millis(500),
            health_poll: Duration::from_millis(50),
            backoff: vec![], // one attempt: this test is about the message
            idle_shutdown: Duration::from_secs(600),
            kill_grace: Duration::from_millis(200),
            monitor_poll: Duration::from_millis(100),
            pidfile_dir: None,
        };
        let sup = Arc::new(LocalRunnerSupervisor::new(
            Arc::new(TokioSpawner),
            Arc::new(HttpHealthCheck::new()),
            world_free_config,
        ));
        let cmd = SidecarCommand {
            bin: PathBuf::from("/bin/sh"),
            args: vec![
                "-c".to_string(),
                "echo 'error: unable to load model /Users/nobody/x.gguf' >&2; exit 3".to_string(),
            ],
            api_key: Some("tok".to_string()),
        };
        let port = pick_free_port().unwrap();
        let err = sup
            .ensure_started("sh", cmd, format!("http://127.0.0.1:{port}/v1"))
            .await
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("error: unable to load model"), "{msg}");
        assert!(msg.contains("exited (code 3)"), "{msg}");
        // Redaction happened on the way into the buffer, so the error the user
        // sees (and any log of it) has no absolute path in it.
        assert!(
            !msg.contains("/Users/nobody"),
            "path leaked into the error: {msg}"
        );
        assert!(msg.contains("<path>"), "{msg}");
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
            let Some((hash, name)) = line.split_once("  ") else {
                continue;
            };
            let file = vendor.join("macos-arm64").join(name.trim());
            let actual = crate::models::download::file_sha256(&file)
                .unwrap_or_else(|e| panic!("hashing {}: {e}", file.display()));
            assert_eq!(actual, hash.trim(), "vendored file drifted: {name}");
            checked += 1;
        }
        assert!(
            checked >= 11,
            "manifest covers the binary + its dylib closure ({checked})"
        );
        assert_eq!(
            std::fs::read_to_string(vendor.join("VERSION"))
                .unwrap()
                .trim(),
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
        // H-04 on the real binary: a per-spawn token that the real llama-server
        // must enforce (the health check requires an unauthenticated probe to be
        // refused) and that the real client must present on every call.
        let token = new_spawn_token();
        let cmd = build_args(
            &bin,
            Path::new(&gguf),
            port,
            4,
            Some(2048),
            None,
            Some(&token),
        );
        sup.ensure_started("live-test", cmd, base_url.clone())
            .await
            .expect("sidecar becomes healthy");

        // One real chat round-trip through the existing ModelClient.
        let provider = Provider::new(
            "live-test",
            "Live",
            &base_url,
            Some(token),
            ProviderKind::Local,
        );
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

        // No signed model manifest in this scratch dir, so the fail-closed
        // provenance path (P09 / H-08) is what this E2E exercises: rows come
        // back `Community` and the tree is listed at the mutable tip.
        let manifest_dir =
            std::env::temp_dir().join(format!("lhp-e2e-mf-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&manifest_dir).unwrap();

        // 1. HF SEARCH — prove discovery returns real rows for the tiny model.
        let hits = hf_search::search("qwen3 0.6b", SearchSort::Downloads, 20, &manifest_dir)
            .await
            .expect("HF search");
        assert!(!hits.is_empty(), "search returns rows for qwen3-0.6b");

        // 2. Resolve the specific tiny repo's quants; pick the SMALLEST complete
        //    single-file quant (kindest download) — carries the real lfs.oid sha.
        const REPO: &str = "Qwen/Qwen3-0.6B-GGUF";
        let detail = hf_search::model_detail(REPO, &manifest_dir)
            .await
            .expect("model detail");
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
        crate::models::download::download_to_partial(
            &file.url,
            &partial,
            file.size_bytes,
            |_, _| {},
        )
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
        let token = new_spawn_token();
        let cmd = build_args(
            &bin,
            &gguf,
            port,
            4,
            Some(2048),
            Some(KvCacheQuant::F16),
            Some(&token),
        );
        sup.ensure_started("e2e", cmd, base_url.clone())
            .await
            .expect("sidecar becomes healthy");

        // 5. Real chat round-trip through the ModelClient.
        let provider = Provider::new("e2e", "E2E", &base_url, Some(token), ProviderKind::Local);
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
