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

  // Active model + the provider it runs on — the SAME pair `MainScreen`'s
  // `armed` requires (selection AND its provider row), for the same reason.
  // Both segments hang off the row: no row, no segment.
  //
  // `engine` used to read `activeModel` on its own. That let this bar name a
  // model whose provider row was absent — the everyday shape while the cached
  // provider blob is unreadable and `hydrateProviders()` hasn't landed yet —
  // so the status bar claimed an endpoint every other display was calling
  // unarmed, and named it without being able to say what it runs on.
  let activeProvider = $derived(
    providersStore.providers.find((p) => p.id === providersStore.activeProviderId),
  );
  let engine = $derived(activeProvider ? (providersStore.activeModel ?? undefined) : undefined);
  let host = $derived(activeProvider?.name);

  // Real build version, resolved once on mount; hidden until it arrives (and
  // stays hidden if the IPC call fails — no fallback string).
  let version = $state("");
  getAppVersion()
    .then((v) => (version = v))
    .catch(() => {});
</script>

<StatusBar {engine} {host} version={version || undefined} {...props} />
