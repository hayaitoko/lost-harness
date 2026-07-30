<script lang="ts">
  // Small routing dot used in the conversation list. `.route-dot` → a 6px dot
  // colored by where the turn went (auto = neutral text-3).
  import type { Route } from "../types";

  interface Props {
    route: Route | "auto";
  }

  let { route }: Props = $props();

  const color: Record<Route | "auto", string> = {
    local: "bg-local",
    cloud: "bg-cloud",
    blocked: "bg-blocked",
    // An unrecorded route gets the warn fill, not the neutral one: neutral
    // reads as "no routing yet", which is a different (and calmer) thing.
    unknown: "bg-warn",
    auto: "bg-text-3",
  };
</script>

<span class="h-1.5 w-1.5 shrink-0 rounded-full {color[route]}"></span>
