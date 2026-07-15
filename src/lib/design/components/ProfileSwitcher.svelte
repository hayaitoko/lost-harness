<script lang="ts">
  // The sidebar footer's profile chip — switching profiles switches the whole
  // posture (memory wall + skills autonomy) together. Clicking opens a popup
  // list of options with a check on the active one. Maps to `.profile` /
  // `.profile-menu`. The parent should be `relative` so the menu anchors to it.
  interface Profile {
    name: string;
    sub: string;
    avatar: string;
  }
  interface Props {
    /** Profiles the user can switch between — each is its own memory-wall + skills posture. */
    profiles: Profile[];
    /** `name` of the currently active profile. */
    active: string;
    onswitch?: (name: string) => void;
  }

  let { profiles, active, onswitch }: Props = $props();

  let open = $state(false);
  let chipEl: HTMLButtonElement | undefined;
  let menuEl: HTMLDivElement | undefined;

  let current = $derived(profiles.find((p) => p.name === active));

  $effect(() => {
    if (!open) return;
    const onDocMouseDown = (e: MouseEvent) => {
      const t = e.target as Node;
      if (chipEl?.contains(t) || menuEl?.contains(t)) return;
      open = false;
    };
    document.addEventListener("mousedown", onDocMouseDown);
    return () => document.removeEventListener("mousedown", onDocMouseDown);
  });
</script>

<button
  bind:this={chipEl}
  type="button"
  aria-haspopup="menu"
  aria-expanded={open}
  onclick={() => (open = !open)}
  class="flex w-full items-center gap-[9px] rounded-[var(--r)] p-1.5 text-left transition hover:bg-surface-hover"
>
  <span
    class="grid h-[26px] w-[26px] place-items-center rounded-[var(--r-sm)] border border-border-strong bg-surface-2 text-[11px] font-[650] text-text-2"
  >
    {current?.avatar}
  </span>
  <span class="min-w-0">
    <span class="block text-[12.5px] font-[550]">{current?.name}</span>
    <span class="block text-[10.5px] text-text-3">{current?.sub}</span>
  </span>
  <svg
    class="ml-auto text-text-3"
    width="14"
    height="14"
    viewBox="0 0 24 24"
    fill="none"
    stroke="currentColor"
    stroke-width="2"
    aria-hidden="true"
  >
    <path d="m7 15 5 5 5-5M7 9l5-5 5 5" />
  </svg>
</button>

<div
  bind:this={menuEl}
  role="menu"
  class="absolute bottom-[calc(100%+4px)] left-2.5 right-2.5 z-[60] overflow-hidden rounded-[var(--r)] border border-border-strong bg-surface shadow-[var(--shadow-pop)] transition
    {open
    ? 'translate-y-0 opacity-100'
    : 'pointer-events-none translate-y-1 opacity-0'}"
>
  {#each profiles as p (p.name)}
    <button
      type="button"
      role="menuitemradio"
      aria-checked={p.name === active}
      onclick={() => {
        onswitch?.(p.name);
        open = false;
      }}
      class="flex w-full items-center gap-[9px] px-[11px] py-2 text-left text-[12.5px] text-text transition hover:bg-surface-hover"
    >
      <span
        class="grid h-[22px] w-[22px] place-items-center rounded-[var(--r-sm)] border border-border-strong bg-surface-2 text-[10px] font-[650] text-text-2"
      >
        {p.avatar}
      </span>
      <span class="min-w-0">
        <span class="block">{p.name}</span>
        <span class="block text-[10.5px] text-text-3">{p.sub}</span>
      </span>
      <span class="ml-auto text-accent {p.name === active ? 'opacity-100' : 'opacity-0'}">
        <svg
          width="14"
          height="14"
          viewBox="0 0 24 24"
          fill="none"
          stroke="currentColor"
          stroke-width="2.4"
          aria-hidden="true"
        >
          <path d="M5 12l4 4L19 6" />
        </svg>
      </span>
    </button>
  {/each}
  <div class="my-0.5 h-px bg-border"></div>
  <button
    type="button"
    onclick={() => (open = false)}
    class="flex w-full items-center gap-[9px] px-[11px] py-2 text-left text-[12.5px] text-text transition hover:bg-surface-hover"
  >
    <span
      class="grid h-[22px] w-[22px] place-items-center rounded-[var(--r-sm)] border border-border-strong bg-surface-2 text-[10px] font-[650] text-text-2"
    >
      ⋯
    </span>
    <span>Manage profiles…</span>
  </button>
</div>
