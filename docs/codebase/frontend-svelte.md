# Frontend (Svelte 5)

- **Purpose** — The Tauri webview UI. As of 2026-07-15 the real UI is the ported design
  system under `src/lib/design/` (chat shell, section screens, settings), rendered by
  `App.svelte` from a screen-router store. The old flat `src/lib/components/*` UI
  (`ChatPanel`/`Sidebar`/`ModelPicker`/`PrivacyIndicator`/`ProviderSettings`) is **superseded**
  and no longer imported by `App.svelte` — see "Superseded old UI" below. It is still a thin
  presentation layer over the Rust core; all backend access is funneled through one IPC
  bridge module.

## The design system (`src/lib/design/`)

Ported from the React component library at `~/Desktop/lost-harness-ui` (commit `a22855e`,
2026-07-15). Decision (Lukas): keep Tailwind — translate the design's plain CSS into
Tailwind utility classes rather than importing it, porting the whole design system in one
pass rather than screen-by-screen. Porting rules live in `src/lib/design/CONVENTIONS.md`
(Svelte 5 runes patterns for props/children/state/events, which utilities to use, when a
small scoped `<style>` is allowed for irreducible CSS) — read it before touching any file
under `design/`.

Shape (list the shape, not every file — see the directory for the full list):

| Path | Role |
|---|---|
| `src/lib/design/components/` | 37 `.svelte` components ported 1:1 from the React lib (`Button`, `Sidebar`, `ChatMessage`, `RoutingBadge`, `RouteDot`, `Toggle`, `SegmentedControl`, `Select`, `Knot`, `ModelPicker`, `SettingRow`, etc.) plus `knot-geometry.ts` (the idle-animation math, ported as-is). |
| `src/lib/design/screens/` | 9 top-level screens (`MainScreen`, `EmptyState`, `Email`, `Whiteboard`, `Files`, `ScheduledJobs`, `Editor`, `Settings`, `Onboarding`) plus `shell-data.ts` (sample data for the still-visual-only screens). Each screen is self-contained — it renders its own `Sidebar`, mirroring the React lib's `Prototype.tsx` pattern rather than a shared app shell. |
| `src/lib/design/types.ts` | Shared vocabulary ported from the React lib's `types.ts`: `Route` (`"local"\|"cloud"\|"blocked"` — what actually happened to a turn, the core meaning-color signal), `Binding` (`"auto"\|"public"\|"private"` — the user's routing *intent*), `ScreenId` + `SCREEN_IDS` (the 9 screen names above). Comments note these mirror the Rust core's `agent::gate::Binding`/`GateDecision` vocabulary. |
| `src/lib/design/nav.svelte.ts` | The screen-router store — a runes class (`Nav`, `current = $state<ScreenId>(...)`) with `nav.go(id)`. Also syncs `window.location.hash` (`#/main`, `#/settings`, …) both ways, so deep-links and back/forward work. Svelte equivalent of the React lib's `nav.tsx` (hash-router + context). |
| `src/lib/design/CONVENTIONS.md` | The porting rulebook: React→Svelte 5 mapping (`useState`→`$state`, `export let`→`$props()`, `on:click`→`onclick`, etc.), which Tailwind utilities map to which design tokens, and the "do NOT" list for this pass (no backend wiring, no new deps, don't import from the old `$lib/components/*`). |

## Token → Tailwind system (`src/app.css`)

Design tokens (verbatim from `~/Desktop/lost-harness-ui/src/styles/tokens.css`) live as CSS
custom properties on `:root`, with a `:root[data-theme="light"]` override block — the same
theme-reactive pattern the old UI used, now carrying the full design palette (surfaces,
text, accent, and the meaning-colors `local`/`cloud`/`blocked`/`warn`).

A `@theme inline` block maps **colors only** into Tailwind v4's namespace (`--color-surface`,
`--color-local`, `--color-accent-soft`, etc.), so utilities like `bg-surface`, `text-local`,
`border-border-strong` work directly and stay theme-reactive — `inline` makes Tailwind emit
`var(--…)` in the generated utility instead of snapshotting the value at build time.

**Radii and shadows are deliberately NOT mapped into `@theme`** — mapping `--r` would collide
with Tailwind's built-in `rounded-r` (right-side border radius) utility. Instead, use arbitrary
values against the raw tokens: `rounded-[var(--r-sm)]` (4px), `rounded-[var(--r)]` (6px),
`rounded-[var(--r-lg)]` (8px), `shadow-[var(--shadow)]`, `shadow-[var(--shadow-pop)]`. This is
called out explicitly in both `app.css`'s comment block and `CONVENTIONS.md` — don't try to
"clean this up" by adding a `--radius-r` mapping.

**Global `.lh-range` slider rule** — the range-input track/thumb styling (used by the `Slider`
component) lives in `app.css` as a plain global rule, not in the component's scoped
`<style>`. Reason: the vendor pseudo-element selectors it needs
(`::-webkit-slider-thumb`, `::-moz-range-thumb`, `::-moz-range-track`) trip up
`svelte2tsx`'s scoped-style parser when written inside a component's `<style>` block. Keep
it global; don't try to move it into `Slider.svelte`.

Meaning-color rule carried over from the design source: saturated color
(`local`/`cloud`/`blocked`/`warn`/accent) is reserved for the privacy/routing signal — all
chrome uses grayscale (`surface*`/`text*`/`border*`).

## `App.svelte` — the screen renderer

`App.svelte` no longer owns a two-column chat layout. It:

1. Renders whichever screen `nav.current` points at via a `SCREENS` lookup map
   (`{ main: MainScreen, empty: EmptyState, email: Email, whiteboard: Whiteboard, files: Files,
   "scheduled-jobs": ScheduledJobs, editor: Editor, settings: Settings, onboarding: Onboarding }`)
   and a `$derived` `Current` component — `<Current />` is the entire top-level markup for the
   app surface. Each screen is self-contained (renders its own `Sidebar`), so there is no shared
   app-shell component to edit for chrome that should appear everywhere.
2. On mount: applies the theme (to avoid a flash), then hydrates the backend-backed stores the
   wired screens read — `hydrateProfiles()` → `Promise.all([hydrateProviders(), hydrateConversations()])`
   — and subscribes a global `onStreamError` logger.
3. Renders a **DEV floating screen-switcher + theme toggle** fixed bottom-right (a `<select>`
   over `SCREEN_LIST` calling `nav.go(...)`, plus a light/dark toggle). This is a QA aid to
   reach every screen while the in-app section-nav cross-links are still sample data — **remove
   it once screens are cross-linked for real**, don't treat it as permanent chrome.
4. Still mounts `ApprovalDialog` unconditionally at the top level (backend-driven, renders only
   when a `tool:approval_request` event arrives) — unchanged behavior from before the port.

**Entry point is `src/app.html`, loaded as `/app.html`, not `/`.** Vite's `root` is `src/`
(see `vite.config.ts`) and the build's `rollupOptions.input` points at `src/app.html`
explicitly, so `app.html` is the real index — this was true before the port and remains true.
`main.ts` is unchanged: `mount(App, { target: document.getElementById("app")! })`.

## Which screens are wired vs. still visual

Backend wiring landed in commit `55ad9d5` (2026-07-15), but only for three surfaces:

| Screen / component | Status | What's wired |
|---|---|---|
| `design/components/Sidebar.svelte` | **Wired** | Real `$conversations` list + row selection (`activeConversationId.set` + `hydrateMessages`), new-chat via `createConversation()`, profile switcher wired to `$profiles`/`switchProfile`. Renders inside every full-app screen. |
| `design/screens/MainScreen.svelte` | **Wired** | The real chat loop: messages come from `$activeConversation`, sending goes through `chat.ts`'s `sendMessage` (with streaming), the composer's Auto/Public/Private binding pill feeds `sendMessage` as a per-send override, the model picker is built from `providersStore` (via `fetchModels` per provider), and each assistant message's `RoutingBadge` is driven by the **real** gate decision (`Message.routing_decision`/`error_source`) — this un-stubs the old client-side-only `PrivacyIndicator`. |
| `design/screens/Settings.svelte` | **Partially wired** | The **Models** tab is wired to `providersStore` (list/add/remove/select provider+model, quick-add presets, the same name+URL validation as the old `ProviderSettings`). The **Appearance** tab's theme segmented-control is wired to the `settings.ts` theme store. The **Routing**, **Privacy guard**, and **Memory** tabs still use local `$state` sample data (no backend yet). |
| `design/screens/Email.svelte`, `Whiteboard.svelte`, `Files.svelte`, `ScheduledJobs.svelte`, `Editor.svelte`, `Onboarding.svelte`, `EmptyState.svelte` | **Visual only** | Reachable via the section nav / dev switcher, render with sample data (`shell-data.ts` or inline), no backend calls. Don't assume any of these reflect real state. |

`src/lib/stores/chat.ts` was extended **additively** to support the wired screens, not rewritten:

- `Message` gained three optional fields — `routing_decision?: string | null`, `model?: string
  | null`, `provider_id?: string | null` — that were previously dropped by `msgFromInfo`.
  `MainScreen.svelte` cross-references `provider_id` against `providersStore` (via
  `getProvider`) to tell a `route_local`/`allow` decision apart from an actual local-vs-cloud
  provider kind, since a plain `"allow"` decision alone doesn't say which endpoint was hit.
- `sendMessage(content, providerId, model, bindingOverride?)` gained an optional 4th
  parameter, `bindingOverride?: Binding` — an explicit per-send binding (from `MainScreen`'s
  binding pill) that wins over the conversation's stored default when present. **This is
  backward-compatible**: the old 3-arg call (`sendMessage(content, providerId, model)`) still
  works unchanged, so nothing in the superseded `ChatPanel.svelte` needed to change for this
  to land.

## Superseded old UI (`src/lib/components/*`)

`src/lib/components/{Sidebar,ChatPanel,ModelPicker,PrivacyIndicator,ProviderSettings}.svelte`
are **superseded and unused** — `App.svelte` no longer imports any of them. They're left in
place for reference but should not be edited or extended; treat `design/components/Sidebar.svelte`,
`design/screens/MainScreen.svelte`, and `design/screens/Settings.svelte` as their replacements.
Delete the superseded files in a later cleanup pass (not done yet — don't do it unprompted, a
future agent may still want to diff against them).

`src/lib/components/ApprovalDialog.svelte` is the one exception — it is **still used**,
mounted directly by `App.svelte`, and is backend-driven (renders only on a real
`tool:approval_request` event). Its "Esc denies, never approves" and display-only `command`
rendering invariants (below) still apply.

## Invariants (do NOT break)

- **`src/lib/api/tauri.ts` is the only IPC touchpoint.** Nothing outside this file may call
  `invoke`/`listen` — stated in the file's own header comment and unchanged by the design-system
  port. Both the old and new UI layers call into stores, which call into `tauri.ts`; no component
  under `design/` calls `invoke` directly. A future agent adding a new backend call must add a
  wrapper function here, not call `invoke` from a store or component.
- **Args-wrapping contract**: any Rust command with an `args: SomeStruct` parameter must be
  invoked as `invoke("cmd", { args: { ...snake_case_fields } })` — never flattened. Bare-scalar
  params (currently only `remove_provider`'s `id`) are the sole exception. Getting this wrong
  produces a serde deserialization error at the Rust boundary, not a TS type error, since the
  args object is typed as `unknown` by `invoke`.
- **Field casing**: JS-side objects passed to `invoke` must use snake_case keys matching the
  Rust struct's serde field names (Tauri's camelCase conversion only applies to top-level
  command parameter names, not to fields nested in a struct). All wrapper functions already do
  this correctly — mirror the existing pattern, don't "clean it up" to camelCase.
- **API keys never round-trip back from the backend.** `ProviderInfo` (Rust → JS) has no
  `api_key` field; the providers store explicitly sets `apiKey: ""` on hydrate. Do not add a
  field or code path that fetches/displays a previously-saved key. `design/screens/Settings.svelte`'s
  provider-edit form placeholder text ("saved — leave blank to keep") depends on this staying true.
- **The approval dialog's `command` field is display-only and must stay unescaped-safe.**
  `ApprovalDialog.svelte` renders `current.command` inside a `<pre>` via plain text
  interpolation (Svelte auto-escapes) — do not switch this to `{@html ...}`.
- **Esc denies, never approves** in the approval dialog. Keep the fail-closed default when
  extending it.
- **`hydrateConversations()` must merge, not replace`** — overwriting the whole list would wipe
  out a transcript already loaded by `hydrateMessages()` for the active conversation. This is
  unchanged by the port; `design/components/Sidebar.svelte` relies on the merged list for its
  conversation rows.
- **A `Block` gate decision never adopts the server's `message_id`** in `sendMessage()`'s error
  path — on Block, the backend persisted no message row, so `response.message_id` is
  unrelated/throwaway; adopting it risks a duplicate-key collision in a Svelte keyed `{#each}`
  block. `MainScreen.svelte`'s message list (`{#each ... as m (m.id)}`) depends on ids staying
  unique.

## Known gaps / watch-items (surfaced 2026-07-15)

- **`ipc::send_message` returns `routing_decision: "allow"` hardcoded** (`src-tauri/src/ipc/mod.rs`,
  the `SendMessageResponse` construction) — a live send can never surface a `route_local` badge
  from the immediate response, even though the real decision *is* persisted and later shows up
  correctly on hydration. `MainScreen.svelte`'s `hasRoutingSignal`/`messageRoute` logic is
  written defensively around this, but a small backend fix (return the true decision instead of
  the literal string) is needed before a fresh in-session `route_local` turn shows the honest
  badge without a reload.
- **`ModelPicker` uses a flat model-name namespace.** `MainScreen.svelte` builds its
  `ModelOption[]` by fetching each provider's models and flattening them into one list keyed by
  model name (`modelOwner: Map<string, string>` — name → provider id); two providers exposing an
  identically-named model collide and only the last-registered one is addressable from the picker.
- **The DEV screen-switcher in `App.svelte` is temporary** — remove it once the section-nav
  cross-links (Sidebar → Email/Whiteboard/Files/Scheduled-jobs, Settings → main, etc.) are wired
  for real navigation instead of being reachable only through the dev `<select>`.
- **Visual-only screens keep stale-looking sample data on purpose** — `Email`, `Files`,
  `Whiteboard`, `ScheduledJobs`, `Editor`, `Onboarding`, `EmptyState` render fixed sample content
  (`shell-data.ts` or inline literals). Don't "fix" what looks like a data bug in these screens
  without checking whether it's just unwired sample data.
- **No frontend test suite exists.** No `*.test.*`/`*.spec.*` files anywhere in the repo, and
  `package.json` has no test runner dependency — only `svelte-check` (`npm run check`) is wired.
- **Toolchain unchanged by the port**: Vite + `@sveltejs/vite-plugin-svelte` + Svelte 5 +
  Tailwind v4 (`@tailwindcss/vite`, CSS-first config — no `tailwind.config.js`, check
  `src/app.css`'s `@theme`/`:root` blocks for design tokens). Fixed dev port 1420 / HMR port
  1421 (`vite.config.ts`) are required by Tauri's `devUrl` in `src-tauri/tauri.conf.json` —
  don't change the Vite port without updating `tauri.conf.json` too.

## How to extend

- **Add a new backend command**: (1) implement it in `src-tauri/src/ipc/mod.rs` and register it
  in the Tauri command handler list; (2) add a wrapper function + type in `src/lib/api/tauri.ts`
  following the existing `isTauri() ? tauriInvoke(...) : browserXxx(...)` pattern; (3) call the
  wrapper only from a store, never directly from a `design/` component.
- **Wire another visual-only screen**: follow the `MainScreen`/`Sidebar`/`Settings` pattern —
  import the relevant store(s) from `$lib/stores/...`, replace the screen's local sample
  `$state` with store reads, and keep the component's existing Tailwind/markup structure intact
  (don't restyle while wiring — those are separate concerns per `CONVENTIONS.md`).
- **Add or change a design-system component/screen**: read `src/lib/design/CONVENTIONS.md`
  first. Match the React source at `~/Desktop/lost-harness-ui` for markup/behavior, translate its
  CSS into the token-backed Tailwind utilities listed there (never hardcode hex or use Tailwind's
  default `neutral-*` palette), and only fall back to a small scoped `<style>` for what utilities
  genuinely can't express (documented list in `CONVENTIONS.md`: caller-authored typography via
  `:global()`, `@keyframes`, `::-webkit-scrollbar`, `::selection`, tooltip attribute selectors).
- **Add a new store**: follow `providers.svelte.ts` (runes, `.svelte.ts` file) for state that
  needs fine-grained reactivity on nested mutation, or the classic `writable`/`derived` pattern
  (`chat.ts`, `profiles.ts`, `settings.ts`) for simpler list/scalar state.

## Tests

- **None exist for this subsystem.** No `*.test.*`/`*.spec.*` files were found anywhere in the
  repo, and `package.json` has no test runner dependency.
- The only automated check available is `npm run check` (`svelte-check --tsconfig ./tsconfig.json`)
  — type-checks `.svelte`/`.ts` files but does not exercise runtime behavior.
- Rust-side tests exist under `src-tauri/src/ipc/mod.rs` and elsewhere but only cover the Rust
  command implementations, not the frontend bridge or the design-system components.
