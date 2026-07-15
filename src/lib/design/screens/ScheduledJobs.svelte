<script lang="ts">
  // Scheduled Jobs — recurring automations with schedule, binding, last-run
  // disposition and enable toggles, plus a 3-step "New job" wizard.
  // Ported from ScheduledJobs.tsx (design UI).
  import Button from "../components/Button.svelte";
  import IconButton from "../components/IconButton.svelte";
  import SegmentedControl from "../components/SegmentedControl.svelte";
  import Toggle from "../components/Toggle.svelte";
  import BindingControl from "../components/BindingControl.svelte";
  import Sidebar from "../components/Sidebar.svelte";
  import AppStatusBar from "../components/AppStatusBar.svelte";
  import { nav } from "$lib/design/nav.svelte";
  import type { Binding } from "$lib/design/types";

  const GRID = "44px minmax(0,1fr) 120px 92px 150px 32px";

  type JobKey = "morning" | "triage" | "expense" | "backup";

  type Job = {
    key: JobKey;
    name: string;
    desc: string;
    schedule: string;
    bindingLabel: string;
    bindingColor: string;
    lastRun: string;
    lastRunColor: string;
    held?: string;
  };

  const JOBS: Job[] = [
    {
      key: "morning",
      name: "Morning brief",
      desc: "Weather, calendar, and inbox summary — delivered as a note",
      schedule: "Daily · 7:00",
      bindingLabel: "Private",
      bindingColor: "var(--local)",
      lastRun: "Today 7:00 · local",
      lastRunColor: "var(--local)",
    },
    {
      key: "triage",
      name: "Inbox triage",
      desc: "Label new mail and draft replies for anything actionable",
      schedule: "Every hour",
      bindingLabel: "Auto",
      bindingColor: "var(--text-3)",
      lastRun: "22m ago · local",
      lastRunColor: "var(--local)",
    },
    {
      key: "expense",
      name: "Weekly expense rollup",
      desc: "Summarize card transactions into the budget sheet",
      schedule: "Mondays · 9:00",
      bindingLabel: "Auto",
      bindingColor: "var(--text-3)",
      lastRun: "Mon 9:02 · held",
      lastRunColor: "var(--blocked)",
      held: "the rollup contains financial details, so the cloud step was stopped. It ran locally instead.",
    },
    {
      key: "backup",
      name: "Backup memory to NAS",
      desc: "Encrypted snapshot of both profile memory stores",
      schedule: "Nightly · 2:00",
      bindingLabel: "Private",
      bindingColor: "var(--local)",
      lastRun: "Paused since Jul 2",
      lastRunColor: "var(--text-3)",
    },
  ];

  const DAYS: [string, string][] = [
    ["M", "M"],
    ["T", "T"],
    ["W", "W"],
    ["Th", "T"],
    ["F", "F"],
    ["Sa", "S"],
    ["Su", "S"],
  ];

  const SCHED_OPTIONS = [
    { value: "hourly", label: "Hourly" },
    { value: "daily", label: "Daily" },
    { value: "weekly", label: "Weekly" },
    { value: "custom", label: "Custom" },
  ];

  let jobs = $state<Record<JobKey, boolean>>({
    morning: true,
    triage: true,
    expense: true,
    backup: false,
  });
  let wizardOpen = $state(false);
  let editing = $state(false);
  let step = $state(1);
  let sched = $state("daily");
  let days = $state<string[]>(["M", "T", "W", "Th", "F"]);
  let newBinding = $state<Binding>("private");
  let pauseOnEgress = $state(true);
  let toastVisible = $state(false);
  let toastTimer: ReturnType<typeof setTimeout> | undefined;

  $effect(() => () => clearTimeout(toastTimer));

  function openWizard(edit: boolean) {
    editing = edit;
    step = 1;
    wizardOpen = true;
  }
  function createJob() {
    wizardOpen = false;
    toastVisible = true;
    clearTimeout(toastTimer);
    toastTimer = setTimeout(() => (toastVisible = false), 3200);
  }
  function toggleDay(id: string) {
    days = days.includes(id) ? days.filter((x) => x !== id) : [...days, id];
  }

  const cellInput =
    "bg-surface-2 border border-border rounded-[var(--r)] text-text text-[13px] px-[9px] py-[6px] outline-none";
  const chip =
    "border border-border bg-surface-2 text-text-2 text-[11.5px] px-[10px] py-1 rounded-xl cursor-pointer";
</script>

<div class="grid h-screen" style="grid-template-columns:260px 1fr 0">
  <Sidebar active="scheduled-jobs" />

  <main class="flex min-w-0 min-h-0 flex-col">
    <div
      class="flex h-12 flex-shrink-0 items-center gap-3 border-b border-border pl-[18px] pr-[14px]"
    >
      <div class="min-w-0 overflow-hidden text-ellipsis whitespace-nowrap text-[13.5px] font-semibold">
        Scheduled jobs
      </div>
      <span class="text-[11.5px] text-text-3">
        3 active · runs happen even when this window is closed
      </span>
      <div class="flex-1"></div>
      <div class="flex flex-shrink-0 items-center gap-1">
        <Button variant="primary" onclick={() => openWizard(false)}>New job</Button>
        <IconButton label="Settings" onclick={() => nav.go("settings")}>
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
            <path d="M4 7h16M4 12h16M4 17h16" />
            <circle cx="9" cy="7" r="2" fill="var(--surface)" />
            <circle cx="15" cy="12" r="2" fill="var(--surface)" />
            <circle cx="8" cy="17" r="2" fill="var(--surface)" />
          </svg>
        </IconButton>
      </div>
    </div>

    <div class="min-h-0 flex-1 overflow-y-auto">
      <div class="mx-auto max-w-[820px] px-6 pb-12 pt-[22px]">
        <div
          class="grid items-center gap-3 px-[14px] pb-2 text-[10.5px] font-semibold uppercase tracking-[0.06em] text-text-3"
          style="grid-template-columns:{GRID}"
        >
          <span></span>
          <span>Job</span>
          <span>Schedule</span>
          <span>Binding</span>
          <span>Last run</span>
          <span></span>
        </div>

        <div class="flex flex-col gap-2">
          {#each JOBS as job (job.key)}
            <div class="rounded-[var(--r-lg)] border border-border bg-surface">
              <div
                class="grid items-center gap-3 px-[14px] py-[13px]"
                style="grid-template-columns:{GRID}"
              >
                <Toggle
                  checked={jobs[job.key]}
                  onchange={(v) => (jobs = { ...jobs, [job.key]: v })}
                />
                <div class="min-w-0">
                  <div class="text-[13px] font-semibold">{job.name}</div>
                  <div class="text-[12px] text-text-3">{job.desc}</div>
                </div>
                <span class="text-[12px] text-text-2">{job.schedule}</span>
                <span class="inline-flex items-center gap-1.5 text-[11.5px] text-text-2">
                  <span
                    class="h-[7px] w-[7px] flex-none rounded-full"
                    style="background:{job.bindingColor}"
                  ></span>
                  {job.bindingLabel}
                </span>
                {#if job.lastRunColor === "var(--text-3)"}
                  <span class="text-[11.5px] text-text-3">{job.lastRun}</span>
                {:else}
                  <span class="inline-flex items-center gap-1.5 text-[11.5px] text-text-2">
                    <span
                      class="h-[7px] w-[7px] flex-none rounded-full"
                      style="background:{job.lastRunColor}"
                    ></span>
                    {job.lastRun}
                  </span>
                {/if}
                <button
                  type="button"
                  aria-label="Edit job"
                  onclick={() => openWizard(true)}
                  class="grid h-7 w-7 place-items-center rounded-[var(--r)] border-0 bg-transparent text-text-3 transition-[background-color,color] duration-100 hover:bg-surface-hover hover:text-text"
                >
                  <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8">
                    <path d="M5 19l1-4L16 5l3 3L9 18z" />
                  </svg>
                </button>
              </div>

              {#if job.held}
                <div
                  class="flex items-center gap-2 rounded-b-[var(--r-lg)] border-t border-border bg-blocked-soft px-[14px] py-[9px] text-[12px] text-text-2"
                >
                  <span class="font-semibold text-blocked">Held:</span>
                  {job.held}
                  <button
                    type="button"
                    class="ml-auto border-0 bg-transparent p-0 text-[12px] font-semibold text-text underline"
                  >
                    Review
                  </button>
                </div>
              {/if}
            </div>
          {/each}
        </div>
      </div>
    </div>

    <AppStatusBar session="0:12" />
  </main>
</div>

{#if wizardOpen}
  <div
    class="fixed inset-0 z-[80] grid place-items-center bg-black/45 backdrop-blur-[3px]"
  >
    <div
      class="w-[500px] overflow-hidden rounded-[var(--r-lg)] border border-border-strong bg-surface shadow-[var(--shadow-pop)]"
    >
      <div class="flex items-center gap-2.5 border-b border-border px-4 py-[13px]">
        <span class="text-[13px] font-semibold">{editing ? "Edit job" : "New job"}</span>
        <span class="text-[11.5px] text-text-3">Step {step} of 3</span>
        <div class="flex-1"></div>
        <IconButton label="Close" onclick={() => (wizardOpen = false)}>
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
            <path d="M6 6l12 12M18 6 6 18" />
          </svg>
        </IconButton>
      </div>

      <div class="min-h-[196px] p-4">
        {#if step === 1}
          <div class="mb-1.5 text-[12.5px] font-semibold">What should it do?</div>
          <div class="mb-2.5 text-[12px] text-text-3">
            Describe the job in plain language — Lost Harness plans the steps and shows them to
            you before the first run.
          </div>
          <textarea
            rows={3}
            placeholder="e.g. Every morning, summarize my calendar and new mail into a note"
            class="w-full resize-none rounded-[var(--r)] border border-border bg-surface-2 p-2.5 text-[13px] text-text outline-none"
          ></textarea>
          <div class="mt-2.5 flex flex-wrap gap-1.5">
            <button type="button" class={chip}>Watch a folder</button>
            <button type="button" class={chip}>Weekly reading digest</button>
            <button type="button" class={chip}>Re-run a past chat on a schedule</button>
          </div>
        {:else if step === 2}
          <div class="mb-1.5 text-[12.5px] font-semibold">When should it run?</div>
          <div class="mb-3 text-[12px] text-text-3">
            Runs happen on tadashi even when this window is closed.
          </div>
          <SegmentedControl options={SCHED_OPTIONS} value={sched} onchange={(v) => (sched = v)} />

          {#if sched === "hourly"}
            <div class="mt-3.5 flex items-center gap-2.5">
              <span class="text-[12.5px] text-text-2">At</span>
              <input value="00" class="{cellInput} w-14" />
              <span class="text-[12px] text-text-3">minutes past every hour</span>
            </div>
          {:else if sched === "daily"}
            <div class="mt-3.5 flex items-center gap-2.5">
              <span class="text-[12.5px] text-text-2">At</span>
              <input value="7:00" class="{cellInput} w-[76px]" />
              <span class="text-[12px] text-text-3">every day, local time</span>
            </div>
          {:else if sched === "weekly"}
            <div class="mt-3.5">
              <div class="mb-[7px] text-[12px] text-text-3">On these days</div>
              <div class="flex gap-[5px]">
                {#each DAYS as [id, label], i (id + i)}
                  {@const on = days.includes(id)}
                  <button
                    type="button"
                    aria-pressed={on}
                    onclick={() => toggleDay(id)}
                    class="h-[30px] w-[30px] rounded-full border text-[12px] font-semibold transition
                      {on
                      ? 'border-accent bg-accent-soft text-text'
                      : 'border-border bg-transparent text-text-2'}"
                  >
                    {label}
                  </button>
                {/each}
              </div>
              <div class="mt-3 flex items-center gap-2.5">
                <span class="text-[12.5px] text-text-2">At</span>
                <input value="9:00" class="{cellInput} w-[76px]" />
                <span class="text-[12px] text-text-3">local time</span>
              </div>
            </div>
          {:else if sched === "custom"}
            <div class="mt-3.5">
              <div class="mb-[7px] text-[12px] text-text-3">Cron expression</div>
              <input
                value="0 9 * * 1-5"
                class="w-full rounded-[var(--r)] border border-border bg-surface-2 px-[11px] py-2 font-mono text-[13px] text-text outline-none"
              />
              <div class="mt-1.5 text-[11px] text-text-3">Runs at 9:00 on weekdays.</div>
            </div>
          {/if}
        {:else if step === 3}
          <div class="mb-1.5 text-[12.5px] font-semibold">Guardrails</div>
          <div class="mb-3 text-[12px] text-text-3">
            Unattended runs get the same egress guard as live chats.
          </div>
          <BindingControl value={newBinding} onchange={(b) => (newBinding = b)} />
          <div
            class="mt-3.5 flex items-center justify-between gap-2.5 rounded-[var(--r)] bg-surface-2 px-3 py-2.5"
          >
            <span class="text-[12.5px]">Pause and notify if a step would leave this machine</span>
            <Toggle checked={pauseOnEgress} onchange={(v) => (pauseOnEgress = v)} />
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
          <Button variant="primary" onclick={createJob}>
            {editing ? "Save changes" : "Create job"}
          </Button>
        {/if}
      </div>
    </div>
  </div>
{/if}

{#if toastVisible}
  <div class="fixed bottom-11 left-1/2 z-[90] -translate-x-1/2">
    <div
      class="flex max-w-[340px] items-center gap-2 rounded-[var(--r)] border border-border-strong bg-surface px-[14px] py-[9px] text-[12.5px] text-text shadow-[var(--shadow-pop)]"
    >
      <span class="h-1.5 w-1.5 flex-shrink-0 rounded-full bg-accent"></span>
      <span>Job created — dry run queued for your review</span>
    </div>
  </div>
{/if}
