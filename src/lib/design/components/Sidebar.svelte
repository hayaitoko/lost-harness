<script lang="ts">
  // The left rail — brand, section nav (Email / Whiteboard / Files / Scheduled
  // jobs), search, conversation list (Pinned + Sessions), the local-engine card
  // with the Knot, and the profile switcher. Shared by every full-app screen.
  // Maps to `.sidebar` + the DC prototype's `.lh-nav-link` chrome.
  import { nav } from "../nav.svelte";
  import type { ScreenId, Route } from "../types";
  import ConversationRow from "./ConversationRow.svelte";
  import ProfileSwitcher from "./ProfileSwitcher.svelte";
  import Knot, { type KnotState } from "./Knot.svelte";
  import {
    conversations,
    activeConversationId,
    createConversationViaBackend,
    hydrateMessages,
    type Conversation,
  } from "$lib/stores/chat";
  import {
    profiles,
    activeProfile,
    switchProfile,
  } from "$lib/stores/profiles";
  import { providersStore } from "$lib/stores/providers.svelte";

  // The engine card shows the REAL active model/provider (honesty: the old
  // hardcoded "Qwen3-14B" label predated provider wiring). Falls back to an
  // honest "No model selected" when nothing is configured.
  const engineModel = $derived(providersStore.activeModel ?? "No model selected");
  const engineHost = $derived(
    providersStore.providers.find((p) => p.id === providersStore.activeProviderId)
      ?.name ?? null,
  );

  interface Props {
    active?: ScreenId;
    /**
     * @deprecated no longer drives row highlighting — that now comes from
     * `$activeConversationId`. Kept so existing call sites (MainScreen,
     * Settings, …) that still pass a sample title don't break typecheck.
     */
    activeConv?: string;
    onnewsession?: () => void;
    /** Agent status shown by the Knot in the local-engine card. */
    engineState?: KnotState;
  }

  let { active, onnewsession, engineState = "idle" }: Props = $props();

  // Nav honesty: only LIVE sections. Email is live (the 2026-07-24 email
  // round); Whiteboard returns here when its backend exists (see
  // design/types.ts ScreenId note).
  const SECTIONS: { id: ScreenId; label: string }[] = [
    { id: "email", label: "Email" },
    { id: "planner", label: "Planner" },
    { id: "files", label: "Files" },
    { id: "scheduled-jobs", label: "Scheduled jobs" },
  ];

  // `Conversation.binding` (chat.ts) is the user's routing *intent*
  // ("auto" | "public" | "private") — there's no backend field yet for the
  // actual per-turn disposition ("local" | "cloud" | "blocked") that
  // RouteDot expects. Pass through if it ever does line up; default to the
  // neutral "auto" dot otherwise.
  function rowRoute(conv: Conversation): Route | "auto" {
    const b = conv.binding as string;
    return b === "local" || b === "cloud" || b === "blocked" ? b : "auto";
  }

  function relativeTime(epochMs: number): string {
    const diffS = Math.floor((Date.now() - epochMs) / 1000);
    if (diffS < 60) return "now";
    const m = Math.floor(diffS / 60);
    if (m < 60) return `${m}m`;
    const h = Math.floor(m / 60);
    if (h < 24) return `${h}h`;
    return `${Math.floor(h / 24)}d`;
  }

  async function handleSelectConversation(id: string) {
    activeConversationId.set(id);
    await hydrateMessages(id);
    nav.go("main");
  }

  async function handleNewSession() {
    if (onnewsession) {
      onnewsession();
      return;
    }
    await createConversationViaBackend();
    nav.go("main");
  }

  // ProfileSwitcher works over its own light-weight `{ name, sub, avatar }`
  // shape (ported as-is from the design lib) and its onswitch callback
  // hands back `name`, not `id` — map the real profile list into that shape
  // and resolve back to the store's `id` before calling `switchProfile`.
  let switcherProfiles = $derived(
    $profiles.map((p) => ({ name: p.name, sub: "", avatar: p.icon })),
  );

  function handleProfileSwitch(name: string) {
    const match = $profiles.find((p) => p.name === name);
    if (match) void switchProfile(match.id);
  }

  const ENGINE_LABEL: Record<KnotState, string> = {
    idle: "local engine · idle",
    local: "answering locally",
    cloud: "routing to cloud",
    held: "held by the guard",
  };

  const navLink =
    "flex w-full items-center gap-[9px] rounded-[var(--r)] border-0 px-2 py-[6px] text-left text-[12.5px] font-medium transition-colors duration-100 cursor-pointer";
</script>

{#snippet sectionIcon(id: ScreenId)}
  {#if id === "email"}
    <svg
      width="14"
      height="14"
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      stroke-width="1.8"
      class="shrink-0"
    >
      <rect x="3" y="5" width="18" height="14" rx="2" />
      <path d="M3 7l9 6 9-6" />
    </svg>
  {:else if id === "files"}
    <svg
      width="14"
      height="14"
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      stroke-width="1.8"
      class="shrink-0"
    >
      <path
        d="M3 8a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2Z"
      />
    </svg>
  {:else if id === "planner"}
    <svg
      width="14"
      height="14"
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      stroke-width="1.8"
      class="shrink-0"
    >
      <rect x="4" y="5" width="16" height="15" rx="2" />
      <path d="M8 3v4M16 3v4M7 11h10M8 15h3" />
    </svg>
  {:else}
    <svg
      width="14"
      height="14"
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      stroke-width="1.8"
      class="shrink-0"
    >
      <circle cx="12" cy="12" r="9" />
      <path d="M12 8v4l3 2" />
    </svg>
  {/if}
{/snippet}

<aside class="flex min-h-0 flex-col border-r border-border bg-sidebar">
  <div
    class="flex h-12 flex-shrink-0 items-center gap-[9px] border-b border-border px-[14px]"
  >
    <span class="text-[13.5px] font-semibold tracking-[-0.005em]"
      >Lost Harness</span
    >
  </div>

  <div class="flex flex-col gap-px px-2 pt-2">
    {#each SECTIONS as s (s.id)}
      <button
        type="button"
        onclick={() => nav.go(s.id)}
        class="{navLink} {active === s.id
          ? 'bg-surface-2 text-text'
          : 'bg-transparent text-text-2 hover:bg-surface-hover hover:text-text'}"
      >
        {@render sectionIcon(s.id)}
        {s.label}
      </button>
    {/each}
  </div>

  <div class="lh-conv-list flex-1 overflow-y-auto px-1.5 pb-2 pt-1">
    <div
      class="flex items-center justify-between px-2.5 pb-1 pr-2 pt-3 text-[10px] font-semibold uppercase tracking-[0.06em] text-text-3"
    >
      Sessions
      <button
        type="button"
        aria-label="New session"
        onclick={handleNewSession}
        class="grid h-[18px] w-[18px] place-items-center rounded-[var(--r)] border-0 bg-transparent p-0 text-text-3 transition-colors duration-100 hover:bg-surface-hover hover:text-text"
      >
        <svg
          width="11"
          height="11"
          viewBox="0 0 24 24"
          fill="none"
          stroke="currentColor"
          stroke-width="2"
        >
          <path d="M12 5v14M5 12h14" />
        </svg>
      </button>
    </div>
    {#if $conversations.length === 0}
      <p class="px-2.5 py-2 text-[12px] text-text-3">No sessions yet.</p>
    {:else}
      {#each $conversations as c (c.id)}
        <ConversationRow
          title={c.name}
          route={rowRoute(c)}
          meta={relativeTime(c.created_at)}
          active={$activeConversationId === c.id}
          onclick={() => handleSelectConversation(c.id)}
        />
      {/each}
    {/if}
  </div>

  <div class="relative flex-shrink-0 border-t border-border px-2.5 pb-2.5 pt-2">
    <div
      title="Local engine"
      class="mb-1.5 flex cursor-pointer items-center gap-[9px] rounded-[var(--r)] border border-border bg-surface px-2.5 py-2 transition-colors duration-100 hover:bg-surface-hover"
    >
      <Knot size={22} state={engineState} seed={-7} />
      <div class="min-w-0">
        <div class="truncate text-[12px] font-[550]">{engineModel}</div>
        <div class="flex items-center gap-[5px] text-[10.5px] text-text-3">
          {ENGINE_LABEL[engineState]}{engineHost ? ` · ${engineHost}` : ""}
        </div>
      </div>
    </div>
    <ProfileSwitcher
      profiles={switcherProfiles}
      active={$activeProfile?.name ?? ""}
      onswitch={handleProfileSwitch}
    />
  </div>
</aside>

<style>
  /* Irreducible: the conversation list's custom scrollbar (`.conv-list`'s
     ::-webkit-scrollbar knobs) — utilities can't express pseudo-elements. */
  .lh-conv-list::-webkit-scrollbar {
    width: 8px;
  }
  .lh-conv-list::-webkit-scrollbar-thumb {
    background: var(--border-strong);
    border-radius: 999px;
    border: 2px solid transparent;
    background-clip: padding-box;
  }
</style>
