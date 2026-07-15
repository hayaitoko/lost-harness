<script lang="ts">
  // Outline risk badge shown next to a tool name in the approval dialog.
  // Deliberately OUTLINE (border, currentColor) to stay visually distinct from
  // the filled routing chips. `.risk` + safe/write/dangerous.
  interface Props {
    risk: "safe" | "write" | "dangerous";
  }

  let { risk }: Props = $props();

  const tone = {
    safe: "text-local",
    write: "text-warn",
    dangerous: "text-blocked",
  };
</script>

<span
  class="rounded-[var(--r-sm)] border border-current bg-transparent px-[7px] py-0.5 text-[10px] font-bold uppercase tracking-[0.04em] {tone[
    risk
  ]}"
>
  {risk}
</span>
