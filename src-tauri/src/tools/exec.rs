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
pub trait SandboxedSpawn: Send + Sync {
    fn spawn(&self, spec: &ExecSpec) -> Result<tokio::process::Child, ExecError>;
}

// ── platform helpers (process-group kill + sandbox-failure detection) ────────

#[cfg(unix)]
mod platform {
    use std::process::ExitStatus;

    /// Kill the whole process group (`pgid == child pid`, because the spawner
    /// set `.process_group(0)` and nothing calls `setpgid`). One `kill(-pgid)`
    /// reaps `sandbox-exec` → its fork → any grandchild.
    pub fn kill_group(pgid: i32) {
        // SAFETY: `kill(2)` with a negative pid targets the process group. A
        // missing group (already-exited) just returns ESRCH, which we ignore.
        unsafe {
            libc::kill(-pgid, libc::SIGKILL);
        }
    }

    /// Distinguish a sandbox-APPLY failure from the command's own result. Two
    /// forms, both verified empirically on macOS 15 (see the module spec): the
    /// `import system.sb`-omission crash → SIGABRT (signal 6); and a profile
    /// syntax / unknown-operation error → exit code 65 with a `sandbox-exec: `
    /// stderr prefix. Only consulted when WE didn't kill the child for timeout.
    pub fn is_sandbox_apply_failure(status: &ExitStatus, stderr: &str) -> bool {
        use std::os::unix::process::ExitStatusExt;
        status.signal() == Some(libc::SIGABRT)
            || (status.code() == Some(65) && stderr.starts_with("sandbox-exec: "))
    }
}

#[cfg(not(unix))]
mod platform {
    use std::process::ExitStatus;
    // No unix process groups / Seatbelt off-unix; the only spawner on these
    // platforms is `UnsupportedSandbox`, which errors before a child exists,
    // so these are unreachable in practice — they exist only to compile.
    pub fn kill_group(_pgid: i32) {}
    pub fn is_sandbox_apply_failure(_status: &ExitStatus, _stderr: &str) -> bool {
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
    let mut child = spawner.spawn(spec)?;
    let pgid = child
        .id()
        .map(|p| p as i32)
        .ok_or_else(|| ExecError::Io("spawned child has no pid".to_string()))?;

    // Drain both pipes concurrently while the process runs.
    let out_handle = tokio::spawn(drain_reader(child.stdout.take()));
    let err_handle = tokio::spawn(drain_reader(child.stderr.take()));

    let mut timed_out = false;
    let status: Option<std::process::ExitStatus> = tokio::select! {
        res = child.wait() => match res {
            Ok(s) => Some(s),
            Err(e) => return Err(ExecError::Io(format!("waiting on child: {e}"))),
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

    let stdout = out_handle.await.unwrap_or_default().render();
    let stderr = err_handle.await.unwrap_or_default().render();
    let duration_ms = start.elapsed().as_millis();

    match status {
        Some(status) => {
            // Only meaningful when we did NOT kill it for timeout: catch a
            // silent Seatbelt failure so it can't be misreported as "the
            // command exited weird."
            if platform::is_sandbox_apply_failure(&status, &stderr) {
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
    fn spawn(&self, spec: &ExecSpec) -> Result<tokio::process::Child, ExecError> {

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

        let profile = build_seatbelt_profile(&ws, &tmp, spec.network);
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
        cmd.spawn()
            .map_err(|e| ExecError::Io(format!("spawning sandbox-exec: {e}")))
    }
}

/// Build the Seatbelt profile. Verified on macOS 15 — see the module spec for
/// the `import system.sb` gotcha and the tested deny/allow set.
#[cfg(target_os = "macos")]
fn build_seatbelt_profile(workspace: &Path, tmp: &Path, network: bool) -> String {
    let ws = workspace.display();
    let tmp = tmp.display();
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
    p
}

// ── UnsupportedSandbox (non-macOS placeholder) ───────────────────────────────

/// The `SandboxedSpawn` for platforms with no backend yet. Every call
/// hard-errors, so `shell_exec` stays *registered* (the model sees it exists)
/// but can never run unsandboxed until a Linux/Windows backend lands.
#[cfg(not(target_os = "macos"))]
pub struct UnsupportedSandbox;

#[cfg(not(target_os = "macos"))]
impl SandboxedSpawn for UnsupportedSandbox {
    fn spawn(&self, _spec: &ExecSpec) -> Result<tokio::process::Child, ExecError> {
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
        _ctx: &'a ExecCtx,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        Box::pin(async move {
            let Some(command) = input.args.get("command").and_then(|v| v.as_str()) else {
                return ToolResult::Err(
                    "shell_exec requires a string 'command' argument".to_string(),
                );
            };
            let command = command.to_string();
            let network = input
                .args
                .get("network")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            // model-supplied timeout can only SHORTEN, never exceed the cap.
            let timeout = match input.args.get("timeout_secs").and_then(|v| v.as_u64()) {
                Some(secs) => std::cmp::min(Duration::from_secs(secs), self.timeout_cap),
                None => self.timeout_cap,
            };

            let spec = ExecSpec {
                command,
                workspace_root: self.workspace_root.clone(),
                tmp_root: self.tmp_root.clone(),
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
                Err(ExecError::Io(msg)) => {
                    ToolResult::Err(format!("shell_exec i/o error: {msg}"))
                }
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
        fn spawn(&self, _spec: &ExecSpec) -> Result<tokio::process::Child, ExecError> {
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
        assert_eq!(text, "rm -rf /", "must be the decoded command, not the JSON envelope");
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

    // ── unix-only: a real subprocess through a plain (non-Seatbelt) spawner ──
    // These exercise run_guarded's output/timeout/exit plumbing on any unix
    // without depending on macOS Seatbelt.

    #[cfg(unix)]
    struct PlainUnixSpawn;
    #[cfg(unix)]
    impl SandboxedSpawn for PlainUnixSpawn {
        fn spawn(&self, spec: &ExecSpec) -> Result<tokio::process::Child, ExecError> {
            let mut cmd = tokio::process::Command::new("/bin/sh");
            cmd.arg("-c")
                .arg(&spec.command)
                .current_dir(&spec.workspace_root)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            cmd.process_group(0);
            cmd.spawn().map_err(|e| ExecError::Io(e.to_string()))
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
    async fn output_is_capped_with_head_tail_and_elision() {
        // Print ~200 KiB of 'a' — over HEAD+TAIL (80 KiB), so it elides.
        let mut s = spec("head -c 204800 /dev/zero | tr '\\0' 'a'");
        s.timeout = Duration::from_secs(20);
        let out = run_guarded(&PlainUnixSpawn, &s).await.expect("must succeed");
        assert!(out.stdout.contains("bytes elided"), "expected an elision marker");
        // Bounded to roughly HEAD + TAIL + a short marker, nowhere near 200 KiB.
        assert!(
            out.stdout.len() < OUTPUT_HEAD_CAP_BYTES + OUTPUT_TAIL_CAP_BYTES + 128,
            "output not bounded: {} bytes",
            out.stdout.len()
        );
        assert!(out.stdout.starts_with("aaaa"), "head content preserved");
        assert!(out.stdout.ends_with("aaaa\n") || out.stdout.ends_with('a'), "tail content preserved");
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
        let out = run_guarded(&PlainUnixSpawn, &s).await.expect("must not error");
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

    // ── macOS-only: the real Seatbelt sandbox ────────────────────────────────

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
        let out = run_guarded(&MacSeatbeltSpawn, &s).await.expect("in-workspace write must run");
        assert_eq!(out.exit_code, Some(0), "stderr: {}", out.stderr);
        assert!(out.stdout.contains("inside"), "stdout: {}", out.stdout);

        // Write OUTSIDE both roots — denied, file never created.
        let outside = std::env::temp_dir().join(format!("lhp-sb-outside-{}", uuid::Uuid::new_v4()));
        let mut s2 = spec(&format!("echo pwned > '{}'", outside.display()));
        s2.workspace_root = ws.clone();
        s2.tmp_root = tmp.clone();
        let out2 = run_guarded(&MacSeatbeltSpawn, &s2).await.expect("must run (and be denied by the OS)");
        assert_ne!(out2.exit_code, Some(0), "an out-of-sandbox write must fail");
        assert!(!outside.exists(), "nothing may be written outside the sandbox roots");

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
        let out = run_guarded(&MacSeatbeltSpawn, &s).await.expect("curl must run (and be blocked)");
        assert!(
            !out.stdout.contains("200"),
            "network is off by default; egress must be blocked, got stdout: {}",
            out.stdout
        );
        let _ = std::fs::remove_dir_all(&ws);
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
