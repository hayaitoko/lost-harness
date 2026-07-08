<script lang="ts">
  // Lost Harness — Model picker (upward popup, grouped by provider).
  //
  // Reference: lost-harness-app/src/components/ModelSelector.jsx
  // Rewritten in Svelte 5 runes. The popup opens upward (above the
  // button) so it never gets clipped by the input area's bottom edge.
  //
  // Behaviour:
  //   • Button shows the active model name (or "Select model" if none).
  //   • Click toggles the popup; click-outside closes it; Esc closes.
  //   • Popup contains a search input and a list grouped by provider.
  //   • Selecting a row dispatches a CustomEvent<{providerId, model}>
  //     named "select" and closes the popup.
  //   • Empty providers list → "No providers configured" message.

  import { providersStore, setActiveModel } from "$lib/stores/providers.svelte";
  import { modelsForProvider } from "$lib/stores/provider-catalog";

  interface Props {
    /** When true, the button is rendered with a slightly different style. */
    compact?: boolean;
  }

  let { compact = true }: Props = $props();

  let open = $state(false);
  let search = $state("");
  let rootEl: HTMLDivElement | null = $state(null);
  let searchEl: HTMLInputElement | null = $state(null);

  // Derived: visible list of providers → their models, filtered by search.
  // Mirrors the structure the React version drives with `useMemo`.
  const filteredGroups = $derived.by(() => {
    const q = search.trim().toLowerCase();
    const matches = (a: string, b: string) =>
      !q || `${a} ${b}`.toLowerCase().includes(q);

    return providersStore.providers
      .map((p) => {
        const models = modelsForProvider(p)
          .filter((m) => matches(p.name, m.name))
          .map((m) => m.name);
        return { provider: p, models };
      })
      .filter((g) => g.models.length > 0 || !q);
  });

  const hasProviders = $derived(providersStore.providers.length > 0);
  const hasAnyMatches = $derived(
    filteredGroups.some((g) => g.models.length > 0),
  );

  // Active label: the model name, falling back to provider name, or
  // "Select model" if nothing is chosen yet.
  const activeProvider = $derived(
    providersStore.providers.find(
      (p) => p.id === providersStore.activeProviderId,
    ) ?? null,
  );
  const buttonLabel = $derived(
    providersStore.activeModel
      ? providersStore.activeModel
      : activeProvider
        ? `${activeProvider.name} — pick model`
        : "Select model",
  );

  function toggle() {
    const next = !open;
    open = next;
    if (next) {
      search = "";
      // Autofocus the search input on the next frame.
      requestAnimationFrame(() => searchEl?.focus());
    }
  }

  function close() {
    open = false;
  }

  function select(providerId: string, model: string) {
    setActiveModel(providerId, model);
    // Bubble the choice up so the parent (ChatPanel) can react even if
    // it doesn't want to subscribe to the store directly.
    rootEl?.dispatchEvent(
      new CustomEvent("select", {
        detail: { providerId, model },
        bubbles: true,
      }),
    );
    close();
  }

  function handleKeydown(e: KeyboardEvent) {
    if (e.key === "Escape" && open) {
      e.preventDefault();
      close();
    }
  }

  // Click-outside via document listener, only attached while open.
  $effect(() => {
    if (!open) return;
    function onDocClick(ev: MouseEvent) {
      if (!rootEl) return;
      if (ev.target instanceof Node && rootEl.contains(ev.target)) return;
      close();
    }
    document.addEventListener("mousedown", onDocClick);
    return () => document.removeEventListener("mousedown", onDocClick);
  });

  const dotClass = $derived.by(() => {
    if (!activeProvider) return "bg-neutral-500";
    return activeProvider.kind === "local" ? "bg-amber-500" : "bg-emerald-500";
  });
</script>

<div
  bind:this={rootEl}
  class="model-picker relative"
>
  <button
    type="button"
    class="model-button inline-flex items-center gap-1.5 rounded-lg border border-neutral-800 bg-neutral-900 px-2.5 py-1.5 text-xs font-medium text-neutral-200 transition hover:border-neutral-700 hover:bg-neutral-800 focus:border-indigo-500 focus:outline-none {compact
      ? ''
      : 'px-3 py-2 text-sm'}"
    aria-haspopup="listbox"
    aria-expanded={open}
    title={buttonLabel}
    onclick={toggle}
    data-testid="model-picker-button"
  >
    <span class="inline-block h-1.5 w-1.5 rounded-full {dotClass}" aria-hidden="true"></span>
    <span class="max-w-[12rem] truncate">{buttonLabel}</span>
    <svg
      class="h-3 w-3 text-neutral-500 transition-transform"
      class:rotate-180={open}
      viewBox="0 0 12 12"
      aria-hidden="true"
    >
      <path
        d="M2 8 L6 4 L10 8"
        fill="none"
        stroke="currentColor"
        stroke-width="1.5"
        stroke-linecap="round"
        stroke-linejoin="round"
      />
    </svg>
  </button>

  {#if open}
    <div
      class="model-dropdown absolute bottom-full left-0 z-50 mb-2 w-72 overflow-hidden rounded-xl border border-neutral-800 bg-neutral-950 shadow-2xl ring-1 ring-black/40"
      role="listbox"
      tabindex="-1"
      aria-label="Model"
      data-testid="model-picker-dropdown"
      onkeydown={handleKeydown}
    >
      <!-- Search -->
      <div class="border-b border-neutral-800 p-2">
        <input
          bind:this={searchEl}
          type="text"
          class="w-full rounded-md border border-neutral-800 bg-neutral-900 px-2 py-1.5 text-xs text-neutral-100 placeholder:text-neutral-500 focus:border-indigo-500 focus:outline-none"
          placeholder="Search models…"
          value={search}
          oninput={(e) => (search = (e.currentTarget as HTMLInputElement).value)}
        />
      </div>

      <!-- List -->
      <div class="max-h-72 overflow-y-auto py-1">
        {#if !hasProviders}
          <p
            class="px-3 py-4 text-center text-xs text-neutral-500"
            data-testid="model-picker-empty"
          >
            No providers configured
          </p>
        {:else if !hasAnyMatches && search}
          <p class="px-3 py-4 text-center text-xs text-neutral-500">No models match</p>
        {:else}
          {#each filteredGroups as group (group.provider.id)}
            <div class="px-1 py-1">
              <div
                class="flex items-center gap-1.5 px-2 py-1 text-[10px] font-semibold uppercase tracking-wider text-neutral-500"
              >
                <span>{group.provider.name}</span>
                <span
                  class="rounded px-1 py-0.5 text-[9px] font-medium normal-case tracking-normal {group.provider.kind ===
                  'local'
                    ? 'bg-amber-500/10 text-amber-400'
                    : 'bg-emerald-500/10 text-emerald-400'}"
                >
                  {group.provider.kind}
                </span>
              </div>
              {#if group.models.length === 0}
                <p class="px-2 py-1 text-xs text-neutral-500">no models</p>
              {:else}
                {#each group.models as model (model)}
                  {@const selected =
                    group.provider.id === providersStore.activeProviderId &&
                    providersStore.activeModel === model}
                  <button
                    type="button"
                    role="option"
                    aria-selected={selected}
                    class="flex w-full items-center justify-between rounded-md px-2 py-1.5 text-left text-xs text-neutral-200 transition hover:bg-neutral-900 {selected
                      ? 'bg-indigo-600/20 text-indigo-100'
                      : ''}"
                    onclick={() => select(group.provider.id, model)}
                    data-testid="model-picker-option"
                  >
                    <span class="truncate">{model}</span>
                    {#if selected}
                      <svg
                        class="h-3 w-3 text-indigo-400"
                        viewBox="0 0 12 12"
                        aria-hidden="true"
                      >
                        <path
                          d="M2 6 L5 9 L10 3"
                          fill="none"
                          stroke="currentColor"
                          stroke-width="1.5"
                          stroke-linecap="round"
                          stroke-linejoin="round"
                        />
                      </svg>
                    {/if}
                  </button>
                {/each}
              {/if}
            </div>
          {/each}
        {/if}
      </div>
    </div>
  {/if}
</div>
