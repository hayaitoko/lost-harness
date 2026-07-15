<script lang="ts" module>
  export interface Seat {
    id: string;
    /** Row label, e.g. "Writer", "Reviewer", "Coding". */
    label: string;
    /** Currently assigned model's `value` (must match one entry in `models`). */
    model: string;
  }

  export interface SeatModelOption {
    value: string;
    label: string;
  }
</script>

<script lang="ts">
  // Providers & models §3 — "seat assignment": bind a model to each role
  // (Writer / Reviewer / Coding). One grayscale SettingRow per seat with a
  // native select styled via the `.sel` look; no routing/risk signal applies
  // here, so no color chips are composed in. Renders inside the Settings
  // "Models" section.
  import SettingsSection from "./SettingsSection.svelte";
  import SettingRow from "./SettingRow.svelte";

  interface Props {
    /** The seats to bind a model to, in display order (Writer / Reviewer / Coding). */
    seats: Seat[];
    /** Every assignable model, in display order. */
    models: SeatModelOption[];
    onassign?: (seatId: string, model: string) => void;
  }

  let { seats, models, onassign }: Props = $props();

  /** Short, optional blurb per well-known seat id — omitted for unrecognized ids. */
  const SEAT_HINTS: Record<string, string> = {
    writer: "Drafts replies and content",
    reviewer: "Checks drafts before they go out",
    coding: "Handles code edits and tool calls",
  };
</script>

<SettingsSection title="Seat assignment">
  {#each seats as seat (seat.id)}
    <SettingRow title={seat.label} desc={SEAT_HINTS[seat.id.toLowerCase()]}>
      {#snippet control()}
        <select
          class="rounded-[var(--r-sm)] border border-border bg-surface-2 px-2 py-[5px] text-[12px] text-text outline-none"
          aria-label={`Model for ${seat.label}`}
          value={seat.model}
          onchange={(e) => onassign?.(seat.id, e.currentTarget.value)}
        >
          {#each models as m (m.value)}
            <option value={m.value}>{m.label}</option>
          {/each}
        </select>
      {/snippet}
    </SettingRow>
  {/each}
</SettingsSection>
