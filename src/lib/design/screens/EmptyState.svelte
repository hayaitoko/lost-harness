<script lang="ts">
  // Empty State — the nothing-open home. Ported from EmptyState.tsx.
  // The `.app` grid + `.main` column are reproduced inline (this is the shell
  // frame); everything else is the centered hero + composer. Sample data /
  // local $state only — no backend wiring this pass.
  import Sidebar from "../components/Sidebar.svelte";
  import AppStatusBar from "../components/AppStatusBar.svelte";
  import ModelPicker from "../components/ModelPicker.svelte";
  import { nav } from "$lib/design/nav.svelte";
  import { MODELS } from "./shell-data";

  let model = $state("Qwen3-14B");
  let modelState = $state<"warm" | "asleep">("warm");
  let taRef = $state<HTMLTextAreaElement>();

  const isWarm = $derived(modelState === "warm");
  const goMain = () => nav.go("main");

  function onComposerKeydown(e: KeyboardEvent) {
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      goMain();
    }
  }
</script>

<div class="grid h-screen grid-cols-[260px_1fr_0]">
  <Sidebar onnewsession={() => taRef?.focus()} />

  <main class="relative flex min-h-0 min-w-0 flex-col">
    <div class="absolute right-[10px] top-[10px] z-[5]">
      <button
        type="button"
        aria-label="Settings"
        class="relative grid h-[30px] w-[30px] place-items-center rounded-[var(--r)] border border-transparent bg-transparent text-text-3 transition hover:bg-surface-hover hover:text-text-2"
        onclick={() => nav.go("settings")}
      >
        <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
          <circle cx="12" cy="12" r="3" />
          <path
            d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1Z"
          />
        </svg>
      </button>
    </div>

    <div class="flex min-h-0 flex-1 items-center justify-center overflow-y-auto">
      <div
        class="flex w-[min(620px,calc(100%-72px))] flex-col items-center pb-[9vh] pt-12 text-center"
      >
        <div
          class="mb-4 grid h-9 w-9 place-items-center rounded-full border border-border-strong"
        >
          <span
            class="h-2 w-2 rounded-full {isWarm ? 'bg-local lh-pulse' : 'bg-text-3'}"
          ></span>
        </div>

        <div class="text-[16px] font-[650] tracking-[-.012em]">Nothing open</div>
        <div class="mt-1.5 max-w-[400px] text-[12.5px] leading-[1.55] text-text-2">
          Pick up a session from the sidebar, or start fresh — it stays on this Mac
          unless you route it elsewhere.
        </div>

        <div
          class="mt-6 w-full rounded-[var(--r-lg)] border border-border-strong bg-surface text-left shadow-[var(--shadow)] transition-[border-color] duration-[.12s] focus-within:border-[color-mix(in_srgb,var(--accent)_45%,var(--border-strong))]"
        >
          <div class="flex items-center gap-2 py-[7px] pl-3 pr-[7px]">
            <button
              type="button"
              aria-label="Attach"
              class="grid h-[30px] w-[30px] flex-none place-items-center self-end rounded-[var(--r)] bg-transparent text-text-3 hover:bg-surface-hover hover:text-text-2"
            >
              <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
                <path d="M12 5v14M5 12h14" />
              </svg>
            </button>
            <textarea
              bind:this={taRef}
              rows="1"
              placeholder="Message Lost Harness…"
              onkeydown={onComposerKeydown}
              class="max-h-[150px] min-w-0 flex-1 resize-none border-0 bg-transparent py-[5px] text-[14px] leading-[1.55] text-text outline-none placeholder:text-text-3"
            ></textarea>
            <span class="flex-none whitespace-nowrap">
              <ModelPicker models={MODELS} value={model} onchange={(v) => (model = v)} />
            </span>
            <button
              type="button"
              aria-label="Voice input"
              class="grid h-[30px] w-[30px] place-items-center self-end rounded-[var(--r)] bg-transparent text-text-3 hover:bg-surface-hover hover:text-text-2"
            >
              <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
                <path d="M12 2a3 3 0 0 0-3 3v6a3 3 0 0 0 6 0V5a3 3 0 0 0-3-3Z" />
                <path d="M19 10v1a7 7 0 0 1-14 0v-1M12 18v4M8 22h8" />
              </svg>
            </button>
            <button
              type="button"
              aria-label="Send"
              onclick={goMain}
              class="grid h-[30px] w-[30px] place-items-center self-end rounded-[var(--r)] border-0 bg-accent text-on-accent transition duration-[.12s] hover:brightness-[1.06]"
            >
              <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
                <path d="M22 2 11 13M22 2l-7 20-4-9-9-4 20-7Z" />
              </svg>
            </button>
          </div>
        </div>

        <div class="mt-2.5 text-[11.5px] text-text-3">
          {isWarm
            ? "Qwen3-14B is warm on tadashi — replies start instantly."
            : "Qwen3-14B is asleep — your first message wakes it (~4 s)."}
        </div>

        <div
          class="mt-[30px] flex flex-wrap items-center justify-center gap-[14px] text-[11.5px] text-text-3"
        >
          <span class="inline-flex items-center gap-1.5">
            <span
              class="rounded-[4px] border border-border px-[5px] py-px text-[10px] text-text-3"
              >⌘K</span
            >Search
          </span>
          <span class="h-3 w-px bg-border"></span>
          <span class="inline-flex items-center gap-1.5">
            <span
              class="rounded-[4px] border border-border px-[5px] py-px text-[10px] text-text-3"
              >⌘N</span
            >New session
          </span>
          <span class="h-3 w-px bg-border"></span>
          <span>Drop files anywhere to add them</span>
        </div>
      </div>
    </div>

    <AppStatusBar session="0:00" />
  </main>
</div>

<style>
  /* Irreducible: the warm-indicator keyframe (mirrors the design's `lhpulse`). */
  .lh-pulse {
    animation: lhpulse 3.2s ease-in-out infinite;
  }
  @keyframes lhpulse {
    0%,
    100% {
      opacity: 1;
    }
    50% {
      opacity: 0.4;
    }
  }
</style>
