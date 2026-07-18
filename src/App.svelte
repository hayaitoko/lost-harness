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
  import { onStreamError, type StreamErrorPayload } from "$lib/api/tauri";

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
  import AskHumanDialog from "$lib/components/AskHumanDialog.svelte";

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
</script>

<Current />

<!-- Backend-driven; renders only when the core raises a tool-approval prompt. -->
<ApprovalDialog />

<!-- Backend-driven; renders only when the agent calls the `ask_human` tool. -->
<AskHumanDialog />
