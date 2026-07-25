<script lang="ts">
  // Planner — the real Google Calendar + Google Tasks surface for the active
  // profile. Both APIs reuse the profile's Google OAuth connection from Email;
  // no sample events or local-only task state is ever rendered as live data.
  import { nav } from "$lib/design/nav.svelte";
  import Sidebar from "../components/Sidebar.svelte";
  import AppStatusBar from "../components/AppStatusBar.svelte";
  import Button from "../components/Button.svelte";
  import IconButton from "../components/IconButton.svelte";
  import { activeProfileId } from "$lib/stores/profiles";
  import {
    listCalendarEvents,
    createCalendarEvent,
    deleteCalendarEvent,
    listGoogleTasks,
    createGoogleTask,
    setGoogleTaskCompleted,
    deleteGoogleTask,
    type CalendarEventInfo,
    type GoogleTaskInfo,
  } from "$lib/api/tauri";

  let events = $state<CalendarEventInfo[]>([]);
  let tasks = $state<GoogleTaskInfo[]>([]);
  let loading = $state(true);
  let error: string | null = $state(null);
  let refreshTick = $state(0);
  let sequence = 0;

  let eventTitle = $state("");
  let eventStart = $state("");
  let eventEnd = $state("");
  let creatingEvent = $state(false);
  let taskTitle = $state("");
  let taskNotes = $state("");
  let creatingTask = $state(false);
  let confirmDelete = $state<string | null>(null);

  $effect(() => {
    const profile = $activeProfileId;
    void refreshTick;
    const token = ++sequence;
    loading = true;
    error = null;
    Promise.all([listCalendarEvents(profile), listGoogleTasks(profile)])
      .then(([nextEvents, nextTasks]) => {
        if (token !== sequence) return;
        events = nextEvents;
        tasks = nextTasks;
      })
      .catch((err) => {
        if (token === sequence) error = String(err);
      })
      .finally(() => {
        if (token === sequence) loading = false;
      });
  });

  function toRfc3339(value: string): string | null {
    const date = new Date(value);
    return Number.isNaN(date.getTime()) ? null : date.toISOString();
  }

  async function addEvent() {
    const start = toRfc3339(eventStart);
    const end = toRfc3339(eventEnd);
    if (!eventTitle.trim() || !start || !end) {
      error = "Give the event a title, start time, and end time.";
      return;
    }
    creatingEvent = true;
    error = null;
    try {
      const event = await createCalendarEvent($activeProfileId, eventTitle.trim(), start, end);
      events = [...events, event].sort((a, b) => a.start.localeCompare(b.start));
      eventTitle = "";
      eventStart = "";
      eventEnd = "";
    } catch (err) {
      error = String(err);
    } finally {
      creatingEvent = false;
    }
  }

  async function addTask() {
    if (!taskTitle.trim()) {
      error = "Give the task a title.";
      return;
    }
    creatingTask = true;
    error = null;
    try {
      const task = await createGoogleTask($activeProfileId, taskTitle.trim(), taskNotes.trim());
      tasks = [...tasks, task];
      taskTitle = "";
      taskNotes = "";
    } catch (err) {
      error = String(err);
    } finally {
      creatingTask = false;
    }
  }

  async function toggleTask(task: GoogleTaskInfo) {
    error = null;
    try {
      const updated = await setGoogleTaskCompleted($activeProfileId, task.id, !task.completed);
      tasks = tasks.map((row) => (row.id === updated.id ? updated : row));
    } catch (err) {
      error = String(err);
    }
  }

  async function remove(kind: "event" | "task", id: string) {
    const key = `${kind}:${id}`;
    if (confirmDelete !== key) {
      confirmDelete = key;
      setTimeout(() => {
        if (confirmDelete === key) confirmDelete = null;
      }, 3000);
      return;
    }
    confirmDelete = null;
    error = null;
    try {
      if (kind === "event") {
        await deleteCalendarEvent($activeProfileId, id);
        events = events.filter((event) => event.id !== id);
      } else {
        await deleteGoogleTask($activeProfileId, id);
        tasks = tasks.filter((task) => task.id !== id);
      }
    } catch (err) {
      error = String(err);
    }
  }

  function fmtDate(raw: string): string {
    const date = new Date(raw);
    if (Number.isNaN(date.getTime())) return raw;
    return date.toLocaleString(undefined, { weekday: "short", month: "short", day: "numeric", hour: "numeric", minute: "2-digit" });
  }
</script>

<div class="grid h-screen" style="grid-template-columns:260px 1fr 0">
  <Sidebar active="planner" />

  <main class="flex min-w-0 min-h-0 flex-col">
    <header class="flex h-12 flex-shrink-0 items-center gap-3 border-b border-border pl-[18px] pr-[14px]">
      <div class="text-[13.5px] font-semibold">Planner</div>
      <span class="text-[11.5px] text-text-3">Google Calendar and Tasks for {$activeProfileId}</span>
      <div class="flex-1"></div>
      <Button variant="ghost" onclick={() => (refreshTick += 1)} disabled={loading}>Refresh</Button>
      <IconButton label="Settings" onclick={() => nav.go("settings")}>
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
          <path d="M4 7h16M4 12h16M4 17h16" />
          <circle cx="9" cy="7" r="2" fill="var(--surface)" />
          <circle cx="15" cy="12" r="2" fill="var(--surface)" />
          <circle cx="8" cy="17" r="2" fill="var(--surface)" />
        </svg>
      </IconButton>
    </header>

    <div class="min-h-0 flex-1 overflow-y-auto">
      <div class="mx-auto grid max-w-[1000px] gap-6 px-6 pb-12 pt-6 lg:grid-cols-2">
        {#if error}
          <div class="lg:col-span-2 rounded-[var(--r)] border border-warn/30 bg-warn-soft px-3 py-2.5 text-[12.5px] text-text">
            {error}
            <span class="text-text-2">Connect or reconnect Google from Email if this profile has not granted Calendar and Tasks access.</span>
          </div>
        {/if}

        <section>
          <div class="mb-2 flex items-baseline justify-between">
            <h1 class="text-[13px] font-semibold">Upcoming calendar</h1>
            <span class="text-[11px] text-text-3">next 7 days</span>
          </div>
          <div class="overflow-hidden rounded-[var(--r-lg)] border border-border bg-surface">
            {#if loading}
              <p class="px-3 py-6 text-[12.5px] text-text-3">Loading calendar…</p>
            {:else if events.length === 0}
              <p class="px-3 py-6 text-[12.5px] text-text-3">No upcoming events.</p>
            {:else}
              {#each events as event (event.id)}
                <div class="flex items-center gap-2 border-b border-border px-3 py-2.5 last:border-b-0">
                  <div class="min-w-0 flex-1">
                    <p class="truncate text-[12.5px] font-[550] text-text">{event.summary}</p>
                    <p class="truncate text-[11.5px] text-text-3">{event.all_day ? event.start : fmtDate(event.start)}</p>
                  </div>
                  <Button variant="ghost" onclick={() => void remove("event", event.id)}>
                    {confirmDelete === `event:${event.id}` ? "Confirm?" : "Delete"}
                  </Button>
                </div>
              {/each}
            {/if}
          </div>

          <form class="mt-3 grid gap-2 rounded-[var(--r-lg)] border border-border bg-surface-2 p-3" onsubmit={(e) => { e.preventDefault(); void addEvent(); }}>
            <input bind:value={eventTitle} placeholder="New event title" class="rounded-[var(--r)] border border-border bg-surface px-2.5 py-2 text-[12.5px] text-text outline-none focus:border-accent" />
            <div class="grid grid-cols-2 gap-2">
              <label class="text-[11px] text-text-3">Start<input bind:value={eventStart} type="datetime-local" class="mt-1 block w-full rounded-[var(--r)] border border-border bg-surface px-2 py-1.5 text-[12px] text-text outline-none focus:border-accent" /></label>
              <label class="text-[11px] text-text-3">End<input bind:value={eventEnd} type="datetime-local" class="mt-1 block w-full rounded-[var(--r)] border border-border bg-surface px-2 py-1.5 text-[12px] text-text outline-none focus:border-accent" /></label>
            </div>
            <div><Button variant="primary" type="submit" disabled={creatingEvent}>{creatingEvent ? "Creating…" : "Add event"}</Button></div>
          </form>
        </section>

        <section>
          <div class="mb-2 flex items-baseline justify-between">
            <h1 class="text-[13px] font-semibold">Tasks</h1>
            <span class="text-[11px] text-text-3">Google Tasks</span>
          </div>
          <div class="overflow-hidden rounded-[var(--r-lg)] border border-border bg-surface">
            {#if loading}
              <p class="px-3 py-6 text-[12.5px] text-text-3">Loading tasks…</p>
            {:else if tasks.length === 0}
              <p class="px-3 py-6 text-[12.5px] text-text-3">No tasks yet.</p>
            {:else}
              {#each tasks as task (task.id)}
                <div class="flex items-center gap-2 border-b border-border px-3 py-2.5 last:border-b-0">
                  <input type="checkbox" checked={task.completed} onchange={() => void toggleTask(task)} aria-label={`Mark ${task.title} ${task.completed ? "incomplete" : "complete"}`} />
                  <div class="min-w-0 flex-1">
                    <p class="truncate text-[12.5px] font-[550] {task.completed ? 'text-text-3 line-through' : 'text-text'}">{task.title}</p>
                    {#if task.notes}<p class="truncate text-[11.5px] text-text-3">{task.notes}</p>{/if}
                  </div>
                  <Button variant="ghost" onclick={() => void remove("task", task.id)}>
                    {confirmDelete === `task:${task.id}` ? "Confirm?" : "Delete"}
                  </Button>
                </div>
              {/each}
            {/if}
          </div>

          <form class="mt-3 grid gap-2 rounded-[var(--r-lg)] border border-border bg-surface-2 p-3" onsubmit={(e) => { e.preventDefault(); void addTask(); }}>
            <input bind:value={taskTitle} placeholder="New task" class="rounded-[var(--r)] border border-border bg-surface px-2.5 py-2 text-[12.5px] text-text outline-none focus:border-accent" />
            <input bind:value={taskNotes} placeholder="Notes (optional)" class="rounded-[var(--r)] border border-border bg-surface px-2.5 py-2 text-[12.5px] text-text outline-none focus:border-accent" />
            <div><Button variant="primary" type="submit" disabled={creatingTask}>{creatingTask ? "Adding…" : "Add task"}</Button></div>
          </form>
        </section>
      </div>
    </div>
    <AppStatusBar />
  </main>
</div>
