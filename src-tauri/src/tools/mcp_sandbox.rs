//! Containment for MCP **stdio** children (round-4; the half H-07 deliberately
//! deferred). H-07 answered *which* program may run — the pinned invocation.
//! This module answers *what that program may touch once it runs*.
//!
//! Before this existed a registered stdio server was a full-privilege process:
//! `env_clear()` scrubbed the environment and nothing else, so the child could
//! read the whole home directory and open sockets at will. The product decision
//! (2026-08-03) is **deny-default with per-server grants**:
//!
//! * it may READ what it needs to *run* — the pinned executable's install tree,
//!   the script files its argv names, and the same system runtime paths
//!   [`super::exec`] already allows a sandboxed shell command;
//! * it may READ-WRITE one private per-server scratch directory, which is also
//!   its `HOME` and `TMPDIR` — so a server that wants a cache or a config file
//!   gets one, in its own island, not in the user's home;
//! * it gets **no network** and **no user files** at all unless the user
//!   granted them at registration time.
//!
//! Everything else — the user's documents, keys, other servers' scratch dirs,
//! the network — is denied by the `(deny default)` at the top of the profile.
//!
//! **Fail-closed at every fork in the road.** A path that won't canonicalize, a
//! path that can't be escaped into the S-expression, a scratch dir that can't be
//! created, or a platform with no Seatbelt all return `Err` — there is no code
//! path from any failure here to an unconfined `Command::new`. On non-macOS the
//! only constructor hard-errors, exactly like [`super::exec::UnsupportedSandbox`]:
//! an MCP server stays *registered* (the user can see it) but can never run.
//!
//! Escaping and the system read set are IMPORTED from [`super::exec`] rather
//! than re-implemented — one Seatbelt-quoting implementation, audited once.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Directory names that mean "this is the `bin/` of an install tree" — the
/// grandparent, not the parent, is the tree a real interpreter needs (its
/// `lib/`, its `share/`, its dylibs).
const BIN_DIR_NAMES: &[&str] = &["bin", "sbin", "libexec"];

/// The per-server grants the user ticked at registration. Everything is OFF
/// unless explicitly turned on — the deny-default posture is the `Default`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct McpGrants {
    /// May the child open sockets at all? (All-or-nothing: Seatbelt cannot
    /// express a per-host allowlist, and pretending otherwise would be theatre.)
    pub network: bool,
    /// Absolute paths the child may READ, granted by the user.
    pub read_paths: Vec<PathBuf>,
    /// Absolute paths the child may READ AND WRITE, granted by the user.
    pub write_paths: Vec<PathBuf>,
}

impl McpGrants {
    /// The grants persisted on a server's row. A row that predates migration
    /// v10 reads back as `Default` (nothing granted), which is exactly the
    /// posture we want it to adopt.
    pub fn from_row(row: &crate::storage::McpServerRow) -> Self {
        Self {
            network: row.network_access,
            read_paths: row.read_paths.iter().map(PathBuf::from).collect(),
            write_paths: row.write_paths.iter().map(PathBuf::from).collect(),
        }
    }
}

/// One read grant in the profile. A directory becomes a `subpath`; a lone file
/// whose directory is too broad to hand over wholesale becomes a `literal`, so
/// the fallback narrows the grant instead of widening it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadGrant {
    Subpath(PathBuf),
    Literal(PathBuf),
}

impl ReadGrant {
    fn path(&self) -> &Path {
        match self {
            Self::Subpath(p) | Self::Literal(p) => p,
        }
    }
}

/// Everything the containment layer needs to run one MCP stdio child. Built by
/// [`McpSandboxSpec::derive`]; consumed by [`sandboxed_command`].
///
/// Deliberately NOT [`super::exec::ExecSpec`]: that type is shaped for a
/// one-shot `sh -c` string with a timeout and a workspace root. An MCP child is
/// an argv vector, a scrubbed environment, piped stdin/stdout, and no timeout at
/// all (it lives as long as the app does). Bending `ExecSpec` to cover both
/// would blur two genuinely different containment shapes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpSandboxSpec {
    /// The canonical, pinned executable. We exec THIS path — not the bare
    /// `command` string — so the thing H-07 measured is the thing that runs.
    pub program: PathBuf,
    /// argv after the program, verbatim from the pinned row.
    pub args: Vec<String>,
    /// The private per-server scratch dir: read-write, and also the child's
    /// `HOME`. Canonical.
    pub scratch_dir: PathBuf,
    /// Read grants the child needs to *run* (install tree(s), script files).
    pub runtime_reads: Vec<ReadGrant>,
    /// User-granted read paths (canonical).
    pub granted_reads: Vec<PathBuf>,
    /// User-granted read-write paths (canonical).
    pub granted_writes: Vec<PathBuf>,
    /// User-granted network.
    pub network: bool,
}

/// Is `dir` too broad to hand a child as a readable subtree?
///
/// Three fail-closed rules, all about not letting a tidy heuristic
/// (`.../bin/x` → grandparent) silently become "read everything":
/// * it is the user's home, or an ancestor of it (`/`, `/Users`) — the exact
///   thing the deny-default exists to protect;
/// * it sits at or above the top level (`/`, `/usr`, `/opt`, `/bin`) — a
///   top-level directory is never one program's install tree;
/// * it is the per-user temp root, where every process on the box drops files.
fn is_too_broad(dir: &Path) -> bool {
    if dir.components().count() <= 2 {
        // `/` is 1 component (RootDir); `/usr` is 2. `/usr/local` is 3.
        return true;
    }
    if let Some(home) = home_dir() {
        // `home.starts_with(dir)` is true for the home dir itself AND for every
        // ancestor of it, which is precisely the set we must refuse.
        if home.starts_with(dir) {
            return true;
        }
    }
    if let Ok(tmp) = std::fs::canonicalize(std::env::temp_dir()) {
        if tmp == dir {
            return true;
        }
    }
    false
}

/// The user's home directory, canonicalized. `None` when `HOME` is unset or
/// unresolvable — [`is_too_broad`]'s other two rules still apply, so a missing
/// home only removes one of three guards.
fn home_dir() -> Option<PathBuf> {
    let raw = std::env::var_os("HOME")?;
    std::fs::canonicalize(raw).ok()
}

/// The readable tree an EXECUTABLE needs: its install root when it lives in a
/// `bin/`, otherwise its own directory, otherwise (both too broad) just the file.
///
/// The `bin/` hop is what makes real servers start. A Homebrew `node` is
/// `<prefix>/bin/node` and links against `<prefix>/opt/libuv/lib/libuv.1.dylib`;
/// a python.org `python3` is `<framework>/bin/python3` with its stdlib and
/// site-packages under `<framework>/lib`. Granting only the `bin/` directory
/// makes both fail at `dyld`/import time, which is a broken product, not
/// hardening.
pub fn executable_read_grant(exe: &Path) -> ReadGrant {
    let Some(parent) = exe.parent() else {
        return ReadGrant::Literal(exe.to_path_buf());
    };
    let is_bin_dir = parent
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| BIN_DIR_NAMES.contains(&n));
    if is_bin_dir {
        if let Some(prefix) = parent.parent() {
            if !is_too_broad(prefix) {
                return ReadGrant::Subpath(prefix.to_path_buf());
            }
        }
    }
    if !is_too_broad(parent) {
        return ReadGrant::Subpath(parent.to_path_buf());
    }
    ReadGrant::Literal(exe.to_path_buf())
}

/// The readable tree a SCRIPT argument needs: its own directory (where a Node
/// server's `node_modules` and a Python server's package live), or just the file
/// when that directory is too broad — a `server.js` dropped straight into `$HOME`
/// must not turn into a grant over `$HOME`.
fn script_read_grant(file: &Path) -> ReadGrant {
    match file.parent() {
        Some(dir) if !is_too_broad(dir) => ReadGrant::Subpath(dir.to_path_buf()),
        _ => ReadGrant::Literal(file.to_path_buf()),
    }
}

/// Drop grants already covered by another grant in the list, so the profile
/// carries each subtree once. Purely cosmetic — Seatbelt would union them
/// anyway — but a profile a human can audit is worth the ten lines.
fn dedupe_grants(mut grants: Vec<ReadGrant>) -> Vec<ReadGrant> {
    grants.sort_by(|a, b| a.path().cmp(b.path()));
    grants.dedup();
    let mut out: Vec<ReadGrant> = Vec::with_capacity(grants.len());
    for g in grants {
        let covered = out
            .iter()
            .any(|kept| matches!(kept, ReadGrant::Subpath(_)) && g.path().starts_with(kept.path()));
        if !covered {
            out.push(g);
        }
    }
    out
}

impl McpSandboxSpec {
    /// Derive the containment for one server from its PINNED invocation plus the
    /// user's grants.
    ///
    /// `program` must be the canonical path H-07 resolved and verified;
    /// `command_as_written` is the pre-canonicalization path it came from (e.g.
    /// `/usr/local/bin/node` for a canonical `/usr/local/Cellar/node/…/bin/node`).
    /// BOTH yield an install-tree grant: a Homebrew-style install is a symlink
    /// farm whose keg does not contain the dylibs its own binary loads, so
    /// canonicalizing away the prefix and granting only the keg breaks `dyld`.
    ///
    /// Fails CLOSED: every path is canonicalized here, and a grant that does not
    /// resolve to something real is an error rather than a silently dropped rule.
    pub fn derive(
        program: &Path,
        command_as_written: &Path,
        args: &[String],
        scratch_dir: &Path,
        grants: &McpGrants,
    ) -> Result<Self, String> {
        let program = std::fs::canonicalize(program)
            .map_err(|e| format!("MCP executable `{}` unavailable: {e}", program.display()))?;

        let mut runtime_reads = vec![executable_read_grant(&program)];
        // The as-written path only matters when it differs from the canonical
        // one (a symlink into a versioned keg); otherwise it is the same grant.
        if let Ok(as_written) = std::fs::canonicalize(command_as_written) {
            if as_written != command_as_written {
                // `command_as_written` was itself a symlink chain — the tree we
                // want is the one around the LINK, not around its target.
                runtime_reads.push(executable_read_grant(command_as_written));
            }
        } else if command_as_written.is_absolute() {
            runtime_reads.push(executable_read_grant(command_as_written));
        }
        // Every absolute file argv names is part of the approved invocation
        // (H-07 already hashes its contents), so the child must be able to read
        // it — and the directory it lives in, which is where an interpreter
        // looks for the rest of the server.
        for arg in args {
            if arg.starts_with('-') || !Path::new(arg).is_absolute() {
                continue;
            }
            if let Ok(p) = std::fs::canonicalize(arg) {
                if p.is_file() {
                    runtime_reads.push(script_read_grant(&p));
                }
            }
        }

        let scratch_dir = std::fs::canonicalize(scratch_dir).map_err(|e| {
            format!(
                "MCP scratch dir `{}` unavailable: {e}",
                scratch_dir.display()
            )
        })?;

        let canon_all = |paths: &[PathBuf], what: &str| -> Result<Vec<PathBuf>, String> {
            paths
                .iter()
                .map(|p| {
                    std::fs::canonicalize(p).map_err(|e| {
                        format!("granted {what} path `{}` unavailable: {e}", p.display())
                    })
                })
                .collect()
        };

        Ok(Self {
            program,
            args: args.to_vec(),
            granted_reads: canon_all(&grants.read_paths, "read")?,
            granted_writes: canon_all(&grants.write_paths, "write")?,
            runtime_reads: dedupe_grants(runtime_reads),
            scratch_dir,
            network: grants.network,
        })
    }

    /// Every path the profile mentions — the set whose ancestors need
    /// `file-read-metadata` (see [`build_mcp_seatbelt_profile`]).
    fn all_allowed_paths(&self) -> Vec<&Path> {
        let mut v: Vec<&Path> = self.runtime_reads.iter().map(|g| g.path()).collect();
        v.push(&self.scratch_dir);
        v.extend(self.granted_reads.iter().map(|p| p.as_path()));
        v.extend(self.granted_writes.iter().map(|p| p.as_path()));
        v
    }
}

/// The child's environment: the SAME allowlist scrub as before (a registered
/// server is third-party code and never sees the app's provider keys), with
/// `HOME` and `TMPDIR` REDIRECTED into the private scratch dir.
///
/// The redirect is containment, not convenience. A server that writes a cache,
/// a config, or a lockfile writes it somewhere — with the real `HOME` the
/// deny-default would simply make every such server crash on `EPERM`, and the
/// pressure would be to grant `$HOME`. Pointing `HOME` at the scratch island
/// makes "well-behaved server" and "touches none of the user's files" the same
/// thing.
pub fn scrubbed_env(scratch_dir: &Path) -> Vec<(String, OsString)> {
    let mut env: Vec<(String, OsString)> = Vec::new();
    for key in ["PATH", "USER", "LANG"] {
        if let Some(value) = std::env::var_os(key) {
            env.push((key.to_string(), value));
        }
    }
    env.push(("HOME".to_string(), scratch_dir.as_os_str().to_owned()));
    env.push((
        "TMPDIR".to_string(),
        scratch_dir.join("tmp").as_os_str().to_owned(),
    ));
    env
}

/// Create (idempotently) the private scratch dir for one server id under
/// `root`, plus its `tmp/`. Returns the directory to hand [`McpSandboxSpec::derive`].
pub fn ensure_scratch_dir(root: &Path, server_id: &str) -> Result<PathBuf, String> {
    // The id is a UUID we minted, but it reaches here from a DB row — refuse
    // anything that could climb out of the root rather than trusting it.
    if server_id.is_empty()
        || server_id.contains('/')
        || server_id.contains('\\')
        || server_id.contains("..")
    {
        return Err(format!(
            "refusing to build an MCP scratch dir for the unsafe server id `{server_id}`"
        ));
    }
    let dir = root.join(server_id);
    std::fs::create_dir_all(dir.join("tmp"))
        .map_err(|e| format!("creating MCP scratch dir `{}`: {e}", dir.display()))?;
    // Canonical from here on: the profile, the child's `HOME`, and its `getcwd`
    // must all agree on one spelling, and Seatbelt matches the real path.
    std::fs::canonicalize(&dir)
        .map_err(|e| format!("resolving MCP scratch dir `{}`: {e}", dir.display()))
}

// ── the Seatbelt profile ────────────────────────────────────────────────────

/// Build the Seatbelt profile for one MCP child.
///
/// Shape notes, all of them load-bearing and all verified against a real
/// Homebrew `node`, a real `python3` and `/bin/sh` on macOS 15:
/// * `(import "system.sb")` must stay — without it `sandbox-exec` SIGABRTs
///   before the child ever runs (see [`super::exec`]).
/// * `file-read-metadata` is granted on the ANCESTORS of every allowed path.
///   Node's module loader `realpath()`s its entry point, which `lstat()`s each
///   component from `/` down; with a bare `(deny default)` that fails at
///   `/private` and the server dies before the handshake. Metadata is not
///   content: `stat` on `/Users/you` still cannot list or read it (verified).
/// * network is a single all-or-nothing `(allow network*)`, present only when
///   the user granted it.
#[cfg(target_os = "macos")]
pub fn build_mcp_seatbelt_profile(spec: &McpSandboxSpec) -> Result<String, String> {
    use super::exec::{seatbelt_escape_path, SYSTEM_READ_SUBPATHS};

    use super::exec::ExecError;
    let escape = |p: &Path| -> Result<String, String> {
        seatbelt_escape_path(p).map_err(|e| match e {
            ExecError::SandboxApply(m) | ExecError::Io(m) => m,
        })
    };

    let mut p = String::from(
        "(version 1)\n\
         (deny default)\n\
         (import \"system.sb\")\n\
         (allow process-exec)\n\
         (allow process-fork)\n\
         (allow signal (target self))\n\
         (allow sysctl-read)\n",
    );

    // 1. System runtime paths — the same set a sandboxed shell command gets.
    p.push_str("(allow file-read*\n");
    for sub in SYSTEM_READ_SUBPATHS {
        p.push_str(&format!("\x20   (subpath \"{sub}\")\n"));
    }
    p.push_str(")\n");
    p.push_str("(allow file-write-data (literal \"/dev/null\") (literal \"/dev/tty\"))\n");

    // 2. What the child needs to RUN: install tree(s) + the script files argv
    //    names. Read-only — a server may not rewrite its own approved code.
    if !spec.runtime_reads.is_empty() {
        p.push_str("(allow file-read*\n");
        for g in &spec.runtime_reads {
            match g {
                ReadGrant::Subpath(dir) => {
                    p.push_str(&format!("\x20   (subpath \"{}\")\n", escape(dir)?))
                }
                ReadGrant::Literal(f) => {
                    p.push_str(&format!("\x20   (literal \"{}\")\n", escape(f)?))
                }
            }
        }
        p.push_str(")\n");
    }

    // 3. The private island: read-write, and the child's HOME/TMPDIR.
    p.push_str(&format!(
        "(allow file-read* file-write*\n\x20   (subpath \"{}\")",
        escape(&spec.scratch_dir)?
    ));
    // 4. User-granted read-write paths ride the same rule.
    for dir in &spec.granted_writes {
        p.push_str(&format!("\n\x20   (subpath \"{}\")", escape(dir)?));
    }
    p.push_str(")\n");

    // 5. User-granted read-only paths.
    if !spec.granted_reads.is_empty() {
        p.push_str("(allow file-read*\n");
        for dir in &spec.granted_reads {
            p.push_str(&format!("\x20   (subpath \"{}\")\n", escape(dir)?));
        }
        p.push_str(")\n");
    }

    // 6. Ancestor traversal — metadata only, never content. See the doc comment.
    let mut ancestors: Vec<PathBuf> = Vec::new();
    for path in spec.all_allowed_paths() {
        let mut cur = path;
        while let Some(parent) = cur.parent() {
            if !ancestors.iter().any(|a| a == parent) {
                ancestors.push(parent.to_path_buf());
            }
            cur = parent;
        }
    }
    if !ancestors.is_empty() {
        ancestors.sort();
        p.push_str("(allow file-read-metadata\n");
        for a in &ancestors {
            p.push_str(&format!("\x20   (literal \"{}\")\n", escape(a)?));
        }
        p.push_str(")\n");
    }

    if spec.network {
        p.push_str("(allow network*)\n");
    }
    Ok(p)
}

// ── the spawn ───────────────────────────────────────────────────────────────

/// A ready-to-spawn, already-contained command plus the profile file that has
/// to outlive the spawn. The caller only adds stdio wiring — there is no
/// constructor here that hands back an unconfined `Command`.
pub struct SandboxedCommand {
    pub command: tokio::process::Command,
    /// The Seatbelt profile on disk. `sandbox-exec` reads it at startup; the
    /// transport deletes it at shutdown (deleting it here would race that read).
    pub profile_path: PathBuf,
}

/// Wrap `spec` in the platform sandbox and return the command to spawn.
///
/// macOS: `sandbox-exec -f <profile> <pinned executable> <args…>`. The pinned
/// canonical path is exec'd directly (no `sh -c`), so argv reaches the child
/// verbatim and the binary that runs is the binary H-07 hashed.
#[cfg(target_os = "macos")]
pub fn sandboxed_command(spec: &McpSandboxSpec) -> Result<SandboxedCommand, String> {
    let profile = build_mcp_seatbelt_profile(spec)?;
    // The profile lives inside the private per-server scratch dir (GLM LOW-1: avoids
    // a TOCTOU swap in shared /tmp). sandbox-exec reads it before the sandbox takes effect.
    let profile_path = spec
        .scratch_dir
        .join(format!("lhp-mcp-sandbox-{}.sb", uuid::Uuid::new_v4()));
    std::fs::write(&profile_path, profile.as_bytes())
        .map_err(|e| format!("writing the MCP sandbox profile: {e}"))?;

    let mut command = tokio::process::Command::new("/usr/bin/sandbox-exec");
    command
        .arg("-f")
        .arg(&profile_path)
        .arg(&spec.program)
        .args(&spec.args)
        // cwd is the private scratch dir: somewhere the child can actually
        // `getcwd()` and write, and never the app's own working directory.
        .current_dir(&spec.scratch_dir)
        .env_clear();
    for (k, v) in scrubbed_env(&spec.scratch_dir) {
        command.env(k, v);
    }
    Ok(SandboxedCommand {
        command,
        profile_path,
    })
}

/// The no-backend platforms. Every call hard-errors, so an MCP server stays
/// REGISTERED (the user can see and remove it) but can never run unconfined —
/// the same fail-closed shape as [`super::exec::UnsupportedSandbox`].
#[cfg(not(target_os = "macos"))]
pub fn sandboxed_command(_spec: &McpSandboxSpec) -> Result<SandboxedCommand, String> {
    Err(unsupported_platform_message())
}

/// The refusal text for a platform with no MCP containment backend. Split out
/// so the message is one string on every platform and can be asserted on from a
/// macOS test run as well as a Linux one — which is exactly why the macOS build
/// has no non-test caller for it.
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub fn unsupported_platform_message() -> String {
    "refusing to start a local MCP server: this platform has no process-containment \
     backend yet, and an MCP stdio child is never run unconfined"
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("lhp-mcpsb-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::canonicalize(&d).unwrap()
    }

    #[test]
    fn a_bin_dir_grants_the_install_tree_not_just_the_bin() {
        // The crux for real servers: `<prefix>/bin/node` must yield `<prefix>`,
        // or dyld can't find `<prefix>/opt/.../libuv.dylib`.
        let root = tmp_dir("tree");
        let exe = root.join("prefix/bin/node");
        assert_eq!(
            executable_read_grant(&exe),
            ReadGrant::Subpath(root.join("prefix")),
        );
    }

    #[test]
    fn a_non_bin_dir_grants_only_the_executables_own_directory() {
        let root = tmp_dir("tree2");
        let exe = root.join("srv/my-server");
        assert_eq!(
            executable_read_grant(&exe),
            ReadGrant::Subpath(root.join("srv")),
        );
    }

    #[test]
    fn a_shallow_or_home_prefix_never_becomes_a_subtree_grant() {
        // /bin/sh: the bin-hop would land on "/" — refused, and /bin itself is
        // a top-level directory, so the grant narrows to the file.
        assert_eq!(
            executable_read_grant(Path::new("/bin/sh")),
            ReadGrant::Literal(PathBuf::from("/bin/sh")),
        );
        // An executable sitting directly in $HOME must not grant $HOME.
        if let Some(home) = home_dir() {
            assert_eq!(
                executable_read_grant(&home.join("rogue-server")),
                ReadGrant::Literal(home.join("rogue-server")),
                "an executable in $HOME must never grant $HOME"
            );
            assert!(is_too_broad(&home), "the home dir is always too broad");
            assert!(
                is_too_broad(home.parent().unwrap()),
                "an ancestor of home is always too broad"
            );
        }
        assert!(is_too_broad(Path::new("/")));
        assert!(is_too_broad(Path::new("/usr")));
        assert!(!is_too_broad(Path::new("/usr/local")));
    }

    #[test]
    fn a_script_in_a_broad_directory_grants_only_the_file() {
        let root = tmp_dir("script");
        let dir = root.join("mcp-server");
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("server.js");
        std::fs::write(&script, "//").unwrap();
        assert_eq!(script_read_grant(&script), ReadGrant::Subpath(dir));

        if let Some(home) = home_dir() {
            assert_eq!(
                script_read_grant(&home.join("server.js")),
                ReadGrant::Literal(home.join("server.js")),
            );
        }
    }

    #[test]
    fn derive_defaults_to_no_network_and_no_user_paths() {
        let root = tmp_dir("derive");
        let bin = root.join("prefix/bin");
        std::fs::create_dir_all(&bin).unwrap();
        let exe = bin.join("srv");
        std::fs::write(&exe, "#!/bin/sh\n").unwrap();
        let scratch = ensure_scratch_dir(&root, "server-1").unwrap();

        let spec =
            McpSandboxSpec::derive(&exe, &exe, &[], &scratch, &McpGrants::default()).unwrap();
        assert!(!spec.network, "deny-default: no network without a grant");
        assert!(spec.granted_reads.is_empty());
        assert!(spec.granted_writes.is_empty());
        assert_eq!(
            spec.runtime_reads,
            vec![ReadGrant::Subpath(root.join("prefix"))],
        );
        assert!(scratch.join("tmp").is_dir(), "TMPDIR must exist");
    }

    #[test]
    fn derive_rejects_a_grant_that_does_not_exist() {
        let root = tmp_dir("derive2");
        let exe = root.join("srv");
        std::fs::write(&exe, "#!/bin/sh\n").unwrap();
        let scratch = ensure_scratch_dir(&root, "server-2").unwrap();
        let grants = McpGrants {
            network: false,
            read_paths: vec![root.join("nope")],
            write_paths: vec![],
        };
        let err = McpSandboxSpec::derive(&exe, &exe, &[], &scratch, &grants)
            .expect_err("a grant that does not resolve must fail closed");
        assert!(err.contains("granted read path"), "got: {err}");
    }

    #[test]
    fn scratch_dir_ids_that_could_climb_out_are_refused() {
        let root = tmp_dir("scratch");
        for bad in ["../escape", "a/b", ""] {
            assert!(
                ensure_scratch_dir(&root, bad).is_err(),
                "`{bad}` must be refused"
            );
        }
        let ok = ensure_scratch_dir(&root, "abc-123").unwrap();
        assert!(ok.starts_with(&root));
    }

    #[test]
    fn the_child_environment_redirects_home_into_the_scratch_island() {
        let scratch = tmp_dir("env");
        let env = scrubbed_env(&scratch);
        let get = |k: &str| {
            env.iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.clone())
                .unwrap()
        };
        assert_eq!(get("HOME"), scratch.as_os_str());
        assert_eq!(get("TMPDIR"), scratch.join("tmp").as_os_str());
        assert!(
            env.iter()
                .all(|(k, _)| ["PATH", "USER", "LANG", "HOME", "TMPDIR"].contains(&k.as_str())),
            "only the allowlisted keys may reach an MCP child"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_profile_denies_by_default_and_omits_network_without_a_grant() {
        let root = tmp_dir("profile");
        let exe = root.join("srv");
        std::fs::write(&exe, "#!/bin/sh\n").unwrap();
        let scratch = ensure_scratch_dir(&root, "s").unwrap();
        let spec =
            McpSandboxSpec::derive(&exe, &exe, &[], &scratch, &McpGrants::default()).unwrap();
        let p = build_mcp_seatbelt_profile(&spec).unwrap();
        assert!(p.contains("(deny default)"));
        assert!(p.contains("(import \"system.sb\")"));
        assert!(!p.contains("(allow network*)"), "profile: {p}");
        assert!(p.contains(&format!("(subpath \"{}\")", scratch.display())));
        // Ancestor traversal is metadata-only — never a read of the ancestor.
        assert!(p.contains("(allow file-read-metadata"), "profile: {p}");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_profile_carries_network_and_granted_paths_when_granted() {
        let root = tmp_dir("profile2");
        let exe = root.join("srv");
        std::fs::write(&exe, "#!/bin/sh\n").unwrap();
        let scratch = ensure_scratch_dir(&root, "s").unwrap();
        let readable = root.join("readable");
        let writable = root.join("writable");
        std::fs::create_dir_all(&readable).unwrap();
        std::fs::create_dir_all(&writable).unwrap();
        let spec = McpSandboxSpec::derive(
            &exe,
            &exe,
            &[],
            &scratch,
            &McpGrants {
                network: true,
                read_paths: vec![readable.clone()],
                write_paths: vec![writable.clone()],
            },
        )
        .unwrap();
        let p = build_mcp_seatbelt_profile(&spec).unwrap();
        assert!(p.contains("(allow network*)"));
        assert!(p.contains(&format!("(subpath \"{}\")", readable.display())));
        assert!(p.contains(&format!("(subpath \"{}\")", writable.display())));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_path_that_cannot_be_escaped_fails_closed() {
        // A newline in a path would let a crafted grant inject its own rules.
        // `derive` canonicalizes, so build the spec by hand to reach the escaper.
        let scratch = tmp_dir("escape");
        let spec = McpSandboxSpec {
            program: PathBuf::from("/bin/sh"),
            args: vec![],
            scratch_dir: scratch,
            runtime_reads: vec![ReadGrant::Subpath(PathBuf::from(
                "/tmp/evil\n(allow default)",
            ))],
            granted_reads: vec![],
            granted_writes: vec![],
            network: false,
        };
        assert!(
            build_mcp_seatbelt_profile(&spec).is_err(),
            "a control character in a path must never reach the profile"
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn no_backend_platform_refuses_to_spawn() {
        let scratch = tmp_dir("nobackend");
        let spec = McpSandboxSpec {
            program: PathBuf::from("/bin/sh"),
            args: vec![],
            scratch_dir: scratch,
            runtime_reads: vec![],
            granted_reads: vec![],
            granted_writes: vec![],
            network: false,
        };
        let err = sandboxed_command(&spec).expect_err("no backend must never spawn");
        assert_eq!(err, unsupported_platform_message());
    }

    #[test]
    fn the_unsupported_platform_refusal_never_suggests_running_unconfined() {
        let m = unsupported_platform_message();
        assert!(m.contains("refusing to start"), "got: {m}");
        assert!(m.contains("never run unconfined"), "got: {m}");
    }
}
