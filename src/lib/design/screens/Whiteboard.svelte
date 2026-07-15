<script lang="ts">
  // Whiteboard — dot-grid canvas with a tool rail, frames + sticky notes, a
  // local "Board summary" card, and a board-chat panel where the assistant's
  // strokes are labeled. New-board setup modal.
  //
  // The canvas (frames, sticky notes, connector, floating cards) is bespoke
  // template chrome with no library component — reproduced as absolute-positioned
  // markup with Tailwind utilities. The left rail uses the shared Sidebar (its
  // "+" opens the New-board modal here).
  import Sidebar from "../components/Sidebar.svelte";
  import AppStatusBar from "../components/AppStatusBar.svelte";
  import Button from "../components/Button.svelte";
  import IconButton from "../components/IconButton.svelte";
  import RoutingBadge from "../components/RoutingBadge.svelte";
  import Toggle from "../components/Toggle.svelte";
  import BindingControl from "../components/BindingControl.svelte";
  import ChatMessage from "../components/ChatMessage.svelte";
  import { nav } from "$lib/design/nav.svelte";
  import type { Binding } from "$lib/design/types";

  let setupOpen = $state(false);
  let step = $state(1);
  let aiView = $state(true);
  let aiDraw = $state(true);
  let aiMove = $state(false);
  let boardBinding = $state<Binding>("private");
  let toastVisible = $state(false);
  let toastTimer: ReturnType<typeof setTimeout> | undefined;

  $effect(() => () => clearTimeout(toastTimer));

  function finishSetup() {
    setupOpen = false;
    toastVisible = true;
    clearTimeout(toastTimer);
    toastTimer = setTimeout(() => (toastVisible = false), 3200);
  }

  const TOOLS: { label: string; on?: boolean }[] = [
    { label: "Select" },
    { label: "Pen" },
    { label: "Sticky note", on: true },
    { label: "Text" },
    { label: "Shape" },
    { label: "Connector" },
  ];

  const railBtn =
    "grid h-8 w-8 place-items-center rounded-[var(--r)] border-0 cursor-pointer transition-colors";
  const note =
    "absolute w-[150px] rounded-[2px] border border-border-strong bg-surface-2 p-3 text-[13px] shadow-[var(--shadow)]";
  const handle = "absolute h-2 w-2 border-[1.5px] border-accent bg-bg";
  const settingRow =
    "flex items-center justify-between gap-2.5 rounded-[var(--r)] bg-surface-2 px-3 py-2.5";
  const modalInput =
    "w-full rounded-[var(--r)] border border-border bg-surface-2 px-[11px] py-[9px] text-[13px] text-text outline-none";
  const zoomBtn =
    "grid h-[26px] w-[26px] place-items-center rounded-[5px] border-0 bg-transparent text-text-2 cursor-pointer";
</script>

{#snippet toolIcon(label: string)}
  {#if label === "Select"}
    <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><path d="M5 3l7 17 2.5-7 7-2.5z" /></svg>
  {:else if label === "Pen"}
    <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><path d="M5 19l1-4L16 5l3 3L9 18z" /></svg>
  {:else if label === "Sticky note"}
    <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><path d="M4 4h16v10l-6 6H4z" /><path d="M14 20v-6h6" /></svg>
  {:else if label === "Text"}
    <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><path d="M5 6V4h14v2M12 4v16M9 20h6" /></svg>
  {:else if label === "Shape"}
    <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><rect x="3" y="3" width="12" height="12" rx="1" /><circle cx="16" cy="16" r="5" /></svg>
  {:else}
    <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><path d="M5 19C10 19 14 5 19 5" /><path d="M15 5h4v4" /></svg>
  {/if}
{/snippet}

<div class="grid h-screen grid-cols-[260px_minmax(0,1fr)_320px]">
  <Sidebar active="whiteboard" onnewsession={() => { step = 1; setupOpen = true; }} />

  <main class="flex min-h-0 min-w-0 flex-col">
    <div class="flex h-12 flex-shrink-0 items-center gap-3 border-b border-border pl-[18px] pr-[14px]">
      <div class="min-w-0 overflow-hidden text-ellipsis whitespace-nowrap text-[13.5px] font-semibold">
        Kyoto trip — planning
      </div>
      <span class="text-[11.5px] text-text-3">Edited 2m ago</span>
      <div class="flex-1"></div>
      <div class="flex flex-shrink-0 items-center gap-1">
        <Button>Share</Button>
        <IconButton label="Export">
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><path d="M12 3v12M7 10l5 5 5-5" /><path d="M4 21h16" /></svg>
        </IconButton>
        <IconButton label="Settings" onclick={() => nav.go("settings")}>
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M4 7h16M4 12h16M4 17h16" /><circle cx="9" cy="7" r="2" fill="var(--surface)" /><circle cx="15" cy="12" r="2" fill="var(--surface)" /><circle cx="8" cy="17" r="2" fill="var(--surface)" /></svg>
        </IconButton>
      </div>
    </div>

    <div
      class="relative flex-1 overflow-hidden"
      style="background: radial-gradient(circle, var(--border-strong) 1px, transparent 1.4px) 0 0 / 22px 22px;"
    >
      <!-- Tool rail -->
      <div
        class="absolute left-[14px] top-1/2 z-10 flex -translate-y-1/2 flex-col gap-0.5 rounded-[var(--r-lg)] border border-border-strong bg-surface p-[5px] shadow-[var(--shadow-pop)]"
      >
        {#each TOOLS as t (t.label)}
          <button
            type="button"
            aria-label={t.label}
            aria-pressed={t.on}
            class="{railBtn} {t.on ? 'bg-accent-soft text-accent' : 'bg-transparent text-text-2 hover:bg-surface-hover'}"
          >
            {@render toolIcon(t.label)}
          </button>
        {/each}
      </div>

      <!-- Day 1 frame -->
      <div class="absolute left-[110px] top-[56px] h-[300px] w-[470px] rounded-[8px] border-[1.5px] border-dashed border-border-strong">
        <span class="absolute -top-[22px] left-0.5 text-[11px] font-semibold uppercase tracking-[0.05em] text-text-3">
          Day 1 — Arrival
        </span>
      </div>

      <!-- Sticky notes -->
      <div class="{note} left-[140px] top-[96px] min-h-[140px] rotate-[-1.2deg]">
        Land HND 14:05
        <div class="mt-[26px] text-[10.5px] text-text-3">Alex</div>
      </div>
      <div class="{note} left-[330px] top-[120px] min-h-[140px] !border-[1.5px] !border-accent">
        Train: Haneda → Kyoto, 15:40
        <div class="mt-3 text-[10.5px] text-text-3">Alex</div>
        <span class="{handle} -left-[5px] -top-[5px]"></span>
        <span class="{handle} -right-[5px] -top-[5px]"></span>
        <span class="{handle} -bottom-[5px] -left-[5px]"></span>
        <span class="{handle} -bottom-[5px] -right-[5px]"></span>
      </div>
      <div class="{note} left-[160px] top-[400px] min-h-[120px] rotate-[0.8deg]">
        Ryokan check-in after 16:00
        <div class="mt-3 text-[10.5px] text-text-3">Nina</div>
      </div>
      <div class="{note} left-[370px] top-[410px] min-h-[120px] rotate-[-0.6deg]">
        Fushimi Inari — go early, before 8am
        <div class="mt-3 text-[10.5px] text-text-3">Nina</div>
      </div>

      <!-- Connector -->
      <svg
        class="pointer-events-none absolute left-[405px] top-[262px] overflow-visible"
        width="60"
        height="150"
        viewBox="0 0 60 150"
        fill="none"
        stroke="var(--text-3)"
        stroke-width="1.5"
      >
        <path d="M0 0C30 50 30 100 30 140" />
        <path d="M24 132l6 9 6-9" />
      </svg>

      <!-- Board summary card -->
      <div class="absolute right-[36px] top-[64px] w-[270px] rounded-[var(--r-lg)] border border-border-strong bg-surface shadow-[var(--shadow-pop)]">
        <div class="flex items-center gap-2 border-b border-border px-[13px] py-2.5">
          <span class="text-[12px] font-semibold">Board summary</span>
          <RoutingBadge route="local" label="Local · Qwen3-14B" />
        </div>
        <div class="px-[13px] py-3 text-[12.5px] text-text-2">
          <p class="mb-2">Day 1 is fully planned: arrival, transfer, and check-in all connect. Two gaps to resolve:</p>
          <p class="mb-1">· No dinner plan near the ryokan</p>
          <p class="m-0">· Fushimi Inari is on Day 1 but check-in is 16:00 — likely a Day 2 item</p>
        </div>
        <div class="flex gap-1.5 border-t border-border px-[13px] py-[9px]">
          <Button variant="ghost">Refresh</Button>
          <Button variant="ghost">Add as note</Button>
        </div>
      </div>

      <!-- Storage pill -->
      <div class="absolute bottom-[14px] left-[14px] flex items-center gap-[7px] rounded-[14px] border border-border bg-surface px-[11px] py-[5px] text-[11px] text-text-2">
        <span class="h-[7px] w-[7px] rounded-full bg-local"></span>
        Board stored on this Mac
      </div>

      <!-- Zoom control -->
      <div class="absolute bottom-[14px] right-[14px] flex items-center gap-0.5 rounded-[var(--r-lg)] border border-border-strong bg-surface p-[3px] shadow-[var(--shadow)]">
        <button type="button" aria-label="Zoom out" class={zoomBtn}>
          <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M5 12h14" /></svg>
        </button>
        <span class="min-w-[38px] text-center text-[11.5px] text-text-2">100%</span>
        <button type="button" aria-label="Zoom in" class={zoomBtn}>
          <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M12 5v14M5 12h14" /></svg>
        </button>
      </div>
    </div>

    <AppStatusBar binding="private" session="0:08" />
  </main>

  <!-- Board chat (3rd column) -->
  <aside class="flex min-h-0 min-w-0 flex-col border-l border-border bg-sidebar">
    <div class="flex h-12 flex-shrink-0 items-center gap-2 border-b border-border px-[14px]">
      <span class="text-[12.5px] font-semibold">Board chat</span>
      <RoutingBadge route="local" label="Local · Qwen3-14B" />
    </div>
    <div class="flex flex-1 flex-col gap-2.5 overflow-y-auto px-3 py-4">
      <ChatMessage role="user">Group Day 1 by time and flag anything that doesn't fit.</ChatMessage>
      <ChatMessage role="assistant">
        {#snippet badge()}
          <RoutingBadge route="local" label="Local · Qwen3-14B" />
        {/snippet}
        <p>Done — the four notes are now in chronological order inside the Day 1 frame.</p>
        <p>
          One flag: <b>Fushimi Inari before 8am</b> can't happen on arrival day (check-in is 16:00). I've pulled it out of the frame — likely Day 2.
        </p>
      </ChatMessage>
      <div class="flex items-center gap-2 rounded-[var(--r)] border border-border bg-surface px-[11px] py-[7px] text-[11.5px] text-text-2">
        <svg class="flex-[0_0_auto]" width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><path d="M5 19l1-4L16 5l3 3L9 18z" /></svg>
        Drew on board — moved 1 note, aligned 4
      </div>
      <ChatMessage role="user">Add a note to find dinner near the ryokan.</ChatMessage>
    </div>
    <div class="flex-shrink-0 border-t border-border p-2.5">
      <div class="flex items-center gap-1.5 rounded-[var(--r-lg)] border border-border-strong bg-surface py-[5px] pl-[11px] pr-[5px] shadow-[var(--shadow)]">
        <input
          placeholder="Ask about this board…"
          class="min-w-0 flex-1 border-0 bg-transparent text-[12.5px] text-text outline-none"
        />
        <button type="button" aria-label="Send" class="grid h-[26px] w-[26px] flex-[0_0_auto] cursor-pointer place-items-center rounded-[var(--r)] border-0 bg-accent text-on-accent">
          <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M12 19V5M5 12l7-7 7 7" /></svg>
        </button>
      </div>
      <div class="px-0.5 pt-[7px] text-[10.5px] text-text-3">
        The assistant can view and draw on this board. Strokes it makes are labeled.
      </div>
    </div>
  </aside>
</div>

{#if setupOpen}
  <div class="fixed inset-0 z-[80] grid place-items-center bg-black/45 backdrop-blur-[3px]">
    <div class="w-[500px] overflow-hidden rounded-[var(--r-lg)] border border-border-strong bg-surface shadow-[var(--shadow-pop)]">
      <div class="flex items-center gap-2.5 border-b border-border px-4 py-[13px]">
        <span class="text-[13px] font-semibold">New board</span>
        <span class="text-[11.5px] text-text-3">Step {step} of 3</span>
        <div class="flex-1"></div>
        <IconButton label="Close" onclick={() => (setupOpen = false)}>
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M6 6l12 12M18 6 6 18" /></svg>
        </IconButton>
      </div>

      <div class="min-h-[196px] p-4">
        {#if step === 1}
          <div class="mb-1.5 text-[12.5px] font-semibold">Name your board</div>
          <div class="mb-3 text-[12px] text-text-3">
            A short name and what it's for — the assistant uses this as context.
          </div>
          <input placeholder="Board name" class={modalInput} />
          <textarea rows="2" placeholder="What is this board for? (optional)" class="{modalInput} mt-2 resize-none"></textarea>
        {:else if step === 2}
          <div class="mb-1.5 text-[12.5px] font-semibold">AI collaborator</div>
          <div class="mb-3 text-[12px] text-text-3">
            What the assistant may do on this board. Everything it does is labeled and undoable.
          </div>
          <div class="flex flex-col gap-2">
            <div class={settingRow}>
              <span class="text-[12.5px]">View the board</span>
              <Toggle checked={aiView} onchange={(v) => (aiView = v)} />
            </div>
            <div class={settingRow}>
              <span class="text-[12.5px]">Draw and add notes</span>
              <Toggle checked={aiDraw} onchange={(v) => (aiDraw = v)} />
            </div>
            <div class={settingRow}>
              <span class="text-[12.5px]">Rearrange existing items</span>
              <Toggle checked={aiMove} onchange={(v) => (aiMove = v)} />
            </div>
          </div>
        {:else}
          <div class="mb-1.5 text-[12.5px] font-semibold">Storage &amp; routing</div>
          <div class="mb-3 text-[12px] text-text-3">
            Boards are files on this Mac. The binding controls what the assistant may do with their contents.
          </div>
          <BindingControl value={boardBinding} onchange={(b) => (boardBinding = b)} />
          <div class="mt-3.5 flex items-center gap-2 rounded-[var(--r)] bg-surface-2 px-3 py-2.5 text-[12px] text-text-2">
            <span class="h-[7px] w-[7px] flex-[0_0_auto] rounded-full bg-local"></span>
            Stored at ~/Documents/workspace/boards — synced to your other devices over your LAN only.
          </div>
        {/if}
      </div>

      <div class="flex items-center gap-2 border-t border-border px-4 py-3">
        {#if step > 1}
          <Button variant="ghost" onclick={() => (step = Math.max(1, step - 1))}>Back</Button>
        {/if}
        <div class="flex-1"></div>
        {#if step < 3}
          <Button variant="primary" onclick={() => (step = Math.min(3, step + 1))}>Next</Button>
        {:else}
          <Button variant="primary" onclick={finishSetup}>Create board</Button>
        {/if}
      </div>
    </div>
  </div>
{/if}

{#if toastVisible}
  <div class="fixed bottom-11 left-1/2 z-[90] -translate-x-1/2">
    <div class="flex max-w-[340px] items-center gap-2 rounded-[var(--r)] border border-border-strong bg-surface px-3.5 py-[9px] text-[12.5px] text-text shadow-[var(--shadow-pop)]">
      <span class="h-1.5 w-1.5 flex-shrink-0 rounded-full bg-accent"></span>
      <span>Board created — stored on this Mac</span>
    </div>
  </div>
{/if}
