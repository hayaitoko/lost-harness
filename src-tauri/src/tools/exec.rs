//! Guarded subprocess executor (do-now item 7, Q2). The **only** way any
//! tool spawns a child process, wired to `ShellExecTool`
//! (`RiskClass::Dangerous`).
//!
//! Enforcement — timeout, output caps, process-group kill, OS sandbox — lives
//! HERE at the execution layer, behind a [`SandboxedSpawn`] trait, not in the
//! hook chain (which only decides *whether* the call may run). The two
//! "sandbox" concepts are separate layers: `hooks::sandbox::SandboxHook` is a
//! hardline denylist in the gating chain; this module is Seatbelt process
//! containment at spawn time. Neither subsumes the other.
//!
//! **Fail-closed is the whole point of the trait boundary.** A profile that
//! can't be built or applied returns [`ExecError::SandboxApply`] — there is no
//! code path from a spawn failure to a bare `Command::new`, ever. On a
//! platform with no backend, [`UnsupportedSandbox`] hard-errors every call, so
//! "never run unsandboxed" holds at the platform-selection level too.
//!
//! The durable target is VM/container isolation (`Virtualization.framework` /
//! `Containerization`) behind this same trait; `sandbox-exec` is
//! deprecated-but-functional (Chrome/Bazel still rely on it) and this module's
//! job is to make it work correctly *today*, per Fable's decision.

use std::collections::VecDeque;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;
use tokio::io::AsyncReadExt;

use crate::tools::{Capability, ExecCtx, RiskClass, Tool, ToolInput, ToolResult};

/// Head bytes kept verbatim from each captured stream.
const OUTPUT_HEAD_CAP_BYTES: usize = 64 * 1024;
/// Tail bytes kept verbatim from each captured stream.
const OUTPUT_TAIL_CAP_BYTES: usize = 16 * 1024;

// ── core types ──────────────────────────────────────────────────────────────

/// One guarded execution request. `command` is the DECODED shell command
/// line, never a JSON envelope.
#[derive(Debug, Clone)]
pub struct ExecSpec {
    pub command: String,
    pub workspace_root: PathBuf,
    pub tmp_root: PathBuf,
    pub network: bool,
    pub timeout: Duration,
}

/// The result of a guarded execution. Present even for a nonzero exit or a
/// timeout — those are the *command's* result (data for the model), not an
/// executor failure.
#[derive(Debug, Clone)]
pub struct ExecOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub duration_ms: u128,
}

/// Both variants MUST be treated as a hard tool error — never a signal to
/// fall back to running unsandboxed.
#[derive(Debug)]
pub enum ExecError {
    /// The sandbox profile could not be built or applied. This includes the
    /// silent Seatbelt-crash trap (SIGABRT) and a profile-syntax error
    /// (exit 65 + `sandbox-exec:` stderr prefix).
    SandboxApply(String),
    /// An I/O error spawning or reaping the process.
    Io(String),
}

/// Build the platform sandbox wrapping for `spec` and spawn it already
/// contained. Implementations MUST return `Err(ExecError::SandboxApply)` —
/// never a bare `Command::new(...)` — if the profile can't be built or
/// applied.
///
/// Returns the spawned child plus any temp files (e.g. the Seatbelt profile)
/// that `run_guarded` must delete once the child has been reaped — deleting
/// after `wait()` is race-free (`sandbox-exec` has finished reading the
/// profile by the time its process exits).
pub trait SandboxedSpawn: Send + Sync {
    fn spawn(&self, spec: &ExecSpec) -> Result<(tokio::process::Child, Vec<PathBuf>), ExecError>;
}

// ── platform helpers (process-group kill + sandbox-failure detection) ────────

#[cfg(unix)]
mod platform {
    use std::collections::HashSet;
    use std::process::ExitStatus;

    /// One row of a process-table snapshot — exactly the links a tree walk
    /// needs. Kept as plain data so the expansion below is a pure function
    /// (unit-testable without spawning anything).
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct ProcRow {
        pub pid: i32,
        pub ppid: i32,
        pub pgid: i32,
    }

    /// Safety stop on the snapshot→signal→re-scan loop. Not a budget: a round
    /// only runs if the previous one discovered a NEW pid, so a quiescent tree
    /// costs two rounds.
    const MAX_KILL_ROUNDS: usize = 8;

    /// Kill the leader's whole process tree: the process group, plus every
    /// descendant that left the group via `setsid(2)`.
    ///
    /// **Order is the fix (M-14).** The descendant set is SNAPSHOT BEFORE
    /// anything is signalled. `kill(-pgid)` destroys the very links a tree walk
    /// needs: each intermediate in the group dies, and the kernel reparents its
    /// children to `launchd` as it exits, so a grandchild that had called
    /// `setsid(2)` (new session, out of the group) becomes unreachable the
    /// instant its parent is reaped. Kill-then-enumerate therefore races
    /// against signal delivery — and loses whenever the enumeration is slower
    /// than the kernel (e.g. a wide tree, which makes the walk expensive).
    /// Enumerate-then-kill cannot lose that race.
    ///
    /// After signalling we re-scan and re-signal until a round finds nothing
    /// new, so a child forked in the window between the snapshot and signal
    /// delivery is caught too.
    ///
    /// Known limit: this is a ppid/pgid walk — the only process-tree primitive
    /// macOS offers (no PID namespace, no cgroup). A process that orphans
    /// itself *before* the snapshot leaves no link to follow.
    pub fn kill_group(pgid: i32) {
        // SAFETY: getpid/getpgrp are always-succeeding syscalls.
        let (self_pid, self_pgid) = unsafe { (libc::getpid(), libc::getpgrp()) };
        kill_tree_with(pgid, self_pid, self_pgid, snapshot_processes, |target| {
            // SAFETY: `kill(2)` with a negative target addresses the process
            // group; a missing target just returns ESRCH, which we ignore.
            unsafe {
                libc::kill(target, libc::SIGKILL);
            }
        });
    }

    /// The discovery/signal loop of [`kill_group`], with the two syscalls it
    /// depends on injected: `snapshot` reads the process table, and `signal`
    /// SIGKILLs one target (negative = process group). Split out so a test can
    /// drive it with a scripted sequence of process tables and observe the exact
    /// ORDER of snapshots and signals — the property this whole function is
    /// about — without racing real processes.
    pub fn kill_tree_with<S, K>(
        pgid: i32,
        protect_pid: i32,
        protect_pgid: i32,
        mut snapshot: S,
        mut signal: K,
    ) where
        S: FnMut() -> Vec<ProcRow>,
        K: FnMut(i32),
    {
        // Every pid we have ever believed to be in the tree. Carried across
        // rounds as BFS *seeds*, so a child of an intermediate that has since
        // died is still reachable. Seeds are never signalled just for being
        // seeds — only what the current table still shows is (a pid that died
        // could have been recycled onto an unrelated process).
        let mut seeds: HashSet<i32> = HashSet::new();
        seeds.insert(pgid);

        for round in 0..MAX_KILL_ROUNDS {
            let table = snapshot();
            let found = expand_tree(&table, &seeds, protect_pid, protect_pgid);
            let discovered = found.difference(&seeds).count();
            seeds.extend(found.iter().copied());
            if round > 0 && discovered == 0 {
                // Nothing new since the previous round's signal: the set is
                // stable, and SIGKILL can be neither blocked nor handled, so
                // re-signalling would be pure noise.
                break;
            }
            // One group kill sweeps up everything that stayed in the group…
            signal(-pgid);
            // …and every individually-discovered pid covers what left it.
            // SIGKILL is idempotent, so re-signalling across rounds is safe.
            let mut targets: Vec<i32> = found
                .iter()
                .copied()
                // Never signal pid 0 (that would mean our own group), pid 1, or
                // ourselves.
                .filter(|&pid| pid > 1 && pid != protect_pid)
                .collect();
            // Deterministic order: the set iteration order is not, and a
            // deterministic kill order makes this function testable.
            targets.sort_unstable();
            for pid in targets {
                signal(pid);
            }
        }
    }

    /// Expand `seeds` into every pid in `table` that belongs to the seeded
    /// tree, transitively, by TWO links:
    ///
    /// * `ppid` — the ordinary parent chain.
    /// * `pgid` whose **leader is itself an in-tree pid** — this is what
    ///   catches the children of a `setsid(2)` escapee: the escapee's new
    ///   process group is led by the escapee, so anything it forks is still
    ///   attributable even if the escapee itself is already gone. Requiring
    ///   the group *leader* to be in-tree is what keeps this from ever
    ///   capturing an unrelated group: a hostile descendant that `setpgid`s
    ///   into some other group in our session names a group we do not lead,
    ///   so it is not followed.
    ///
    /// `protect_pid` / `protect_pgid` (our own pid and process group) are never
    /// returned — the walk must not be able to turn into a self-kill.
    ///
    /// Pure function over a snapshot: no syscalls, so the caller pays for
    /// exactly one process-table read per round.
    pub fn expand_tree(
        table: &[ProcRow],
        seeds: &HashSet<i32>,
        protect_pid: i32,
        protect_pgid: i32,
    ) -> HashSet<i32> {
        let mut known: HashSet<i32> = seeds.clone();
        // pgids whose group LEADER is a known in-tree pid.
        let mut groups: HashSet<i32> = seeds.clone();
        loop {
            let mut grew = false;
            for row in table {
                if row.pid <= 1 || row.pid == protect_pid || known.contains(&row.pid) {
                    continue;
                }
                let by_parent = known.contains(&row.ppid);
                let by_group = row.pgid != protect_pgid && groups.contains(&row.pgid);
                if by_parent || by_group {
                    known.insert(row.pid);
                    // If this pid leads a group, that group is in-tree too.
                    groups.insert(row.pid);
                    grew = true;
                }
            }
            if !grew {
                break;
            }
        }
        known.remove(&protect_pid);
        known
    }

    /// Read the whole process table once: `(pid, ppid, pgid)` for every process
    /// we can query.
    #[cfg(target_os = "macos")]
    fn snapshot_processes() -> Vec<ProcRow> {
        // libproc(3): proc_listallpids gives the pid list (called with a null
        // buffer it returns just the count, so we never truncate the way a
        // hardcoded ceiling would); proc_pidinfo/PROC_PIDTBSDINFO fills
        // proc_bsdinfo, which carries pbi_ppid and pbi_pgid.
        use libc::c_int;
        use std::mem;

        const PROC_PIDTBSDINFO: c_int = 3;
        const INFO_SIZE: usize = mem::size_of::<libc::proc_bsdinfo>();

        extern "C" {
            fn proc_listallpids(buffer: *mut c_int, buffersize: c_int) -> c_int;
            fn proc_pidinfo(
                pid: c_int,
                flavor: c_int,
                arg: u64,
                buffer: *mut libc::c_void,
                buffersize: c_int,
            ) -> c_int;
        }

        unsafe {
            // Ask for the count first, then over-allocate: the table can grow
            // between the two calls (that is exactly what a fork bomb does).
            let probed = proc_listallpids(std::ptr::null_mut(), 0);
            let cap = if probed > 0 {
                (probed as usize).saturating_mul(2).saturating_add(256)
            } else {
                8192
            };
            let mut pids: Vec<c_int> = vec![0; cap];
            let count =
                proc_listallpids(pids.as_mut_ptr(), (cap * mem::size_of::<c_int>()) as c_int);
            if count <= 0 {
                return Vec::new();
            }
            let count = (count as usize).min(cap);

            let mut rows = Vec::with_capacity(count);
            for &pid in &pids[..count] {
                if pid <= 0 {
                    continue;
                }
                let mut info: libc::proc_bsdinfo = mem::zeroed();
                let ret = proc_pidinfo(
                    pid,
                    PROC_PIDTBSDINFO,
                    0,
                    &mut info as *mut _ as *mut libc::c_void,
                    INFO_SIZE as c_int,
                );
                // proc_pidinfo returns the number of bytes written.
                if ret as usize == INFO_SIZE {
                    rows.push(ProcRow {
                        pid: info.pbi_pid as i32,
                        ppid: info.pbi_ppid as i32,
                        pgid: info.pbi_pgid as i32,
                    });
                }
            }
            rows
        }
    }

    /// Stub for non-macOS unices. With an empty table `expand_tree` yields only
    /// the leader, so `kill_group` degrades to the plain group kill it always
    /// was there. A Linux backend should read `/proc/*/stat` (or iterate the
    /// PID cgroup) here.
    #[cfg(not(target_os = "macos"))]
    fn snapshot_processes() -> Vec<ProcRow> {
        Vec::new()
    }

    /// Distinguish a sandbox-APPLY failure from the command's own result. Two
    /// forms, both verified empirically on macOS 15 (see the module spec): the
    /// `import system.sb`-omission crash → SIGABRT (signal 6); and a profile
    /// syntax / unknown-operation error → exit code 65 with a `sandbox-exec: `
    /// stderr prefix. Only consulted when WE didn't kill the child for timeout.
    ///
    /// The exit-65 form ALSO requires empty stdout: a genuine apply failure
    /// never let the target process execute, so it can't have produced stdout.
    /// That guard stops a command that legitimately exits 65 while also
    /// happening to print `sandbox-exec: ` to its own stderr from being
    /// misclassified as an apply failure (which would silently discard its
    /// real output).
    pub fn is_sandbox_apply_failure(status: &ExitStatus, stdout: &str, stderr: &str) -> bool {
        use std::os::unix::process::ExitStatusExt;
        status.signal() == Some(libc::SIGABRT)
            || (status.code() == Some(65)
                && stderr.starts_with("sandbox-exec: ")
                && stdout.is_empty())
    }
}

#[cfg(not(unix))]
mod platform {
    use std::process::ExitStatus;
    // No unix process groups / Seatbelt off-unix; the only spawner on these
    // platforms is `UnsupportedSandbox`, which errors before a child exists,
    // so these are unreachable in practice — they exist only to compile.
    pub fn kill_group(_pgid: i32) {}
    pub fn is_sandbox_apply_failure(_status: &ExitStatus, _stdout: &str, _stderr: &str) -> bool {
        false
    }
}

// ── bounded head+tail output collector ───────────────────────────────────────

/// Keeps the first `HEAD` bytes and a rolling window of the last `TAIL` bytes
/// of a stream, so a runaway command can't blow up memory. Renders with an
/// elision marker only when the total exceeded `HEAD + TAIL`.
#[derive(Default)]
struct HeadTail {
    head: Vec<u8>,
    tail: VecDeque<u8>,
    total: usize,
}

impl HeadTail {
    fn push_chunk(&mut self, chunk: &[u8]) {
        for &b in chunk {
            self.total += 1;
            if self.head.len() < OUTPUT_HEAD_CAP_BYTES {
                self.head.push(b);
            } else {
                self.tail.push_back(b);
                if self.tail.len() > OUTPUT_TAIL_CAP_BYTES {
                    self.tail.pop_front();
                }
            }
        }
    }

    fn render(self) -> String {
        if self.total <= OUTPUT_HEAD_CAP_BYTES + OUTPUT_TAIL_CAP_BYTES {
            // Everything fit: head holds the first bytes, tail the rest, in
            // order — concatenating reproduces the stream exactly.
            let mut all = self.head;
            all.extend(self.tail);
            String::from_utf8_lossy(&all).into_owned()
        } else {
            let elided = self.total - self.head.len() - self.tail.len();
            let head = String::from_utf8_lossy(&self.head).into_owned();
            let tail_bytes: Vec<u8> = self.tail.into_iter().collect();
            let tail = String::from_utf8_lossy(&tail_bytes);
            format!("{head}\n...[{elided} bytes elided]...\n{tail}")
        }
    }
}

/// Grace, after the child has exited or been killed, for its pipes to reach
/// EOF before we abandon them and return what we already have. A pipe is only
/// still open at that point because some process INHERITED the write end and
/// outlived the child, which is precisely the case that used to hang the
/// caller for as long as that process felt like living.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// Drain a piped child stream into `sink` until EOF, error, or `budget`
/// expires. Bounded on purpose: an unbounded read lets any process holding the
/// write end keep this task — and, through it, `run_guarded` — alive forever.
/// Bytes are pushed into the shared sink as they arrive, so whatever arrived
/// before the bound is still reported.
async fn drain_reader<R>(reader: Option<R>, sink: Arc<Mutex<HeadTail>>, budget: Duration)
where
    R: tokio::io::AsyncRead + Unpin,
{
    let Some(mut r) = reader else { return };
    let deadline = tokio::time::Instant::now() + budget;
    let mut buf = [0u8; 8192];
    loop {
        match tokio::time::timeout_at(deadline, r.read(&mut buf)).await {
            // Budget exhausted: someone is holding the pipe open. Stop.
            Err(_elapsed) => break,
            Ok(Ok(0)) | Ok(Err(_)) => break,
            Ok(Ok(n)) => {
                // The guard is never held across an await, so an abort can
                // neither poison the mutex nor strand it locked.
                lock_sink(&sink).push_chunk(&buf[..n]);
            }
        }
    }
}

/// Lock the sink, tolerating poisoning: a partially-pushed chunk is still
/// usable output, and a panicking drain must not turn into a panicking caller.
fn lock_sink(sink: &Arc<Mutex<HeadTail>>) -> std::sync::MutexGuard<'_, HeadTail> {
    sink.lock().unwrap_or_else(|e| e.into_inner())
}

/// A running drain task plus the sink it fills. Runs as its own task so
/// stdout+stderr are consumed concurrently with the process running (never
/// letting a full pipe buffer deadlock the child), and holds the sink
/// separately so the caller can take the collected bytes even when the task is
/// still blocked on a pipe an escaped descendant is holding open.
struct Drain {
    sink: Arc<Mutex<HeadTail>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drain {
    fn spawn<R>(reader: Option<R>, budget: Duration) -> Self
    where
        R: tokio::io::AsyncRead + Unpin + Send + 'static,
    {
        let sink = Arc::new(Mutex::new(HeadTail::default()));
        let into = Arc::clone(&sink);
        let task = tokio::spawn(drain_reader(reader, into, budget));
        Self { sink, task }
    }

    /// Wait at most `grace` for the drain to reach EOF, then abort it and
    /// render whatever it collected. Never waits unboundedly, whoever holds
    /// the write end.
    async fn finish(self, grace: Duration) -> String {
        let abort = self.task.abort_handle();
        if tokio::time::timeout(grace, self.task).await.is_err() {
            abort.abort();
        }
        let collected = std::mem::take(&mut *lock_sink(&self.sink));
        collected.render()
    }
}

// ── run_guarded ───────────────────────────────────────────────────────────────

/// Spawn `spec` via `spawner` (already sandbox-contained), drain its output
/// bounded, and race its exit against `spec.timeout`, killing the whole
/// process group on timeout. A sandbox-apply failure is a hard `Err`.
pub async fn run_guarded(
    spawner: &dyn SandboxedSpawn,
    spec: &ExecSpec,
) -> Result<ExecOutput, ExecError> {
    let start = std::time::Instant::now();
    // Propagates a sandbox-apply failure immediately, hard, before anything
    // runs — there is no fall-through to an unsandboxed spawn.
    let (mut child, cleanup_paths) = spawner.spawn(spec)?;
    let pgid = child
        .id()
        .map(|p| p as i32)
        .ok_or_else(|| ExecError::Io("spawned child has no pid".to_string()))?;

    // Drain both pipes concurrently while the process runs. The per-task
    // budget is a backstop in case this future is dropped before `finish`:
    // nothing may read a pipe for longer than the command's own budget plus
    // the grace period.
    let drain_budget = spec.timeout.saturating_add(DRAIN_GRACE);
    let out_drain = Drain::spawn(child.stdout.take(), drain_budget);
    let err_drain = Drain::spawn(child.stderr.take(), drain_budget);

    let mut timed_out = false;
    let mut wait_err: Option<String> = None;

    let status: Option<std::process::ExitStatus> = tokio::select! {
        res = child.wait() => match res {
            Ok(s) => Some(s),
            Err(e) => {
                // L-05 fix: collect the error + kill the group before the
                // drain/cleanup below, so EVERY exit path joins drain tasks
                // and deletes temp profiles.
                platform::kill_group(pgid);
                wait_err = Some(format!("waiting on child: {e}"));
                None
            }
        },
        _ = tokio::time::sleep(spec.timeout) => {
            platform::kill_group(pgid);
            timed_out = true;
            None
        }
    };

    if timed_out {
        // Reap the killed group leader so it isn't left a zombie.
        let _ = child.wait().await;
    }

    // ── cleanup (L-05 fix: runs on EVERY exit path, including wait_err) ──
    // stdout/stderr pipes close when the process exits (even on a wait(2)
    // error like ECHILD) UNLESS something inherited the write end and outlived
    // it — hence the bounded `finish`, which gives up after DRAIN_GRACE and
    // keeps the bytes already collected instead of hanging here. sandbox-exec
    // read the profile at exec time, so deleting it after wait/close is
    // race-free.
    let (stdout, stderr) =
        tokio::join!(out_drain.finish(DRAIN_GRACE), err_drain.finish(DRAIN_GRACE));
    let duration_ms = start.elapsed().as_millis();

    for p in &cleanup_paths {
        let _ = std::fs::remove_file(p);
    }

    // Now handle any error that was collected during the select.
    if let Some(msg) = wait_err {
        return Err(ExecError::Io(msg));
    }

    match status {
        Some(status) => {
            // Only meaningful when we did NOT kill it for timeout: catch a
            // silent Seatbelt failure so it can't be misreported as "the
            // command exited weird."
            if platform::is_sandbox_apply_failure(&status, &stdout, &stderr) {
                return Err(ExecError::SandboxApply(stderr));
            }
            Ok(ExecOutput {
                stdout,
                stderr,
                exit_code: status.code(),
                timed_out: false,
                duration_ms,
            })
        }
        None => Ok(ExecOutput {
            stdout,
            stderr,
            exit_code: None,
            timed_out: true,
            duration_ms,
        }),
    }
}

// ── resource ceilings (rlimits) ──────────────────────────────────────────────

/// Per-child kernel resource ceilings, installed with `setrlimit(2)` between
/// `fork` and `exec` so they are inherited by `sandbox-exec` and everything it
/// runs. The wall-clock timeout and the process-tree kill bound *time*; these
/// bound what a single process can consume while it has it.
///
/// Scope of the guarantee: rlimits are PER PROCESS, not per tree, so N children
/// get N budgets — this narrows a runaway, it does not cap the tree. What it
/// does buy is a hard stop for a descendant that outlives the tree kill (see
/// `platform::kill_group`'s known limit): it cannot burn CPU forever or fill
/// the disk with one file after we have stopped watching.
///
/// macOS-only, alongside the only real spawner. A Linux backend must install
/// the equivalent when it lands (its `setrlimit` resource argument is a
/// different type, so the code is not shared blindly).
#[cfg(target_os = "macos")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourceCeilings {
    /// `RLIMIT_CPU` — CPU-seconds; exceeding it is SIGKILL.
    pub cpu_seconds: u64,
    /// `RLIMIT_FSIZE` — bytes any single file may reach; exceeding it is
    /// SIGXFSZ (which by default terminates the writer).
    pub file_size_bytes: u64,
    /// `RLIMIT_NOFILE` — simultaneously open descriptors.
    pub open_files: u64,
}

/// 4 GiB. A ceiling, not a quota: no legitimate in-workspace artifact reaches
/// it, while a `yes > file` runaway stops there instead of at "disk full".
#[cfg(target_os = "macos")]
const FILE_SIZE_CEILING_BYTES: u64 = 4 * 1024 * 1024 * 1024;
/// The conventional POSIX default. This machine's login shell may be far more
/// generous; a sandboxed one-shot command has no business needing more.
#[cfg(target_os = "macos")]
const OPEN_FILES_CEILING: u64 = 1024;

#[cfg(target_os = "macos")]
impl ResourceCeilings {
    /// Derive the ceilings for a command whose wall-clock budget is `timeout`.
    ///
    /// CPU is deliberately generous: `timeout` seconds on every core, plus
    /// slack. Anything less would kill legitimate parallel work that the wall
    /// budget still permits (a `-j8` build spends up to 8 CPU-seconds per wall
    /// second) — a ceiling that a well-behaved command can hit is a bug, not
    /// hardening. What it forbids is spending CPU *after* the wall budget is
    /// gone, which is the escapee case.
    pub fn for_timeout(timeout: Duration) -> Self {
        let cores = std::thread::available_parallelism()
            .map(|n| n.get() as u64)
            .unwrap_or(1);
        Self {
            cpu_seconds: timeout.as_secs().saturating_mul(cores).saturating_add(30),
            file_size_bytes: FILE_SIZE_CEILING_BYTES,
            open_files: OPEN_FILES_CEILING,
        }
    }
}

/// Install `ceilings` on `cmd`'s child via a post-fork/pre-exec hook.
///
/// Fails CLOSED: a `setrlimit` that does not take makes `spawn()` fail with
/// `ExecError::Io`, so a command never runs with the ceilings silently absent.
#[cfg(target_os = "macos")]
fn apply_ceilings(cmd: &mut tokio::process::Command, ceilings: ResourceCeilings) {
    // SAFETY: the closure runs in the forked child before exec, so it must be
    // async-signal-safe: get/setrlimit are bare syscalls, and it neither
    // allocates nor takes locks.
    unsafe {
        cmd.pre_exec(move || {
            tighten_rlimit(libc::RLIMIT_CPU, ceilings.cpu_seconds)?;
            tighten_rlimit(libc::RLIMIT_FSIZE, ceilings.file_size_bytes)?;
            tighten_rlimit(libc::RLIMIT_NOFILE, ceilings.open_files)?;
            Ok(())
        });
    }
}

/// Clamp one rlimit to `desired`, lowering the SOFT and the HARD limit
/// together — soft alone would be theatre, since the process could raise it
/// straight back up to hard. Only ever tightens: an existing limit stricter
/// than `desired` is left alone, and neither half is ever raised.
///
/// Address space is NOT among the limits we set: macOS rejects `RLIMIT_AS`
/// (== `RLIMIT_RSS`) with EINVAL for any value small enough to be a meaningful
/// ceiling — see the deferral recorded in `review-fixes/progress/P11.md`.
#[cfg(target_os = "macos")]
fn tighten_rlimit(resource: libc::c_int, desired: u64) -> std::io::Result<()> {
    let desired = desired as libc::rlim_t;
    // SAFETY: both calls take a pointer to a live, correctly-sized rlimit.
    unsafe {
        let mut cur: libc::rlimit = std::mem::zeroed();
        if libc::getrlimit(resource, &mut cur) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let clamp = |v: libc::rlim_t| -> libc::rlim_t {
            if v == libc::RLIM_INFINITY {
                desired
            } else {
                std::cmp::min(v, desired)
            }
        };
        let next = libc::rlimit {
            rlim_cur: clamp(cur.rlim_cur),
            rlim_max: clamp(cur.rlim_max),
        };
        if next.rlim_cur == cur.rlim_cur && next.rlim_max == cur.rlim_max {
            return Ok(()); // already at least this tight
        }
        if libc::setrlimit(resource, &next) != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

// ── MacSeatbeltSpawn ─────────────────────────────────────────────────────────

/// macOS backend: wraps the command in `sandbox-exec -f <profile> /bin/sh -c`.
/// The profile is the empirically-verified template below — do NOT drop the
/// `(import "system.sb")` line: without it, `sandbox-exec` itself SIGABRTs
/// (silent crash that looks like "nothing happened," not a denial).
#[cfg(target_os = "macos")]
pub struct MacSeatbeltSpawn;

#[cfg(target_os = "macos")]
impl SandboxedSpawn for MacSeatbeltSpawn {
    fn spawn(&self, spec: &ExecSpec) -> Result<(tokio::process::Child, Vec<PathBuf>), ExecError> {
        // Both roots must be canonical absolute paths for the Seatbelt
        // `subpath` rules to match the process's real cwd/writes.
        let ws = spec
            .workspace_root
            .canonicalize()
            .map_err(|e| ExecError::SandboxApply(format!("workspace root unavailable: {e}")))?;
        let tmp = spec
            .tmp_root
            .canonicalize()
            .map_err(|e| ExecError::SandboxApply(format!("tmp root unavailable: {e}")))?;

        let profile = build_seatbelt_profile(&ws, &tmp, spec.network)?;
        // The profile file lives OUTSIDE the sandboxed dirs — sandbox-exec
        // reads it before the restriction takes effect, so its location is
        // unconstrained. It's small and left in the OS temp dir (OS-cleaned);
        // deleting it here would race sandbox-exec's own read of it.
        let profile_path =
            std::env::temp_dir().join(format!("lhp-sandbox-{}.sb", uuid::Uuid::new_v4()));
        std::fs::write(&profile_path, profile.as_bytes())
            .map_err(|e| ExecError::SandboxApply(format!("writing sandbox profile: {e}")))?;

        let mut cmd = tokio::process::Command::new("/usr/bin/sandbox-exec");
        cmd.arg("-f")
            .arg(&profile_path)
            .arg("/bin/sh")
            .arg("-c")
            .arg(&spec.command)
            // cwd set on the Rust Command (chdir at spawn, before the sandbox
            // restricts) — a runtime `cd`/`getcwd` inside the sandbox would
            // need traversal perms this profile doesn't grant.
            .current_dir(&ws)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        cmd.process_group(0);
        // Kernel resource ceilings, inherited through sandbox-exec into the
        // command itself. A failure to install them fails the spawn.
        apply_ceilings(&mut cmd, ResourceCeilings::for_timeout(spec.timeout));
        let child = cmd
            .spawn()
            .map_err(|e| ExecError::Io(format!("spawning sandbox-exec: {e}")))?;
        // run_guarded deletes the profile once the child is reaped.
        Ok((child, vec![profile_path]))
    }
}

/// Escape a path for embedding as a `<string>` in a Seatbelt S-expression
/// profile.  Backslash and double-quote are the only characters meaningful
/// inside a double-quoted Scheme string; we escape them and fail-closed on
/// any control character (NUL, newlines, etc.) because a canonicalised real
/// path cannot contain one, and a malicious input must not reach the parser.
#[cfg(target_os = "macos")]
fn seatbelt_escape_path(path: &Path) -> Result<String, ExecError> {
    let s = path.to_str().ok_or_else(|| {
        ExecError::SandboxApply(
            "sandbox path is not valid UTF-8 — refusing to embed in profile".to_string(),
        )
    })?;
    if s.contains('\0') {
        return Err(ExecError::SandboxApply(
            "sandbox path contains null byte — refusing to embed in profile".to_string(),
        ));
    }
    let mut out = String::with_capacity(s.len() + 4);
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            // Control characters (including \n, \r, \t) could truncate or
            // alter the S-expression.  A canonicalised path from the OS
            // should never contain them; reject rather than risk weakening
            // the policy (fail-closed).
            c if c.is_control() => {
                return Err(ExecError::SandboxApply(format!(
                    "sandbox path contains control character U+{:04X} — refusing to embed",
                    c as u32
                )));
            }
            c => out.push(c),
        }
    }
    Ok(out)
}

/// Build the Seatbelt profile. Verified on macOS 15 — see the module spec for
/// the `import system.sb` gotcha and the tested deny/allow set.
///
/// Paths are escaped via [`seatbelt_escape_path`] — a quote, backslash, or
/// control character in a workspace/tmp path cannot break or weaken the policy.
#[cfg(target_os = "macos")]
fn build_seatbelt_profile(
    workspace: &Path,
    tmp: &Path,
    network: bool,
) -> Result<String, ExecError> {
    let ws = seatbelt_escape_path(workspace)?;
    let tmp = seatbelt_escape_path(tmp)?;
    let mut p = format!(
        "(version 1)\n\
         (deny default)\n\
         (import \"system.sb\")\n\
         (allow process-exec)\n\
         (allow process-fork)\n\
         (allow signal (target self))\n\
         (allow file-read*\n\
         \x20   (subpath \"/usr\")\n\
         \x20   (subpath \"/bin\")\n\
         \x20   (subpath \"/sbin\")\n\
         \x20   (subpath \"/System\")\n\
         \x20   (subpath \"/private/etc\")\n\
         \x20   (subpath \"/private/var/select\")\n\
         \x20   (subpath \"/dev\")\n\
         \x20   (subpath \"/Library/Preferences\"))\n\
         (allow file-write-data (literal \"/dev/null\") (literal \"/dev/tty\"))\n\
         (allow file-read* file-write*\n\
         \x20   (subpath \"{ws}\")\n\
         \x20   (subpath \"{tmp}\"))\n"
    );
    if network {
        p.push_str("(allow network*)\n");
    }
    Ok(p)
}

// ── UnsupportedSandbox (non-macOS placeholder) ───────────────────────────────

/// The `SandboxedSpawn` for platforms with no backend yet. Every call
/// hard-errors, so `shell_exec` stays *registered* (the model sees it exists)
/// but can never run unsandboxed until a Linux/Windows backend lands.
#[cfg(not(target_os = "macos"))]
pub struct UnsupportedSandbox;

#[cfg(not(target_os = "macos"))]
impl SandboxedSpawn for UnsupportedSandbox {
    fn spawn(&self, _spec: &ExecSpec) -> Result<(tokio::process::Child, Vec<PathBuf>), ExecError> {
        Err(ExecError::SandboxApply(
            "no sandbox backend for this platform yet".to_string(),
        ))
    }
}

// ── ShellExecTool ─────────────────────────────────────────────────────────────

/// The one shell tool. `Dangerous`: every invocation re-prompts (no standing
/// grant can cover it — enforced in `dispatch.rs`'s Approve arm).
pub struct ShellExecTool {
    workspace_root: PathBuf,
    tmp_root: PathBuf,
    spawner: Arc<dyn SandboxedSpawn>,
    timeout_cap: Duration,
    /// M7 Tier-K Slice 2: the storage handle used to load the CALLER's per-profile
    /// `sandbox_config` at run time (a network CEILING). `None` (tests, or an
    /// unwired build) = no per-profile ceiling, today's behavior.
    storage: Option<crate::storage::Storage>,
}

impl ShellExecTool {
    pub fn new(
        workspace_root: impl Into<PathBuf>,
        tmp_root: impl Into<PathBuf>,
        spawner: Arc<dyn SandboxedSpawn>,
        timeout_cap: Duration,
    ) -> Self {
        Self {
            workspace_root: workspace_root.into(),
            tmp_root: tmp_root.into(),
            spawner,
            timeout_cap,
            storage: None,
        }
    }

    /// Wire the storage handle so `run` can enforce the caller's per-profile
    /// `sandbox_config` network ceiling (M7 Tier-K Slice 2).
    pub fn with_storage(mut self, storage: crate::storage::Storage) -> Self {
        self.storage = Some(storage);
        self
    }

    /// The effective network permission for a call: the request is a MAXIMUM, the
    /// profile's stored `sandbox_config` is a CEILING. Fail-safe by construction —
    /// if the profile has a config that forbids network, or its row is corrupt/
    /// unreadable, network is DENIED even when the call asks for it; a missing row
    /// (or no storage wired) leaves today's behavior (the request stands).
    fn effective_network(&self, requested: bool, profile: &str) -> bool {
        if !requested {
            return false;
        }
        let Some(storage) = &self.storage else {
            return true; // unwired → legacy behavior
        };
        match storage
            .open_profile(profile)
            .and_then(|db| db.get_sandbox_config())
        {
            Ok(None) => true,                             // unconfigured → unconstrained
            Ok(Some(cfg)) => cfg.permits_shell_network(), // configured → the ceiling
            Err(e) => {
                tracing::warn!(
                    profile = %profile,
                    error = %e,
                    "sandbox_config unreadable — denying shell network (fail-safe)"
                );
                false
            }
        }
    }
}

impl Tool for ShellExecTool {
    fn name(&self) -> &str {
        "shell_exec"
    }

    fn description(&self) -> &str {
        "Run a shell command in a sandboxed, workspace-confined subprocess (no network by default). \
         args: {\"command\": \"ls -la\", \"network\": false, \"timeout_secs\": 60}"
    }

    fn risk(&self) -> RiskClass {
        RiskClass::Dangerous
    }

    fn requires(&self) -> &[Capability] {
        &[Capability::Shell]
    }

    /// Match on the bare decoded command, NOT the JSON envelope — the
    /// hardening Q2 calls for (quotes/escaping inside JSON create needless
    /// denylist-mismatch surface).
    fn match_text(&self, args: &serde_json::Value) -> String {
        args.get("command")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    }

    fn run<'a>(
        &'a self,
        input: ToolInput,
        ctx: &'a ExecCtx,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        Box::pin(async move {
            let Some(command) = input.args.get("command").and_then(|v| v.as_str()) else {
                return ToolResult::Err(
                    "shell_exec requires a string 'command' argument".to_string(),
                );
            };
            let command = command.to_string();
            let requested_network = input
                .args
                .get("network")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            // The call's request is a MAXIMUM; the caller's per-profile
            // `sandbox_config` is a CEILING (M7 Tier-K Slice 2). A locked-down
            // profile denies shell network even when the call asks for it.
            let network = self.effective_network(requested_network, &ctx.profile);
            // model-supplied timeout can only SHORTEN, never exceed the cap.
            let timeout = match input.args.get("timeout_secs").and_then(|v| v.as_u64()) {
                Some(secs) => std::cmp::min(Duration::from_secs(secs), self.timeout_cap),
                None => self.timeout_cap,
            };

            // M7 Tier-P: re-root the shell's cwd AND its sandbox subpaths to the
            // caller's PER-PROFILE roots — the same call-time resolution the fs
            // tools do. Without this, `shell_exec` (in `app_default`, so every
            // conversation has it) runs with cwd at the SHARED base whose Seatbelt
            // grant (`build_seatbelt_profile` → `(subpath "{ws}") (subpath
            // "{tmp}")`) covers every profile's subtree as a sibling, so `cat
            // ../personal/secret.txt` reads across profiles. BOTH roots must be
            // re-rooted: the workspace AND the tmp scratch — else two profiles
            // still share one granted `tmp/`, and `work` can stage a file at
            // `../../tmp/x` that `personal` reads straight back (a review-caught
            // exfil channel). Per-profile roots make every granted subpath
            // profile-scoped, so a cross-profile path is denied by the sandbox.
            let ws = crate::tools::fs::profile_workspace_path(&self.workspace_root, &ctx.profile);
            let tmp = crate::tools::fs::profile_workspace_path(&self.tmp_root, &ctx.profile);
            let _ = std::fs::create_dir_all(&ws);
            let _ = std::fs::create_dir_all(&tmp);

            let spec = ExecSpec {
                command,
                workspace_root: ws,
                tmp_root: tmp,
                network,
                timeout,
            };

            match run_guarded(self.spawner.as_ref(), &spec).await {
                // The executor mechanism succeeded; the command's own result —
                // even a nonzero exit or a timeout — is DATA for the model.
                Ok(out) => ToolResult::Ok(json!({
                    "stdout": out.stdout,
                    "stderr": out.stderr,
                    "exit_code": out.exit_code,
                    "timed_out": out.timed_out,
                    "duration_ms": out.duration_ms as u64,
                })),
                Err(ExecError::SandboxApply(msg)) => ToolResult::Err(format!(
                    "shell sandbox failed to apply — the command did NOT run: {msg}"
                )),
                Err(ExecError::Io(msg)) => ToolResult::Err(format!("shell_exec i/o error: {msg}")),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `SandboxedSpawn` that always reports a sandbox-apply failure — proves
    /// `run_guarded` fails closed with nothing having run (no code path exists
    /// between `spawn()` erroring and returning).
    struct AlwaysFailSpawn;
    impl SandboxedSpawn for AlwaysFailSpawn {
        fn spawn(
            &self,
            _spec: &ExecSpec,
        ) -> Result<(tokio::process::Child, Vec<PathBuf>), ExecError> {
            Err(ExecError::SandboxApply("test: apply failed".to_string()))
        }
    }

    fn spec(command: &str) -> ExecSpec {
        ExecSpec {
            command: command.to_string(),
            workspace_root: std::env::temp_dir(),
            tmp_root: std::env::temp_dir(),
            network: false,
            timeout: Duration::from_secs(5),
        }
    }

    fn dummy_tool() -> ShellExecTool {
        ShellExecTool::new(
            std::env::temp_dir(),
            std::env::temp_dir(),
            Arc::new(AlwaysFailSpawn),
            Duration::from_secs(120),
        )
    }

    #[test]
    fn risk_is_dangerous() {
        assert_eq!(dummy_tool().risk(), RiskClass::Dangerous);
    }

    #[test]
    fn match_text_returns_bare_decoded_command_not_json_envelope() {
        let t = dummy_tool();
        let text = t.match_text(&serde_json::json!({"command": "rm -rf /", "network": false}));
        assert_eq!(
            text, "rm -rf /",
            "must be the decoded command, not the JSON envelope"
        );
    }

    #[tokio::test]
    async fn hard_errs_when_sandbox_apply_fails() {
        let out = run_guarded(&AlwaysFailSpawn, &spec("echo hi")).await;
        assert!(
            matches!(out, Err(ExecError::SandboxApply(_))),
            "a sandbox-apply failure must be a hard Err, got {out:?}"
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[tokio::test]
    async fn no_sandbox_backend_hard_errs_never_runs_unsandboxed() {
        let out = run_guarded(&UnsupportedSandbox, &spec("echo hi")).await;
        assert!(matches!(out, Err(ExecError::SandboxApply(_))));
    }

    #[tokio::test]
    async fn shell_exec_missing_command_is_an_error_no_spawn() {
        // No 'command' arg → a plain tool error, and (by construction, the
        // arg check runs before run_guarded) the spawner is never touched.
        let t = dummy_tool();
        let ctx = ExecCtx::default();
        let res = t
            .run(ToolInput::new(serde_json::json!({"network": true})), &ctx)
            .await;
        assert!(matches!(res, ToolResult::Err(ref e) if e.contains("requires a string 'command'")));
    }

    /// A `SandboxedSpawn` that records BOTH roots (workspace + tmp) it was handed
    /// and then declines to spawn — lets a test assert WHICH roots shell_exec
    /// jails to without launching a real process.
    struct CaptureRootSpawn {
        seen_ws: Arc<std::sync::Mutex<Option<PathBuf>>>,
        seen_tmp: Arc<std::sync::Mutex<Option<PathBuf>>>,
        seen_network: Arc<std::sync::Mutex<Option<bool>>>,
    }
    impl SandboxedSpawn for CaptureRootSpawn {
        fn spawn(
            &self,
            spec: &ExecSpec,
        ) -> Result<(tokio::process::Child, Vec<PathBuf>), ExecError> {
            *self.seen_ws.lock().unwrap() = Some(spec.workspace_root.clone());
            *self.seen_tmp.lock().unwrap() = Some(spec.tmp_root.clone());
            *self.seen_network.lock().unwrap() = Some(spec.network);
            Err(ExecError::Io("captured; not spawning".to_string()))
        }
    }

    #[tokio::test]
    async fn shell_exec_reroots_cwd_and_jail_to_the_callers_profile() {
        // M7 Tier-P regression (adversarial review, HIGH): shell_exec must jail
        // to the caller's PER-PROFILE roots — BOTH the workspace and the tmp
        // scratch — the same values the fs tools resolve. Otherwise the macOS
        // Seatbelt subpaths cover the shared base (every profile's subtree as
        // siblings) and `cat ../personal/secret.txt` OR a staged `../../tmp/x`
        // reads across profiles.
        let base = std::env::temp_dir().join(format!("lhp-exec-tierp-{}", uuid::Uuid::new_v4()));
        let tmp_base = std::env::temp_dir().join(format!("lhp-exec-tmp-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&base).unwrap();
        std::fs::create_dir_all(&tmp_base).unwrap();
        let seen_ws = Arc::new(std::sync::Mutex::new(None));
        let seen_tmp = Arc::new(std::sync::Mutex::new(None));
        let seen_network = Arc::new(std::sync::Mutex::new(None));
        let tool = ShellExecTool::new(
            base.clone(),
            tmp_base.clone(),
            Arc::new(CaptureRootSpawn {
                seen_ws: seen_ws.clone(),
                seen_tmp: seen_tmp.clone(),
                seen_network: seen_network.clone(),
            }),
            Duration::from_secs(5),
        );

        // A `work`-profile call jails BOTH roots to their per-profile subdirs.
        let ctx = ExecCtx {
            profile: "work".to_string(),
            ..ExecCtx::default()
        };
        let _ = tool
            .run(
                ToolInput::new(serde_json::json!({"command": "echo hi"})),
                &ctx,
            )
            .await;
        assert_eq!(
            seen_ws
                .lock()
                .unwrap()
                .clone()
                .expect("spawner should have been called"),
            base.join("work"),
            "shell must run under the per-profile workspace root, not the shared base"
        );
        assert_eq!(
            seen_tmp.lock().unwrap().clone().unwrap(),
            tmp_base.join("work"),
            "the tmp scratch must ALSO be per-profile — a shared tmp is a cross-profile exfil channel"
        );
        assert!(
            base.join("work").is_dir(),
            "the per-profile workspace root is created before spawn"
        );
        assert!(
            tmp_base.join("work").is_dir(),
            "the per-profile tmp root is created before spawn"
        );

        // Empty profile (default ctx) → the shared base, unchanged (tests).
        *seen_ws.lock().unwrap() = None;
        *seen_tmp.lock().unwrap() = None;
        let _ = tool
            .run(
                ToolInput::new(serde_json::json!({"command": "echo hi"})),
                &ExecCtx::default(),
            )
            .await;
        assert_eq!(
            seen_ws.lock().unwrap().clone().unwrap(),
            base,
            "empty profile → shared base ws"
        );
        assert_eq!(
            seen_tmp.lock().unwrap().clone().unwrap(),
            tmp_base,
            "empty profile → shared base tmp"
        );

        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&tmp_base);
    }

    #[tokio::test]
    async fn shell_exec_applies_the_per_profile_sandbox_config_network_ceiling() {
        // M7 Tier-K Slice 2: the caller's per-profile `sandbox_config` is a CEILING
        // on the shell's network — a locked-down profile denies network even when
        // the call requests it; an unconfigured profile keeps today's behavior; a
        // call that doesn't ask for network never gets it.
        use crate::hooks::{SandboxConfig, SandboxNetworkConfig};
        let base = std::env::temp_dir().join(format!("lhp-exec-net-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&base).unwrap();
        let storage = crate::storage::Storage::open(&base).unwrap();

        // Lock down "work": no localhost, no allowed domains → no shell network.
        let locked = SandboxConfig {
            enabled: true,
            auto_allow_if_sandboxed: false,
            excluded_commands: vec![],
            network: SandboxNetworkConfig {
                allowed_domains: vec![],
                allow_localhost: false,
                allow_unix_sockets: vec![],
            },
        };
        storage
            .open_profile("work")
            .unwrap()
            .set_sandbox_config(&locked)
            .unwrap();
        // A network-permitting config for "school".
        storage
            .open_profile("school")
            .unwrap()
            .set_sandbox_config(&SandboxConfig::default())
            .ok();
        // Note: SandboxConfig::default().network.allow_localhost == true → permits.
        let permit = SandboxConfig {
            network: SandboxNetworkConfig {
                allow_localhost: true,
                ..Default::default()
            },
            ..locked.clone()
        };
        storage
            .open_profile("school")
            .unwrap()
            .set_sandbox_config(&permit)
            .unwrap();

        let seen_network = Arc::new(std::sync::Mutex::new(None));
        let tool = ShellExecTool::new(
            base.join("workspace"),
            base.join("tmp"),
            Arc::new(CaptureRootSpawn {
                seen_ws: Arc::new(std::sync::Mutex::new(None)),
                seen_tmp: Arc::new(std::sync::Mutex::new(None)),
                seen_network: seen_network.clone(),
            }),
            Duration::from_secs(5),
        )
        .with_storage(storage.clone());

        let run = |profile: &'static str, net: bool| {
            let tool = &tool;
            let input = ToolInput::new(serde_json::json!({"command": "echo hi", "network": net}));
            let ctx = ExecCtx {
                profile: profile.to_string(),
                ..ExecCtx::default()
            };
            async move {
                let _ = tool.run(input, &ctx).await;
            }
        };

        // Locked profile requests network → DENIED by the ceiling.
        run("work", true).await;
        assert_eq!(
            *seen_network.lock().unwrap(),
            Some(false),
            "locked profile denies shell network"
        );

        // Unconfigured profile (no row) → the request stands (legacy behavior).
        *seen_network.lock().unwrap() = None;
        run("personal", true).await;
        assert_eq!(
            *seen_network.lock().unwrap(),
            Some(true),
            "unconfigured profile keeps today's behavior"
        );

        // Permitting config → ceiling lifted.
        *seen_network.lock().unwrap() = None;
        run("school", true).await;
        assert_eq!(
            *seen_network.lock().unwrap(),
            Some(true),
            "a network-permitting config allows it"
        );

        // A call that doesn't ask for network never gets it, regardless of config.
        *seen_network.lock().unwrap() = None;
        run("school", false).await;
        assert_eq!(
            *seen_network.lock().unwrap(),
            Some(false),
            "no request → no network"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    // ── unix-only: a real subprocess through a plain (non-Seatbelt) spawner ──
    // These exercise run_guarded's output/timeout/exit plumbing on any unix
    // without depending on macOS Seatbelt.

    #[cfg(unix)]
    struct PlainUnixSpawn;
    #[cfg(unix)]
    impl SandboxedSpawn for PlainUnixSpawn {
        fn spawn(
            &self,
            spec: &ExecSpec,
        ) -> Result<(tokio::process::Child, Vec<PathBuf>), ExecError> {
            let mut cmd = tokio::process::Command::new("/bin/sh");
            cmd.arg("-c")
                .arg(&spec.command)
                .current_dir(&spec.workspace_root)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            cmd.process_group(0);
            let child = cmd.spawn().map_err(|e| ExecError::Io(e.to_string()))?;
            Ok((child, Vec::new()))
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn exit_code_and_stdout_reported_for_a_normal_command() {
        let out = run_guarded(&PlainUnixSpawn, &spec("echo hello"))
            .await
            .expect("normal command must succeed");
        assert_eq!(out.exit_code, Some(0));
        assert!(out.stdout.contains("hello"), "stdout: {}", out.stdout);
        assert!(!out.timed_out);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn exit_65_with_stdout_is_not_misreported_as_sandbox_failure() {
        // Regression (review MED): a command that legitimately exits 65 AND
        // prints `sandbox-exec: ` on its own stderr with real stdout must be a
        // normal ExecOutput, not a FALSE SandboxApply that discards its output.
        // The `&& stdout.is_empty()` guard is what distinguishes this from a
        // genuine apply failure (which never lets the target run).
        let out = run_guarded(
            &PlainUnixSpawn,
            &spec("echo real-output; echo 'sandbox-exec: not really' >&2; exit 65"),
        )
        .await
        .expect("must be a normal Ok, not a false SandboxApply Err");
        assert_eq!(out.exit_code, Some(65));
        assert!(
            out.stdout.contains("real-output"),
            "stdout must be preserved: {}",
            out.stdout
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn output_is_capped_with_head_tail_and_elision() {
        // Print ~200 KiB of 'a' — over HEAD+TAIL (80 KiB), so it elides.
        let mut s = spec("head -c 204800 /dev/zero | tr '\\0' 'a'");
        s.timeout = Duration::from_secs(20);
        let out = run_guarded(&PlainUnixSpawn, &s)
            .await
            .expect("must succeed");
        assert!(
            out.stdout.contains("bytes elided"),
            "expected an elision marker"
        );
        // Bounded to roughly HEAD + TAIL + a short marker, nowhere near 200 KiB.
        assert!(
            out.stdout.len() < OUTPUT_HEAD_CAP_BYTES + OUTPUT_TAIL_CAP_BYTES + 128,
            "output not bounded: {} bytes",
            out.stdout.len()
        );
        assert!(out.stdout.starts_with("aaaa"), "head content preserved");
        assert!(
            out.stdout.ends_with("aaaa\n") || out.stdout.ends_with('a'),
            "tail content preserved"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_kills_the_whole_process_group() {
        // A backgrounded grandchild would touch `marker` after 2s; the main
        // shell sleeps 30s. With a 300ms timeout, a *group* kill must reap the
        // grandchild too, so `marker` NEVER appears — proving we kill the
        // group, not just the direct child.
        let dir = std::env::temp_dir().join(format!("lhp-exec-timeout-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("marker");
        let command = format!("( sleep 2 && touch '{}' ) & sleep 30", marker.display());

        let mut s = spec(&command);
        s.workspace_root = dir.clone();
        s.tmp_root = dir.clone();
        s.timeout = Duration::from_millis(300);

        let start = std::time::Instant::now();
        let out = run_guarded(&PlainUnixSpawn, &s)
            .await
            .expect("must not error");
        assert!(out.timed_out, "must report a timeout");
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "must return near the timeout, not after 30s"
        );

        // Wait past when the grandchild WOULD have touched the marker.
        tokio::time::sleep(Duration::from_millis(3500)).await;
        assert!(
            !marker.exists(),
            "the backgrounded grandchild survived — group kill did not reap it"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── M-14: setsid escape is caught by process-tree walk ─────────────────

    /// A process layout that actively tries to outrun the tree kill.
    ///
    /// * 40 sibling sleepers in the process group — their only job is to make a
    ///   kill-FIRST implementation's post-hoc walk expensive (the old code ran
    ///   one `proc_pidinfo` per pid *per BFS level*, so a wide tree cost tens of
    ///   ms), which is exactly the window in which the group dies and the
    ///   parent links that walk needs evaporate.
    /// * a 5-deep chain of intermediates, all inside the group, so a post-hoc
    ///   walk needs five sequential levels of links that the kill it just sent
    ///   is busy destroying.
    /// * at the bottom, `setsid()` (out of the process group, so `kill(-pgid)`
    ///   misses it) and then ANOTHER fork, so the process that would touch the
    ///   marker is a child of the escapee rather than the escapee itself.
    ///
    /// The marker is written relative to the command's cwd (the workspace
    /// root), so nothing has to be interpolated into the program text.
    #[cfg(unix)]
    const ESCAPE_ARTIST: &str = concat!(
        "i=0; while [ $i -lt 40 ]; do sleep 25 & i=$((i+1)); done; ",
        "perl -MPOSIX -e '",
        "for my $d (1..5) { my $p = fork; defined $p or exit 1; if ($p) { sleep 25; exit } } ",
        "setsid(); ",
        "my $q = fork; defined $q or exit 1; if ($q) { sleep 25; exit } ",
        "sleep 3; open(my $f, \">\", \"marker\") or exit 1; close $f; exit",
        "' & exec sleep 25"
    );

    #[cfg(unix)]
    #[tokio::test]
    async fn setsid_descendant_that_orphans_itself_is_still_reaped() {
        // POSITIVE CONTROL first: spawn the same layout and DON'T kill it, to
        // prove the escape actually happens. Without this the real assertion
        // below could pass vacuously — e.g. if perl merely failed to start.
        let ctl = std::env::temp_dir().join(format!("lhp-exec-ctl-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&ctl).unwrap();
        let mut s_ctl = spec(ESCAPE_ARTIST);
        s_ctl.workspace_root = ctl.clone();
        s_ctl.tmp_root = ctl.clone();
        let (mut ctl_child, _) = PlainUnixSpawn
            .spawn(&s_ctl)
            .expect("control layout must spawn");
        let ctl_pid = ctl_child.id().expect("control child has a pid") as i32;
        tokio::time::sleep(Duration::from_millis(4500)).await;
        assert!(
            ctl.join("marker").exists(),
            "control: the layout never reached the marker, so this test proves nothing"
        );
        platform::kill_group(ctl_pid);
        let _ = ctl_child.wait().await;
        let _ = std::fs::remove_dir_all(&ctl);

        // The real thing: run_guarded must reap the escapee's child even though
        // the group kill orphans it and every parent link in between dies.
        let dir = std::env::temp_dir().join(format!("lhp-exec-setsid-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("marker");
        let mut s = spec(ESCAPE_ARTIST);
        s.workspace_root = dir.clone();
        s.tmp_root = dir.clone();
        s.timeout = Duration::from_millis(300);

        let start = std::time::Instant::now();
        let out = run_guarded(&PlainUnixSpawn, &s)
            .await
            .expect("must not error");
        assert!(out.timed_out, "must report a timeout");
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "must return near the timeout, not after 25s"
        );

        // Wait past when the escapee's child would have touched the marker.
        tokio::time::sleep(Duration::from_millis(4500)).await;
        assert!(
            !marker.exists(),
            "the setsid escapee's child survived — the tree kill lost the race"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    fn row(pid: i32, ppid: i32, pgid: i32) -> platform::ProcRow {
        platform::ProcRow { pid, ppid, pgid }
    }

    #[cfg(unix)]
    fn seeds(pids: &[i32]) -> std::collections::HashSet<i32> {
        pids.iter().copied().collect()
    }

    #[cfg(unix)]
    #[test]
    fn expand_tree_follows_ppid_and_groups_led_by_an_in_tree_pid() {
        // 100 = the leader (process_group(0) ⇒ pgid == pid).
        // 101 = an intermediate that stayed in the group.
        // 102 = called setsid(2): new session, own group — kill(-100) misses it.
        // 103 = forked by the escapee, so it is in the ESCAPEE's group.
        // 104 = also forked by the escapee, but the escapee has since died, so
        //       it has been reparented to launchd: its ONLY remaining link is
        //       that its process group is led by 102. The ppid chain alone
        //       cannot find it.
        // 200/201 = unrelated processes that must never be touched.
        let table = vec![
            row(100, 10, 100),
            row(101, 100, 100),
            row(102, 101, 102),
            row(103, 102, 102),
            row(104, 1, 102),
            row(200, 1, 200),
            row(201, 200, 200),
        ];
        let found = platform::expand_tree(&table, &seeds(&[100]), 9999, 9998);
        assert_eq!(
            found,
            seeds(&[100, 101, 102, 103, 104]),
            "the setsid escapee, its child, and its orphaned child are in the tree; 200/201 are not"
        );
    }

    #[cfg(unix)]
    #[test]
    fn expand_tree_finds_a_child_forked_after_the_first_scan() {
        // Round 1 saw 100→101→102(setsid)→103. By round 2 the group kill has
        // taken 101 (so 102 is reparented to launchd, its link to the leader
        // gone) and 103 has forked 104 in the meantime.
        let round2 = vec![
            row(100, 10, 100),
            row(102, 1, 102),
            row(103, 102, 102),
            row(104, 103, 103),
            row(200, 1, 200),
        ];
        // Seeded with the CUMULATIVE set from round 1, the late child is found.
        let found = platform::expand_tree(&round2, &seeds(&[100, 101, 102, 103]), 9999, 9998);
        assert!(
            found.contains(&104),
            "a child forked between snapshot and signal must be caught by the re-scan: {found:?}"
        );
        assert!(!found.contains(&200), "unrelated pid pulled in: {found:?}");

        // And this is the race that the snapshot-first order exists to avoid:
        // with only the leader as a seed — all a kill-first walk has left once
        // the group is dead — the whole escaped branch is invisible.
        let leader_only = platform::expand_tree(&round2, &seeds(&[100]), 9999, 9998);
        assert_eq!(
            leader_only,
            seeds(&[100]),
            "post-kill links are gone, so discovery must happen BEFORE signalling"
        );
    }

    #[cfg(unix)]
    #[test]
    fn expand_tree_never_targets_the_harness_or_its_process_group() {
        // We are pid 500 in pgid 400. 102 is a genuine descendant that has
        // setpgid(2)'d itself INTO our group — a hostile move that must not
        // make our group in-tree, since killing 400 would kill the harness.
        let table = vec![
            row(100, 10, 100),
            row(101, 100, 100),
            row(102, 101, 400),
            row(500, 100, 400), // us — even though our parent is the leader
            row(499, 1, 400),   // an unrelated member of our group
        ];
        let found = platform::expand_tree(&table, &seeds(&[100]), 500, 400);
        assert!(!found.contains(&500), "must never target our own pid");
        assert!(
            !found.contains(&499),
            "a descendant joining our group must not drag the group in: {found:?}"
        );
        assert!(
            found.contains(&102),
            "the descendant itself is still in the tree: {found:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn kill_tree_snapshots_before_it_signals_and_rescans_until_stable() {
        // Drives the real discovery loop with a scripted process table, so the
        // ORDER of syscalls is observable without racing anything.
        #[derive(Debug, PartialEq, Eq)]
        enum Ev {
            Snapshot,
            Signal(i32),
        }
        let log = std::cell::RefCell::new(Vec::<Ev>::new());
        let round = std::cell::Cell::new(0usize);

        // Round 0: the full tree, still linked. Round 1: the group kill has
        // taken 101, and 103 has forked 104 in the meantime. Round 2: quiet.
        let tables = [
            vec![
                row(100, 10, 100),
                row(101, 100, 100),
                row(102, 101, 102),
                row(103, 102, 102),
                row(900, 1, 900),
            ],
            vec![row(102, 1, 102), row(103, 102, 102), row(104, 103, 103)],
            vec![row(104, 103, 103)],
        ];

        platform::kill_tree_with(
            100,
            500,
            400,
            || {
                log.borrow_mut().push(Ev::Snapshot);
                let i = round.get();
                round.set(i + 1);
                tables.get(i).cloned().unwrap_or_default()
            },
            |t| log.borrow_mut().push(Ev::Signal(t)),
        );

        let log = log.into_inner();
        // THE ordering property: discovery precedes every signal. If the first
        // event were a signal, the links the discovery needs would already be
        // being destroyed — that is the race this packet exists to close.
        assert_eq!(
            log.first(),
            Some(&Ev::Snapshot),
            "must snapshot BEFORE signalling anything: {log:?}"
        );
        let first_signal = log
            .iter()
            .position(|e| matches!(e, Ev::Signal(_)))
            .expect("something must be signalled");
        assert_eq!(
            log.iter()
                .take(first_signal)
                .filter(|e| **e == Ev::Snapshot)
                .count(),
            1,
            "exactly one snapshot must precede the first signal: {log:?}"
        );

        // Round 0 signals the group plus every pid discovered from the snapshot,
        // and nothing else — 900 is unrelated, 500 is us.
        assert_eq!(
            log[first_signal..first_signal + 5],
            [
                Ev::Signal(-100),
                Ev::Signal(100),
                Ev::Signal(101),
                Ev::Signal(102),
                Ev::Signal(103)
            ],
            "round 0 kills the group and the snapshotted set: {log:?}"
        );
        assert!(
            !log.contains(&Ev::Signal(900)) && !log.contains(&Ev::Signal(500)),
            "signalled something outside the tree: {log:?}"
        );

        // The re-scan must catch 104, forked after the first snapshot — reachable
        // only because the seed set carried 103 forward.
        assert!(
            log.contains(&Ev::Signal(104)),
            "a child forked after the first snapshot was never signalled: {log:?}"
        );

        // And the loop must settle instead of spinning to MAX_KILL_ROUNDS: the
        // third table adds nothing new, so that round breaks before signalling.
        assert_eq!(
            log.iter().filter(|e| **e == Ev::Snapshot).count(),
            3,
            "must stop re-scanning once the set is stable: {log:?}"
        );
    }

    // ── bounded drain: an inherited pipe must not hang the caller ───────────

    #[cfg(unix)]
    #[tokio::test]
    async fn drain_is_bounded_when_an_orphan_holds_the_pipe_open() {
        // The command itself exits at once, but a backgrounded subshell
        // inherited its stdout write end and lives on for 20s. The unbounded
        // read used to block run_guarded for as long as that process felt like
        // living; it must now give up after the grace period WITH the real
        // output intact.
        let mut s = spec("( sleep 20 ) & echo hi");
        s.timeout = Duration::from_secs(30);

        let start = std::time::Instant::now();
        let out = run_guarded(&PlainUnixSpawn, &s)
            .await
            .expect("must not error");
        let elapsed = start.elapsed();

        assert_eq!(out.exit_code, Some(0), "the command itself exited cleanly");
        assert!(!out.timed_out, "the command did not time out");
        assert!(
            out.stdout.contains("hi"),
            "bytes read before the bound must be kept: {:?}",
            out.stdout
        );
        assert!(
            elapsed < DRAIN_GRACE + Duration::from_secs(5),
            "drain was not bounded — returned after {elapsed:?}, with a 20s pipe holder"
        );
    }

    // ── resource ceilings (rlimits) ─────────────────────────────────────────

    /// A plain (non-Seatbelt) spawner that installs `ResourceCeilings` through
    /// the SAME `apply_ceilings` hook the real spawner uses, so these tests
    /// exercise the production path with values small enough to observe.
    #[cfg(target_os = "macos")]
    struct CeilingSpawn(ResourceCeilings);
    #[cfg(target_os = "macos")]
    impl SandboxedSpawn for CeilingSpawn {
        fn spawn(
            &self,
            spec: &ExecSpec,
        ) -> Result<(tokio::process::Child, Vec<PathBuf>), ExecError> {
            let mut cmd = tokio::process::Command::new("/bin/sh");
            cmd.arg("-c")
                .arg(&spec.command)
                .current_dir(&spec.workspace_root)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            cmd.process_group(0);
            apply_ceilings(&mut cmd, self.0);
            let child = cmd.spawn().map_err(|e| ExecError::Io(e.to_string()))?;
            Ok((child, Vec::new()))
        }
    }

    #[cfg(target_os = "macos")]
    fn soft_nofile() -> u64 {
        // SAFETY: read-only getrlimit into a live, correctly-sized struct.
        unsafe {
            let mut r: libc::rlimit = std::mem::zeroed();
            assert_eq!(libc::getrlimit(libc::RLIMIT_NOFILE, &mut r), 0);
            r.rlim_cur as u64
        }
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn resource_ceilings_reach_the_child_soft_and_hard() {
        // Both halves must be lowered: a soft-only limit is theatre, since the
        // child can raise it back to hard whenever it likes.
        let ceilings = ResourceCeilings {
            cpu_seconds: 77,
            file_size_bytes: 512 * 1024,
            open_files: 64,
        };
        let out = run_guarded(
            &CeilingSpawn(ceilings),
            &spec("ulimit -St; ulimit -Ht; ulimit -Sn; ulimit -Hn"),
        )
        .await
        .expect("must run");
        assert_eq!(out.exit_code, Some(0), "stderr: {}", out.stderr);
        let seen: Vec<&str> = out.stdout.split_whitespace().collect();
        assert_eq!(
            seen,
            vec!["77", "77", "64", "64"],
            "child did not inherit the ceilings (soft AND hard): {:?}",
            out.stdout
        );
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn file_size_ceiling_stops_a_runaway_writer() {
        // Not just reported — enforced. 64 KiB of output into a 4 KiB ceiling.
        let dir = std::env::temp_dir().join(format!("lhp-exec-fsize-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let ceilings = ResourceCeilings {
            cpu_seconds: 30,
            file_size_bytes: 4096,
            open_files: 64,
        };
        let mut s = spec("head -c 65536 /dev/zero > big.bin 2>/dev/null; echo rc=$?");
        s.workspace_root = dir.clone();
        s.tmp_root = dir.clone();
        let out = run_guarded(&CeilingSpawn(ceilings), &s)
            .await
            .expect("must run");
        assert!(
            out.stdout.contains("rc=") && !out.stdout.contains("rc=0"),
            "the oversized write must fail: {:?}",
            out.stdout
        );
        let written = std::fs::metadata(dir.join("big.bin"))
            .expect("the file is created, just capped")
            .len();
        assert!(
            written <= 4096,
            "file grew past the ceiling: {written} bytes"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn a_ceiling_looser_than_the_inherited_limit_never_raises_it() {
        // "Tighten only": asking for more descriptors than we ourselves have
        // must not hand the child more than we had.
        let parent = soft_nofile();
        let ceilings = ResourceCeilings {
            cpu_seconds: 77,
            file_size_bytes: 512 * 1024,
            open_files: parent.saturating_mul(4).saturating_add(4096),
        };
        let out = run_guarded(&CeilingSpawn(ceilings), &spec("ulimit -Sn"))
            .await
            .expect("must run");
        let child: u64 = out
            .stdout
            .trim()
            .parse()
            .unwrap_or_else(|_| panic!("expected a number, got {:?}", out.stdout));
        assert!(
            child <= parent,
            "raised the child's soft limit from {parent} to {child}"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn cpu_ceiling_exceeds_what_the_wall_budget_can_legitimately_buy() {
        // A ceiling a well-behaved command can hit is a bug: `timeout` seconds
        // of wall clock can buy `timeout * cores` CPU-seconds.
        let cores = std::thread::available_parallelism()
            .map(|n| n.get() as u64)
            .unwrap_or(1);
        let c = ResourceCeilings::for_timeout(Duration::from_secs(60));
        assert!(
            c.cpu_seconds >= 60 * cores,
            "{} CPU-seconds would kill legitimate parallel work on {cores} cores",
            c.cpu_seconds
        );
        // …and it must still be finite and scale with the budget, not a constant.
        assert!(
            ResourceCeilings::for_timeout(Duration::from_secs(600)).cpu_seconds > c.cpu_seconds,
            "the CPU ceiling must track the wall budget"
        );
    }

    // ── macOS-only: the real Seatbelt sandbox ────────────────────────────────

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn seatbelt_spawn_installs_the_resource_ceilings() {
        // The ceilings must be wired into the REAL spawner, not merely
        // available: this is what fails if `apply_ceilings` is dropped from
        // MacSeatbeltSpawn::spawn.
        let ws = std::env::temp_dir().join(format!("lhp-sb-rl-{}", uuid::Uuid::new_v4()));
        let tmp = std::env::temp_dir().join(format!("lhp-sb-rltmp-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::create_dir_all(&tmp).unwrap();

        let mut s = spec("ulimit -Sn; ulimit -Hn; ulimit -St");
        s.workspace_root = ws.clone();
        s.tmp_root = tmp.clone();
        s.timeout = Duration::from_secs(10);
        let out = run_guarded(&MacSeatbeltSpawn, &s)
            .await
            .expect("must run under Seatbelt");
        assert_eq!(out.exit_code, Some(0), "stderr: {}", out.stderr);

        let expected = ResourceCeilings::for_timeout(Duration::from_secs(10));
        let seen: Vec<&str> = out.stdout.split_whitespace().collect();
        assert_eq!(
            seen,
            vec![
                OPEN_FILES_CEILING.to_string(),
                OPEN_FILES_CEILING.to_string(),
                expected.cpu_seconds.to_string(),
            ],
            "the real spawner did not install the ceilings: {:?}",
            out.stdout
        );
        let _ = std::fs::remove_dir_all(&ws);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // ── M-15: seatbelt_escape_path adversarial tests ──────────────────────

    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_escape_normal_path() {
        let p = Path::new("/Users/test/workspace");
        assert_eq!(seatbelt_escape_path(p).unwrap(), "/Users/test/workspace");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_escape_backslash() {
        let p = Path::new("/Users/test/back\\slash");
        assert_eq!(
            seatbelt_escape_path(p).unwrap(),
            "/Users/test/back\\\\slash"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_escape_double_quote() {
        let p = Path::new("/Users/test/\"quote");
        assert_eq!(seatbelt_escape_path(p).unwrap(), "/Users/test/\\\"quote");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_escape_backslash_and_quote() {
        let p = Path::new("/Users/test/\"both\\path");
        assert_eq!(
            seatbelt_escape_path(p).unwrap(),
            "/Users/test/\\\"both\\\\path"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_escape_null_fails_closed() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let bytes = b"/Users/test/\0evil";
        let p = Path::new(OsStr::from_bytes(bytes));
        assert!(
            seatbelt_escape_path(p).is_err(),
            "null byte must be rejected (fail-closed)"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_escape_control_char_fails_closed() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let bytes = b"/Users/test/\nnewline";
        let p = Path::new(OsStr::from_bytes(bytes));
        assert!(
            seatbelt_escape_path(p).is_err(),
            "control character must be rejected (fail-closed)"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_escape_non_utf8_fails_closed() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let bytes = b"/Users/test/\xff\xffevil";
        let p = Path::new(OsStr::from_bytes(bytes));
        assert!(
            seatbelt_escape_path(p).is_err(),
            "non-UTF-8 path must be rejected (fail-closed)"
        );
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn workspace_writes_succeed_outside_writes_denied() {
        let ws = std::env::temp_dir().join(format!("lhp-sb-ws-{}", uuid::Uuid::new_v4()));
        let tmp = std::env::temp_dir().join(format!("lhp-sb-tmp-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::create_dir_all(&tmp).unwrap();

        // Write inside the workspace — allowed, readable back.
        let mut s = spec("echo inside > allowed.txt && cat allowed.txt");
        s.workspace_root = ws.clone();
        s.tmp_root = tmp.clone();
        let out = run_guarded(&MacSeatbeltSpawn, &s)
            .await
            .expect("in-workspace write must run");
        assert_eq!(out.exit_code, Some(0), "stderr: {}", out.stderr);
        assert!(out.stdout.contains("inside"), "stdout: {}", out.stdout);

        // Write OUTSIDE both roots — denied, file never created.
        let outside = std::env::temp_dir().join(format!("lhp-sb-outside-{}", uuid::Uuid::new_v4()));
        let mut s2 = spec(&format!("echo pwned > '{}'", outside.display()));
        s2.workspace_root = ws.clone();
        s2.tmp_root = tmp.clone();
        let out2 = run_guarded(&MacSeatbeltSpawn, &s2)
            .await
            .expect("must run (and be denied by the OS)");
        assert_ne!(out2.exit_code, Some(0), "an out-of-sandbox write must fail");
        assert!(
            !outside.exists(),
            "nothing may be written outside the sandbox roots"
        );

        let _ = std::fs::remove_dir_all(&ws);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn network_off_by_default_blocks_egress() {
        let ws = std::env::temp_dir().join(format!("lhp-sb-net-{}", uuid::Uuid::new_v4()));
        let tmp = std::env::temp_dir().join(format!("lhp-sb-nettmp-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::create_dir_all(&tmp).unwrap();

        // A connection attempt with network off must fail to connect.
        let mut s = spec("curl -s -m 4 -o /dev/null -w '%{http_code}' https://example.com");
        s.workspace_root = ws.clone();
        s.tmp_root = tmp.clone();
        s.timeout = Duration::from_secs(10);
        let out = run_guarded(&MacSeatbeltSpawn, &s)
            .await
            .expect("curl must run (and be blocked)");
        assert!(
            !out.stdout.contains("200"),
            "network is off by default; egress must be blocked, got stdout: {}",
            out.stdout
        );
        let _ = std::fs::remove_dir_all(&ws);
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
