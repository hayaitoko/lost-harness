<script module lang="ts">
  /** A single selectable model. */
  export interface ModelOption {
    name: string;
    /**
     * Stable identity for selection. REQUIRED, and never derived from `name`:
     * two providers routinely expose an identically-named model, and keying
     * ownership by bare name let the last-registered provider silently shadow
     * the others — i.e. send the turn to an endpoint the user didn't pick.
     * Callers pass a composite such as `providerId::name`.
     */
    key: string;
  }

  /** One provider's section of the popover. A group is listed even when it has
   *  no items, so a provider can never quietly disappear from the picker. */
  export interface ModelGroup {
    /**
     * Stable identity for this section: the PROVIDER ID, never the name.
     * REQUIRED, and the only thing the each-block may key on.
     *
     * Provider names are not unique — ids are backend UUIDs, the `endpoints`
     * table has no `UNIQUE(name)`, and nothing stops a user (or a
     * double-clicked quick-add preset) from registering two providers both
     * called "Ollama". Keying the section list by name made that a
     * `each_key_duplicate` throw, and because this popover is always mounted
     * the throw took down the whole composer — not just the list.
     */
    id: string;
    /** Display name — the provider's name. Presentation only. */
    group: string;
    kind: "local" | "cloud";
    items: ModelOption[];
    /** Why this group is empty, when it is: the listing failed, or the
     *  endpoint returned nothing. Shown inline in place of the models. */
    notice?: string | null;
  }
</script>

<script lang="ts">
  // The composer's model control — the visible provider+model label plus an
  // upward-facing popover with model selection and thinking-strength choices.

  interface Props {
    /** Every provider's models, in display order. */
    groups: ModelGroup[];
    /** The currently selected model's `key`, or "" when nothing is selected. */
    value: string;
    /** Shown on the button when nothing is selected. */
    placeholder?: string;
    onchange?: (key: string) => void;
    /** Lets the surrounding composer close any competing popover before
     *  opening this one — and re-list models past the cache. */
    onopen?: () => void;
  }

  let { groups, value, placeholder = "No model selected", onchange, onopen }: Props = $props();

  let open = $state(false);
  let query = $state("");
  let thinkingStrength = $state<"light" | "balanced" | "deep">("balanced");
  let rootEl: HTMLDivElement;

  // The selected model plus the provider it belongs to — the composer's
  // trust-zone label, so it names both halves, never just the model.
  const selected = $derived.by(() => {
    for (const g of groups) {
      const item = g.items.find((m) => m.key === value);
      if (item) return { name: item.name, group: g.group, kind: g.kind };
    }
    return undefined;
  });

  const visibleGroups = $derived.by(() => {
    const q = query.trim().toLowerCase();
    return groups
      .map((g) => ({
        ...g,
        items: g.items.filter((m) => !q || m.name.toLowerCase().includes(q)),
      }))
      // A group with nothing to list stays visible (with its notice) — that is
      // the whole point. Only hide a group the SEARCH emptied, where the
      // provider is plainly still there under a different query.
      .filter((g) => g.items.length > 0 || g.notice != null || !q);
  });

  $effect(() => {
    if (!open) return;
    const onDocMouseDown = (e: MouseEvent) => {
      if (rootEl && !rootEl.contains(e.target as Node)) open = false;
    };
    document.addEventListener("mousedown", onDocMouseDown);
    return () => document.removeEventListener("mousedown", onDocMouseDown);
  });

  function pick(key: string) {
    onchange?.(key);
    open = false;
  }

  const dotClass: Record<ModelGroup["kind"], string> = {
    local: "bg-local",
    cloud: "bg-cloud",
  };
  const tagClass: Record<ModelGroup["kind"], string> = {
    local: "bg-local-soft text-local",
    cloud: "bg-cloud-soft text-cloud",
  };

  const THINKING_STRENGTHS: { id: typeof thinkingStrength; label: string; description: string }[] = [
    { id: "light", label: "Light", description: "Quicker" },
    { id: "balanced", label: "Balanced", description: "Default" },
    { id: "deep", label: "Deep", description: "More deliberate" },
  ];
</script>

<div bind:this={rootEl} class="relative flex items-center">
  <button
    type="button"
    aria-haspopup="listbox"
    aria-expanded={open}
    onclick={() => {
      onopen?.();
      open = !open;
    }}
    title={selected
      ? `Sending to ${selected.group} · ${selected.name} — click to change`
      : "No model selected — click to pick one"}
    class="relative flex h-[36px] max-w-[240px] items-center gap-[7px] rounded-full border px-[9px] transition-[background-color,color,border-color] duration-150 focus-visible:border-accent focus-visible:outline-none
      {selected
      ? open
        ? 'border-border-strong bg-surface-hover text-text'
        : 'border-transparent text-text-2 hover:bg-surface-hover hover:text-text'
      : 'border-warn/40 bg-warn-soft text-warn hover:brightness-[1.03]'}"
  >
    <svg width="19" height="19" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" aria-hidden="true" class="shrink-0">
      <rect x="5.5" y="7" width="13" height="11" rx="3" />
      <path d="M12 4v3M8.7 12h.1M15.2 12h.1M9.2 15h5.6" stroke-linecap="round" />
      <path d="M4 11v3M20 11v3" stroke-linecap="round" opacity=".7" />
    </svg>
    <!-- The endpoint label is VISIBLE, not sr-only. Which provider a message
         goes to is a trust-zone fact, and the status bar alone is too quiet
         for it — the user must be able to see it while typing. -->
    {#if selected}
      <span class="h-1.5 w-1.5 shrink-0 rounded-full {dotClass[selected.kind]}"></span>
      <span class="min-w-0 truncate text-[12px] font-medium leading-none">
        <span class="text-text-3">{selected.group}</span>
        <span class="text-text-3"> · </span>{selected.name}
      </span>
    {:else}
      <span class="truncate text-[12px] font-semibold leading-none">{placeholder}</span>
    {/if}
    <!-- The provider+model is already in the visible label above, so it is
         part of the accessible name; only the thinking strength needs adding. -->
    <span class="sr-only">; thinking strength {thinkingStrength}</span>
  </button>

  <div
    role="listbox"
    class="absolute bottom-[calc(100%+8px)] right-0 z-40 w-[292px] overflow-hidden rounded-[var(--r-lg)] border border-border-strong bg-surface shadow-[var(--shadow-pop)] transition-[0.12s]
      {open
      ? 'opacity-100 translate-y-0 pointer-events-auto'
      : 'opacity-0 translate-y-[4px] pointer-events-none'}"
  >
    <div class="border-b border-border px-3 py-2.5">
      <div class="mb-2 flex items-baseline justify-between">
        <span class="text-[10px] font-semibold uppercase tracking-[0.07em] text-text-3">Thinking strength</span>
        <span class="text-[10px] text-text-3">Applies when supported</span>
      </div>
      <div class="grid grid-cols-3 gap-1" role="group" aria-label="Thinking strength">
        {#each THINKING_STRENGTHS as strength (strength.id)}
          <button
            type="button"
            aria-pressed={thinkingStrength === strength.id}
            onclick={() => (thinkingStrength = strength.id)}
            class="rounded-[var(--r-sm)] px-1.5 py-1.5 text-left transition-[0.1s] {thinkingStrength === strength.id
              ? 'bg-accent-soft text-text'
              : 'text-text-3 hover:bg-surface-hover hover:text-text-2'}"
          >
            <span class="block text-[11px] font-semibold">{strength.label}</span>
            <span class="block text-[9.5px] leading-[1.2] opacity-75">{strength.description}</span>
          </button>
        {/each}
      </div>
    </div>
    <div class="border-b border-border p-2">
      <input
        placeholder="Search models…"
        bind:value={query}
        class="w-full rounded-[var(--r-sm)] border border-border bg-surface-2 px-[9px] py-[6px] text-[12px] text-text outline-none"
      />
    </div>
    <div class="max-h-[250px] overflow-y-auto p-[5px]">
      <!-- Keyed by the provider ID. Two providers may legitimately share a
           display name; keying by that name throws `each_key_duplicate` and,
           since this popover is always in the DOM, takes the whole composer
           down with it. -->
      {#each visibleGroups as g (g.id)}
        <div>
          <div
            class="flex items-center gap-[7px] px-2 pt-2 pb-1 text-[10px] font-[650] uppercase tracking-[0.05em] text-text-3"
          >
            {g.group}
            <span
              class="rounded-[var(--r-sm)] px-[6px] py-[1.5px] text-[9px] font-semibold normal-case tracking-normal {tagClass[
                g.kind
              ]}"
            >
              {g.kind === "local" ? "on device" : "cloud"}
            </span>
          </div>
          {#if g.items.length === 0}
            <!-- The provider stays listed with an explanation. Dropping it
                 from the popover (the old behaviour) hid the endpoint the
                 user was trying to reach while a DIFFERENT one stayed armed
                 and served every turn. -->
            <p class="px-[9px] py-[7px] text-[11.5px] leading-[1.35] text-text-3">
              {g.notice ?? "Couldn't list models — check the endpoint or key."}
            </p>
          {/if}
          {#each g.items as m (m.key)}
            <button
              type="button"
              role="option"
              aria-selected={m.key === value}
              onclick={() => pick(m.key)}
              class="flex w-full items-center gap-[9px] rounded-[var(--r-sm)] border-0 px-[9px] py-[7px] text-left text-[12.5px] text-text transition-[0.08s] hover:bg-surface-hover
                {m.key === value ? 'bg-accent-soft' : 'bg-transparent'}"
            >
              <span class="h-1.5 w-1.5 shrink-0 rounded-full {dotClass[g.kind]}"></span>
              {m.name}
              <span
                class="ml-auto text-accent {m.key === value ? 'opacity-100' : 'opacity-0'}"
              >
                <svg
                  width="13"
                  height="13"
                  viewBox="0 0 24 24"
                  fill="none"
                  stroke="currentColor"
                  stroke-width="2.5"
                >
                  <path d="M5 12l4 4L19 6" />
                </svg>
              </span>
            </button>
          {/each}
        </div>
      {:else}
        <p class="px-[9px] py-[10px] text-[11.5px] leading-[1.35] text-text-3">
          {query.trim()
            ? "No models match that search."
            : "No providers configured yet. Add one in Settings → Models."}
        </p>
      {/each}
    </div>
  </div>
</div>
