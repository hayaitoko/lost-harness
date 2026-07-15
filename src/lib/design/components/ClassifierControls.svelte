<script lang="ts" module>
  /** How wide the "unsure — keep local" margin is around the strictness threshold. */
  export type UncertaintyBand = "narrow" | "medium" | "wide";

  /** One row in the hard-block category list. Built-ins ship locked (immutable);
   * custom ones the user adds can be toggled. */
  export interface HardBlockCategory {
    /** Stable id used in `oncategorychange`. */
    id: string;
    label: string;
    desc?: string;
    /** Locked categories render a disabled toggle that's always on (e.g. Health,
     * Credentials, Financial, SSN). */
    locked?: boolean;
    enabled: boolean;
  }
</script>

<script lang="ts">
  // Settings §3 "Classifier / privacy" page content, per-profile: detection
  // strictness, the uncertainty band, redaction on/off, and the immutable
  // hard-block category list. Purely presentational — values + change handlers
  // are owned by the caller. Maps to `.set-sec` / `.set-row` groups.
  import SettingsSection from "./SettingsSection.svelte";
  import SettingRow from "./SettingRow.svelte";
  import SegmentedControl from "./SegmentedControl.svelte";
  import Toggle from "./Toggle.svelte";
  import Button from "./Button.svelte";

  interface Props {
    /** Detection strictness, 0 (low/permissive) – 100 (high/paranoid). */
    strictness: number;
    onstrictnesschange?: (value: number) => void;
    /** How wide the "unsure" band is — wider keeps more borderline content local. */
    uncertaintyBand: UncertaintyBand;
    onuncertaintybandchange?: (value: UncertaintyBand) => void;
    /** When on, a mostly-safe message can black out sensitive spans and send only
     * the remainder to the cloud. */
    redactionEnabled: boolean;
    onredactionchange?: (value: boolean) => void;
    /** The hard-block category list — content that never leaves, under any binding. */
    categories: HardBlockCategory[];
    oncategorychange?: (id: string, enabled: boolean) => void;
    /** "Add category…" handler — parent owns the add flow (dialog, picker, etc). */
    onaddcategory?: () => void;
  }

  let {
    strictness,
    onstrictnesschange,
    uncertaintyBand,
    onuncertaintybandchange,
    redactionEnabled,
    onredactionchange,
    categories,
    oncategorychange,
    onaddcategory,
  }: Props = $props();

  const BAND_OPTIONS: { value: UncertaintyBand; label: string }[] = [
    { value: "narrow", label: "Narrow" },
    { value: "medium", label: "Medium" },
    { value: "wide", label: "Wide" },
  ];
</script>

<SettingsSection title="Detection">
  <SettingRow
    title="Detection strictness"
    desc="Lower is more paranoid — more content stays local."
  >
    {#snippet control()}
      <div class="slider flex max-w-[200px] items-center gap-[11px]">
        <input
          type="range"
          min={0}
          max={100}
          value={strictness}
          aria-label="Detection strictness"
          oninput={(e) =>
            onstrictnesschange?.(Number(e.currentTarget.value))}
        />
        <span
          class="shrink-0 rounded-[var(--r-sm)] bg-surface-2 px-[7px] py-0.5 text-[10px] font-semibold text-text-2"
        >
          {strictness}
        </span>
      </div>
    {/snippet}
  </SettingRow>

  <SettingRow
    title="Uncertainty band"
    desc="How wide a margin counts as unsure. Unsure stays local rather than guessing."
  >
    {#snippet control()}
      <SegmentedControl
        options={BAND_OPTIONS}
        value={uncertaintyBand}
        onchange={(v) => onuncertaintybandchange?.(v as UncertaintyBand)}
      />
    {/snippet}
  </SettingRow>
</SettingsSection>

<SettingsSection title="Redaction">
  <SettingRow
    title="Redaction / partial delegation"
    desc="Black out sensitive spans and send only the safe parts to the cloud."
  >
    {#snippet control()}
      <Toggle
        checked={redactionEnabled}
        label="Redaction / partial delegation"
        onchange={onredactionchange}
      />
    {/snippet}
  </SettingRow>
</SettingsSection>

<SettingsSection
  title="Hard-block categories (never leave — any binding, no override)"
>
  {#each categories as c (c.id)}
    <SettingRow title={c.label} desc={c.desc}>
      {#snippet control()}
        <Toggle
          checked={c.enabled}
          locked={c.locked}
          label={c.label}
          onchange={(v) => oncategorychange?.(c.id, v)}
        />
      {/snippet}
    </SettingRow>
  {/each}

  <div class="mt-1">
    <Button variant="ghost" onclick={onaddcategory}>
      <span class="inline-flex items-center gap-1.5">
        <svg
          width="13"
          height="13"
          viewBox="0 0 24 24"
          fill="none"
          stroke="currentColor"
          stroke-width="2.1"
          aria-hidden="true"
        >
          <path d="M12 5v14M5 12h14" stroke-linecap="round" />
        </svg>
        Add category…
      </span>
    </Button>
  </div>
</SettingsSection>

<style>
  /* Irreducible: the native range track + thumb can't be expressed as Tailwind
     utilities. Mirrors `input[type=range]` in the design's components.css. */
  input[type="range"] {
    -webkit-appearance: none;
    appearance: none;
    height: 4px;
    border-radius: 999px;
    background: var(--surface-2);
    flex: 1;
    outline: none;
  }
  input[type="range"]::-webkit-slider-thumb {
    -webkit-appearance: none;
    width: 15px;
    height: 15px;
    border-radius: 50%;
    background: var(--accent);
    border: 2px solid var(--bg);
    cursor: pointer;
  }
  input[type="range"]::-moz-range-thumb {
    width: 15px;
    height: 15px;
    border-radius: 50%;
    background: var(--accent);
    border: 2px solid var(--bg);
    cursor: pointer;
  }
</style>
