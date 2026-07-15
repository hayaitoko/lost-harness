<script lang="ts">
  // Settings §3 Profiles page: the global list of profiles (click to select,
  // "New profile…" to add one) plus, for the selected profile, a read/link
  // summary of its posture bundle — memory wall, classifier strictness, and
  // skills autonomy travel together as one identity's posture. Grayscale
  // throughout; the selected row's border/check use --accent as the active-
  // control signal, not a routing color. Maps rows to `.set-row` (via
  // SettingRow) and the list to the `.ps-*` classes.
  import Button from "./Button.svelte";
  import SettingsSection from "./SettingsSection.svelte";
  import SettingRow from "./SettingRow.svelte";

  /** A switchable profile — plus a stable `id` for list selection. */
  export interface ProfilesSettingsProfile {
    id: string;
    name: string;
    /** Initials/emoji for the avatar chip. Falls back to the name's first letter. */
    avatar?: string;
    /** Secondary caption under the name, e.g. "Personal · walled memory". */
    sub?: string;
  }

  interface Props {
    /** Every profile the user can switch into, in display order. */
    profiles?: ProfilesSettingsProfile[];
    /** `id` of the profile whose posture bundle is summarized below the list. */
    selectedId?: string;
    onselect?: (id: string) => void;
    /** "New profile…" action — parent owns the create flow. */
    oncreate?: () => void;
    /** Selected profile's memory wall: a shared store vs. its own walled store. */
    memoryWallMode?: "shared" | "walled";
    /** Selected profile's classifier detection strictness, 0 (permissive) – 100 (paranoid). */
    classifierStrictness?: number;
    /** Selected profile's skills autonomy: approve-first review vs. autonomous. */
    skillsAutonomy?: "approve" | "autonomous";
    /** Optional deep-links out to the full per-profile pages. Omit to render plain read-only text. */
    onopenmemory?: () => void;
    onopenclassifier?: () => void;
    onopenskills?: () => void;
  }

  let {
    profiles = [
      { id: "personal", name: "Personal", avatar: "P", sub: "Personal · walled memory" },
      { id: "work", name: "Work", avatar: "W", sub: "Work · shared memory" },
      { id: "research", name: "Research", sub: "Research · walled memory" },
    ],
    selectedId = "personal",
    onselect,
    oncreate,
    memoryWallMode = "walled",
    classifierStrictness = 72,
    skillsAutonomy = "approve",
    onopenmemory,
    onopenclassifier,
    onopenskills,
  }: Props = $props();

  const rowBase =
    "flex items-center gap-[10px] w-full px-[10px] py-[9px] border rounded-[var(--r)] bg-surface text-text text-left transition hover:bg-surface-hover";
</script>

{#snippet chevron()}
  <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
    <path d="m9 6 6 6-6 6" />
  </svg>
{/snippet}

{#snippet check()}
  <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4">
    <path d="M5 12l4 4L19 6" />
  </svg>
{/snippet}

<!-- A posture-bundle value: plain text if there's nowhere to jump to, a chevroned link button if there is. -->
{#snippet bundleValue(value: string, onopen?: () => void)}
  {#if onopen}
    <button
      type="button"
      class="group inline-flex items-center gap-[3px] bg-transparent p-[2px] text-text-2 transition hover:text-text"
      onclick={onopen}
    >
      <span class="text-[12.5px] font-[550] text-text-2 group-hover:text-text">{value}</span>
      <span class="flex shrink-0 text-text-3">{@render chevron()}</span>
    </button>
  {:else}
    <span class="text-[12.5px] font-[550] text-text-2">{value}</span>
  {/if}
{/snippet}

<SettingsSection title="Profiles">
  <div class="mb-[10px] flex flex-col gap-[6px]" role="listbox" aria-label="Profiles">
    {#each profiles as p (p.id)}
      {@const sel = p.id === selectedId}
      <button
        type="button"
        role="option"
        aria-selected={sel}
        class="{rowBase} {sel ? 'border-accent' : 'border-border'}"
        onclick={() => onselect?.(p.id)}
      >
        <span
          class="grid size-7 shrink-0 place-items-center rounded-[var(--r-sm)] border border-border-strong bg-surface-2 text-[11px] font-[650] text-text-2"
        >
          {p.avatar ?? p.name.slice(0, 1).toUpperCase()}
        </span>
        <div class="min-w-0 flex-1">
          <div class="overflow-hidden text-ellipsis whitespace-nowrap text-[12.5px] font-[550]">
            {p.name}
          </div>
          {#if p.sub}
            <div class="mt-px overflow-hidden text-ellipsis whitespace-nowrap text-[11px] text-text-3">
              {p.sub}
            </div>
          {/if}
        </div>
        <span class="flex shrink-0 text-accent transition-opacity {sel ? 'opacity-100' : 'opacity-0'}">
          {@render check()}
        </span>
      </button>
    {/each}
  </div>
  <div class="mt-[2px]">
    <Button onclick={oncreate}>+ New profile…</Button>
  </div>
</SettingsSection>

<SettingsSection title="Posture bundle">
  <SettingRow
    title="Memory wall"
    desc="Shared memory store vs. this profile's own walled store."
  >
    {#snippet control()}
      {@render bundleValue(memoryWallMode === "walled" ? "Walled" : "Shared", onopenmemory)}
    {/snippet}
  </SettingRow>
  <SettingRow
    title="Classifier strictness"
    desc="How aggressively the privacy gate flags spans for this profile."
  >
    {#snippet control()}
      {@render bundleValue(`${classifierStrictness}/100`, onopenclassifier)}
    {/snippet}
  </SettingRow>
  <SettingRow
    title="Skills autonomy"
    desc="Approve-first review vs. letting self-taught skills run on their own."
  >
    {#snippet control()}
      {@render bundleValue(skillsAutonomy === "autonomous" ? "Autonomous" : "Approve-first", onopenskills)}
    {/snippet}
  </SettingRow>
</SettingsSection>
