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

  /**
   * The composer's ARMED selection — the provider+model pair a Send will
   * actually address. It comes from the provider store (`providersStore.active`
   * plus that provider's row), which is the same source `canSend` and
   * `handleSend` read.
   *
   * Deliberately NOT derived from {@link ModelGroup}s. The chip used to be
   * found by searching the FETCHED listings for the selected key, so a provider
   * whose `GET /models` failed (`items: []`) made the chip fall through to the
   * amber "No model selected" placeholder — while the store's `active` pair was
   * still set, Send stayed armed, and clicking it sent to that very provider.
   * A failed listing must never be able to make an armed selection look
   * unarmed: that is the original endpoint-routing bug's own shape (the UI
   * saying one thing while the send does another) reintroduced one layer up.
   */
  export interface ArmedSelection {
    /** `providerId::model`. Also what the popover highlights as chosen. */
    key: string;
    /** The model name the user picked. */
    model: string;
    /** The armed provider's display name. */
    provider: string;
    /** Trust zone of that endpoint as configured NOW — from `isPrivate` (the
     *  base URL the bytes go to), never from the user-typed `kind` label. */
    kind: "local" | "cloud";
    /**
     * Set when this endpoint's model listing did not confirm the armed model —
     * the listing failed, or the endpoint stopped offering it. The selection is
     * STILL armed and Send still goes exactly here; this is a warning about
     * what we know of the endpoint, not about the selection. Rendered as its
     * own affordance, distinct from both the healthy chip and the unarmed
     * placeholder.
     */
    unconfirmed?: string | null;
  }
</script>

<script lang="ts">
  // The composer's model control — the visible provider+model label plus an
  // upward-facing popover with model selection and thinking-strength choices.

  interface Props {
    /** Every provider's models, in display order. */
    groups: ModelGroup[];
    /**
     * The ARMED provider+model pair, or `null` when the composer really is
     * unarmed. This is the picker's ONLY source of truth for what is selected —
     * see {@link ArmedSelection} for why it must not come from `groups`.
     */
    selection: ArmedSelection | null;
    /** Shown on the button when nothing is selected. */
    placeholder?: string;
    onchange?: (key: string) => void;
    /**
     * Fired when the popover OPENS — never when it closes. Lets the surrounding
     * composer close any competing popover.
     *
     * It must stay side-effect-cheap. It was previously fired on both edges and
     * wired to a cache-bypassing re-list of every configured endpoint, so a
     * single open-then-close produced two authenticated `GET /models` fan-outs.
     * Network re-listing lives on {@link Props.onrefresh}, which the user asks
     * for explicitly.
     */
    onopen?: () => void;
    /** User-initiated re-list of the endpoints' models. This CONTACTS every
     *  configured endpoint, so it is a button, not a side effect of opening. */
    onrefresh?: () => void;
    /** True while an {@link Props.onrefresh} round is in flight. */
    refreshing?: boolean;
  }

  let {
    groups,
    selection,
    placeholder = "No model selected",
    onchange,
    onopen,
    onrefresh,
    refreshing = false,
  }: Props = $props();

  let open = $state(false);
  let query = $state("");
  let thinkingStrength = $state<"light" | "balanced" | "deep">("balanced");
  let rootEl: HTMLDivElement;

  // The highlighted option key follows the armed pair — one source of truth for
  // "what is selected", shared by the chip and the option checkmarks. When the
  // armed endpoint's listing failed there is no matching option to highlight,
  // which is correct: nothing in the list IS the armed model right now. The
  // chip still shows it, because the chip reports the SELECTION, not the list.
  const value = $derived(selection?.key ?? "");

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
      // Rising edge only. `onopen?.(); open = !open;` fired on the CLOSING
      // click too, which doubled whatever the host did on open — and the host
      // did a live, cache-bypassing model listing against every configured
      // endpoint, cloud ones included.
      const next = !open;
      if (next) onopen?.();
      open = next;
    }}
    title={selection
      ? selection.unconfirmed
        ? `Sending to ${selection.provider} · ${selection.model} — ${selection.unconfirmed} Click to change.`
        : `Sending to ${selection.provider} · ${selection.model} — click to change`
      : "No model selected — click to pick one"}
    class="relative flex h-[36px] max-w-[240px] items-center gap-[7px] rounded-full border px-[9px] transition-[background-color,color,border-color] duration-150 focus-visible:border-accent focus-visible:outline-none
      {selection
      ? selection.unconfirmed
        ? 'border-warn/45 text-text-2 hover:bg-surface-hover hover:text-text'
        : open
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
         for it — the user must be able to see it while typing.

         Rendered from the ARMED PAIR, never from the fetched listings: an
         endpoint whose `GET /models` failed still has an armed selection, and
         showing the "no model selected" placeholder for it while Send remained
         live is precisely the UI-says-one-thing / send-does-another failure
         this component is meant to prevent. -->
    {#if selection}
      <span class="h-1.5 w-1.5 shrink-0 rounded-full {dotClass[selection.kind]}"></span>
      <span class="min-w-0 truncate text-[12px] font-medium leading-none">
        <span class="text-text-3">{selection.provider}</span>
        <span class="text-text-3"> · </span>{selection.model}
      </span>
      {#if selection.unconfirmed}
        <!-- Armed, but this endpoint's model list didn't confirm the choice.
             Its own affordance: the selection is real and Send WILL use it, so
             it must not wear the unarmed placeholder's amber fill — but the
             user is owed the fact that we couldn't check the endpoint. -->
        <svg
          width="13"
          height="13"
          viewBox="0 0 24 24"
          fill="none"
          stroke="currentColor"
          stroke-width="2"
          aria-hidden="true"
          class="shrink-0 text-warn"
        >
          <path d="M12 4.5 2.8 20h18.4L12 4.5Z" stroke-linejoin="round" />
          <path d="M12 10v4M12 17h.01" stroke-linecap="round" />
        </svg>
      {/if}
    {:else}
      <span class="truncate text-[12px] font-semibold leading-none">{placeholder}</span>
    {/if}
    <!-- The provider+model is already in the visible label above, so it is
         part of the accessible name; only the thinking strength (and the
         unconfirmed-listing warning) need adding. -->
    <span class="sr-only"
      >{selection?.unconfirmed ? `; ${selection.unconfirmed}` : ""}; thinking strength {thinkingStrength}</span
    >
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
    <div class="flex items-center gap-1.5 border-b border-border p-2">
      <input
        placeholder="Search models…"
        bind:value={query}
        class="min-w-0 flex-1 rounded-[var(--r-sm)] border border-border bg-surface-2 px-[9px] py-[6px] text-[12px] text-text outline-none"
      />
      <!-- Re-listing CONTACTS every configured endpoint with its stored key, so
           it is an explicit, labelled action. Doing it silently on every picker
           click — which is what `onopen` used to do — is egress the user never
           asked for, and in this app that is a privacy regression, not a
           freshness feature. -->
      <button
        type="button"
        onclick={() => onrefresh?.()}
        disabled={refreshing || !onrefresh}
        title="Ask every configured endpoint for its model list again. This contacts each endpoint, including cloud ones, with its stored key."
        class="shrink-0 rounded-[var(--r-sm)] border border-border px-[9px] py-[6px] text-[11px] font-semibold text-text-2 transition-[0.1s] enabled:hover:bg-surface-hover enabled:hover:text-text disabled:opacity-50"
      >
        {refreshing ? "Refreshing…" : "Refresh"}
      </button>
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
