<script lang="ts">
  // M1 bootstrap: full chat shell.
  //
  // Two-column flex layout:
  //   ┌─────────┬───────────────────┐
  //   │ Sidebar │   Top bar [⚙]     │
  //   │         ├───────────────────┤
  //   │         │   ChatPanel       │
  //   │         │                   │
  //   └─────────┴───────────────────┘
  //
  // On mount, hydrate the profile store from the backend (or browser
  // fallback) and apply the persisted theme. The chat store starts empty
  // — the first send or "New chat" click creates the initial conversation.
  //
  // The ⚙ gear in the top bar opens the ProviderSettings modal — a small
  // overlay that drives the providers store. Keeping the modal at the
  // App level (not inside Sidebar) means any future entry point (a
  // keyboard shortcut, the command palette, etc.) just flips
  // `settingsOpen = true`.

  import { onMount } from "svelte";
  import Sidebar from "$lib/components/Sidebar.svelte";
  import ChatPanel from "$lib/components/ChatPanel.svelte";
  import ProviderSettings from "$lib/components/ProviderSettings.svelte";
  import { hydrate as hydrateProfiles } from "$lib/stores/profiles";
  import { applyTheme, theme } from "$lib/stores/settings";

  onMount(async () => {
    // Apply theme as early as possible to avoid a flash. `applyTheme`
    // is also wired to the store via subscribe, but doing it here
    // catches the case where the store value was set before mount.
    applyTheme($theme);

    // Pull the profile list + active id from the backend.
    await hydrateProfiles();
  });

  let settingsOpen = $state(false);

  function openSettings() {
    settingsOpen = true;
  }

  function closeSettings() {
    settingsOpen = false;
  }

  // Allow Esc to close the modal — keep it local so it doesn't fight
  // the PrivacyIndicator / other components' key handlers.
  function onWindowKeydown(e: KeyboardEvent) {
    if (e.key === "Escape" && settingsOpen) {
      e.preventDefault();
      closeSettings();
    }
  }
</script>

<svelte:window onkeydown={onWindowKeydown} />

<div class="app-root flex h-full min-h-0">
  <Sidebar />
  <main class="flex min-h-0 flex-1 flex-col">
    <!-- Top bar with settings gear. -->
    <header
      class="flex shrink-0 items-center justify-end border-b border-neutral-800/60 bg-neutral-950/40 px-4 py-1.5"
      data-testid="app-topbar"
    >
      <button
        type="button"
        onclick={openSettings}
        class="inline-flex items-center gap-1.5 rounded-md px-2 py-1 text-xs text-neutral-400 transition hover:bg-neutral-900 hover:text-neutral-200"
        aria-label="Open provider settings"
        title="Provider settings"
        data-testid="open-provider-settings"
      >
        <svg class="h-3.5 w-3.5" viewBox="0 0 16 16" aria-hidden="true">
          <path
            d="M8 1.5 a1 1 0 0 1 1 1 v1.2 a4.6 4.6 0 0 1 1.3 0.55 l0.85 -0.85 a1 1 0 0 1 1.4 0 l0.85 0.85 a1 1 0 0 1 0 1.4 l-0.85 0.85 a4.6 4.6 0 0 1 0.55 1.3 h1.2 a1 1 0 0 1 1 1 v1.2 a1 1 0 0 1 -1 1 h-1.2 a4.6 4.6 0 0 1 -0.55 1.3 l0.85 0.85 a1 1 0 0 1 0 1.4 l-0.85 0.85 a1 1 0 0 1 -1.4 0 l-0.85 -0.85 a4.6 4.6 0 0 1 -1.3 0.55 v1.2 a1 1 0 0 1 -1 1 h-1.2 a1 1 0 0 1 -1 -1 v-1.2 a4.6 4.6 0 0 1 -1.3 -0.55 l-0.85 0.85 a1 1 0 0 1 -1.4 0 l-0.85 -0.85 a1 1 0 0 1 0 -1.4 l0.85 -0.85 a4.6 4.6 0 0 1 -0.55 -1.3 h-1.2 a1 1 0 0 1 -1 -1 v-1.2 a1 1 0 0 1 1 -1 h1.2 a4.6 4.6 0 0 1 0.55 -1.3 l-0.85 -0.85 a1 1 0 0 1 0 -1.4 l0.85 -0.85 a1 1 0 0 1 1.4 0 l0.85 0.85 a4.6 4.6 0 0 1 1.3 -0.55 v-1.2 a1 1 0 0 1 1 -1 z M8 6 a2 2 0 1 0 0 4 a2 2 0 0 0 0 -4 z"
            fill="currentColor"
            fill-rule="evenodd"
          />
        </svg>
        <span>Settings</span>
      </button>
    </header>
    <ChatPanel />
  </main>
</div>

{#if settingsOpen}
  <ProviderSettings modal={true} onclose={closeSettings} />
{/if}

<style>
  /* Make sure the app fills the viewport. The host <body> is already
   * height: 100% via app.css, so this is a belt-and-suspenders rule. */
  :global(html),
  :global(body),
  :global(#app) {
    height: 100%;
  }
</style>
