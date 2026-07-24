<script lang="ts">
  // Files — the LIVE read-only browser over this profile's Tier-P workspace
  // (`<base>/workspace/<profile>` — the same tree the agent's fs tools write
  // into), via the confined `list_workspace_files` IPC. The old mockup's
  // fabricated per-file "exposure" column is gone: no backend records
  // per-file egress today, and showing invented routing history would break
  // the honesty invariant. Writes stay exclusively behind the gated fs tools.
  import { nav } from "$lib/design/nav.svelte";
  import Sidebar from "../components/Sidebar.svelte";
  import AppStatusBar from "../components/AppStatusBar.svelte";
  import Button from "../components/Button.svelte";
  import IconButton from "../components/IconButton.svelte";
  import { activeProfileId } from "$lib/stores/profiles";
  import { listWorkspaceFiles, type WorkspaceEntry } from "$lib/api/tauri";

  // The row grid: name (flex) · modified · size.
  const GRID = "grid-cols-[minmax(0,1fr)_150px_84px]";

  let entries = $state<WorkspaceEntry[]>([]);
  let subpath = $state("");
  let loading = $state(true);
  let error: string | null = $state(null);

  $effect(() => {
    const profile = $activeProfileId;
    const path = subpath;
    loading = true;
    error = null;
    listWorkspaceFiles(profile, path)
      .then((rows) => (entries = rows))
      .catch((err) => (error = String(err)))
      .finally(() => (loading = false));
  });

  // Reset navigation when the profile changes (the tree is per-profile).
  $effect(() => {
    void $activeProfileId;
    subpath = "";
  });

  const crumbs = $derived(subpath === "" ? [] : subpath.split("/"));

  function enterDir(name: string) {
    subpath = subpath === "" ? name : `${subpath}/${name}`;
  }
  function goToCrumb(idx: number) {
    // idx -1 = root.
    subpath = idx < 0 ? "" : crumbs.slice(0, idx + 1).join("/");
  }

  function fmtSize(bytes: number): string {
    if (bytes < 1024) return `${bytes} B`;
    if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(0)} KB`;
    if (bytes < 1024 * 1024 * 1024) return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
    return `${(bytes / (1024 * 1024 * 1024)).toFixed(2)} GB`;
  }
  function fmtModified(secs: number | null): string {
    if (secs == null) return "—";
    return new Date(secs * 1000).toLocaleString(undefined, {
      month: "short",
      day: "numeric",
      hour: "numeric",
      minute: "2-digit",
    });
  }
</script>

<div class="grid h-screen" style="grid-template-columns:260px 1fr 0">
  <Sidebar active="files" />

  <main class="flex min-w-0 min-h-0 flex-col">
    <div
      class="flex h-12 flex-shrink-0 items-center gap-3 border-b border-border pl-[18px] pr-[14px]"
    >
      <div class="min-w-0 overflow-hidden text-ellipsis whitespace-nowrap text-[13.5px] font-semibold">
        Files
      </div>
      <span class="text-[11.5px] text-text-3">
        the {$activeProfileId} profile's workspace — what the assistant reads and writes
      </span>
      <div class="flex-1"></div>
      <IconButton label="Settings" onclick={() => nav.go("settings")}>
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
          <path d="M4 7h16M4 12h16M4 17h16" />
          <circle cx="9" cy="7" r="2" fill="var(--surface)" />
          <circle cx="15" cy="12" r="2" fill="var(--surface)" />
          <circle cx="8" cy="17" r="2" fill="var(--surface)" />
        </svg>
      </IconButton>
    </div>

    <div class="min-h-0 flex-1 overflow-y-auto">
      <div class="mx-auto max-w-[820px] px-6 pb-12 pt-[22px]">
        <!-- Breadcrumbs -->
        <div class="flex flex-wrap items-center gap-1 px-1 pb-3 text-[12px]">
          <button
            type="button"
            onclick={() => goToCrumb(-1)}
            class="border-0 bg-transparent p-0 font-semibold {subpath === ''
              ? 'text-text'
              : 'text-text-3 hover:text-text'} cursor-pointer"
          >
            workspace
          </button>
          {#each crumbs as c, i (i)}
            <span class="text-text-3">/</span>
            <button
              type="button"
              onclick={() => goToCrumb(i)}
              class="border-0 bg-transparent p-0 font-semibold {i === crumbs.length - 1
                ? 'text-text'
                : 'text-text-3 hover:text-text'} cursor-pointer"
            >
              {c}
            </button>
          {/each}
        </div>

        {#if error}
          <div class="mb-3 px-1 text-[12.5px] text-red-400">{error}</div>
        {/if}

        {#if loading}
          <div class="px-1 py-6 text-[12.5px] text-text-3">Loading…</div>
        {:else if entries.length === 0}
          <div
            class="rounded-[var(--r-lg)] border border-dashed border-border-strong px-6 py-12 text-center"
          >
            <div class="text-[13px] font-semibold text-text">
              {subpath === "" ? "Nothing here yet" : "Empty folder"}
            </div>
            {#if subpath === ""}
              <p class="mx-auto mt-1.5 max-w-[420px] text-[12.5px] text-text-3">
                Files the assistant reads or writes for this profile land here —
                each profile has its own physically separate tree. Ask for
                something in chat that produces a file and it will show up.
              </p>
              <div class="mt-4">
                <Button variant="primary" onclick={() => nav.go("main")}>Open chat</Button>
              </div>
            {/if}
          </div>
        {:else}
          <div
            class="grid {GRID} items-center gap-3 px-[14px] pb-2 text-[10.5px] font-semibold uppercase tracking-[0.06em] text-text-3"
          >
            <span>Name</span>
            <span>Modified</span>
            <span class="text-right">Size</span>
          </div>
          <div class="overflow-hidden rounded-[var(--r-lg)] border border-border bg-surface">
            {#each entries as f (f.name)}
              {#snippet rowBody()}
                <span class="flex min-w-0 items-center gap-2.5">
                  <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="var(--text-3)" stroke-width="1.7" class="shrink-0">
                    {#if f.is_dir}
                      <path d="M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v9a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z" />
                    {:else}
                      <path d="M6 2h9l5 5v15H6z" />
                      <path d="M15 2v5h5" />
                    {/if}
                  </svg>
                  <span class="truncate text-[12.5px] {f.is_dir ? 'font-semibold' : ''} text-text">
                    {f.name}
                  </span>
                </span>
                <span class="text-left text-[11.5px] text-text-3">{fmtModified(f.modified_at)}</span>
                <span class="text-right text-[11.5px] tabular-nums text-text-3">
                  {f.is_dir ? "—" : fmtSize(f.size_bytes)}
                </span>
              {/snippet}
              {#if f.is_dir}
                <button
                  type="button"
                  onclick={() => enterDir(f.name)}
                  class="grid {GRID} w-full cursor-pointer items-center gap-3 border-0 border-b border-border bg-transparent px-[14px] py-[10px] text-left last:border-b-0 hover:bg-surface-hover"
                >
                  {@render rowBody()}
                </button>
              {:else}
                <div
                  class="grid {GRID} items-center gap-3 border-b border-border px-[14px] py-[10px] last:border-b-0"
                >
                  {@render rowBody()}
                </div>
              {/if}
            {/each}
          </div>
          <p class="mt-4 px-1 text-[11.5px] text-text-3">
            Read-only view. The assistant changes these files only through its
            gated tools (each write asks or runs under a rule you granted);
            this browser never modifies anything.
          </p>
        {/if}
      </div>
    </div>

    <AppStatusBar session="0:12" />
  </main>
</div>
