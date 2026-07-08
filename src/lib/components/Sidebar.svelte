<script lang="ts">
  // Lost Harness — Sidebar (Svelte 5 runes).
  //
  // Left rail of the app: profile chip at the bottom, conversation list in
  // the middle, "New chat" button at the top. Click a conversation to
  // make it active. M1 stub: no rename / pin / delete actions yet — the
  // real sidebar (M2) gets the full command-palette-driven context menu.

  import {
    conversations,
    activeConversationId,
    createConversationViaBackend,
    hydrateMessages,
  } from "$lib/stores/chat";
  import {
    activeProfile,
    activeProfileId,
    profiles,
    switchProfile,
  } from "$lib/stores/profiles";

  // Local: which profile is currently selected in the dropdown. We
  // initialize from the store and resync via $effect whenever the store
  // changes (so external switches update the UI).
  let profilePickerValue = $state<string>("personal");
  $effect(() => {
    profilePickerValue = $activeProfileId;
  });

  // The conversation list is hydrated once at app start (App.svelte); this
  // sidebar just reads the store and loads a transcript on selection.

  async function handleNewChat() {
    await createConversationViaBackend();
  }

  async function handleSelectConversation(id: string) {
    activeConversationId.set(id);
    // Load messages from the backend if this conversation hasn't been
    // hydrated yet.
    await hydrateMessages(id);
  }

  function handleProfileChange(e: Event) {
    const target = e.currentTarget as HTMLSelectElement;
    void switchProfile(target.value).catch((err: unknown) => {
      // Fall back silently — the UI shows the active profile from the
      // store, which is the source of truth.
      console.error("switchProfile failed", err);
    });
  }
</script>

<aside
  class="sidebar flex h-full min-h-0 w-64 flex-col border-r border-neutral-800 bg-neutral-950"
>
  <!-- New chat button -->
  <div class="px-3 pt-3 pb-2">
    <button
      type="button"
      onclick={handleNewChat}
      class="w-full rounded-lg border border-neutral-800 bg-neutral-900 px-3 py-2 text-left text-sm font-medium text-neutral-200 transition hover:border-neutral-700 hover:bg-neutral-800"
      data-testid="new-chat-button"
    >
      + New chat
    </button>
  </div>

  <!-- Conversation list -->
  <nav
    class="flex-1 overflow-y-auto px-2 pb-2"
    aria-label="Conversations"
  >
    {#if $conversations.length === 0}
      <p class="px-2 py-3 text-xs text-neutral-500">No conversations yet.</p>
    {:else}
      <ul class="flex flex-col gap-0.5">
        {#each $conversations as c (c.id)}
          {@const active = c.id === $activeConversationId}
          <li>
            <button
              type="button"
              onclick={() => handleSelectConversation(c.id)}
              class="flex w-full items-center gap-2 truncate rounded-md px-2.5 py-1.5 text-left text-sm transition {active
                ? 'bg-indigo-600/20 text-indigo-100'
                : 'text-neutral-300 hover:bg-neutral-900'}"
              data-testid="conversation-item"
              data-conversation-id={c.id}
              data-active={active}
            >
              {#if c.pinned}
                <span class="text-[10px] text-neutral-500" aria-hidden="true">★</span>
              {/if}
              <span class="truncate">{c.name}</span>
              <span
                class="ml-auto rounded px-1.5 py-0.5 text-[9px] uppercase tracking-wider text-neutral-500"
              >
                {c.binding}
              </span>
            </button>
          </li>
        {/each}
      </ul>
    {/if}
  </nav>

  <!-- Profile chip -->
  <div
    class="border-t border-neutral-800 px-3 py-3"
    data-testid="profile-chip"
  >
    <label
      class="block text-[10px] uppercase tracking-wider text-neutral-500"
      for="profile-select"
    >
      Profile
    </label>
    <div class="mt-1 flex items-center gap-2">
      <span class="text-base" aria-hidden="true">{$activeProfile?.icon ?? "•"}</span>
      <select
        id="profile-select"
        bind:value={profilePickerValue}
        onchange={handleProfileChange}
        class="flex-1 rounded-md border border-neutral-800 bg-neutral-900 px-2 py-1 text-sm text-neutral-200 focus:border-indigo-500 focus:outline-none"
      >
        {#each $profiles as p (p.id)}
          <option value={p.id}>{p.name}</option>
        {/each}
      </select>
    </div>
  </div>
</aside>

<style>
  /* Slim scrollbar to match the chat panel. */
  nav::-webkit-scrollbar {
    width: 6px;
  }
  nav::-webkit-scrollbar-thumb {
    background: rgb(64 64 70);
    border-radius: 9999px;
  }
  nav {
    scrollbar-width: thin;
    scrollbar-color: rgb(64 64 70) transparent;
  }
</style>
