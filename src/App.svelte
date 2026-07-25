<script lang="ts">
  // Root shell for the ported design system. Each screen is self-contained
  // (renders its own Sidebar), so the root just renders whichever screen the
  // nav store points at — the React `Prototype.tsx` pattern. Screens are reached
  // through the real in-app nav (sidebar sections, the composer's Settings
  // button, the profile switcher); theme lives in Settings → Appearance.
  import { onMount } from "svelte";
  import { nav } from "$lib/design/nav.svelte";
  import { theme, applyTheme } from "$lib/stores/settings";
  import { hydrate as hydrateProfiles } from "$lib/stores/profiles";
  import { hydrateProviders } from "$lib/stores/providers.svelte";
  import { hydrateConversations } from "$lib/stores/chat";
  import {
    onStreamError,
    onLocalReroute,
    onBudgetWarning,
    type StreamErrorPayload,
  } from "$lib/api/tauri";
  import { pushToast } from "$lib/stores/toasts";

  import MainScreen from "$lib/design/screens/MainScreen.svelte";
  import Email from "$lib/design/screens/Email.svelte";
  import Files from "$lib/design/screens/Files.svelte";
  import ScheduledJobs from "$lib/design/screens/ScheduledJobs.svelte";
  import Settings from "$lib/design/screens/Settings.svelte";
  import ApprovalDialog from "$lib/components/ApprovalDialog.svelte";
  import AskHumanDialog from "$lib/components/AskHumanDialog.svelte";
  import Toasts from "$lib/components/Toasts.svelte";

  // Only LIVE screens (see the ScreenId note in design/types.ts — Email is
  // live as of the 2026-07-24 email round; the Whiteboard/Editor/Onboarding/
  // EmptyState mockups stay out until their backends exist).
  const SCREENS = {
    main: MainScreen,
    email: Email,
    files: Files,
    "scheduled-jobs": ScheduledJobs,
    settings: Settings,
  } as const;

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
    // Non-silent routing signal (C5): a turn was force-moved to a local
    // provider. Green "stayed local" tone — this is the privacy system
    // working, not an error.
    await onLocalReroute((e) => {
      pushToast(
        "local",
        e.to_is_bundled_runner
          ? "Started your local model"
          : `Switched to ${e.to_provider}`,
        e.reason,
      );
    });
    // Non-blocking budget signal (C1): the turn proceeded, but it's over the
    // profile's spend cap. Amber caution tone.
    await onBudgetWarning((e) => {
      pushToast("warn", "Over budget", e.message);
    });
  });
</script>

<Current />

<!-- Backend-driven; renders only when the core raises a tool-approval prompt. -->
<ApprovalDialog />

<!-- Backend-driven; renders only when the agent calls the `ask_human` tool. -->
<AskHumanDialog />

<!-- App-level transient signals: local reroute (C5) + budget warning (C1). -->
<Toasts />
