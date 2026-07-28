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
use std::sync::Arc;
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
    use std::process::ExitStatus;

    /// Kill the whole process group (`pgid == child pid`) AND walk the
    /// process tree to catch any descendant that escaped via `setsid(2)`.
    /// One `kill(-pgid)` reaps `sandbox-exec` → its fork → any grandchild
    /// that stayed in the group; the tree walk catches the rest.
    pub fn kill_group(pgid: i32) {
        // SAFETY: `kill(2)` with a negative pid targets the process group. A
        // missing group (already-exited) just returns ESRCH, which we ignore.
        unsafe {
            libc::kill(-pgid, libc::SIGKILL);
        }
        // Catch any descendant that escaped the group kill via setsid(2).
        kill_descendants(pgid);
    }

    /// Walk the process tree from `leader_pid` and SIGKILL every descendant.
    /// This defeats `setsid(2)` escape: the descendant still has a parent
    /// link back to the leader even after creating a new session.
    #[cfg(target_os = "macos")]
    fn kill_descendants(leader_pid: i32) {
        // Use libproc(3) to list all PIDs and query each for its BSD info.
        // proc_listallpids returns the full pid list; proc_pidinfo with
        // PROC_PIDTBSDINFO fills proc_bsdinfo which has pbi_ppid + pbi_pgid.
        // BFS from the leader to find every descendant — catches setsid(2)
        // escapees because the parent chain is intact at enumeration time.
        use libc::c_int;
        use std::mem;

        const PROC_PIDTBSDINFO: c_int = 3;

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
            let max_pids = 4096;
            let mut pids: Vec<c_int> = vec![0; max_pids];
            let count = proc_listallpids(
                pids.as_mut_ptr(),
                (max_pids * mem::size_of::<c_int>()) as c_int,
            );
            if count <= 0 {
                return;
            }
            let count = count as usize;

            // BFS: every process whose parent pid is in the worklist
            // is a descendant.
            let mut to_kill: Vec<i32> = vec![leader_pid];
            let mut i = 0;
            while i < to_kill.len() {
                let parent = to_kill[i];
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
                        mem::size_of::<libc::proc_bsdinfo>() as c_int,
                    );
                    // proc_pidinfo returns the number of bytes written.
                    if ret as usize == mem::size_of::<libc::proc_bsdinfo>() {
                        if info.pbi_ppid as i32 == parent {
                            let child = info.pbi_pid as i32;
                            if child != leader_pid && !to_kill.contains(&child) {
                                to_kill.push(child);
                            }
                        }
                    }
                }
                i += 1;
            }

            // Kill every descendant (leader was already signalled above).
            for &pid in &to_kill {
                if pid != leader_pid {
                    libc::kill(pid, libc::SIGKILL);
                }
            }
        }
    }

    /// Stub for non-macOS unices.  No portable tree-walk; group kill suffices.
    /// A Linux sandbox backend should use /proc or PID-cgroup iteration.
    #[cfg(not(target_os = "macos"))]
    fn kill_descendants(_leader_pid: i32) {}

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

/// Drain a piped child stream fully into a bounded collector. Runs as its own
/// task so stdout+stderr are consumed concurrently with the process running
/// (never letting a full pipe buffer deadlock the child).
async fn drain_reader<R>(reader: Option<R>) -> HeadTail
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut ht = HeadTail::default();
    if let Some(mut r) = reader {
        let mut buf = [0u8; 8192];
        loop {
            match r.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => ht.push_chunk(&buf[..n]),
            }
        }
    }
    ht
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

    // Drain both pipes concurrently while the process runs.
    let out_handle = tokio::spawn(drain_reader(child.stdout.take()));
    let err_handle = tokio::spawn(drain_reader(child.stderr.take()));

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
    // error like ECHILD), and sandbox-exec read the profile at exec time so
    // deleting it after wait/close is race-free.
    let stdout = out_handle.await.unwrap_or_default().render();
    let stderr = err_handle.await.unwrap_or_default().render();
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

    #[cfg(unix)]
    #[tokio::test]
    async fn setsid_descendant_is_killed_on_timeout() {
        // A grandchild that calls setsid(2) creates a new session/process
        // group and escapes kill(-pgid,SIGKILL).  The tree-walk in
        // kill_descendants must find and kill it.
        //
        // Race-safe design: the Perl fork-parent stays alive for 10s (past
        // the 300 ms timeout), so the setsid grandchild's parent (ppid) is
        // still in the process table when the BFS runs.
        let dir = std::env::temp_dir().join(format!("lhp-exec-setsid-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("marker");
        let command = format!(
            "perl -MPOSIX -e 'defined(my$p=fork) or die 1; if($p){{sleep 10;exit}} setsid();sleep 3;open(my$f,\">\",\"{}\");close$f;exit' & exec sleep 30",
            marker.display()
        );
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

        // Wait past when the setsid escapee would have touched the marker.
        tokio::time::sleep(Duration::from_millis(4500)).await;
        assert!(
            !marker.exists(),
            "the setsid grandchild survived — tree kill did not find it"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── macOS-only: the real Seatbelt sandbox ────────────────────────────────

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
