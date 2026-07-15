# Design-system port — conventions (React 19 → Svelte 5 + Tailwind)

We are porting the design system at `~/Desktop/lost-harness-ui` into this Svelte 5 +
Tauri product. Decision (Lukas, 2026-07-15): **keep Tailwind — translate the look into
utility classes** (don't import the design's plain `.css`), and port the **whole design
system in one pass**. Match the design's appearance exactly; the tokens make it faithful.

## Where files go

| Source (React) | Target (Svelte) |
|---|---|
| `ui/src/components/X.tsx` | `product/src/lib/design/components/X.svelte` |
| `ui/app/screens/X.tsx` | `product/src/lib/design/screens/X.svelte` |
| `ui/src/types.ts` | `product/src/lib/design/types.ts` (done) |
| `ui/app/nav.tsx` | `product/src/lib/design/nav.svelte.ts` (done) |

Reference ports already done (copy these patterns): `Button`, `RouteDot`, `RoutingBadge`,
`RiskBadge`, `Toggle`, `SegmentedControl`, `ChatMessage`.

## Svelte 5 runes patterns

- **Props**: `let { a, b = 'x', onclick, children }: Props = $props();` with a local
  `interface Props {}`. No `export let`.
- **Children / slots**: a `children: Snippet` prop rendered with `{@render children()}`.
  For extra content slots (React `badge?: ReactNode`), use a **named Snippet prop**
  (`badge?: Snippet`) rendered `{@render badge?.()}`. Import `type { Snippet } from 'svelte'`.
- **State**: `let x = $state(0)`; derived: `let y = $derived(...)`; effects: `$effect(() => …)`.
  `useState`→`$state`, `useEffect`→`$effect`, `useMemo`/derived→`$derived`.
- **Callbacks**: name callback props lowercase. A native-ish click → `onclick?: () => void`
  bound as `{onclick}`. A value-carrying callback (React `onChange(v)`) → `onchange?: (v: T) => void`.
- **Events**: Svelte 5 uses native attributes — `onclick`, `oninput`, `onkeydown` (NOT `on:click`).
- **Lists**: `{#each items as it (it.id)}`. Conditionals: `{#if}`. No `.map()` in markup.
- **Context** (React `createContext`): use the `nav` store (`import { nav } from '$lib/design/nav.svelte'`
  — note: NO `.ts` extension in import paths, TS rejects it) or Svelte `getContext/setContext` otherwise.

## Styling — Tailwind with the design tokens

The tokens are mapped into Tailwind in `src/app.css` via `@theme inline`. Use these utilities;
do NOT hardcode hex or use `neutral-*`. Everything is theme-reactive (light/dark) automatically.

**Color utilities** (work with `bg-`, `text-`, `border-`):
`bg`, `sidebar`, `surface`, `surface-2`, `surface-hover`, `border`, `border-strong`,
`text`, `text-2`, `text-3`, `accent`, `accent-soft`, `on-accent`,
`local`, `local-soft`, `cloud`, `cloud-soft`, `blocked`, `blocked-soft`, `warn`, `warn-soft`.
So: `bg-surface`, `text-text-2`, `border-border-strong`, `bg-local-soft text-local`, `bg-accent text-on-accent`.

**Radii / shadows / font-sizes** — use arbitrary values against the raw tokens (radii are
NOT mapped, to avoid Tailwind's built-in `rounded-r` collision):
`rounded-[var(--r-sm)]` (4px) · `rounded-[var(--r)]` (6px) · `rounded-[var(--r-lg)]` (8px) ·
`shadow-[var(--shadow)]` · `shadow-[var(--shadow-pop)]` · `text-[12.5px]` etc.

**Meaning-color rule**: saturated color (`local`/`cloud`/`blocked`/`warn` + accent) is ONLY for
the privacy/routing signal. All chrome is grayscale (`surface*`, `text*`, `border*`).

**Class-per-state**: build a lookup object for variant→classes (see `RoutingBadge`/`Button`) and
interpolate; don't string-concat conditionals inline where a map is clearer.

## Irreducible CSS → scoped `<style>`

Tailwind can't cleanly express: pseudo-element knobs already covered by `before:`/`after:`
utilities (do those in Tailwind — see `Toggle`), BUT for these, use a small scoped `<style>`:
- descendant typography on caller-authored content (`.content p/code/pre`) → `:global()` (see `ChatMessage`)
- `@keyframes` animations, `::-webkit-scrollbar`, `::selection`, `[data-tip]` tooltips
- the `Knot` SVG animation (port `knot.css` + `knot-geometry.ts` as-is into the component)

Keep scoped `<style>` minimal — Tailwind utilities first, CSS only for what utilities can't do.

## Fidelity

The design's CSS class → look is the spec. Read the matching selector in the design's
`src/styles/components.css` (and per-component `.css`) to get exact px/weights, and reproduce
them with the utilities above. When unsure of a value, match the CSS literally with an
arbitrary utility (`px-[13px]`, `gap-[9px]`), not an approximation.

## Do NOT (this pass)

- Wire to backend stores/IPC — port visual + local `$state` only (screens keep their sample data).
  Wiring is a later phase. Keep the same sample data the React screen uses.
- Import anything from `$lib/components/*` (the old Tailwind components being replaced).
- Add new dependencies.
