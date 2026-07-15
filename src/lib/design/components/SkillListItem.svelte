<script lang="ts">
  // One row of the Skills feed. `active` skills show an inline-editable name
  // (click to edit) with edit + forget controls; `draft` skills show an amber
  // "draft" flag and an Approve / Reject pair. Approve is deliberately NOT a
  // primary button — self-taught skills shouldn't be fast-granted by muscle
  // memory (same rule the tool-approval dialog applies to dangerous calls).
  import Button from "./Button.svelte";

  /** One self-taught skill — either live (`active`) or awaiting review (`draft`). */
  export interface SkillItem {
    id: string;
    name: string;
    /** When it was learned, pre-formatted, e.g. "Jul 10". */
    learnedDate: string;
    /** What taught it / when it fires. */
    source: string;
    status?: "active" | "draft";
  }

  interface Props {
    skill: SkillItem;
    /** Commit an edited name (active rows only). */
    onedit?: (id: string, name: string) => void;
    /** Forget an active skill. */
    ondelete?: (id: string) => void;
    /** Approve a draft skill into the active feed. */
    onapprove?: (id: string) => void;
    /** Reject/discard a draft skill. */
    onreject?: (id: string) => void;
  }

  let { skill, onedit, ondelete, onapprove, onreject }: Props = $props();

  let editing = $state(false);
  const isDraft = $derived(skill.status === "draft");

  function commit(value: string) {
    const v = value.trim();
    if (v && v !== skill.name) onedit?.(skill.id, v);
    editing = false;
  }
  function onKey(e: KeyboardEvent) {
    const el = e.currentTarget as HTMLInputElement;
    if (e.key === "Enter") {
      e.preventDefault();
      el.blur();
    } else if (e.key === "Escape") {
      e.preventDefault();
      editing = false;
    }
  }
  function focus(node: HTMLInputElement) {
    node.focus();
    node.select();
  }

  const rowBase =
    "flex items-start gap-2.5 border-b border-border py-[9px] pl-3 pr-2.5 last:border-b-0";
  const nameBtn =
    "inline-block max-w-full text-left rounded-[var(--r-sm)] px-[3px] py-0.5 -mx-[3px] -my-0.5 text-[13px] font-[550] text-text hover:bg-surface-hover";
</script>

<div class="{rowBase} {isDraft ? 'bg-surface' : ''}">
  <div class="min-w-0 flex-1">
    {#if editing && !isDraft}
      <input
        use:focus
        class="w-full rounded-[var(--r)] border border-accent bg-surface-2 px-2 py-1 text-[13px] text-text outline-none"
        value={skill.name}
        aria-label="Edit skill name"
        onkeydown={onKey}
        onblur={(e) => commit((e.currentTarget as HTMLInputElement).value)}
      />
    {:else if isDraft}
      <span class="inline-block max-w-full px-[3px] py-0.5 -mx-[3px] -my-0.5 text-[13px] font-[550] text-text">
        {skill.name}
      </span>
    {:else}
      <button type="button" class={nameBtn} title="Click to edit" onclick={() => (editing = true)}>
        {skill.name}
      </button>
    {/if}
    <div class="mt-0.5 text-[11.5px] leading-[1.45] text-text-3">{skill.source}</div>
  </div>

  {#if isDraft}
    <span
      class="shrink-0 rounded-[var(--r-sm)] bg-warn-soft px-[7px] py-0.5 text-[10px] font-semibold text-warn"
    >
      draft
    </span>
    <span class="shrink-0 self-center text-[11px] text-text-3">{skill.learnedDate}</span>
    <div class="flex shrink-0 items-center gap-2 self-center">
      <Button variant="ghost" onclick={() => onreject?.(skill.id)}>Reject</Button>
      <Button onclick={() => onapprove?.(skill.id)}>Approve</Button>
    </div>
  {:else}
    <div class="flex shrink-0 items-center gap-2 self-center">
      <span class="text-[11px] text-text-3">{skill.learnedDate}</span>
      <div class="flex shrink-0 items-center gap-0.5 self-center">
        <button
          type="button"
          class="grid size-6 place-items-center rounded-[var(--r-sm)] text-text-3 transition-[background-color,color] duration-100 hover:bg-surface-hover hover:text-text-2"
          aria-label="Edit this skill"
          onclick={() => (editing = true)}
        >
          <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8">
            <path d="M5 19l1-4L16 5l3 3L9 18z" />
          </svg>
        </button>
        <button
          type="button"
          class="grid size-6 place-items-center rounded-[var(--r-sm)] text-text-3 transition-[background-color,color] duration-100 hover:bg-surface-hover hover:text-text-2"
          aria-label="Forget this skill"
          onclick={() => ondelete?.(skill.id)}
        >
          <svg width="11" height="11" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
            <path d="M6 6l12 12M18 6 6 18" />
          </svg>
        </button>
      </div>
    </div>
  {/if}
</div>
