<script lang="ts">
  // Onboarding — first-run, three-step setup wrapped in AppFrame:
  //   1. Hardware detect  — read-only RAM / GPU / CPU rows + which model tier fits
  //   2. Model catalog    — downloadable local models with size + "fits your machine"
  //   3. Seat assignment  — bind a local model to Writer / Reviewer / Coding
  // Chrome stays grayscale; the only saturated color is the routing signal — a
  // `local` green dot/tag meaning "this model runs on your device." Reproduces the
  // design's `.ob-*`, `.catalog`, `.cat-*`, `.seat-*` selectors as Tailwind utilities.
  import AppFrame from "../components/AppFrame.svelte";
  import Button from "../components/Button.svelte";
  import RouteDot from "../components/RouteDot.svelte";
  import SegmentedControl from "../components/SegmentedControl.svelte";
  import { nav } from "$lib/design/nav.svelte";

  interface Props {
    /** Called when the user finishes or skips first-run setup. Defaults to nav.go('main'). */
    ondone?: () => void;
  }

  let { ondone }: Props = $props();
  const done = () => (ondone ? ondone() : nav.go("main"));

  const STEPS = ["Hardware", "Models", "Seats"] as const;

  type HwRow = { k: string; v: string };
  const HARDWARE: HwRow[] = [
    { k: "Memory", v: "64 GB" },
    { k: "GPU", v: "Apple M3 Max" },
    { k: "CPU", v: "14-core" },
  ];
  /** Which local tier the detected machine can run — capability, not a routing claim. */
  const TIER = "Fits models up to ~30B at 4-bit, comfortably on-device.";

  type CatalogModel = { name: string; params: string; size: string; fits: boolean };
  const CATALOG: CatalogModel[] = [
    { name: "Qwen3-14B", params: "Instruct · 4-bit", size: "9.0 GB", fits: true },
    { name: "Llama 3.3 8B", params: "Instruct · 4-bit", size: "4.7 GB", fits: true },
    { name: "Qwen3-32B", params: "Instruct · 4-bit", size: "20 GB", fits: true },
    { name: "Llama 3.3 70B", params: "Instruct · 4-bit", size: "40 GB", fits: false },
  ];

  type Seat = "writer" | "reviewer" | "coding";
  const SEATS: { key: Seat; role: string; sub: string }[] = [
    { key: "writer", role: "Writer", sub: "Drafts and replies — the default voice" },
    { key: "reviewer", role: "Reviewer", sub: "Second-pass edits and critique" },
    { key: "coding", role: "Coding", sub: "Code, diffs, and tool calls" },
  ];

  let step = $state(0);
  // downloaded / selected local models (step 2)
  let downloaded = $state<Set<string>>(new Set(["Qwen3-14B"]));
  // seat -> model name (step 3); only downloaded local models are assignable
  let seats = $state<Record<Seat, string>>({
    writer: "Qwen3-14B",
    reviewer: "Qwen3-14B",
    coding: "Qwen3-14B",
  });

  function toggleDownload(name: string) {
    const next = new Set(downloaded);
    if (next.has(name)) {
      next.delete(name);
      // drop any seat that pointed at a now-removed model
      const cleaned = { ...seats };
      for (const k of Object.keys(cleaned) as Seat[]) {
        if (cleaned[k] === name) cleaned[k] = "";
      }
      seats = cleaned;
    } else {
      next.add(name);
    }
    downloaded = next;
  }

  let seatOptions = $derived(
    Array.from(downloaded).map((name) => ({ value: name, label: name }))
  );
  let isLast = $derived(step === STEPS.length - 1);
</script>

<AppFrame>
  <div class="flex min-h-[calc(100vh-48px)] flex-col">
    <!-- step / dot rail -->
    <div class="flex items-center justify-center gap-2 p-5">
      {#each STEPS as label, i (label)}
        {#if i > 0}
          <span class="h-px w-9 bg-border-strong"></span>
        {/if}
        <div
          class="flex items-center gap-[7px] text-[11.5px] {i <= step
            ? 'text-text'
            : 'text-text-3'}"
        >
          <span
            class="grid h-5 w-5 place-items-center rounded-full border text-[11px] font-[650] {i <=
            step
              ? 'border-transparent bg-accent text-on-accent'
              : 'border-border bg-surface-2'}"
          >
            {i + 1}
          </span>
          {label}
        </div>
      {/each}
    </div>

    <!-- current step -->
    <div class="grid flex-1 place-items-center overflow-y-auto px-5 pb-5">
      {#if step === 0}
        <div class="w-[min(560px,94vw)] text-center">
          <h1 class="mb-2 text-[22px] font-[650] tracking-[-0.01em]">
            Let's meet your machine
          </h1>
          <p class="mx-auto mb-[22px] max-w-[400px] text-[14px] leading-[1.55] text-text-2">
            Lost Harness read your hardware so it can keep as much as possible on this Mac.
          </p>
          <div class="mb-[22px] flex justify-center gap-2.5">
            {#each HARDWARE as h (h.k)}
              <div class="min-w-[120px] rounded-[var(--r)] border border-border bg-surface px-4 py-3">
                <div class="text-[10.5px] uppercase tracking-[0.05em] text-text-3">{h.k}</div>
                <div class="mt-[3px] text-[15px] font-semibold">{h.v}</div>
              </div>
            {/each}
          </div>
          <div
            class="inline-flex items-center gap-2 rounded-[var(--r)] bg-local-soft px-3 py-2 text-[12.5px] font-[550] text-local"
          >
            <RouteDot route="local" />
            {TIER}
          </div>
        </div>
      {:else if step === 1}
        <div class="w-[min(560px,94vw)] text-center">
          <h1 class="mb-2 text-[22px] font-[650] tracking-[-0.01em]">Pick your local models</h1>
          <p class="mx-auto mb-[22px] max-w-[400px] text-[14px] leading-[1.55] text-text-2">
            These download to this Mac and run offline. Cloud models stay optional and are added later.
          </p>
          <div class="mb-[22px] grid grid-cols-2 gap-2.5 text-left">
            {#each CATALOG as m (m.name)}
              {@const have = downloaded.has(m.name)}
              <div class="flex items-center gap-[11px] rounded-[var(--r)] border border-border bg-surface p-3">
                <div class="flex-1">
                  <div class="text-[13px] font-semibold">{m.name}</div>
                  <div class="text-[11px] text-text-3">
                    {m.params} · {m.size}
                    {#if m.fits}
                      <span class="font-semibold text-local"> · fits your machine</span>
                    {:else}
                      <span class="text-text-3"> · too large for this Mac</span>
                    {/if}
                  </div>
                </div>
                <button
                  type="button"
                  disabled={!m.fits}
                  onclick={() => m.fits && toggleDownload(m.name)}
                  class="inline-flex items-center gap-1.5 rounded-[var(--r-sm)] border px-[11px] py-[5px] text-[11.5px] font-semibold disabled:cursor-not-allowed disabled:opacity-45 {have
                    ? 'border-transparent bg-local-soft text-local'
                    : 'border-border-strong bg-surface-2'}"
                >
                  {#if have}
                    <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4">
                      <path d="M4 12l5 5L20 7" />
                    </svg>
                    Ready
                  {:else}
                    <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
                      <path d="M12 4v11m0 0l-4-4m4 4l4-4M5 20h14" />
                    </svg>
                    {m.fits ? "Download" : "Unavailable"}
                  {/if}
                </button>
              </div>
            {/each}
          </div>
        </div>
      {:else}
        <div class="w-[min(560px,94vw)] text-center">
          <h1 class="mb-2 text-[22px] font-[650] tracking-[-0.01em]">Assign your seats</h1>
          <p class="mx-auto mb-[22px] max-w-[400px] text-[14px] leading-[1.55] text-text-2">
            Bind a local model to each role. You can rebalance any time in Settings.
          </p>
          {#if downloaded.size === 0}
            <div
              class="rounded-[var(--r)] border border-dashed border-border-strong px-4 py-[14px] text-[12.5px] text-text-2"
            >
              No local models yet — go back and download at least one.
            </div>
          {:else}
            <div class="mb-[22px] flex flex-col gap-2 text-left">
              {#each SEATS as s (s.key)}
                <div class="flex items-center gap-[11px] rounded-[var(--r)] border border-border bg-surface px-[13px] py-[11px]">
                  <RouteDot route="local" />
                  <div class="flex-1 text-[13px] font-semibold">
                    {s.role}
                    <div class="text-[11px] font-normal text-text-3">{s.sub}</div>
                  </div>
                  <SegmentedControl
                    options={seatOptions}
                    value={seats[s.key] || seatOptions[0]?.value || ""}
                    onchange={(v) => (seats = { ...seats, [s.key]: v })}
                  />
                </div>
              {/each}
            </div>
          {/if}
        </div>
      {/if}
    </div>

    <!-- footer nav -->
    <div class="flex items-center justify-center gap-2.5 border-t border-border p-[18px]">
      <Button variant="ghost" disabled={step === 0} onclick={() => (step = Math.max(0, step - 1))}>
        Back
      </Button>
      <div class="flex-1"></div>
      <Button variant="ghost" onclick={done}>Skip setup</Button>
      <Button
        variant="primary"
        onclick={() => (isLast ? done() : (step = Math.min(STEPS.length - 1, step + 1)))}
      >
        {isLast ? "Finish" : "Next"}
      </Button>
    </div>
  </div>
</AppFrame>
