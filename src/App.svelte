<script lang="ts">
  // Root shell for the ported design system. Each screen is self-contained
  // (renders its own Sidebar), so the root just renders whichever screen the
  // nav store points at — the React `Prototype.tsx` pattern. The floating
  // switcher (bottom-right) is a DEV aid to reach every screen while the
  // section-nav cross-links are still sample data; remove once screens are wired.
  import { onMount } from "svelte";
  import { nav } from "$lib/design/nav.svelte";
  import { theme, applyTheme } from "$lib/stores/settings";
  import { hydrate as hydrateProfiles } from "$lib/stores/profiles";
  import { hydrateProviders } from "$lib/stores/providers.svelte";
  import { hydrateConversations } from "$lib/stores/chat";
  import { onStreamError, type StreamErrorPayload } from "$lib/api/tauri";
  import type { ScreenId } from "$lib/design/types";

  import MainScreen from "$lib/design/screens/MainScreen.svelte";
  import EmptyState from "$lib/design/screens/EmptyState.svelte";
  import Email from "$lib/design/screens/Email.svelte";
  import Whiteboard from "$lib/design/screens/Whiteboard.svelte";
  import Files from "$lib/design/screens/Files.svelte";
  import ScheduledJobs from "$lib/design/screens/ScheduledJobs.svelte";
  import Editor from "$lib/design/screens/Editor.svelte";
  import Settings from "$lib/design/screens/Settings.svelte";
  import Onboarding from "$lib/design/screens/Onboarding.svelte";
  import ApprovalDialog from "$lib/components/ApprovalDialog.svelte";

  const SCREENS = {
    main: MainScreen,
    empty: EmptyState,
    email: Email,
    whiteboard: Whiteboard,
    files: Files,
    "scheduled-jobs": ScheduledJobs,
    editor: Editor,
    settings: Settings,
    onboarding: Onboarding,
  } as const;

  const SCREEN_LIST: { id: ScreenId; label: string }[] = [
    { id: "main", label: "Main screen" },
    { id: "empty", label: "Empty state" },
    { id: "email", label: "Email" },
    { id: "whiteboard", label: "Whiteboard" },
    { id: "files", label: "Files" },
    { id: "scheduled-jobs", label: "Scheduled jobs" },
    { id: "editor", label: "Editor" },
    { id: "settings", label: "Settings" },
    { id: "onboarding", label: "Onboarding" },
  ];

  const Current = $derived(SCREENS[nav.current]);

  onMount(async () => {
    // Apply theme first to avoid a flash, then hydrate the backend-backed
    // stores the wired screens read (profiles → providers + conversations).
    applyTheme($theme);
    await hydrateProfiles();
    await Promise.all([hydrateProviders(), hydrateConversations()]);
    // App-level catch for gate/stream errors that land outside an active send.
    await onStreamError((payload: StreamErrorPayload) => {
      console.warn(
        `[stream:error] source=${payload.source} conv=${payload.conversation_id}: ${payload.error}`,
      );
    });
  });

  function toggleTheme() {
    const next = $theme === "light" ? "dark" : "light";
    theme.set(next);
    applyTheme(next);
  }
</script>

<Current />

<!-- DEV: floating screen switcher + theme toggle (mirrors the design prototype). -->
<div
  class="fixed bottom-4 right-4 z-[90] flex items-center gap-[11px] rounded-[var(--r-lg)] border border-border-strong bg-surface/90 px-[11px] py-[7px] shadow-[var(--shadow-pop)] backdrop-blur"
>
  <span class="text-[10px] font-semibold uppercase tracking-[0.05em] text-text-3">Screen</span>
  <select
    class="rounded-[var(--r-sm)] border border-border bg-surface-2 px-2 py-1 text-[12px] text-text outline-none"
    value={nav.current}
    onchange={(e) => nav.go(e.currentTarget.value as ScreenId)}
  >
    {#each SCREEN_LIST as s (s.id)}
      <option value={s.id}>{s.label}</option>
    {/each}
  </select>
  <span class="h-[18px] w-px bg-border-strong"></span>
  <button
    type="button"
    onclick={toggleTheme}
    class="rounded-[var(--r)] border border-border px-[10px] py-1 text-[11px] font-medium text-text-2 transition hover:bg-surface-hover hover:text-text"
  >
    {$theme === "light" ? "☾ Dark" : "☀ Light"}
  </button>
</div>

<!-- Backend-driven; renders only when the core raises a tool-approval prompt. -->
<ApprovalDialog />
