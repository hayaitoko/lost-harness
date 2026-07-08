<script lang="ts">
  // M1 bootstrap: full chat shell.
  //
  // Two-column flex layout:
  //   ┌─────────┬───────────────────┐
  //   │ Sidebar │   ChatPanel       │
  //   │         │                   │
  //   └─────────┴───────────────────┘
  //
  // On mount, hydrate the profile store from the backend (or browser
  // fallback) and apply the persisted theme. The chat store starts empty
  // — the first send or "New chat" click creates the initial conversation.

  import { onMount } from "svelte";
  import Sidebar from "$lib/components/Sidebar.svelte";
  import ChatPanel from "$lib/components/ChatPanel.svelte";
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
</script>

<div class="app-root flex h-full min-h-0">
  <Sidebar />
  <main class="flex min-h-0 flex-1 flex-col">
    <ChatPanel />
  </main>
</div>

<style>
  /* Make sure the app fills the viewport. The host <body> is already
   * height: 100% via app.css, so this is a belt-and-suspenders rule. */
  :global(html),
  :global(body),
  :global(#app) {
    height: 100%;
  }
</style>
