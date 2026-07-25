<script lang="ts">
  // The bottom status bar wired to REAL app state — never fabricated values.
  // A thin wrapper over StatusBar: every screen drops <AppStatusBar/> and
  // passes only the fields it genuinely knows (e.g. binding, lastRoute after
  // a turn); call-site props spread LAST so they win over the derived ones.
  //
  // tools / skills / cost / session are deliberately NOT passed here: there
  // is no cheap real source for them, and a hidden segment beats a fabricated
  // one (honest-Unknown).
  import type { ComponentProps } from "svelte";
  import StatusBar from "./StatusBar.svelte";
  import { providersStore } from "$lib/stores/providers.svelte";
  import { getAppVersion } from "$lib/api/tauri";

  let props: Partial<ComponentProps<typeof StatusBar>> = $props();

  // Active model + the provider it runs on, straight from the provider store;
  // undefined (segment hidden) until the user has actually picked one.
  let engine = $derived(providersStore.activeModel ?? undefined);
  let host = $derived(
    providersStore.providers.find((p) => p.id === providersStore.activeProviderId)
      ?.name,
  );

  // Real build version, resolved once on mount; hidden until it arrives (and
  // stays hidden if the IPC call fails — no fallback string).
  let version = $state("");
  getAppVersion()
    .then((v) => (version = v))
    .catch(() => {});
</script>

<StatusBar {engine} {host} version={version || undefined} {...props} />
