<script lang="ts">
  // The composer's model selector — a button showing the active model that
  // opens an upward-facing popover with a search box and grouped options.
  // Maps to `.model-picker` / `.model-btn` / `.model-pop`.

  /** A single selectable model, grouped by provider/source. */
  interface ModelOption {
    name: string;
    kind: "local" | "cloud";
    group: string;
  }

  interface Props {
    /** Every selectable model, in display order. Consecutive items sharing a `group` are grouped under one header. */
    models: ModelOption[];
    /** The currently selected model's `name`. */
    value: string;
    onchange?: (name: string) => void;
  }

  let { models, value, onchange }: Props = $props();

  let open = $state(false);
  let query = $state("");
  let rootEl: HTMLDivElement;

  const selected = $derived(models.find((m) => m.name === value));

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
</script>

<div bind:this={rootEl} class="relative flex items-center">
  <div class="overflow-hidden max-w-[220px] opacity-100">
    <button
      type="button"
      aria-haspopup="listbox"
      aria-expanded={open}
      onclick={() => (open = !open)}
      class="inline-flex items-center gap-[7px] rounded-[var(--r)] border border-border bg-transparent px-[9px] py-[5px] text-[12px] font-medium text-text-2 transition-[0.1s]
        hover:bg-surface-hover hover:text-text hover:border-border-strong"
    >
      <span class="h-1.5 w-1.5 rounded-full {dotClass[selected?.kind ?? 'local']}"></span>
      <span>{selected?.name ?? value}</span>
      <svg
        width="11"
        height="11"
        viewBox="0 0 12 12"
        fill="none"
        stroke="currentColor"
        stroke-width="1.6"
        class="text-text-3 transition-transform duration-[0.12s] {open ? 'rotate-180' : ''}"
      >
        <path d="M2 4.5 6 8l4-3.5" />
      </svg>
    </button>
  </div>

  <div
    role="listbox"
    class="absolute bottom-[calc(100%+6px)] right-0 z-40 w-[280px] overflow-hidden rounded-[var(--r)] border border-border-strong bg-surface shadow-[var(--shadow-pop)] transition-[0.12s]
      {open
      ? 'opacity-100 translate-y-0 pointer-events-auto'
      : 'opacity-0 translate-y-[4px] pointer-events-none'}"
  >
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
          {#each g.items as m (m.name)}
            <button
              type="button"
              role="option"
              aria-selected={m.name === value}
              onclick={() => pick(m.name)}
              class="flex w-full items-center gap-[9px] rounded-[var(--r-sm)] border-0 px-[9px] py-[7px] text-left text-[12.5px] text-text transition-[0.08s] hover:bg-surface-hover
                {m.name === value ? 'bg-accent-soft' : 'bg-transparent'}"
            >
              <span class="h-1.5 w-1.5 shrink-0 rounded-full {dotClass[m.kind]}"></span>
              {m.name}
              <span
                class="ml-auto text-accent {m.name === value ? 'opacity-100' : 'opacity-0'}"
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
