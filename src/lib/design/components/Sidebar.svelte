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
  import { PROFILES } from "../screens/shell-data";

  interface Props {
    active?: ScreenId;
    activeConv?: string;
    onnewsession?: () => void;
    /** Agent status shown by the Knot in the local-engine card. */
    engineState?: KnotState;
  }

  let { active, activeConv, onnewsession, engineState = "idle" }: Props =
    $props();

  let profile = $state("Personal");

  const SECTIONS: { id: ScreenId; label: string }[] = [
    { id: "email", label: "Email" },
    { id: "whiteboard", label: "Whiteboard" },
    { id: "files", label: "Files" },
    { id: "scheduled-jobs", label: "Scheduled jobs" },
  ];

  const PINNED: { title: string; route: Route | "auto"; meta: string }[] = [
    { title: "Reply to landlord", route: "auto", meta: "2m" },
    { title: "Blood panel summary", route: "blocked", meta: "1h" },
  ];
  const SESSIONS: { title: string; route: Route | "auto"; meta: string }[] = [
    { title: "Rust retry helper", route: "cloud", meta: "3h" },
    { title: "Trip planning — Kyoto", route: "auto", meta: "1d" },
    { title: "Skills for tax season", route: "cloud", meta: "1d" },
  ];

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
  {:else if id === "whiteboard"}
    <svg
      width="14"
      height="14"
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      stroke-width="1.8"
      class="shrink-0"
    >
      <rect x="3" y="4" width="18" height="13" rx="2" />
      <path d="M8 20l4-3 4 3" />
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

  <div class="px-2.5 pb-1 pt-2.5">
    <div
      class="flex cursor-text items-center gap-2 rounded-[var(--r)] border border-border bg-surface px-2.5 py-[6px] text-text-3"
    >
      <svg
        width="12"
        height="12"
        viewBox="0 0 24 24"
        fill="none"
        stroke="currentColor"
        stroke-width="2"
        class="shrink-0"
      >
        <circle cx="11" cy="11" r="7" />
        <path d="M20 20l-4-4" />
      </svg>
      <input
        placeholder="Search"
        class="w-full border-0 bg-transparent text-[12.5px] text-text outline-none placeholder:text-text-3"
      />
      <span
        class="rounded-[4px] border border-border px-[5px] py-px text-[10px] text-text-3"
        >⌘K</span
      >
    </div>
  </div>

  <div class="lh-conv-list flex-1 overflow-y-auto px-1.5 pb-2 pt-1">
    <div
      class="px-2.5 pb-1 pt-3 text-[10px] font-semibold uppercase tracking-[0.06em] text-text-3"
    >
      Pinned
    </div>
    {#each PINNED as c (c.title)}
      <ConversationRow
        title={c.title}
        route={c.route}
        meta={c.meta}
        active={activeConv === c.title}
        onclick={() => nav.go("main")}
      />
    {/each}

    <div
      class="flex items-center justify-between px-2.5 pb-1 pr-2 pt-3 text-[10px] font-semibold uppercase tracking-[0.06em] text-text-3"
    >
      Sessions
      <button
        type="button"
        aria-label="New session"
        onclick={() => (onnewsession ? onnewsession() : nav.go("empty"))}
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
    {#each SESSIONS as c (c.title)}
      <ConversationRow
        title={c.title}
        route={c.route}
        meta={c.meta}
        active={activeConv === c.title}
        onclick={() => nav.go("main")}
      />
    {/each}
  </div>

  <div class="relative flex-shrink-0 border-t border-border px-2.5 pb-2.5 pt-2">
    <div
      title="Local engine"
      class="mb-1.5 flex cursor-pointer items-center gap-[9px] rounded-[var(--r)] border border-border bg-surface px-2.5 py-2 transition-colors duration-100 hover:bg-surface-hover"
    >
      <Knot size={22} state={engineState} seed={-7} />
      <div class="min-w-0">
        <div class="text-[12px] font-[550]">Qwen3-14B</div>
        <div class="flex items-center gap-[5px] text-[10.5px] text-text-3">
          {ENGINE_LABEL[engineState]}
        </div>
      </div>
    </div>
    <ProfileSwitcher
      profiles={PROFILES}
      active={profile}
      onswitch={(name) => (profile = name)}
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
