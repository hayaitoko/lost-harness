<script lang="ts">
  // Scheduled Jobs — the LIVE list of this profile's cron automations
  // (per-profile `cron_jobs` via list/set_enabled/delete IPC). Jobs are
  // CREATED by asking the agent in chat (the Dangerous `manage_cron` tool —
  // standing automation is minted through the approval gate, never from a
  // settings form), so the old mock 3-step wizard is gone; this screen is
  // the human's review/pause/delete surface.
  import Button from "../components/Button.svelte";
  import IconButton from "../components/IconButton.svelte";
  import Toggle from "../components/Toggle.svelte";
  import Sidebar from "../components/Sidebar.svelte";
  import AppStatusBar from "../components/AppStatusBar.svelte";
  import { nav } from "$lib/design/nav.svelte";
  import { activeProfileId } from "$lib/stores/profiles";
  import {
    listCronJobs,
    setCronJobEnabled,
    deleteCronJob,
    type CronJobInfo,
  } from "$lib/api/tauri";

  const GRID = "44px minmax(0,1fr) 150px 170px 32px";

  let jobs = $state<CronJobInfo[]>([]);
  let loading = $state(true);
  let error: string | null = $state(null);
  let confirmDeleteId: string | null = $state(null);
  // Drop stale profile responses: an old profile's list must never become
  // actionable after the user has switched to another profile.
  let jobsSeq = 0;

  $effect(() => {
    const profile = $activeProfileId;
    const token = ++jobsSeq;
    loading = true;
    error = null;
    listCronJobs(profile)
      .then((rows) => {
        if (token === jobsSeq) jobs = rows;
      })
      .catch((err) => {
        if (token === jobsSeq) error = String(err);
      })
      .finally(() => {
        if (token === jobsSeq) loading = false;
      });
  });

  const activeCount = $derived(jobs.filter((j) => j.enabled).length);

  async function toggleJob(job: CronJobInfo, enabled: boolean) {
    const profile = $activeProfileId;
    const token = jobsSeq;
    error = null;
    // Optimistic; on failure restore the snapshotted pre-request value
    // (reverting to `!enabled` can land opposite reality when two failed
    // toggles overlap), then re-list — cheap and authoritative.
    const prev = job.enabled;
    jobs = jobs.map((j) => (j.id === job.id ? { ...j, enabled } : j));
    try {
      const ok = await setCronJobEnabled(profile, job.id, enabled);
      if (token !== jobsSeq || profile !== $activeProfileId) return;
      if (!ok) jobs = jobs.filter((j) => j.id !== job.id); // vanished backend-side
    } catch (err) {
      if (token !== jobsSeq || profile !== $activeProfileId) return;
      error = String(err);
      jobs = jobs.map((j) => (j.id === job.id ? { ...j, enabled: prev } : j));
      try {
        const rows = await listCronJobs(profile);
        if (token === jobsSeq && profile === $activeProfileId) jobs = rows;
      } catch {
        // Keep the reverted snapshot if even the reconcile fetch fails.
      }
    }
  }

  async function removeJob(id: string) {
    if (confirmDeleteId !== id) {
      confirmDeleteId = id;
      setTimeout(() => {
        if (confirmDeleteId === id) confirmDeleteId = null;
      }, 3000);
      return;
    }
    const profile = $activeProfileId;
    const token = jobsSeq;
    confirmDeleteId = null;
    error = null;
    try {
      await deleteCronJob(profile, id);
      if (token !== jobsSeq || profile !== $activeProfileId) return;
      jobs = jobs.filter((j) => j.id !== id);
    } catch (err) {
      if (token !== jobsSeq || profile !== $activeProfileId) return;
      error = String(err);
    }
  }

  function lastRunText(j: CronJobInfo): { text: string; color: string } {
    if (!j.enabled) return { text: "Paused", color: "var(--text-3)" };
    if (j.last_run_at == null) return { text: "Hasn't run yet", color: "var(--text-3)" };
    const when = new Date(j.last_run_at * 1000).toLocaleString(undefined, {
      month: "short",
      day: "numeric",
      hour: "numeric",
      minute: "2-digit",
    });
    if (j.last_status === "ok") return { text: `${when} · ok`, color: "var(--local)" };
    if (j.last_status) return { text: `${when} · ${j.last_status}`, color: "var(--blocked)" };
    return { text: when, color: "var(--text-2)" };
  }
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
        {activeCount} active · unattended runs stay local and halt at your spend cap
      </span>
      <div class="flex-1"></div>
      <div class="flex flex-shrink-0 items-center gap-1">
        <Button variant="primary" onclick={() => nav.go("main")}>Ask for a job in chat</Button>
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
        {#if error}
          <div class="mb-3 px-1 text-[12.5px] text-red-400">{error}</div>
        {/if}

        {#if loading}
          <div class="px-1 py-6 text-[12.5px] text-text-3">Loading jobs…</div>
        {:else if jobs.length === 0}
          <div
            class="rounded-[var(--r-lg)] border border-dashed border-border-strong px-6 py-12 text-center"
          >
            <div class="text-[13px] font-semibold text-text">No scheduled jobs yet</div>
            <p class="mx-auto mt-1.5 max-w-[420px] text-[12.5px] text-text-3">
              Ask for one in chat — e.g. “every morning at 7, summarize my day into a
              note.” Lost Harness plans it, asks your approval once, and it appears
              here so you can pause or delete it any time.
            </p>
            <div class="mt-4">
              <Button variant="primary" onclick={() => nav.go("main")}>Open chat</Button>
            </div>
          </div>
        {:else}
          <div
            class="grid items-center gap-3 px-[14px] pb-2 text-[10.5px] font-semibold uppercase tracking-[0.06em] text-text-3"
            style="grid-template-columns:{GRID}"
          >
            <span></span>
            <span>Job</span>
            <span>Schedule</span>
            <span>Last run</span>
            <span></span>
          </div>

          <div class="flex flex-col gap-2">
            {#each jobs as job (job.id)}
              {@const last = lastRunText(job)}
              <div class="rounded-[var(--r-lg)] border border-border bg-surface">
                <div
                  class="grid items-center gap-3 px-[14px] py-[13px]"
                  style="grid-template-columns:{GRID}"
                >
                  <Toggle
                    checked={job.enabled}
                    onchange={(v) => void toggleJob(job, v)}
                  />
                  <div class="min-w-0">
                    <div class="text-[13px] font-semibold">{job.name}</div>
                    <div class="truncate text-[12px] text-text-3">{job.prompt}</div>
                  </div>
                  <span class="font-mono text-[12px] text-text-2">{job.schedule}</span>
                  <span class="inline-flex items-center gap-1.5 text-[11.5px] text-text-2">
                    <span
                      class="h-[7px] w-[7px] flex-none rounded-full"
                      style="background:{last.color}"
                    ></span>
                    {last.text}
                  </span>
                  <button
                    type="button"
                    aria-label="Delete job"
                    onclick={() => void removeJob(job.id)}
                    class="grid h-7 w-7 place-items-center rounded-[var(--r)] border-0 bg-transparent text-[10px] text-text-3 transition-[background-color,color] duration-100 hover:bg-surface-hover hover:text-blocked"
                  >
                    {#if confirmDeleteId === job.id}
                      <span class="font-semibold text-blocked">?</span>
                    {:else}
                      <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8">
                        <path d="M4 7h16M10 11v6M14 11v6M6 7l1 13h10l1-13M9 7V4h6v3" />
                      </svg>
                    {/if}
                  </button>
                </div>
              </div>
            {/each}
          </div>

          <p class="mt-4 px-1 text-[11.5px] text-text-3">
            Jobs run unattended under this profile's guardrails: always on a local,
            private model — never cloud — and they halt at the profile's spend cap.
            To change what a job does or when it runs, ask in chat; to stop one,
            pause or delete it here.
          </p>
        {/if}
      </div>
    </div>

    <AppStatusBar />
  </main>
</div>
