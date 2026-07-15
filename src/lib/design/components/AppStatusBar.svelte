<script lang="ts">
  // The bottom status bar pre-filled with the DC templates' default props.
  // A thin wrapper over StatusBar: every screen drops <AppStatusBar/> and only
  // overrides the fields that differ (e.g. lastRoute after a turn).
  import type { ComponentProps } from "svelte";
  import StatusBar from "./StatusBar.svelte";

  let props: Partial<ComponentProps<typeof StatusBar>> = $props();

  const defaults: ComponentProps<typeof StatusBar> = {
    engine: "Qwen3-14B",
    host: "tadashi",
    guardOn: true,
    tools: 12,
    skills: 4,
    binding: "auto",
    lastRoute: "local",
    session: "0:00",
    cost: "$0.00",
    version: "v0.1",
  };
</script>

<StatusBar {...defaults} {...props} />
