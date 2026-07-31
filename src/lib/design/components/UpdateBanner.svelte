<script lang="ts">
  // Round-2 item 3 — the "Update available" strip.
  //
  // Shows only once something has FOUND a newer version — the launch check
  // (Rust emits `update:available`) or the Settings → About button, both of
  // which land in the `availableUpdate` store. It then does nothing until the
  // user clicks. The phases are deliberately separate clicks: offered →
  // downloading → installed-awaiting-relaunch. The app never installs on its
  // own and never restarts on its own.
  //
  // Calm rather than urgent: the design system's accent, not the blocked/alert
  // colour ClassifierHealthBanner uses. A pending update is good news, not a
  // problem. Dismissable for the same reason — unlike degraded privacy
  // screening, ignoring an update is a perfectly fine choice.
  import { onMount } from "svelte";
  import { installUpdate, onUpdateAvailable, relaunchApp } from "$lib/api/tauri";
  import { availableUpdate, clearAvailableUpdate, setAvailableUpdate } from "$lib/stores/update";

  let phase = $state<"offered" | "installing" | "ready" | "failed">("offered");
  let error = $state<string | null>(null);

  let found = $derived($availableUpdate);

  onMount(() => {
    let unlisten: (() => void) | undefined;
    onUpdateAvailable((info) => {
      phase = "offered";
      error = null;
      setAvailableUpdate(info);
    }).then((fn) => {
      unlisten = fn;
    });
    return () => unlisten?.();
  });

  async function install() {
    phase = "installing";
    error = null;
    try {
      await installUpdate();
      phase = "ready";
    } catch (err) {
      // A refused signature lands here. Say so plainly rather than retrying —
      // a bundle that failed verification is not something to try harder at.
      error = String(err);
      phase = "failed";
    }
  }

  function dismiss() {
    clearAvailableUpdate();
    phase = "offered";
    error = null;
  }
</script>

{#if found}
  <div
    class="fixed inset-x-0 bottom-0 z-40 flex items-center gap-3 border-t border-border-strong bg-surface px-4 py-2.5 text-[12.5px] text-text-2"
    role="status"
    aria-live="polite"
    data-testid="update-banner"
  >
    <span class="h-[7px] w-[7px] shrink-0 rounded-full bg-accent"></span>

    <div class="min-w-0 flex-1">
      {#if phase === "ready"}
        <span class="font-[550] text-text" data-testid="update-ready">
          Version {found.version} is installed
        </span>
        <span class="text-text-3"> — restart to start using it.</span>
      {:else if phase === "failed"}
        <span class="font-[550] text-text">Update not installed</span>
        <span class="text-text-3"> — the download didn't verify, so nothing was changed.</span>
      {:else}
        <span class="font-[550] text-text" data-testid="update-version">
          Update available {found.version}
        </span>
        <span class="text-text-3">
          — you're on {found.current_version}. Nothing downloads until you choose.
        </span>
      {/if}
      {#if error}
        <div class="mt-0.5 font-mono text-[11px] text-text-3" data-testid="update-error">
          {error}
        </div>
      {/if}
    </div>

    {#if phase === "ready"}
      <button
        type="button"
        class="shrink-0 rounded-[var(--r)] border border-transparent bg-accent px-[13px] py-[6px] text-[12.5px] font-semibold text-on-accent transition hover:brightness-105"
        data-testid="update-relaunch"
        onclick={() => relaunchApp()}
      >
        Restart now
      </button>
      <button
        type="button"
        class="shrink-0 rounded-[var(--r)] px-2 py-[6px] text-[12px] text-text-3 hover:text-text"
        data-testid="update-dismiss"
        onclick={dismiss}
      >
        Later
      </button>
    {:else if phase === "installing"}
      <span class="shrink-0 text-[12px] text-text-3" data-testid="update-installing">
        Downloading…
      </span>
    {:else}
      <button
        type="button"
        class="shrink-0 rounded-[var(--r)] border border-transparent bg-accent px-[13px] py-[6px] text-[12.5px] font-semibold text-on-accent transition hover:brightness-105"
        data-testid="update-install"
        onclick={install}
      >
        {phase === "failed" ? "Try again" : "Download and install"}
      </button>
      <button
        type="button"
        class="shrink-0 rounded-[var(--r)] px-2 py-[6px] text-[12px] text-text-3 hover:text-text"
        data-testid="update-dismiss"
        onclick={dismiss}
      >
        Not now
      </button>
    {/if}
  </div>
{/if}
