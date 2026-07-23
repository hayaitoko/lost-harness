# Frontend (Svelte 5)

- **Purpose** — The Tauri webview UI. The real UI is the ported design system
  under `src/lib/design/` (chat shell, section screens, settings), rendered by
  `App.svelte` from a screen-router store. The old flat `src/lib/components/*`
  UI (`ChatPanel`/`Sidebar`/`ModelPicker`/`PrivacyIndicator`/`ProviderSettings`)
  was **deleted** (confirmed gone from the tree as of this doc; verify with
  `git log --diff-filter=D -- src/lib/components/` if you need the exact
  commit) — see "Deleted / dead UI" below. Wiring has landed incrementally
  across many commits since 2026-07-15; as of HEAD `ca54251` most of the app —
  chat, routing explainability, and the bulk of Settings — is real, backend-
  wired UI, not a mockup. It is still a thin presentation layer over the Rust
  core; all backend access is funneled through one IPC bridge module.

## The design system (`src/lib/design/`)

Ported from the React component library at `~/Desktop/lost-harness-ui`.
Decision (Lukas): keep Tailwind — translate the design's plain CSS into
Tailwind utility classes rather than importing it. Porting rules live in
`src/lib/design/CONVENTIONS.md` (Svelte 5 runes patterns for props/children/
state/events, which utilities to use, when a small scoped `<style>` is
allowed for irreducible CSS) — read it before touching any file under
`design/`.

Shape (list the shape, not every file — see the directory for the full list):

| Path | Role |
|---|---|
| `src/lib/design/components/` | 37 `.svelte` components ported from the React lib (`Button`, `Sidebar`, `ChatMessage`, `RoutingBadge`, `RouteDot`, `Toggle`, `SegmentedControl`, `Select`, `Knot`, `ModelPicker`, `SettingRow`, etc.) plus `knot-geometry.ts` (the idle-animation math, ported as-is). Not every component here has a call site yet — see "Ported but unimported" below. |
| `src/lib/design/screens/` | 9 top-level screens (`MainScreen`, `EmptyState`, `Email`, `Whiteboard`, `Files`, `ScheduledJobs`, `Editor`, `Settings`, `Onboarding`) plus `shell-data.ts` (sample data for the still-visual-only screens). Each screen is self-contained — it renders its own `Sidebar`, mirroring the React lib's `Prototype.tsx` pattern rather than a shared app shell. |
| `src/lib/design/types.ts` | Shared vocabulary ported from the React lib's `types.ts`: `Route` (`"local"\|"cloud"\|"blocked"` — what actually happened to a turn), `Binding` (`"auto"\|"public"\|"private"` — the user's routing *intent*), `ScreenId` + `SCREEN_IDS` (the 9 screen names above). Comments note these mirror the Rust core's `agent::gate::Binding`/`GateDecision` vocabulary. |
| `src/lib/design/nav.svelte.ts` | The screen-router store — a runes class (`Nav`, `current = $state<ScreenId>(...)`) with `nav.go(id)`. Also syncs `window.location.hash` (`#/main`, `#/settings`, …) both ways, so deep-links and back/forward work. |
| `src/lib/design/CONVENTIONS.md` | The porting rulebook: React→Svelte 5 mapping, which Tailwind utilities map to which design tokens, and the "do NOT" list for a pure-porting pass (no new deps, don't import from the old `$lib/components/*`). |

## Token → Tailwind system (`src/app.css`)

Design tokens live as CSS custom properties on `:root`, with a
`:root[data-theme="light"]` override block — surfaces, text, accent, and the
meaning-colors `local`/`cloud`/`blocked`/`warn`.

A `@theme inline` block maps **colors only** into Tailwind v4's namespace
(`--color-surface`, `--color-local`, `--color-accent-soft`, etc.), so
utilities like `bg-surface`, `text-local`, `border-border-strong` work
directly and stay theme-reactive.

**Radii and shadows are deliberately NOT mapped into `@theme`** — mapping
`--r` would collide with Tailwind's built-in `rounded-r` utility. Instead, use
arbitrary values against the raw tokens: `rounded-[var(--r-sm)]` (4px),
`rounded-[var(--r)]` (6px), `rounded-[var(--r-lg)]` (8px),
`shadow-[var(--shadow)]`, `shadow-[var(--shadow-pop)]`.

**Global `.lh-range` slider rule** — the range-input track/thumb styling lives
in `app.css` as a plain global rule, not in a component's scoped `<style>`
(the vendor pseudo-element selectors trip up `svelte2tsx`'s scoped-style
parser). Keep it global.

Meaning-color rule carried over from the design source: saturated color
(`local`/`cloud`/`blocked`/`warn`/accent) is reserved for the privacy/routing
signal — all chrome uses grayscale (`surface*`/`text*`/`border*`).

## `App.svelte` — the screen renderer (`src/App.svelte`)

`App.svelte` no longer owns a two-column chat layout. It:

1. Renders whichever screen `nav.current` points at via a `SCREENS` lookup
   map (`{ main, empty, email, whiteboard, files, "scheduled-jobs", editor,
   settings, onboarding }`) and a `$derived` `Current` component —
   `<Current />` is the entire top-level markup for the app surface. Each
   screen is self-contained (renders its own `Sidebar`).
2. On mount: applies the theme (to avoid a flash), then hydrates the
   backend-backed stores every screen needs —
   `hydrateProfiles()` → `Promise.all([hydrateProviders(), hydrateConversations()])`
   — and subscribes a global `onStreamError` logger.
3. Mounts `ApprovalDialog` and `AskHumanDialog` unconditionally at the top
   level (both backend-driven; each renders only when its own event arrives —
   `tool:approval_request` / `tool:ask_human_request`).

There is **no dev floating screen-switcher anymore** — the version of this
doc written against the design-system port (2026-07-15) described one; it has
since been removed from `App.svelte` (`src/App.svelte` is 62 lines total: 18
import statements, a `SCREENS` map, an `onMount`, and three lines of markup —
no `<select>`, no theme toggle). Screens not yet cross-linked from the real
in-app nav are reachable today only by manually setting the `#/<screen-id>`
hash (`nav.svelte.ts` syncs it both ways) — e.g. `#/email`, `#/onboarding`.

**Entry point is `src/app.html`, loaded as `/app.html`, not `/`.** Vite's
`root` is `src/` (see `vite.config.ts`) and the build's `rollupOptions.input`
points at `src/app.html` explicitly — loading `/` 404s → blank white window.
`main.ts` is unchanged: `mount(App, { target: document.getElementById("app")! })`.

## Which screens are wired vs. still visual

| Screen / component | Status | What's wired |
|---|---|---|
| `design/components/Sidebar.svelte` | **Wired** | Real `$conversations` list + row selection (`activeConversationId.set` + `hydrateMessages`), new-chat via `createConversation()`, profile switcher wired to `$profiles`/`switchProfile` (a switch clears the chat stores and rehydrates the new profile's conversation list — see the Invariants note). The section-nav links (Email/Whiteboard/Files/Scheduled-jobs) navigate via `nav.go(...)` — real navigation, but the destination screens themselves are still visual-only (below). The local-engine card's model name ("Qwen3-14B") is a hardcoded label, not read from the active provider/model. |
| `design/screens/MainScreen.svelte` | **Wired** (chat loop + routing + "why" panel; the other 4 side-panel tabs are sample data) | The chat loop: messages come from `$activeConversation`, sending goes through `chat.ts`'s `sendMessage` (streaming via `stream:token`/`stream:error`), the composer's Auto/Public/Private binding pill + the Q11 mode pill (Normal/Plan/Accept edits) feed `sendMessage` as per-send overrides, the model picker is built from `providersStore` (via `fetchModels` per provider, keyed by `providerId::name` so two providers can't collide on an identical model name), and each assistant message's `RoutingBadge` is driven by the **real** persisted `routing_decision`/`error_source`. The right-hand "Routing" panel tab calls `explainClassification(text, profile)` live and renders the classifier's actual spans (annotated highlight + hard-block flag) — the "why was this routed here" sidebar (PLAN §11) is real, not mocked. The panel's other 4 tabs (**Files in this chat**, **Background tasks**, **Sub-agents**, **Terminal**) render fixed inline sample markup (`heater-reply.md`/`lease.pdf`, a 62% progress bar, three fake sub-agent rows, a canned terminal transcript) — none of these four call into any store or IPC command. |
| `design/screens/Settings.svelte` | **Mostly wired** (9 tabs; see breakdown below) | 7 of 9 tabs are real backend panes (privacy guard, permissions, models, memory, skills, agent types, usage); **Routing** is entirely local `$state`, and **Appearance** is wired only for its theme control — the rest of Appearance is cosmetic-only. |
| `design/screens/Email.svelte`, `Whiteboard.svelte`, `Files.svelte`, `ScheduledJobs.svelte`, `Editor.svelte`, `Onboarding.svelte`, `EmptyState.svelte` | **Visual only** | Reachable via the section nav / a `#/<screen>` hash, render with sample data (`shell-data.ts` or inline), no backend calls. Don't assume any of these reflect real state. |

### Settings tab breakdown (`design/screens/Settings.svelte`, 1802 lines)

The `SECTIONS` list (line 71) is `routing`, `privacy`, `permissions`,
`models`, `memory`, `skills`, `agents`, `usage`, `appearance` — 9 tabs, not
the 5 (Routing/Privacy guard/Models/Memory/Appearance) an earlier version of
this doc described.

| Tab | Wired? | Backend calls |
|---|---|---|
| **Routing** | **No — still sample data** | `defaultBinding`/`uncertainty` are local `$state` only (lines 127-128); no `$effect` loads or saves them. The comment at line 864 explicitly redirects the *real* redaction toggle to the Privacy guard tab — don't "fix" this tab by wiring these two controls without checking whether a backend field for them exists yet (it doesn't). |
| **Privacy guard** | **Mostly wired** | `getClassifierSettings`/`setClassifierSettings`/`setRedactionEnabled`/`resetClassifierSettings` (lines 356-421) drive the strictness slider (0-100) and the narrow/medium/wide uncertainty-band control, live per profile. The top "Egress guard" toggle and the four "hard-block category" toggles are decorative — `guard` (line 130) is local `$state` with no effect, and the category toggles are `<Toggle checked locked />` with no backend field at all (the real, un-tunable floor lives server-side in the rules classifier, not behind a UI switch). |
| **Permissions** | **Wired** | `listToolRules`/`deleteToolRule` (lines 232-247, 423-439) — lists a profile's persisted "Always allow" `tool_rules` rows and revokes them (two-click confirm). |
| **Models** | **Wired** | Provider CRUD (`addProvider`/`removeProvider`/`setActiveModel`, quick-add presets + a name/URL-validated form), the downloadable model catalog (`probeHardware` + `listModelCatalog` + `downloadModel`, sized/fit-badged per machine), the downloaded-models list (`listLocalModels`/`removeLocalModel`), and **Seats** (`listSeatBindings`/`setSeatBinding`/`deleteSeatBinding` — name a role, bind it to a provider+model, per profile). The backend streams byte-level progress via a `model:download-progress` event (`ipc/mod.rs`'s `download_model`) during the download, but nothing in the frontend listens for it — `tauri.ts` has no `onModelDownloadProgress` export and `Settings.svelte` shows only a static "Downloading…" label (set before the call, cleared after `downloadModel()` resolves). A real progress bar needs a new listener wired end to end, not just a UI change. |
| **Memory** | **Wired** | `getMemorySettings`/`setMemorySettings` (walled vs. shared, semantic-search toggle) + `listMemory`/`saveMemory`/`deleteMemory`/`setMemoryPinned` — a real per-profile fact list with pin/forget and the sensitivity-routing note ("saved to this device only" / "not saved anywhere"). |
| **Skills** | **Wired** | `getSkillReflectEnabled`/`setSkillReflectEnabled` (autonomous drafting on/off) + `listSkills`/`setSkillApproval`/`deleteSkill` — an inline approve/reject/delete pane with an expandable body view. This is a hand-rolled inline implementation, **not** the `SkillsSettings`/`SkillListItem` components (see "Dead / superseded components" below). |
| **Agent types** (`agents`) | **Wired** | `listAgentTypes`/`setAgentTypeApproval`/`deleteAgentType` (approve/reject/delete personas, each showing its seat + tools allowlist) plus `installPack` (paste a Capability Pack's JSON; installs skills+agent-types+cron inert/pending). |
| **Usage** | **Wired** | `getUsageSummary` — total calls, known cost, and the honest "unpriced cloud calls" count. |
| **Appearance** | **Partially wired** | The theme segmented control (dark/light/system) is wired to `settings.ts`'s `theme` store + `applyTheme`. Accent color, background tone, density, text size, and "reduce motion" are all local `$state` (lines 156-160) with no persistence and no visible effect on the rest of the app — cosmetic-only mockup controls. |

## `src/lib/api/tauri.ts` — the bridge is bigger than its own header comment

`tauri.ts` is 1160 lines and exports **54** functions/consts (`grep -c
"^export async function\|^export function\|^export const"
src/lib/api/tauri.ts`), but its header
"Backend contract" comment (lines 9-21) still lists only the original 11 M1
commands (`get_app_version` … `get_messages`). Everything added since —
`getClassifierSettings`, `explainClassification`, the memory/skills/seats/
agent-type/pack/hardware/catalog/download/tool-rule/usage/ask-human functions
— is real, working code, just undocumented in that comment block. If you're
tracing "does command X have a frontend wrapper," `grep -n "^export"
src/lib/api/tauri.ts` is more reliable right now than reading the header.

## Superseded / deleted / dead UI

- **`src/lib/components/{Sidebar,ChatPanel,ModelPicker,PrivacyIndicator,ProviderSettings}.svelte`
  no longer exist in the tree** (confirmed by directory listing — only
  `ApprovalDialog.svelte` and `AskHumanDialog.svelte` remain under
  `src/lib/components/`). The design-system port's replacements are
  `design/components/Sidebar.svelte`, `design/screens/MainScreen.svelte`, and
  `design/screens/Settings.svelte`. There is nothing left to diff against —
  don't go looking for them.
- **`src/lib/components/ApprovalDialog.svelte`** is still used, mounted
  directly by `App.svelte`, backend-driven (renders only on a real
  `tool:approval_request` event). Its "Esc denies, never approves" and
  display-only `command` rendering invariants (below) still apply.
- **`src/lib/components/AskHumanDialog.svelte`** is the ask-human counterpart
  — also mounted unconditionally by `App.svelte`, renders on
  `tool:ask_human_request`, queues requests so a burst never drops one, and
  submits via `resolveAskHuman`. Escape or "Skip" declines (`answer: null`);
  Cmd/Ctrl+Enter submits. The question text is untrusted (model-authored) and
  rendered as plain interpolation, never `{@html}`.
- **`design/components/SkillsSettings.svelte` + `SkillListItem.svelte` are
  dead.** They implement the same approve-first/autonomous toggle + learned-
  skills feed + draft-review UI that Settings' **Skills** tab has, but
  `SkillsSettings.svelte` has zero import sites anywhere in `src/` — Settings
  ended up with its own hand-rolled inline markup instead (lines 1458-1561)
  rather than using this component pair. Both files are a superseded, never-
  wired mockup left over from the design-system port; verified via
  `grep -rn "SkillsSettings\|SkillListItem" src` returning only their own
  definitions. Safe to delete in a cleanup pass, or to wire in and delete the
  inline duplicate instead — but don't edit `SkillsSettings.svelte` expecting
  it to affect the real Skills tab, it doesn't.
- **Ported but unimported** — a number of other `design/components/*.svelte`
  files have no call site anywhere in `src/` today (verified by grep, not
  exhaustively audited beyond this list): `WhyPanel.svelte` (MainScreen
  re-implemented its own inline routing-explainer markup instead of using
  it), `CommandPalette.svelte` (mounted nowhere — no `⌘K` handler wires it
  up, even though Sidebar's search box renders a `⌘K` hint), `ContextMenu.svelte`,
  `NotificationRollup.svelte`, `BatonBanner.svelte`, `ComposerRoutingNote.svelte`,
  `ClassifierControls.svelte`, `CostUsage.svelte` (and the `FlyingBlindBanner.svelte`
  it alone imports), `SeatAssignment.svelte` (Settings' Seats UI is hand-rolled
  inline instead), `ServerPairing.svelte`, and `ProfilesSettings.svelte`
  (profile switching lives in `Sidebar`'s `ProfileSwitcher.svelte` instead).
  These aren't necessarily bugs — some may be intended for screens not yet
  built out — but don't assume "it's in `design/components/`" means "it's on
  screen somewhere."

## Invariants (do NOT break)

- **`src/lib/api/tauri.ts` is the only IPC touchpoint.** Nothing outside this
  file may call `invoke`/`listen` — stated in the file's own header comment.
  Components call stores (or, in a few Settings/MainScreen cases, `tauri.ts`
  functions directly — see below), stores call `tauri.ts`; no component under
  `design/` calls `invoke` directly.
  - Note: `Settings.svelte` and `MainScreen.svelte` import many `tauri.ts`
    functions (`listMemory`, `getClassifierSettings`, `explainClassification`,
    etc.) directly rather than through an intermediate store — this is the
    established pattern for the newer panes (no dedicated store exists per
    domain the way `chat.ts`/`providers.svelte.ts` exist for conversations/
    providers). Follow it for a new Settings pane rather than inventing a new
    store per feature.
- **Args-wrapping contract**: any Rust command with an `args: SomeStruct`
  parameter must be invoked as `invoke("cmd", { args: { ...snake_case_fields } })`
  — never flattened. Getting this wrong produces a serde deserialization
  error at the Rust boundary, not a TS type error (the `args` object is typed
  `unknown` by `invoke`). See `ipc-and-app-wiring.md` for how thin the
  regression-test coverage for this actually is on the newer commands.
- **Field casing**: JS-side objects passed to `invoke` must use snake_case
  keys matching the Rust struct's serde field names (Tauri's camelCase
  conversion only applies to top-level command parameter names, never to
  fields nested in a struct).
- **API keys never round-trip back from the backend.** `ProviderInfo` has no
  `api_key` field; the providers store explicitly sets `apiKey: ""` on
  hydrate. `Settings.svelte`'s provider-edit form placeholder text ("saved —
  leave blank to keep") depends on this staying true.
- **The approval dialog's `command` field is display-only and must stay
  unescaped-safe.** `ApprovalDialog.svelte` renders `current.command` inside
  a `<pre>` via plain text interpolation (Svelte auto-escapes) — never
  `{@html ...}`. `AskHumanDialog.svelte`'s `current.question` follows the
  same rule (model-authored, untrusted, plain text only).
- **Esc denies, never approves** in the approval dialog; Esc/"Skip" in the
  ask-human dialog is a decline, never an implicit answer. Keep the
  fail-closed default when extending either.
- **`hydrateConversations()` must merge, not replace`** — overwriting the
  whole list would wipe out a transcript already loaded by
  `hydrateMessages()` for the active conversation. `design/components/Sidebar.svelte`
  relies on the merged list for its conversation rows. The one deliberate
  exception is a profile switch: `switchProfile` (`stores/profiles.ts`)
  clears `conversations` + `activeConversationId` *before* rehydrating,
  because merging across profiles would carry the old profile's rows over as
  "local-only" entries and leave a stale `activeConversationId` pointing at
  a conversation the new profile's DB doesn't have.
- **A `Block` gate decision never adopts the server's `message_id`** in
  `sendMessage()`'s error path — on Block, the backend persisted no message
  row, so `response.message_id` is unrelated/throwaway; adopting it risks a
  duplicate-key collision in a keyed `{#each}`. `MainScreen.svelte`'s message
  list (`{#each ... as m (m.id)}`) depends on ids staying unique.
- **`sendMessage()` now trusts the response's real `routing_decision`.** This
  is a fix, not just an invariant: `chat.ts`'s `finalizeMessage` call on the
  success path (chat.ts:397-404) passes `response.routing_decision` straight
  through, and the backend (`ipc::send_message`, see `ipc-and-app-wiring.md`)
  now re-queries the persisted assistant row for the real decision instead of
  hardcoding `"allow"`. **An earlier version of this doc (and of
  `README.md`) flagged the hardcoded-`"allow"` response as a live bug — it is
  fixed** (backed by unit tests: `ipc::tests::latest_assistant_routing_reads_the_real_decision`
  et al.). Don't reintroduce a hardcoded literal here.

## Known gaps / watch-items

- **`tauri.ts`'s header-comment contract is stale** (see above) — it
  documents 11 of the 54 exported functions/consts. Treat `grep -n "^export"` as the
  actual source of truth, not the comment block.
- **`ModelPicker` uses a composite `providerId::name` key** (fixed from an
  earlier flat name-only key that let two providers with an identically-named
  model collide) — `MainScreen.svelte`'s `modelOwner` map is keyed this way
  now (lines 199-236). No known collision bug remains here; noted only
  because an earlier version of this doc flagged the old flat-key version as
  a bug.
- **`MainScreen`'s side panel is a mix of real and sample content on one
  screen** — the "Routing" tab is live (`explainClassification`), the other
  four (Files/Tasks/Sub-agents/Terminal) are permanently-fake inline markup
  with no loading state, no empty state, and no IPC call. There's no visual
  cue in the UI itself distinguishing "this tab is real" from "this tab is a
  mockup" — rely on this doc, not the UI, to know which is which.
- **Visual-only screens keep stale-looking sample data on purpose** —
  `Email`, `Files`, `Whiteboard`, `ScheduledJobs`, `Editor`, `Onboarding`,
  `EmptyState` render fixed sample content (`shell-data.ts` or inline
  literals). Don't "fix" what looks like a data bug in these screens without
  checking whether it's just unwired sample data.
- **No frontend test suite exists.** No `*.test.*`/`*.spec.*` files anywhere
  in the repo, and `package.json` has no test runner dependency — only
  `svelte-check` (`npm run check`) is wired.
- **Toolchain unchanged**: Vite + `@sveltejs/vite-plugin-svelte` + Svelte 5 +
  Tailwind v4 (`@tailwindcss/vite`, CSS-first config — no `tailwind.config.js`).
  Fixed dev port 1420 / HMR port 1421 (`vite.config.ts`) are required by
  Tauri's `devUrl` in `src-tauri/tauri.conf.json` — don't change the Vite
  port without updating `tauri.conf.json` too.

## How to extend

- **Add a new backend command**: (1) implement it in `src-tauri/src/ipc/mod.rs`
  and register it in `lib.rs`'s `tauri::generate_handler![...]` list (and, if
  it's model-free, add it to `contract_tests.rs`'s mock harness with a
  correct-shape/broken-shape test pair — most commands added since the M1
  surface have skipped this; see `ipc-and-app-wiring.md`); (2) add a wrapper
  function + type in `src/lib/api/tauri.ts`, following the existing
  `isTauri() ? tauriInvoke(...) : browserXxx(...)` pattern (and, ideally,
  update the stale header-comment contract while you're there); (3) call the
  wrapper from a store where one exists for the domain, or directly from a
  `design/` component where the newer panes' precedent (Settings, MainScreen)
  already does that.
- **Wire another visual-only screen**: follow the `MainScreen`/`Sidebar`/
  `Settings` pattern — import the relevant `tauri.ts` function(s) or store(s),
  replace the screen's local sample `$state` with real reads inside an
  `$effect`, and keep the component's existing Tailwind/markup structure
  intact (don't restyle while wiring — separate concerns per `CONVENTIONS.md`).
  If a matching `design/components/*.svelte` already exists unimported (see
  "Dead / superseded components"), prefer wiring it in over hand-rolling
  another inline copy — Settings' Skills tab is the cautionary example of the
  latter.
- **Add or change a design-system component/screen**: read
  `src/lib/design/CONVENTIONS.md` first. Match the React source at
  `~/Desktop/lost-harness-ui` for markup/behavior, translate its CSS into the
  token-backed Tailwind utilities, and only fall back to a small scoped
  `<style>` for what utilities genuinely can't express.
- **Add a new store**: follow `providers.svelte.ts` (runes, `.svelte.ts`
  file) for state needing fine-grained reactivity on nested mutation, or the
  classic `writable`/`derived` pattern (`chat.ts`, `profiles.ts`,
  `settings.ts`) for simpler list/scalar state. Not every domain needs a
  store — see the note under Invariants about calling `tauri.ts` directly
  from a Settings-style pane.

## Tests

- **None exist for this subsystem.** No `*.test.*`/`*.spec.*` files were
  found anywhere in the repo, and `package.json` has no test runner
  dependency.
- The only automated check available is `npm run check`
  (`svelte-check --tsconfig ./tsconfig.json`) — type-checks `.svelte`/`.ts`
  files but does not exercise runtime behavior.
- Rust-side tests exist under `src-tauri/src/` (542 lib tests as of HEAD
  `ca54251`) but only cover the Rust command implementations, not the
  frontend bridge or the design-system components. `ipc::contract_tests`
  covers the args-wrapping shape for a handful of commands
  (`ipc-and-app-wiring.md` has the exact list) — it is the closest thing to a
  regression test for anything in this file, and it doesn't touch this file
  at all.
