# M7 / 5.4 — Per-profile isolation + OS sandbox (design)

> **STATUS: design-pass, revised (2026-07-18). Skeptical review verdict was NEEDS-REVISION; addressed in `## Revision v2` at the bottom — READ THAT LAST.** The v2 section supersedes the original where they conflict: it restates INV-M7b as two honest enforcement tiers, re-roots `ProtectedPathHook` per profile, splits the kernel backends into their own spike-gated milestones, pulls MCP-child confinement out of scope, adds Email/Calendar/Tasks to `app_default`, and closes Q4. Build-ready for the macOS-first path (Slices 1/2/4a/5/6); Linux/Windows and Q1–Q3 remain open. Read the original §§1–7, then the Design Review, then Revision v2.


Status: DESIGN (Wave-5 flagship, design-pass-first per BUILD-MANIFEST §"Scope honesty").
Author pass: 2026-07-18. Owner decisions needed: see §7 (4 open).

Scope source: BUILD-MANIFEST row 5.4 + PLAN §8 M7 + §6 (from-scratch) + §7/§9 (walled
principle) + §12 item 4 (permission↔sandbox recheck). Capability Packs are **not** in
this item — they already landed (Wave 4.5, `src-tauri/src/packs/mod.rs`).

---

## 1. Goal · scope · non-goals

**Goal.** Make a profile a real isolation boundary at three layers that today are
partial or fake: (a) **storage** — email/calendar/tasks live per-profile, physically,
the way walled memory already does (§7); (b) **defaults** — activating a profile seeds
its memory/seat/permission/sandbox defaults; (c) **execution** — replace the *dead*
`SandboxConfig` passthrough with **real OS-kernel confinement**, per-profile, on all
three platforms, so a tool running under profile P cannot touch profile Q's files, or
the wider machine, even if every in-process check is bypassed by a bug.

**In scope.**
1. Per-profile physical workspace/tmp roots (replaces the single shared `workspace/`).
2. `SandboxConfig` (dead struct, `hooks/sandbox.rs:142`) wired to live per-profile OS
   enforcement, driven from the active profile.
3. Real OS sandbox backends: macOS Seatbelt (generalize existing), Linux
   (Landlock+seccomp / bubblewrap), Windows (AppContainer + Job Object).
4. Confinement extended past `shell_exec` to: MCP stdio child processes; per-profile
   `network.allowed_domains` egress bound on `fetch`; per-profile re-rooting of the fs
   tools' confinement.
5. Profile-activation default seeding (memory/seat/permission/sandbox), incl. a
   **server-flavored** seed variant (server-track prep).
6. Email/calendar/tasks per-profile subsystem + wire the visual-only Email screen.
7. PLAN §12 item 4: a permission↔sandbox reconciliation invariant + test suite.

**Non-goals.**
- Separate OS *user account* per profile (heavier than warranted — see §7 default).
- Real email/calendar **sync** protocol work (IMAP/CalDAV/OAuth) — the *subsystem shape*
  + local store + the Email screen wiring is in scope; live provider sync is deferred
  (§7 Q2 gates it).
- VM/container isolation (`Virtualization.framework`/`Containerization`) — the durable
  target already named in `exec.rs:18`; this item stays at kernel-sandbox tier.
- The gating `SandboxHook` denylist (`hooks/sandbox.rs:32`) is **not** the thing being
  replaced — it already enforces. The "v1 no-op passthrough" is the *config-driven OS
  layer* (`SandboxConfig`), which today enforces nothing.

---

## 2. Architecture

### 2.1 The two "sandbox" layers today (and which one is fake)

There are deliberately two, neither subsuming the other (`exec.rs:6-10`):

| Layer | Where | Status today |
|---|---|---|
| Gating denylist | `hooks/sandbox.rs::SandboxHook` (`:113`) in the PreToolUse chain | **Real.** Hardline `rm -rf /`, curl\|sh, etc. Non-overridable floor. Keep as-is. |
| Process containment | `tools/exec.rs::SandboxedSpawn` + `MacSeatbeltSpawn` (`:86`,`:301`) | **Real but narrow.** macOS-only, `shell_exec`-only, hardcoded workspace + per-call `network:bool`. |
| **Config-driven OS enforcement** | `hooks/sandbox.rs::SandboxConfig` (`:142`) | **DEAD.** `enabled`, `auto_allow_if_sandboxed`, `excluded_commands`, `network.allowed_domains` are consumed by **nothing**. This is the "v1 no-op passthrough" M7 replaces. |

M7 does **not** rebuild the first row. It (a) makes `SandboxConfig` live and
per-profile, (b) generalizes the second row's `SandboxedSpawn` to all platforms and to
every child-process spawn (not just `shell_exec`), and (c) binds both to the active
profile's physical roots.

### 2.2 New module: `src-tauri/src/platform/sandbox/`

The `platform/` tree (`platform/mod.rs`, cfg-split macos/windows/linux) is the natural
home — it already exists for M5 computer-use and is empty today. Add:

```
platform/sandbox/
  mod.rs        // ProfileConfinement trait + ConfinementSpec + selection
  macos.rs      // Seatbelt backend (lift MacSeatbeltSpawn here, generalized)
  linux.rs      // Landlock+seccomp (+ optional bubblewrap) backend
  windows.rs    // AppContainer + restricted token + Job Object backend
```

`tools/exec.rs::SandboxedSpawn` is **renamed/relocated** to
`platform::sandbox::ProfileConfinement` and widened so it isn't shell-specific:

```rust
/// A per-profile OS jail. One instance per (profile, backend); cheap to hold.
pub trait ProfileConfinement: Send + Sync {
    /// Spawn `argv` already contained to `spec`. MUST return Err(Confinement::Apply)
    /// — never a bare Command::new — if the jail can't be built/applied.
    fn spawn(&self, spec: &ConfinementSpec) -> Result<(tokio::process::Child, Vec<PathBuf>), ConfinementError>;
    /// True if this backend actually enforces on the running kernel (probe at
    /// startup: Landlock ABI present? bwrap on PATH? AppContainer supported?).
    fn is_enforcing(&self) -> bool;
}

pub struct ConfinementSpec {
    pub argv: Vec<String>,          // program + args (was: `command: String`)
    pub profile_root: PathBuf,      // the profile's own workspace root (rw)
    pub tmp_root: PathBuf,          // the profile's own tmp scratch (rw)
    pub ro_paths: Vec<PathBuf>,     // system read paths (/usr,/bin,…)
    pub allowed_domains: Vec<String>, // from SandboxConfig.network; [] = no net
    pub allow_localhost: bool,
    pub timeout: Duration,
}
```

`ExecSpec` (`exec.rs:45`) collapses into `ConfinementSpec`. `run_guarded`
(`exec.rs:224`) — the timeout/output-cap/process-group-kill/fail-closed engine — stays
almost verbatim; it already treats an apply failure as a hard `Err` with **no**
fall-through to an unsandboxed spawn (`exec.rs:229-231`), which is the invariant we must
preserve verbatim.

### 2.3 Per-profile physical roots (the biggest structural change)

Today `build_tool_dispatcher` (`lib.rs:449`) creates **one shared**
`base_path/workspace` and one `base_path/tmp`, wired into every fs tool and the shell
spawner, and the dispatcher is built **once** and shared across all profiles
(`lib.rs:178`). So profiles share a filesystem today — isolation is a fiction.

New layout (mirrors walled-memory's `walled-memory/<name>.db`, `storage/mod.rs:160`):

```
<base>/profiles/<name>/workspace/   ← fs tools + shell_exec rw root for profile <name>
<base>/profiles/<name>/tmp/
<base>/profiles/<name>.db            ← unchanged (ProfileDb)
```

Because the dispatcher is a singleton but the workspace is now per-profile, the
per-profile root must be resolved **at call time** from `ExecCtx.profile`
(`tools/mod.rs:211`, already threaded — `dispatch.rs:313`,`:485` set it), not baked into
each tool at construction. Two options; recommend **B**:

- **A.** One dispatcher per profile. Rejected: multiplies the singleton, complicates the
  shared `ApprovalLedger`/audit wiring in `lib.rs:428-622`.
- **B.** Tools/spawner take a `WorkspaceResolver` (`fn root_for(profile) -> PathBuf`)
  instead of a fixed `PathBuf`; `ExecCtx.profile` selects the root inside `run()`. The
  `resolve_within`/`resolve_within_new` confinement (`fs.rs:39`,`:338`) then anchors to
  the per-profile root. `ShellExecTool` (`exec.rs:411`) passes the resolved root into
  `ConfinementSpec.profile_root`. Empty profile (tests/default `ExecCtx`) → a scratch
  root, consistent with existing default-ctx handling.

### 2.4 How execution slots into the spine (unchanged control flow)

The hook chain is untouched in *ordering* — the OS sandbox is **below** it, at spawn
time, exactly as `exec.rs:6-10` documents. Flow for a `shell_exec`/MCP-child call:

```
PreToolUse chain (hooks/mod.rs:467 build_pretooluse_chain_full)
  PrivacyFilter → SandboxHook(denylist) → ProtectedPath → SessionMode → Permission → FirstUse
     └─ any Deny/Ask short-circuits; call never reaches spawn
Tool::run()  (only if chain returned Continue)
  └─ ProfileConfinement::spawn(ConfinementSpec{profile_root = root_for(ctx.profile), …})
        └─ run_guarded: fail-closed, timeout, kill-group, output caps
```

The OS jail can only make an *already-permitted* call **more** restricted (deny a write
the permission gate happened to allow); it can never turn a chain `Deny` into a run.
That directionality is the flagship invariant (§3).

### 2.5 Capabilities / RiskClass mapping

`Capability` (`tools/mod.rs:48`) already has `Email`, `Calendar`. Additions:
- Add `Capability::Tasks` (local task/todo store). `app_default` (`tools/mod.rs:105`) and
  `headless_server_default` (`tools/mod.rs:120`) gain the new caps as appropriate.
- No new capability for the sandbox itself — confinement is not a tool.

New tools' RiskClass:
- `email_send`, `email_search`/`email_read` → **External** (reach a mail account;
  `destination()` = recipient/account, surfaced in approval, like `fetch` `:87`).
- `calendar_*` → **External** for anything that hits a remote calendar, **Write** for a
  purely local store.
- `task_*` (local todo) → **Write** (create/complete) / **Safe** (list).

All route through the existing spine automatically via the risk→policy derivation in
`lib.rs:573-583` (Safe pre-trusted; Write/External/Dangerous → approval spine). No new
gating code.

### 2.6 Profile-activation default seeding

The `AppLaunch` hook event (`hooks/mod.rs:123`) is reserved *exactly* for "per-profile
default seeding (memory tags, seats, permissions)" but is never fired. M7 introduces a
`ProfileActivated` moment (app boot for the default profile; profile switch when the UI
cycle-chip lands — `get_active_profile` is still a stub, `ipc/mod.rs:251`). A
`seed_profile_defaults(storage, profile, seed: SeedProfile)` runs idempotently on first
activation and seeds four things into `ProfileDb`:

1. **memory** — `memory_settings` row (`walled` default false; migration exists,
   `storage/migrations.rs:271`).
2. **seat** — no-op default (`resolve_seat` already inherits when unbound,
   `models/seat.rs:36`); server seed may pin forced-local seats.
3. **permission** — `tool_rules` defaults (`storage/migrations.rs:225`) per `SeedProfile`.
4. **sandbox** — a new per-profile `sandbox_config` row (see §2.7), `enabled = true`.

`SeedProfile::{App, Server}` is the server-flavored seeding: the `Server` variant
seeds a conservative unattended set (no human present to answer an `Ask`) — see §7 Q4.

### 2.7 Storage additions (per-profile, physical — the §7 principle)

New `PROFILE_MIGRATIONS` (`storage/migrations.rs:182`) entries in the **profile** DB
(never `global.db` — that is the whole walled point):

- `sandbox_config` (id=1 singleton row): `enabled`, `auto_allow_if_sandboxed`,
  `allow_localhost`, JSON `allowed_domains`, JSON `excluded_commands`. Serializes
  `SandboxConfig` (`hooks/sandbox.rs:142`). `ProfileDb::{get,set}_sandbox_config`.
- `emails`, `calendar_events`, `tasks` tables (+ FTS as memory does). Profile-scoped by
  construction — a personal-profile email row is unreachable from the work profile
  because it is a different DB file, not a filtered query. Same guarantee walled memory
  gives (`storage/mod.rs:117-136`).

---

## 3. Flagship-specific invariant (the NEW one, per PLAN §6/§12)

Each Wave-5 flagship introduces one new privacy/security invariant. For M7 it is
**confinement-under-permission + physical profile separation**, stated as two
non-negotiables and one reconciliation:

> **INV-M7a (floor, never ceiling).** The OS sandbox may only ever *narrow* what a call
> can do relative to the permission gate's decision. It is structurally impossible for
> the sandbox layer to permit an action the hook chain denied, or to widen a grant.
> Concretely: the sandbox is consulted only *after* `run_gating` returns `Continue`
> (§2.4); a jail-apply failure is a hard tool error with no unsandboxed fall-through
> (`exec.rs:229-231`, `:66-75`); and on a platform with no enforcing backend,
> `is_enforcing()==false` makes the confinement hard-**deny** every process spawn
> (today's `UnsupportedSandbox` behavior, `exec.rs:392`), so "no sandbox" fails closed,
> never open.

> **INV-M7b (physical, not a filter — extends §7 walled memory to execution + data).**
> A tool executing under profile P touches only P's `workspace/`, P's `tmp/`, P's
> `.db`, and P's `allowed_domains`, enforced by the kernel and by physically separate
> files — so the boundary holds even if the in-process path-confinement
> (`resolve_within`) or the hook chain is bypassed by a bug. This is the "the separation
> is physical, not a query filter" rule (PLAN §7, `storage/mod.rs:120`) applied to
> filesystem, process, and email/calendar/task data — not just memory.

> **INV-M7c (PLAN §12 item 4 — reconcile once it lands).** A test suite proves the
> sandbox denies a **superset** of what the permission gate denies: for a representative
> matrix of (tool, path, network) calls, `permission_allows(call) ⇒ sandbox may still
> deny; permission_denies(call) ⇒ call never spawns`. No cell where the sandbox is the
> *only* thing standing between an agent and an out-of-profile resource is allowed to
> depend on an in-process check the sandbox doesn't also back.

INV-M7b is the genuinely new bit vs. the shipped invariants (walled memory covered
*data*; local-routing covered *model egress*; this covers *execution + PIM data*).

---

## 4. Cross-platform strategy

Same `ProfileConfinement` trait, one backend each. All must pass the identical behavior
suite already written for macOS (`exec.rs:698-748`): in-workspace write succeeds,
out-of-workspace write denied, network-off blocks egress — re-run per platform in CI.

**macOS — Seatbelt (`sandbox-exec`).** Lift `build_seatbelt_profile` (`exec.rs:357`) into
`platform/sandbox/macos.rs`, parameterize the `subpath` rw rules by
`spec.profile_root`/`tmp_root` (was fixed), and derive the `(allow network*)` line from
`spec.allowed_domains` (empty ⇒ no net; non-empty ⇒ allow net — Seatbelt can't filter by
domain, so domain-granularity is enforced in-process for `fetch`, kernel-coarse for
child processes: net-on/net-off). Keep the `(import "system.sb")` gotcha
(`exec.rs:298-300`) and the SIGABRT/exit-65 apply-failure detection (`exec.rs:134`).

**Linux — Landlock + seccomp, bubblewrap optional (see §7 Q1).** Default backend:
Landlock LSM (kernel ≥5.13) to restrict the child's filesystem to `profile_root`+`tmp`+
`ro_paths`, plus a seccomp-bpf filter to drop dangerous syscalls, plus a network
namespace (`unshare(CLONE_NEWNET)` with no veth ⇒ no egress) when `allowed_domains` is
empty. `is_enforcing()` probes the Landlock ABI at startup. Where present and chosen,
`bwrap` gives stronger mount/pid/user-namespace isolation as a superset. The
process-group-kill limitation (`exec.rs:99-113`, a `setsid` child escapes) is closed on
Linux by putting the child in its own **cgroup** (or PID namespace) and killing the
cgroup, not the pgid.

**Windows — AppContainer + restricted token + Job Object.** Spawn the child in a
low-privilege **AppContainer** with a per-profile capability SID and an ACL granting
write only to `profile_root`/`tmp`; deny network by omitting the
`internetClient` capability when `allowed_domains` is empty. Wrap the process in a **Job
Object** with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` — this is the Windows analog that
fixes the `setsid`-escape orphan problem for free (a job kills the whole tree). PLAN §6
calls Windows depth out explicitly as real work, not a bolt-on.

**Fail-closed default.** Until a platform's backend reports `is_enforcing()==true`, that
platform keeps the `UnsupportedSandbox` hard-deny (`exec.rs:392-405`) so `shell_exec`
and MCP children stay registered-but-unrunnable rather than silently unsandboxed.

---

## 5. Build slices (committable, each with a gate)

**Slice 1 — Per-profile physical roots.** Introduce `WorkspaceResolver`; re-root the fs
tools (`fs.rs`) and `ShellExecTool` (`exec.rs:411`) from a fixed `PathBuf` to a
`root_for(ctx.profile)`; create `profiles/<name>/workspace|tmp` on profile open
(`storage/mod.rs:178`); migrate/leave the legacy shared `workspace/`.
*Gate:* a file written by `write_file` under profile A is physically absent from B's
workspace dir and unreadable via B's `read_file`; all existing `cargo test --lib` green.

**Slice 2 — Wire `SandboxConfig` live on macOS.** Rename `SandboxedSpawn`→
`ProfileConfinement`, `ExecSpec`→`ConfinementSpec`; move Seatbelt to
`platform/sandbox/macos.rs`; add `sandbox_config` migration + `ProfileDb` accessors; feed
the active profile's config + per-profile root into the spawn. Keep `run_guarded`
fail-closed semantics byte-for-byte.
*Gate:* `shell_exec` under profile A is OS-denied when it tries to read B's workspace
(not just logically refused); `allowed_domains=[]` blocks egress; an apply failure is a
hard `Err` (existing `hard_errs_when_sandbox_apply_fails`, `exec.rs:562`, still passes).

**Slice 3 — Linux + Windows backends.** Implement `platform/sandbox/linux.rs`
(Landlock+seccomp, cgroup kill) and `windows.rs` (AppContainer + Job Object). CI runs the
three-test behavior suite on each OS.
*Gate:* the workspace-in/out + net-off suite passes on Linux and Windows; `is_enforcing`
gating verified (no path from a non-enforcing backend to a bare spawn); Job-Object /
cgroup kill reaps a `setsid`/detached grandchild (the escape `exec.rs:107` documents).

**Slice 4 — Confinement beyond `shell_exec`.** (a) `fetch` (`tools/fetch.rs`) enforces the
active profile's `allowed_domains` as an allowlist layered **on top of** the existing
SSRF guard (`egress::is_private_endpoint`, `fetch.rs:41`) — never replacing it. (b) When
the MCP stdio transport lands (`mcp.rs:9` is a stub today), its child spawn routes
through `ProfileConfinement`, so an external tool server inherits the profile jail.
*Gate:* a `fetch` to a host outside profile P's `allowed_domains` is refused with a clear
reason; the SSRF localhost/RFC-1918 block still holds; a spawned MCP child cannot write
outside P's workspace.

**Slice 5 — Profile-activation seeding + server flavor.** `seed_profile_defaults` fires on
activation; seeds memory/seat/permission/sandbox defaults idempotently; `SeedProfile::
{App,Server}` variants. Fire the reserved `AppLaunch`/new `ProfileActivated` path.
*Gate:* a fresh profile activation seeds exactly its four defaults once (re-activation is
a no-op); `Server` seed yields the conservative unattended permission set (§7 Q4);
switching profiles re-resolves memory/seat/sandbox to the new profile.

**Slice 6 — Email/calendar/tasks + Email screen.** `emails`/`calendar_events`/`tasks`
profile tables; `email_*`/`calendar_*`/`task_*` tools with correct RiskClass (§2.5)
routing through the existing spine; wire the visual-only Email screen (`src/lib/design/`)
to live per-profile data.
*Gate:* the Email screen shows profile A's mail and, on switching to B, shows B's (or
empty) with no A rows leaking; email/calendar tools trigger the approval dialog as
External; INV-M7b holds for PIM data (physical separation test).

**Cross-cutting — INV-M7c reconciliation suite** (land alongside Slices 2–3, finalize in
6): the permission↔sandbox superset test matrix (§3). This is the concrete discharge of
PLAN §12 item 4.

---

## 6. `--no-default-features` / local-first impact

- **The OS sandbox must never be feature-gated off.** It is security-critical; there is
  no cargo feature that disables confinement. The only "off" is a platform whose backend
  isn't implemented yet, and that path **hard-denies** (fail-closed, §4). A reviewer
  check: no `#[cfg(feature = …)]` may guard the `ProfileConfinement` selection such that
  stripping a feature yields a bare spawn.
- **Per-profile isolation is fully offline.** Filesystem/process confinement and the
  memory/seat/permission/sandbox seeding need zero network — they work in the local-first
  common case (PLAN §6 "offline-as-the-default"). Tasks are local (Write), so the task
  subsystem works with `--no-default-features` too.
- **Email/calendar degrade cleanly.** They ride `Capability::{Email,Calendar,Network}`;
  a local-only build that drops `Network`/`Email` from its `BodyEnv` simply omits those
  tools from `available_tools` (`tools/mod.rs:423`) — absent, not erroring — exactly the
  capability-filter contract M3 established. The Email *screen* renders the local store
  (empty if never synced) regardless.
- Backends use **OS-native facilities only** (Seatbelt, Landlock/seccomp, AppContainer/Job
  Objects) — no new network deps, consistent with the app carrying its own brain (§9).
  bubblewrap is the one *optional external binary* (probed, not required — §7 Q1).

---

## 7. Open questions (genuinely need Lukas) vs. sensible defaults

**Needs a Lukas decision:**

1. **Linux sandbox tech.** Landlock+seccomp (in-process, kernel ≥5.13, no external
   binary, coarse network) vs. bubblewrap (stronger namespacing, but needs `bwrap`
   present and sometimes setuid) vs. both (Landlock default, bwrap when present). This is
   a real distribution/security tradeoff and his homelab is Linux-heavy (TrueNAS apps,
   VMs). *My default if unanswered:* Landlock+seccomp as the guaranteed floor, bubblewrap
   as an opportunistic upgrade.

2. **Email/calendar sync backend.** Native protocols (IMAP/CalDAV/JMAP) vs. per-provider
   OAuth (Gmail/Graph) vs. lean on MCP connectors. Large, unspecced product surface.
   *My default:* build the subsystem + local store + Email screen behind a
   provider-agnostic trait now; defer live sync to its own item.

3. **Walled profile ⇒ network-deny by default?** PLAN §7 says a walled/private profile is
   a "full island … both directions." Should its OS sandbox therefore also default
   `allowed_domains=[]` (hard no-net for its tools) unless the user opts a domain in?
   Security-strong but changes UX. *My default:* yes for `shell_exec`/MCP children; leave
   `fetch` governed by explicit per-profile allowlist.

4. **Server-flavored seeding — which tools auto-allow unattended?** The headless server
   has no human to answer an `Ask`. Which RiskClasses may auto-run there (e.g. Safe+Write
   yes, External/Dangerous never)? This is a product/security call for the server track,
   not in the specs. *My default:* `Server` seed auto-allows Safe only, forces
   `Ask`→deny for everything else until an explicit standing grant exists.

**Sensible defaults (proceeding unless told otherwise):**
- No separate OS *user* per profile — per-workspace + OS sandbox is the boundary.
- Legacy shared `workspace/` is migrated into the default profile's root, not deleted.
- Dispatcher stays a singleton; per-profile root resolved at call time (§2.3 option B).
- `Capability::Tasks` added; tasks are local-only (Write/Safe), no network.
- Reuse `GlobalDb`-shaped storage for the walled email/calendar/tasks the same way
  walled memory reuses it (`storage/mod.rs:79`).


---

## Design Review (skeptical pass, 2026-07-18)

*An independent staff-engineer critique of the design above, grounded in the actual codebase. Address these in the build phase.*

I've verified every load-bearing claim in the design against the actual codebase. Here is my review.

---

## VERDICT: NEEDS-REVISION

Not because it's ungrounded — the opposite. This is an unusually well-researched doc: every file:line reference I spot-checked is accurate (`exec.rs:45/86/301/357/392`, `hooks/sandbox.rs:143`, `lib.rs:573-583`, `tools/mod.rs:210-211`, `storage/mod.rs`, `migrations.rs`, `hooks/mod.rs:123`, `seat.rs:36`, `fetch.rs:41`, `mcp.rs:9` all check out). Existing invariants are preserved, not weakened: the danger-floor denylist is explicitly kept as-is (§2.1 ✓ `hooks/sandbox.rs:32`), the privacy gate and hook ordering are untouched, `shell_exec` stays `Dangerous`/per-call (§2.4 ✓), and §6 *strengthens* the `--no-default-features` rule (no feature may strip confinement — correct; the only real feature is `onnx-classifier`). The revision is needed because the **flagship-new invariant is partly overclaimed** and two slices rest on unbuilt machinery. Details below.

## Top 3 gaps / risks (most severe first)

**1. INV-M7b (§3) overclaims kernel enforcement for the in-process fs tools — and that's the whole new invariant.** It states the P-vs-Q boundary is "enforced by the kernel and by physically separate files … holds even if `resolve_within` … is bypassed by a bug." But the native filesystem tools (`read_file`/`write_file`/`edit_file`/`list_dir`/`search_files`, `tools/fs.rs`) run **in-process in the Tauri app** — the OS jail (`ProfileConfinement`) only wraps *child-process spawns* (`shell_exec`, future MCP children). For every fs-tool file access the sole enforcement is `resolve_within`/`resolve_within_new` (`fs.rs:39`,`:338`), an in-process check, plus physically-separate dirs. So "even if `resolve_within` is bypassed" the kernel does **not** backstop the majority of file-touching operations — the invariant as written is false for them. Either restate INV-M7b honestly (kernel enforcement = child processes only; fs tools = physical dirs + in-process check, i.e. the *same* tier walled memory already gives), or actually route fs I/O through a kernel boundary (not proposed, and arguably not worth it). Because this is the milestone's headline invariant, it must be stated truthfully. Related under-inventory: §2.3 option B re-roots the fs tools + `ShellExecTool` but misses the **`ProtectedPathHook`**, which is built once with a fixed `hook_workspace_root` (`lib.rs:460`, passed at `:617`) and would keep resolving path args against the shared workspace after the per-profile split.

**2. Slice 3 collapses three deep, independent kernel-security backends into one "committable slice."** macOS Seatbelt already exists; Linux (Landlock ABI probe + seccomp-bpf + cgroup/PID-ns kill + net-ns) and Windows (AppContainer + per-profile capability SID + restricted token + ACLs + Job Object) are each a multi-week security effort in domains with **zero existing code** — `platform/{linux,windows}/mod.rs` are 2-line stubs. The gate "behavior suite passes on Linux and Windows in CI" is optimistic: CI does run `cargo test` on all three OSes (`build.yml:83`), but GitHub runners may not let `unshare(CLONE_NEWNET)`, Landlock, or AppContainer actually enforce under their own sandboxing — in which case the `is_enforcing()==false` fail-closed path makes those tests *skip* rather than verify, hollowing out the CI guarantee the slice leans on. Split into 3a (Linux) / 3b (Windows), each its own milestone, gated by an "is the backend actually enforcing on the runner" spike first.

**3. Two in-scope deliverables depend on machinery that isn't there, without closing the gap.** (a) "MCP stdio child processes" is listed in-scope (§1 #4) but Slice 4(b) silently makes it conditional — "when the MCP stdio transport lands" — and `mcp.rs` is an explicit inert stub (`UnwiredTransport`, no wire transport, `#![allow(dead_code)]`). A headline scope item is gated on separate, larger, unbuilt work; move it to non-goals or pull the transport in. (b) The Email screen + `email_*`/`calendar_*` tools are an **app** deliverable (Slice 6), but the app dispatcher is built with `BodyEnv::app_default()` (`lib.rs:570`), which does **not** include `Capability::Email`/`Calendar` — only `headless_server_default()` does (`tools/mod.rs:105` vs `:120`). As-designed those tools are filtered out of the app by `available_tools` and unreachable by the agent. The doc waves at "app_default … gain the new caps as appropriate" but never confronts that Email/Calendar/Tasks must be *added* to `app_default` (with the local-first degrade story) for the flagship Email wiring to function at all.

## Open question already answered (should not block)

**Q4 ("which RiskClasses may auto-run unattended?" — "not in the specs") is substantially already answered by shipped, tested code.** `hooks/headless.rs::QueueingPrompter` already implements the unattended authorization policy and encodes the decided autonomy model: `Dangerous` is **never** pre-authorized; `External` only if a rule *names the destination* (no bare `*`); everything else is pre-authorized **only** via an explicit `(tool, pattern, Allow)` rule, otherwise **park-and-deny** (fail-closed) — exactly the "no human present" problem the doc frames as unspecced. The enforcement mechanism exists; the only genuinely-open sliver is *what standing rules `SeedProfile::Server` writes into `tool_rules`*. Worse, the doc's proposed default ("Server seed auto-allows Safe only, Ask→deny for everything else") is **inconsistent with** — more restrictive than — the existing prompter, and the doc never references `headless.rs` at all, even though Slice 5's server seed must feed precisely that machinery. Fold Q4 into "use the existing `QueueingPrompter` contract; decide only the seed ruleset."

By contrast **Q3 (walled ⇒ network-deny) is correctly flagged as genuinely open**: the specs' "full island … both directions" language (`PLAN.md:350-360`,`:578-600`) is a **memory** island (separate DB), not a network one — nothing in the specs answers egress default, so it does need Lukas. Q1 (Linux tech) and Q2 (email sync backend) are also legitimately open.

## Credit where due
The parts that matter most are right: RiskClass→policy derivation (§2.5) matches `lib.rs:573-583` exactly with no new gating code; `ExecCtx.profile` is genuinely threaded end-to-end (`args.profile` → `AgentLoop::run` → `ExecCtx` → `dispatch.rs:485`), so §2.3 option B is well-founded (the `get_active_profile` stub at `ipc/mod.rs:254` only affects the UI chip, not the data path); INV-M7a's fail-closed claims map precisely to real code (`exec.rs:229-231`, `UnsupportedSandbox :392`); and per-profile physical storage correctly mirrors the walled-memory pattern (`storage/mod.rs:117-160`).

---

## Revision v2 (addressing the review)

*Written after re-reading the cited code end-to-end. Each subsection resolves one review finding, grounded in how the spine ACTUALLY works. No new parallel machinery — everything below reuses `RiskClass`, `Capability`, the `HookChain`, `ActionFingerprint`/`resolve_grant`, `run_guarded`'s fail-closed spawn, and `enforce_local_routing`. Where the revision changes a slice, the slice is re-stated with an honestly-achievable gate (and flagged when it needs an integration/real-subprocess harness rather than `cargo test --lib`).*

### R0. The one correction that reshapes everything: two enforcement tiers, not one

The review is right that the design blurred **two** enforcement tiers under one word "sandbox." Grounding them precisely, because the rest of the revision hangs off this:

- **Tier K (kernel jail).** Applies to **child-process spawns only** — today `shell_exec` via `MacSeatbeltSpawn` (`exec.rs:305`), tomorrow any other spawn routed through the `SandboxedSpawn`/`ProfileConfinement` trait (`exec.rs:86`). This is the tier that "holds even if an in-process check is bypassed," because the confinement is imposed by the OS on a *separate process image*.
- **Tier P (in-process path confinement + physical separation).** Applies to the **native fs tools** — `ReadFileTool`/`ListDirTool`/`SearchFilesTool`/`WriteFileTool`/`EditFileTool`/`DeleteFileTool`, each holding a fixed `root: PathBuf` (`fs.rs:73,146,204,445,551,646`) and gating every access through `resolve_within` (`fs.rs:39`) / `resolve_within_new` (`fs.rs:338`). These run **in the Tauri process**. Their boundary is exactly the tier walled memory already gives data: an in-process guard *plus* physically-separate files — no kernel backstop, because there is no child process to jail.

M7 makes **both** tiers per-profile. It does **not** promote Tier P to Tier K (routing every `read_file` through a subprocess jail is not worth the per-call fork cost, and isn't proposed). The fix is to state the invariant truthfully per tier — done in R1.

### R1. INV-M7b restated honestly (review gap #1)

The old INV-M7b claimed kernel enforcement "even if `resolve_within` is bypassed" for *all* file-touching operations. That is false for Tier P. Replacement:

> **INV-M7b (physical, not a filter — extends §7 walled memory to execution + PIM data).** A tool executing under profile P touches only P's `workspace/`, P's `tmp/`, P's `.db`, and P's `allowed_domains`. The boundary is enforced at **two tiers, each stated at its true strength**:
> - **Child processes (`shell_exec`, and any future spawn through `ProfileConfinement`)** are confined by the **OS kernel** to P's `profile_root`+`tmp_root` and P's network posture. This tier holds even if the in-process path checks or the hook chain are bypassed by a bug, because the confinement is applied to a separate process image (`exec.rs:6-10`, `run_guarded` fail-closed at `exec.rs:229-231`).
> - **Native fs tools** are confined by the **in-process** `resolve_within`/`resolve_within_new` check (`fs.rs:39,338`) re-rooted to P's `workspace/`, **plus physically-separate directories and DB files** — the *same tier walled memory already provides for data* (`storage/mod.rs:117-160`). This is defense-in-depth (path check ∧ physical separation), not kernel enforcement; a bug that defeats *both* the path check and the physical layout is required to cross the boundary, which is strictly stronger than today's single shared `workspace/`, but is honestly weaker than the kernel tier.

The genuinely-new-vs-shipped bit survives: walled memory covered *data*, local-routing covered *model egress*; INV-M7b adds *execution + PIM data*, with the kernel tier as a real strengthening for the child-process surface. It is no longer overclaimed for fs tools.

**Under-inventory fix — `ProtectedPathHook` must re-root per profile.** The review is correct: `ProtectedPathHook` is built once with a fixed `hook_workspace_root` (`lib.rs`, passed as `Some(hook_workspace_root)` into `build_pretooluse_chain_full`), and it resolves a call's `path` arg against that fixed root via `canonicalize_best_effort(root, rel)` (`protected_path.rs:143-154`). After the per-profile split, a `write_file` under profile B whose `path` is a symlink would be canonicalized against the *shared/default* root — the floor could miss a protected target, or resolve the wrong one. The clean fix reuses the machinery already in place: **the hook already receives the active profile** — `EventContext.profile` (`hooks/mod.rs:194`) is stamped by the dispatcher via `.with_profile(ctx.profile.as_str())` (`dispatch.rs:485`). So:

- Replace `ProtectedPathHook`'s `workspace_root: Option<PathBuf>` with a `resolver: Option<Arc<WorkspaceResolver>>` (the same `WorkspaceResolver` §2.3 introduces).
- In `on_event`, resolve against `self.resolver.root_for(&ctx.profile)` instead of a fixed root (`protected_path.rs:143`). Empty profile → the scratch/default root, exactly as the fs tools handle a `Default` `ExecCtx`.
- No change to the hook's position, its hardcoded `PROTECTED` list, its Once-only `covers_once` contract, or the dispatcher's forced-Once piggyback (`dispatch.rs:611-616`). The floor stays non-overridable; it just canonicalizes against the correct profile's root.

This keeps the protected-path floor *fed, not weakened*: it still runs before `PermissionHook` (`hooks/mod.rs:540-551`), still Ask-only-satisfiable by a fresh `Once` grant.

### R2. Split the kernel backends; gate each on an enforcement-verification spike (review gap #2)

The old Slice 3 folded macOS-done + Linux + Windows into one committable slice with the gate "behavior suite passes on Linux and Windows in CI." That is not honestly achievable as one slice, for the reason the review names: `platform/{linux,windows}/mod.rs` are stubs, each backend is a multi-week security effort, and — critically — the CI matrix (`build.yml:16` = `[macos-latest, ubuntu-latest, windows-latest]`, running `cargo test --verbose`) may run those OSes *under GitHub's own sandbox*, where `unshare(CLONE_NEWNET)`, Landlock, or AppContainer may not actually enforce. If they don't, `is_enforcing()==false` makes the behavior tests **skip**, and a green CI would falsely imply a working jail.

Revised split:

- **Slice 3-spike (blocking, precedes 3a/3b) — "does the backend enforce on the runner?"** A tiny, loud probe test per platform that asserts `is_enforcing()==true` on that CI runner AND that a known out-of-root write is actually OS-denied by a real subprocess. If a runner can't enforce, this test **fails or is explicitly reported as UNVERIFIED** (a printed skip marker + a non-passing status the merge gate reads) — never a silent green. This converts the review's "hollow CI" risk into a visible gate. *This is a real-subprocess integration test, not `cargo test --lib`* — it spawns a child and inspects kernel behavior, exactly like the existing macOS Seatbelt tests (`exec.rs:698-748`), which already run under `cargo test` but only on macOS.
- **Slice 3a — Linux backend (own milestone).** Landlock ABI probe + seccomp-bpf + net-namespace + cgroup-kill, per §4. `is_enforcing()` probes Landlock at startup; non-enforcing → `UnsupportedSandbox`-style hard-deny (fail closed, `exec.rs:392-405`). *Gate:* on a runner the spike proved enforcing, the workspace-in/out + net-off suite passes against a **real** subprocess, and the cgroup kill reaps a `setsid` grandchild (the escape `exec.rs:106-113` documents). Integration harness required.
- **Slice 3b — Windows backend (own milestone).** AppContainer + per-profile capability SID + restricted token + Job Object (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`), per §4. Same gate shape as 3a, same integration-harness requirement.

Both 3a and 3b keep the fail-closed default: until the spike proves a platform enforces, that platform stays hard-deny (`shell_exec` registered-but-unrunnable), never a bare spawn. §6's "no `#[cfg(feature)]` may strip confinement" reviewer check is unchanged.

### R3. The two deliverables that rested on unbuilt machinery (review gap #3)

**(a) MCP-child confinement → out of M7's execution scope; kept as the documented seam.** `mcp.rs` is an explicit inert stub (`UnwiredTransport`, `#![allow(dead_code)]`, `mcp.rs:9`) — no stdio/JSON-RPC transport exists. Confining an MCP child is meaningless until there is a child. So:

- **Non-goal (moved from §1 #4):** wiring MCP stdio child processes through the jail is **not** in M7 — it lands with the MCP transport, its own larger item.
- **Seam kept:** the `ProfileConfinement` trait + `ConfinementSpec` (§2.2) are written so that when the transport lands, its child spawn routes through the *same* `run_guarded(spawner, spec)` path `shell_exec` uses — one line, no new confinement machinery. This is the identical "shape now, mechanism later" split the codebase already used for `SandboxedSpawn` itself (`mcp.rs:9` calls this out). The invariant we preserve is architectural: *there is exactly one spawn chokepoint, and it is fail-closed* — so a future MCP child cannot be added except through the jail.

Old Slice 4(b) is therefore deleted from M7. Slice 4 becomes 4a (`fetch` allowlist) only.

**(b) Email/Calendar/Tasks must be added to `app_default`, with the local-first degrade — the design now confronts it.** The review is exactly right: `app_default` (`tools/mod.rs:105`) lacks `Email`/`Calendar`; only `headless_server_default` (`:120`) has them, so as-written the Email tools are filtered out of the app by `available_tools` (`tools/mod.rs:423`) and unreachable. Concrete fix, split by what actually needs the network so the degrade is honest:

- **Capability additions.** Add `Capability::Tasks` to the enum (`tools/mod.rs:49`). Add `Email`, `Calendar`, `Tasks` to `app_default` (`tools/mod.rs:105`); add `Tasks` to `headless_server_default` (`:120`, which already has `Email`/`Calendar`).
- **Split the tools by tier so `Capability` gating carries the degrade for free:**
  - `email_search` / `email_read`, `calendar_list`, `task_list` → read the **local per-profile** `emails`/`calendar_events`/`tasks` tables. Require **`Email`/`Calendar`/`Tasks` only, NOT `Network`**. RiskClass `Safe` (list/read) → whole-tool `Allow` + pre-trusted by the risk-derivation (`lib.rs:573-583`), no approval prompt.
  - `task_create` / `task_complete`, `calendar_add` (local) → `Write` → approval spine. Require `Tasks`/`Calendar`, no `Network`.
  - `email_send`, and any `calendar_*`/`email_*` that hits a **remote** account → `External` + additionally require `Network`. `destination()` returns the recipient/account (`tools/mod.rs:321`), surfaced in the approval dialog exactly like `fetch` (`fetch.rs`).
- **Degrade contract (satisfies §6 / local-first).** A build that keeps `Filesystem`/`Email`/`Calendar`/`Tasks` but drops `Network` from its `BodyEnv` keeps every local read/write tool and simply *omits* the `External` send/sync tools from `available_tools` — absent, not erroring, the exact capability-filter contract M3 established. The **Email screen** reads the local store over IPC (not through a tool), so it renders the per-profile store (empty if never synced) regardless of caps. This is why the visual-only screen can be wired now even though live sync (§7 Q2) is deferred.

### R4. Q4 is resolved: reuse `QueueingPrompter`, decide only the seed ruleset (review's "already answered")

The review is right that the unattended-authorization policy already ships, fully tested, in `hooks/headless.rs::QueueingPrompter`. It encodes the decided model exactly:

- `Dangerous` is **never** pre-authorized (`headless.rs:131`) — matches `resolve_grant`'s invariant #8 (`approval.rs`).
- `External` is pre-authorized only if a rule *names the destination* (a non-`*` pattern matching the call's `destination`) — `headless.rs:151-161`.
- Everything else pre-authorizes only via an explicit `Allow` resolved with the **same** deny>ask>allow / most-specific-wins precedence the interactive `PermissionHook` uses (`resolve_effective_mode`, `headless.rs:142`); otherwise **park-and-deny** (fail closed).
- A pre-authorized grant is always per-action `Once` (`headless.rs:163`) — the prompter never hands itself a standing grant.

So Slice 5's server seed does **not** invent a policy — it *feeds this prompter*. Two consequences:

1. **The old §2.6/§7-Q4 proposal ("Server seed auto-allows Safe only, Ask→deny for everything else") is redundant AND was inconsistent (more restrictive) with the shipped prompter — drop it.** With an **empty** `tool_rules` seed, `QueueingPrompter` already yields precisely "Safe runs (Safe is whole-tool `Allow` + pre-trusted by the risk derivation, so it never reaches an Ask or the prompter at all), everything else parks-and-denies." That is the safe default, for free, from existing code.
2. **The only genuinely-open sliver** is whether `SeedProfile::Server` should write *any* standing `Allow` rows (e.g. `write_file` within the profile workspace) to reduce parking friction on a headless box. That is a small product tuning question, **not a blocker** — the empty-ruleset default ships safely. Slice 5 must reference `headless.rs` explicitly and wire the server seed to author `tool_rules` rows the `QueueingPrompter` consumes (via the same `StorageToolRuleWriter` the interactive "Always allow" path uses), respecting `persist_rule_allowed` (`approval.rs:271`: only `Write` persists; `External`/`Dangerous` never earn a standing row).

**Q1 (Linux tech), Q2 (email sync backend), Q3 (walled ⇒ network-deny) remain genuinely open for Lukas** — the review confirms Q3 is a memory island in the specs, not a network one, so the egress default is unspecified.

### R5. INV-M7c (the §12-item-4 reconciliation) restated per tier — and the "re-check permission output against the sandbox" discharge

PLAN §12 item 4 asks that the OS sandbox **compose** with the danger-floor + permission gate *without weakening them*, and that permission output be *re-checked against the sandbox*. Grounded in the real control flow (`dispatch.rs:492-539`), the composition is:

1. **Ordering guarantees the floor is never weakened.** The `HookChain` runs `PrivacyFilter → Sandbox(denylist) → ProtectedPath → SessionMode → Permission → FirstUse` and short-circuits on the first `Deny`/`Ask` (`hooks/mod.rs:424-438,537-557`). The kernel jail is consulted **only after** the chain returns `Continue`, at spawn time inside `Tool::run` → `run_guarded` (`dispatch.rs:536`, `exec.rs:224`). A jail-apply failure is a hard `Err` with no unsandboxed fall-through (`exec.rs:229-231`). On approve, the dispatcher re-runs the **full** chain (`dispatch.rs:617-619`), so the non-overridable `Sandbox`/`ProtectedPath` floors are re-checked every round. Therefore the jail can only ever *narrow* an already-permitted call; it can never turn a chain `Deny` into a run. (This is INV-M7a, unchanged and correct.)

2. **The "re-check permission output against the sandbox" is a single-source-of-profile assertion.** The subtle failure §12 item 4 guards against is a *profile mismatch*: the permission gate resolving rules for profile P while the jail confines to profile Q's root. Both are derived from **one** source — `ExecCtx.profile` (`tools/mod.rs:211`): the permission context reads it via `EventContext::with_profile(ctx.profile)` (`dispatch.rs:485`), and the jail's `ConfinementSpec.profile_root` reads it via `WorkspaceResolver::root_for(ctx.profile)` (§2.3). The reconciliation invariant is that these are provably the same profile identity — no cell where permission thinks P but the jail confines to Q.

Restated INV-M7c, split by tier (this is what the test suite must prove):

> **INV-M7c-K (kernel tier).** For the child-process surface, the kernel jail denies a **superset** of what the permission gate denies: `permission_denies(call) ⇒ call never spawns` (chain short-circuit, provable in-process), and `permission_allows(call) ⇒ the jail may still deny` an out-of-`profile_root` write / out-of-`allowed_domains` egress (provable only against a **real subprocess on an enforcing backend**). Same profile identity feeds both sides (R5.2).
>
> **INV-M7c-P (in-process tier).** For the native fs tools, `permission_denies(call) ⇒ never runs` (chain short-circuit, in-process), and `permission_allows(call) ⇒ resolve_within still confines to the active profile's `workspace/`` (physical separation + path check), so a write permitted under A is physically absent from B. No cell relies on an in-process check that *only* Tier P backs while claiming Tier-K strength.

**Test-harness honesty:** INV-M7c-P is `cargo test --lib`-able (pure in-process: two profile roots, assert a write via A's `WriteFileTool` is unreadable via B's `ReadFileTool`, and that a chain `Deny` never reaches `Tool::run` — the existing `sandbox_denied_call_never_runs_the_tool` pattern, `dispatch.rs:1193`). INV-M7c-K is **not** `--lib`-able — it requires spawning a real jailed subprocess and observing kernel denial, so it lives with the Slice 2/3 integration tests (macOS today, `exec.rs:698-748`) and is gated by the Slice 3-spike on Linux/Windows.

### R6. Revised build slices (with honest gates)

**Slice 1 — Per-profile physical roots (Tier P).** Introduce `WorkspaceResolver { root_for(&profile) -> PathBuf }`. Re-root the six fs tools (`fs.rs`) and `ShellExecTool` (`exec.rs:411`) from a fixed `PathBuf` to the resolver; **also re-root `ProtectedPathHook`** (R1) onto the same resolver, reading `EventContext.profile`. Create `profiles/<name>/workspace|tmp` on profile open (`storage/mod.rs`); migrate the legacy shared `workspace/` into the default profile's root (not deleted). Empty profile → scratch root.
*Gate (`cargo test --lib`):* a file written by `write_file` under profile A is physically absent from B's workspace dir and unreadable via B's `read_file`; `ProtectedPathHook` fires on a per-profile symlink under the active profile's root; all existing `cargo test` green.

**Slice 2 — Wire `SandboxConfig` live on macOS (Tier K).** Rename `SandboxedSpawn`→`ProfileConfinement`, `ExecSpec`→`ConfinementSpec`; move Seatbelt to `platform/sandbox/macos.rs`, parameterizing `subpath` rw + the `(allow network*)` line by `spec.profile_root`/`tmp_root`/`allowed_domains` (`exec.rs:357`); keep the `import system.sb` gotcha and SIGABRT/exit-65 apply-failure detection (`exec.rs:134`). Add the `sandbox_config` migration (`PROFILE_MIGRATIONS`, `migrations.rs:195`) + `ProfileDb::{get,set}_sandbox_config` serializing `SandboxConfig` (`hooks/sandbox.rs:142`). Feed the active profile's config + resolver root into the spawn. `run_guarded` fail-closed semantics kept byte-for-byte (`exec.rs:229-231`).
*Gate (integration / real subprocess, macOS):* `shell_exec` under A is OS-denied reading B's workspace; `allowed_domains=[]` blocks egress; an apply failure is a hard `Err` (`hard_errs_when_sandbox_apply_fails`, `exec.rs:562`, still passes). Includes the INV-M7c-K reconciliation cell for macOS.

**Slice 3-spike — enforcement verification (blocks 3a/3b).** Per-platform loud probe that `is_enforcing()==true` AND a real out-of-root write is OS-denied on the CI runner; UNVERIFIED is a visible non-pass, never a silent skip (R2).

**Slice 3a — Linux backend / Slice 3b — Windows backend.** Each its own milestone, per R2; integration harness; fail-closed until the spike passes.

**Slice 4a — `fetch` per-profile allowlist.** `fetch` (`tools/fetch.rs`) enforces the active profile's `allowed_domains` as an allowlist **layered on top of** the existing SSRF guard (`is_private_endpoint`, per-hop DNS re-check) — never replacing it. (MCP-child confinement removed from M7 — R3a.)
*Gate (`cargo test --lib`):* a `fetch` outside P's `allowed_domains` is refused with a clear reason; the SSRF localhost/RFC-1918/metadata block still holds on every hop.

**Slice 5 — Profile-activation seeding + server flavor (uses `QueueingPrompter`).** `seed_profile_defaults(storage, profile, SeedProfile)` fires idempotently on activation (fire the reserved `AppLaunch`/new `ProfileActivated` path, `hooks/mod.rs:123`); seeds `memory_settings`, seat (no-op default), `sandbox_config` (`enabled=true`), and — for `SeedProfile::Server` — any standing `tool_rules` rows via `StorageToolRuleWriter`, respecting `persist_rule_allowed` (R4). **Default Server ruleset is empty** (safe: Safe pre-trusted, everything else parks via `QueueingPrompter`).
*Gate (`cargo test --lib`):* fresh activation seeds its defaults exactly once (re-activation is a no-op); with an empty Server ruleset, `QueueingPrompter` pre-authorizes Safe-only and parks the rest (assert against the shipped `headless.rs` tests' contract); switching profiles re-resolves memory/seat/sandbox/`ProtectedPathHook` root to the new profile.

**Slice 6 — Email/calendar/tasks + Email screen.** Add `Capability::Tasks`; add `Email`/`Calendar`/`Tasks` to `app_default` (R3b); `emails`/`calendar_events`/`tasks` profile tables (+FTS); the tier-split `email_*`/`calendar_*`/`task_*` tools with the RiskClass + `Capability` split from R3b, routing through the existing spine; wire the visual-only Email screen to the local per-profile store over IPC.
*Gate:* the Email screen shows A's mail and, on switch to B, shows B's (or empty) with no A rows leaking (INV-M7b-P physical-separation test, `--lib`); `email_send` triggers the approval dialog as `External` with a surfaced destination; a `Network`-less `BodyEnv` omits the send/sync tools but keeps the local read/write ones (degrade test, `--lib`).

**Cross-cutting — INV-M7c reconciliation.** INV-M7c-P lands with Slices 1/6 (`--lib`). INV-M7c-K lands with Slice 2 (macOS, integration) and per-platform with 3a/3b behind the spike.

### R7. Updated STATUS

**Build-ready for the macOS-first path — Slices 1, 2, 4a, 5, 6 — with the invariants restated truthfully (INV-M7b two-tier; INV-M7c-K/P split; `ProtectedPathHook` re-rooted).** These reuse the existing spine end-to-end and their gates are honestly achievable (`--lib` for Tier-P/degrade/seeding; a real-subprocess integration test for the macOS Tier-K cell — the same harness `exec.rs:698-748` already uses). Slices **3a/3b (Linux/Windows kernel backends)** are each their own milestone, **blocked on the Slice 3-spike** that proves the backend actually enforces on the CI runner (else a green build lies). MCP-child confinement is **out of M7** (kept as the documented single-spawn-chokepoint seam). Q4 is **closed** (reuse `QueueingPrompter`; default Server ruleset empty).

**Still open (genuine Lukas decisions, do not block the macOS path):** Q1 Linux tech (Landlock floor vs. bubblewrap upgrade), Q2 email sync backend, Q3 whether a walled profile defaults `allowed_domains=[]`. None of these gate Slices 1/2/4a/5/6.
