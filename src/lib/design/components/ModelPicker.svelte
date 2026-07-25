<script lang="ts">
  // The composer's model control — a compact robot icon that opens an
  // upward-facing popover with model selection and thinking-strength choices.

  /** A single selectable model, grouped by provider/source. */
  interface ModelOption {
    name: string;
    kind: "local" | "cloud";
    group: string;
    /**
     * Stable identity for selection. Defaults to `name`, but when two providers
     * expose an identically-named model the caller must pass a disambiguating
     * key (e.g. `providerId::name`) so the right one is selected/highlighted.
     */
    key?: string;
  }

  interface Props {
    /** Every selectable model, in display order. Consecutive items sharing a `group` are grouped under one header. */
    models: ModelOption[];
    /** The currently selected model's identity — its `key` (or `name` if no key was given). */
    value: string;
    /** Shown on the button when nothing is selected. */
    placeholder?: string;
    onchange?: (key: string) => void;
    /** Lets the surrounding composer close any competing popover before opening this one. */
    onopen?: () => void;
  }

  let { models, value, placeholder = "Select model", onchange, onopen }: Props = $props();

  let open = $state(false);
  let query = $state("");
  let thinkingStrength = $state<"light" | "balanced" | "deep">("balanced");
  let rootEl: HTMLDivElement;

  const keyOf = (m: ModelOption) => m.key ?? m.name;
  const selected = $derived(models.find((m) => keyOf(m) === value));

  const groups = $derived.by(() => {
    const q = query.trim().toLowerCase();
    const order: string[] = [];
    const byGroup = new Map<string, ModelOption[]>();
    for (const m of models) {
      if (!byGroup.has(m.group)) {
        order.push(m.group);
        byGroup.set(m.group, []);
      }
      byGroup.get(m.group)!.push(m);
    }
    return order
      .map((group) => ({
        group,
        kind: byGroup.get(group)![0].kind,
        items: byGroup.get(group)!.filter((m) => !q || m.name.toLowerCase().includes(q)),
      }))
      .filter((g) => g.items.length > 0);
  });

  $effect(() => {
    if (!open) return;
    const onDocMouseDown = (e: MouseEvent) => {
      if (rootEl && !rootEl.contains(e.target as Node)) open = false;
    };
    document.addEventListener("mousedown", onDocMouseDown);
    return () => document.removeEventListener("mousedown", onDocMouseDown);
  });

  function pick(name: string) {
    onchange?.(name);
    open = false;
  }

  const dotClass: Record<ModelOption["kind"], string> = {
    local: "bg-local",
    cloud: "bg-cloud",
  };
  const tagClass: Record<ModelOption["kind"], string> = {
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
    title="Choose model and thinking strength"
    class="relative grid h-[36px] w-[36px] place-items-center rounded-full border border-transparent text-text-3 transition-[background-color,color,border-color] duration-150 focus-visible:border-accent focus-visible:outline-none
      {open ? 'border-border-strong bg-surface-hover text-text' : 'hover:bg-surface-hover hover:text-text-2'}"
  >
    <svg width="21" height="21" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" aria-hidden="true">
      <rect x="5.5" y="7" width="13" height="11" rx="3" />
      <path d="M12 4v3M8.7 12h.1M15.2 12h.1M9.2 15h5.6" stroke-linecap="round" />
      <path d="M4 11v3M20 11v3" stroke-linecap="round" opacity=".7" />
    </svg>
    {#if selected}
      <span class="absolute bottom-[7px] right-[7px] h-1.5 w-1.5 rounded-full ring-2 ring-surface {dotClass[selected.kind]}"></span>
    {/if}
    <span class="sr-only">{selected?.name ?? placeholder}; thinking strength {thinkingStrength}</span>
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
      {#each groups as g (g.group)}
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
          {#each g.items as m (keyOf(m))}
            <button
              type="button"
              role="option"
              aria-selected={keyOf(m) === value}
              onclick={() => pick(keyOf(m))}
              class="flex w-full items-center gap-[9px] rounded-[var(--r-sm)] border-0 px-[9px] py-[7px] text-left text-[12.5px] text-text transition-[0.08s] hover:bg-surface-hover
                {keyOf(m) === value ? 'bg-accent-soft' : 'bg-transparent'}"
            >
              <span class="h-1.5 w-1.5 shrink-0 rounded-full {dotClass[m.kind]}"></span>
              {m.name}
              <span
                class="ml-auto text-accent {keyOf(m) === value ? 'opacity-100' : 'opacity-0'}"
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
      {/each}
    </div>
  </div>
</div>
