<script lang="ts">
  // Settings §3 Skills page (per-profile): an approve-first vs. autonomous
  // control, a "what I taught myself" feed of learned skills (each editable /
  // forgettable, mirroring the memory viewer), and — when the assistant has
  // taught itself something under approve-first — a draft-review section where
  // each pending skill gets an Approve / Reject pair (no one-click primary grant).
  //
  // The .set-sec / .set-row chrome is inlined here (grayscale); the only
  // saturated color is the amber "draft" flag inside SkillListItem — a genuine
  // needs-review signal, not decoration.
  import SegmentedControl from "./SegmentedControl.svelte";
  import SkillListItem, { type SkillItem } from "./SkillListItem.svelte";

  /** How the assistant handles skills it teaches itself for this profile. */
  export type SkillAutonomy = "approve" | "autonomous";

  interface Props {
    autonomy: SkillAutonomy;
    onautonomychange?: (value: SkillAutonomy) => void;
    /** The full skill list; `draft` items are split into the review section. */
    skills: SkillItem[];
    oneditskill?: (id: string, name: string) => void;
    ondeleteskill?: (id: string) => void;
    onapproveskill?: (id: string) => void;
    onrejectskill?: (id: string) => void;
  }

  let {
    autonomy,
    onautonomychange,
    skills,
    oneditskill,
    ondeleteskill,
    onapproveskill,
    onrejectskill,
  }: Props = $props();

  const drafts = $derived(skills.filter((s) => s.status === "draft"));
  const learned = $derived(skills.filter((s) => s.status !== "draft"));

  const sectionTitle =
    "mb-2 text-[11px] font-[650] uppercase tracking-[0.05em] text-text-3";
  const note = "mx-0.5 mb-3.5 mt-1 text-[11.5px] leading-[1.5] text-text-3";
  const feed =
    "flex flex-col overflow-hidden rounded-[var(--r-lg)] border border-border";
</script>

<div class="mb-5 last:mb-0">
  <h3 class={sectionTitle}>When I learn a new skill</h3>
  <div
    class="mb-1.5 flex items-center gap-[11px] rounded-[var(--r)] border border-border bg-surface px-3 py-2.5"
  >
    <div class="min-w-0 flex-1">
      <div class="text-[13px] font-[550]">New skills</div>
      <div class="text-[11.5px] leading-[1.4] text-text-3">
        Approve-first asks before using anything I teach myself · Autonomous lets
        me apply it right away
      </div>
    </div>
    <SegmentedControl
      options={[
        { value: "approve", label: "Approve-first" },
        { value: "autonomous", label: "Autonomous" },
      ]}
      value={autonomy}
      onchange={(v) => onautonomychange?.(v as SkillAutonomy)}
    />
  </div>
</div>

{#if drafts.length > 0}
  <div class="mb-5 last:mb-0">
    <h3 class={sectionTitle}>Waiting for your review</h3>
    <div class={note}>
      I taught myself these but haven't used them yet. Approve to add them to the
      feed, or reject to discard.
    </div>
    <div class={feed}>
      {#each drafts as s (s.id)}
        <SkillListItem skill={s} onapprove={onapproveskill} onreject={onrejectskill} />
      {/each}
    </div>
  </div>
{/if}

<div class="mb-5 last:mb-0">
  <h3 class={sectionTitle}>What I've taught myself</h3>
  <div class={note}>
    Skills I picked up from how you work — each stays on this profile. Click a
    name to rename it.
  </div>
  <div class={feed}>
    {#if learned.length === 0}
      <div class="px-3.5 py-4 text-[12px] text-text-3">
        Nothing learned yet. Skills show up here as I pick up how you like things
        done.
      </div>
    {:else}
      {#each learned as s (s.id)}
        <SkillListItem skill={s} onedit={oneditskill} ondelete={ondeleteskill} />
      {/each}
    {/if}
  </div>
</div>
