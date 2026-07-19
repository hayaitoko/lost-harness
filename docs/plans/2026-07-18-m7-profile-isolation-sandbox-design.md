# M7 / 5.4 — Per-profile isolation + OS sandbox (design)

> **STATUS: design-pass draft (2026-07-18). Skeptical review verdict: NEEDS-REVISION.** Read the **Design Review** at the bottom before building — it flags concrete architecture gaps to fold in during the build phase.


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
