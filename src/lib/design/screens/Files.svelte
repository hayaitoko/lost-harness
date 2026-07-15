<script lang="ts">
  // Files — workspace browser where every file carries its exposure: local only,
  // hard-blocked, or sent to cloud (the routing/meaning signal). Plus a 3-step
  // "Set up workspace" modal, and a success toast on finish.
  // Ported from ui/app/screens/Files.tsx.
  import { onDestroy, type Snippet } from "svelte";
  import { nav } from "$lib/design/nav.svelte";
  import type { Route } from "$lib/design/types";
  import Sidebar from "../components/Sidebar.svelte";
  import AppStatusBar from "../components/AppStatusBar.svelte";
  import Button from "../components/Button.svelte";
  import IconButton from "../components/IconButton.svelte";
  import Toggle from "../components/Toggle.svelte";

  // The row grid: name (flex) · exposure · modified · size.
  const GRID = "grid-cols-[minmax(0,1fr)_230px_110px_64px]";

  type Exposure = Route; // "local" | "cloud" | "blocked" — the routing signal.
  type FileRow = {
    name: string;
    kind: "folder" | "doc";
    exposureLabel: string;
    exposure: Exposure;
    pill?: string;
    modified: string;
    size: string;
    openable?: boolean;
  };

  const FILES: FileRow[] = [
    { name: "notes", kind: "folder", exposureLabel: "Local only", exposure: "local", modified: "Today", size: "—" },
    { name: "taxes-2025", kind: "folder", exposureLabel: "Local only", exposure: "local", modified: "Jun 30", size: "—" },
    { name: "heater-reply.md", kind: "doc", exposureLabel: "Never left this Mac", exposure: "local", modified: "Today 9:12", size: "2 KB", openable: true },
    { name: "lab-results.pdf", kind: "doc", exposureLabel: "Hard-blocked", exposure: "blocked", pill: "health", modified: "Jul 8", size: "1.2 MB", openable: true },
    { name: "kyoto-itinerary.md", kind: "doc", exposureLabel: "Sent to cloud · Jul 10", exposure: "cloud", modified: "Jul 10", size: "6 KB", openable: true },
    { name: "retry_helper.rs", kind: "doc", exposureLabel: "Sent to cloud · Jul 11", exposure: "cloud", modified: "Jul 11", size: "4 KB", openable: true },
  ];

  // Exposure dot color — the one place saturated color is allowed (routing signal).
  const DOT: Record<Exposure, string> = {
    local: "bg-local",
    cloud: "bg-cloud",
    blocked: "bg-blocked",
  };

  let setupOpen = $state(false);
  let step = $state(1);
  let idxLocal = $state(true);
  let edLive = $state(true);
  let edLines = $state(true);
  let toastVisible = $state(false);
  let toastTimer: ReturnType<typeof setTimeout> | undefined;

  onDestroy(() => clearTimeout(toastTimer));

  function openSetup() {
    step = 1;
    setupOpen = true;
  }

  function finishSetup() {
    setupOpen = false;
    toastVisible = true;
    clearTimeout(toastTimer);
    toastTimer = setTimeout(() => (toastVisible = false), 3200);
  }
</script>

{#snippet folderIcon()}
  <svg
    width="15"
    height="15"
    viewBox="0 0 24 24"
    fill="none"
    stroke="currentColor"
    stroke-width="1.7"
    class="shrink-0 text-text-3"
  >
    <path
      d="M3 8a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2Z"
    />
  </svg>
{/snippet}

{#snippet docIcon()}
  <svg
    width="15"
    height="15"
    viewBox="0 0 24 24"
    fill="none"
    stroke="currentColor"
    stroke-width="1.7"
    class="shrink-0 text-text-3"
  >
    <path d="M6 2h9l5 5v15H6z" />
    <path d="M15 2v5h5" />
  </svg>
{/snippet}

{#snippet settingsIcon()}
  <svg
    width="16"
    height="16"
    viewBox="0 0 24 24"
    fill="none"
    stroke="currentColor"
    stroke-width="1.8"
  >
    <circle cx="12" cy="12" r="3" />
    <path
      d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1Z"
    />
  </svg>
{/snippet}

{#snippet closeIcon()}
  <svg
    width="14"
    height="14"
    viewBox="0 0 24 24"
    fill="none"
    stroke="currentColor"
    stroke-width="2"
  >
    <path d="M6 6l12 12M18 6 6 18" />
  </svg>
{/snippet}

<div class="grid h-screen grid-cols-[260px_1fr]">
  <Sidebar active="files" />

  <main class="flex min-h-0 min-w-0 flex-col">
    <div
      class="flex h-12 flex-shrink-0 items-center gap-3 border-b border-border pl-[18px] pr-[14px]"
    >
      <div class="min-w-0 truncate text-[13.5px] font-semibold">Files</div>
      <span class="text-[12px] text-text-3">workspace</span>
      <div class="flex-1"></div>
      <div class="flex flex-shrink-0 items-center gap-1">
        <Button variant="ghost" onclick={openSetup}>Set up workspace</Button>
        <Button>New folder</Button>
        <Button variant="primary">Upload</Button>
        <IconButton label="Settings" onclick={() => nav.go("settings")}>
          {@render settingsIcon()}
        </IconButton>
      </div>
    </div>

    <div class="min-h-0 flex-1 overflow-y-auto">
      <div class="mx-auto max-w-[860px] px-6 pb-12 pt-[22px]">
        <div
          class="grid {GRID} items-center gap-3 border-b border-border px-3 pb-2 text-[10.5px] font-semibold uppercase tracking-[0.06em] text-text-3"
        >
          <span>Name</span>
          <span>Exposure</span>
          <span>Modified</span>
          <span class="text-right">Size</span>
        </div>

        <div class="flex flex-col">
          {#each FILES as f (f.name)}
            <button
              type="button"
              onclick={() => f.openable && nav.go("editor")}
              class="grid {GRID} cursor-pointer items-center gap-3 rounded-[var(--r)] px-3 py-2.5 text-left hover:bg-surface-hover"
            >
              <span
                class="flex min-w-0 items-center gap-2.5 text-[13px] {f.openable
                  ? 'font-normal'
                  : 'font-medium'}"
              >
                {#if f.kind === "folder"}
                  {@render folderIcon()}
                {:else}
                  {@render docIcon()}
                {/if}
                <span class="truncate">{f.name}</span>
              </span>
              <span
                class="inline-flex items-center gap-[7px] text-[11.5px] text-text-2"
              >
                <span
                  class="h-[7px] w-[7px] flex-none rounded-full {DOT[f.exposure]}"
                ></span>
                {f.exposureLabel}
                {#if f.pill}
                  <span
                    class="rounded-lg bg-blocked-soft px-[7px] py-px text-[10px] text-blocked"
                    >{f.pill}</span
                  >
                {/if}
              </span>
              <span class="text-[12px] text-text-3">{f.modified}</span>
              <span class="text-right text-[12px] text-text-3">{f.size}</span>
            </button>
          {/each}
        </div>

        <div class="mt-3.5 px-3 text-[11.5px] text-text-3">
          6 items · 2 touched the cloud · exposure is per-file and permanent in
          the log
        </div>
      </div>
    </div>

    <AppStatusBar session="0:12" />
  </main>
</div>

{#if setupOpen}
  <div
    class="fixed inset-0 z-[80] grid place-items-center bg-black/45 backdrop-blur-[3px]"
  >
    <div
      class="w-[500px] overflow-hidden rounded-[var(--r-lg)] border border-border-strong bg-surface shadow-[var(--shadow-pop)]"
    >
      <div
        class="flex items-center gap-2.5 border-b border-border px-4 py-[13px]"
      >
        <span class="text-[13px] font-semibold">Set up workspace</span>
        <span class="text-[11.5px] text-text-3">Step {step} of 3</span>
        <div class="flex-1"></div>
        <IconButton label="Close" onclick={() => (setupOpen = false)}>
          {@render closeIcon()}
        </IconButton>
      </div>

      <div class="min-h-[196px] p-4">
        {#if step === 1}
          <div class="mb-1.5 text-[12.5px] font-semibold">
            Choose a workspace folder
          </div>
          <div class="mb-3 text-[12px] text-text-3">
            Lost Harness can read and edit files here. Nothing outside this
            folder is touched.
          </div>
          <div
            class="flex items-center gap-2.5 rounded-[var(--r)] border border-border bg-surface-2 px-3 py-2.5"
          >
            {@render folderIcon()}
            <span class="flex-1 font-mono text-[12.5px]">~/Documents/workspace</span>
            <Button variant="ghost">Change…</Button>
          </div>
        {:else if step === 2}
          <div class="mb-1.5 text-[12.5px] font-semibold">Indexing</div>
          <div class="mb-3 text-[12px] text-text-3">
            The index makes search and chat-about-your-files instant.
          </div>
          <div class="flex flex-col gap-2">
            <div
              class="flex items-center justify-between gap-2.5 rounded-[var(--r)] bg-surface-2 px-3 py-2.5"
            >
              <span class="text-[12.5px]">Index files locally</span>
              <Toggle
                checked={idxLocal}
                onchange={(v) => (idxLocal = v)}
                label="Index files locally"
              />
            </div>
            <div
              class="flex items-center justify-between gap-2.5 rounded-[var(--r)] bg-surface-2 px-3 py-2.5"
            >
              <span class="text-[12.5px]">
                Index into cloud models
                <span class="text-text-3">— never available</span>
              </span>
              <Toggle checked={false} locked label="Index into cloud models" />
            </div>
          </div>
        {:else}
          <div class="mb-1.5 text-[12.5px] font-semibold">Editor defaults</div>
          <div class="mb-3 text-[12px] text-text-3">
            Applies to the built-in editor for every file type.
          </div>
          <div class="flex flex-col gap-2">
            <div
              class="flex items-center justify-between gap-2.5 rounded-[var(--r)] bg-surface-2 px-3 py-2.5"
            >
              <span class="text-[12.5px]">
                Live markdown preview while typing
              </span>
              <Toggle
                checked={edLive}
                onchange={(v) => (edLive = v)}
                label="Live markdown preview while typing"
              />
            </div>
            <div
              class="flex items-center justify-between gap-2.5 rounded-[var(--r)] bg-surface-2 px-3 py-2.5"
            >
              <span class="text-[12.5px]">Line numbers in code files</span>
              <Toggle
                checked={edLines}
                onchange={(v) => (edLines = v)}
                label="Line numbers in code files"
              />
            </div>
          </div>
        {/if}
      </div>

      <div class="flex items-center gap-2 border-t border-border px-4 py-3">
        {#if step > 1}
          <Button variant="ghost" onclick={() => (step = Math.max(1, step - 1))}>
            Back
          </Button>
        {/if}
        <div class="flex-1"></div>
        {#if step < 3}
          <Button variant="primary" onclick={() => (step = Math.min(3, step + 1))}>
            Next
          </Button>
        {:else}
          <Button variant="primary" onclick={finishSetup}>Finish</Button>
        {/if}
      </div>
    </div>
  </div>
{/if}

{#if toastVisible}
  <div class="fixed bottom-11 left-1/2 z-[90] -translate-x-1/2">
    <div
      class="flex max-w-[340px] items-center gap-2 rounded-[var(--r)] border border-border-strong bg-surface px-3.5 py-[9px] text-[12.5px] text-text shadow-[var(--shadow-pop)]"
    >
      <span class="h-1.5 w-1.5 flex-shrink-0 rounded-full bg-accent"></span>
      <span>Workspace ready — indexing locally</span>
    </div>
  </div>
{/if}
