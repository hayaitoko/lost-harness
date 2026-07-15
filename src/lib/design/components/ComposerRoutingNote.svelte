<script lang="ts">
  // The composer's live "where will this go" line (§2.1) — a leading RouteDot
  // plus a short grayscale prediction; the dot carries the only color. Maps to
  // the mockup's `.routing-note` row under the composer's row2.
  import type { Binding } from "../types";
  import RouteDot from "./RouteDot.svelte";

  interface Props {
    /** The conversation's binding — the user's routing intent. */
    binding: Binding;
    /**
     * Live per-message prediction for where the *next* message will go —
     * only meaningful while `binding` is `'auto'`. Omit (e.g. while the
     * composer is empty) to fall back to the default Auto guidance line.
     */
    predicted?: "local" | "cloud";
  }

  let { binding, predicted }: Props = $props();

  function describe(binding: Binding, predicted?: "local" | "cloud") {
    if (binding === "private") {
      return { route: "local" as const, prefix: "Private ·", bold: "nothing leaves this device", suffix: "" };
    }
    if (binding === "public") {
      return { route: "cloud" as const, prefix: "Public ·", bold: "this chat may go to the cloud", suffix: "" };
    }
    // binding === 'auto' — reflects the live per-message prediction when known
    if (predicted === "cloud") {
      return { route: "cloud" as const, prefix: "Likely", bold: "cloud", suffix: " — nothing sensitive detected" };
    }
    if (predicted === "local") {
      return { route: "local" as const, prefix: "Likely", bold: "local", suffix: " — sensitive content detected" };
    }
    return { route: "local" as const, prefix: "Auto-routing ·", bold: "sensitive content stays local", suffix: "" };
  }

  let note = $derived(describe(binding, predicted));
</script>

<div class="flex items-center gap-[7px] text-[11px] text-text-3">
  <RouteDot route={note.route} />
  <span>
    {note.prefix} <b class="font-semibold text-text-2">{note.bold}</b>{note.suffix}
  </span>
</div>
