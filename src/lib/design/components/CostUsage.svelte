<script lang="ts">
  // Settings §3 Cost/usage page (per-profile): a spend summary (period total,
  // split local $0 vs cloud), a budget cap for unattended server work, and —
  // when this period's cost can't be reported — the FlyingBlindBanner amber
  // warning so an unattended run never spends silently. Local compute is always
  // free; only cloud turns and unattended server work carry cost.
  import FlyingBlindBanner from "./FlyingBlindBanner.svelte";
  import SettingsSection from "./SettingsSection.svelte";
  import SettingRow from "./SettingRow.svelte";
  import Slider from "./Slider.svelte";

  export interface CostUsageSpend {
    /** Section heading for the period/profile, e.g. "This month — Personal". */
    periodLabel: string;
    /** Cloud spend in dollars this period — the only part that isn't free. */
    cloudUsd: number;
    /** Cloud provider/model detail line, e.g. "Anthropic · Opus + Sonnet". */
    cloudDetail?: string;
    /** Fill percent (0-100) of the cost bar shown under cloud spend. */
    cloudBarPercent?: number;
    /** Local compute detail line, e.g. "Qwen3-14B · 1,204 turns". */
    localDetail?: string;
  }

  interface Props {
    /** Spend summary for the current period: total, split local $0 vs cloud. */
    spend: CostUsageSpend;
    /** True when this period's unattended server-work cost can't be computed yet. */
    flyingBlind?: boolean;
    /** Body text for the flying-blind banner and the amber summary row. */
    flyingBlindDetail?: string;
    /** Current budget cap (dollars) for unattended server work. */
    budgetCapUsd: number;
    /** Slider bounds for the cap input. */
    budgetCapMin?: number;
    budgetCapMax?: number;
    onbudgetcapchange?: (value: number) => void;
  }

  let {
    spend,
    flyingBlind = false,
    flyingBlindDetail = "This period's unattended server-work cost hasn't been reported yet — set a cap so it can't run away.",
    budgetCapUsd,
    budgetCapMin = 0,
    budgetCapMax = 100,
    onbudgetcapchange,
  }: Props = $props();

  const fmtUsd = (n: number) => `$${n.toFixed(2)}`;

  let totalUsd = $derived(spend.cloudUsd); // local is always $0
  let barPct = $derived(
    spend.cloudBarPercent != null
      ? Math.min(100, Math.max(0, spend.cloudBarPercent))
      : null,
  );
</script>

{#if flyingBlind}
  <FlyingBlindBanner>
    {#snippet children()}{flyingBlindDetail}{/snippet}
  </FlyingBlindBanner>
{/if}

<SettingsSection title={spend.periodLabel}>
  <!-- Total spend (raw .set-row, not a SettingRow — it carries a plain value) -->
  <div
    class="mb-[6px] flex items-center gap-[11px] rounded-[var(--r)] border border-border bg-surface px-[12px] py-[10px]"
  >
    <div class="min-w-0 flex-1">
      <div class="text-[13px] font-[550]">Total spend</div>
      <div class="text-[11.5px] leading-[1.4] text-text-3">
        Local $0.00 + cloud {fmtUsd(spend.cloudUsd)}
      </div>
    </div>
    <div class="text-[15px] font-[650]">{fmtUsd(totalUsd)}</div>
  </div>

  <!-- Cloud spend -->
  <div
    class="mb-[6px] flex items-center gap-[11px] rounded-[var(--r)] border border-border bg-surface px-[12px] py-[10px]"
  >
    <div class="min-w-0 flex-1">
      <div class="text-[13px] font-[550]">Cloud spend</div>
      {#if spend.cloudDetail}
        <div class="text-[11.5px] leading-[1.4] text-text-3">{spend.cloudDetail}</div>
      {/if}
      {#if barPct != null}
        <div class="mt-[7px] h-[6px] overflow-hidden rounded-full bg-surface-2">
          <span class="block h-full bg-accent" style="width: {barPct}%"></span>
        </div>
      {/if}
    </div>
    <div class="text-[15px] font-[650]">{fmtUsd(spend.cloudUsd)}</div>
  </div>

  <SettingRow title="Local compute" desc={spend.localDetail}>
    {#snippet control()}
      <span
        class="shrink-0 rounded-[var(--r-sm)] px-[7px] py-[2px] text-[10px] font-semibold bg-local-soft text-local"
      >
        free
      </span>
    {/snippet}
  </SettingRow>

  {#if flyingBlind}
    <SettingRow
      title="Unattended server work"
      desc={flyingBlindDetail}
      dotColor="var(--warn)"
    >
      {#snippet control()}
        <span
          class="shrink-0 rounded-[var(--r-sm)] px-[7px] py-[2px] text-[10px] font-semibold bg-warn-soft text-warn"
        >
          flying blind
        </span>
      {/snippet}
    </SettingRow>
  {/if}
</SettingsSection>

<SettingsSection title="Budget cap (unattended)">
  <SettingRow
    title="Stop server work above"
    desc="Applies only to unattended / background server runs, not messages you send live"
  >
    {#snippet control()}
      <div class="max-w-[220px]">
        <Slider
          min={budgetCapMin}
          max={budgetCapMax}
          value={budgetCapUsd}
          onchange={(v) => onbudgetcapchange?.(v)}
          format={(v) => `$${v}`}
        />
      </div>
    {/snippet}
  </SettingRow>
</SettingsSection>
