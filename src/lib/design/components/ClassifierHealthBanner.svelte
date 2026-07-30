<script lang="ts">
  // C-01: the READ side of the gate's degraded flag.
  //
  // The finding was that the fail-closed flag had zero call sites — nothing
  // could react to it, so a user on a fresh install (no classifier models on
  // disk) silently got rules-only screening with no way to know. This banner is
  // that reaction: a persistent, app-level strip that says screening is reduced
  // and what the gate is doing about it.
  //
  // Deliberately NOT a toast: a toast disappears, and this condition lasts for
  // the whole session (the flag only clears on a restart that actually loads the
  // models). Deliberately not dismissable for the same reason.
  import { onMount } from "svelte";
  import { getClassifierHealth, type ClassifierHealthInfo } from "$lib/api/tauri";

  let health = $state<ClassifierHealthInfo | null>(null);
  let showReason = $state(false);

  onMount(async () => {
    try {
      health = await getClassifierHealth();
    } catch (err) {
      // A failed health read is itself a reason to warn rather than stay quiet:
      // we cannot show that screening is fine, so say what we know.
      console.warn("[classifier] health read failed", err);
      health = {
        degraded: true,
        reason: "Could not read the classifier's status.",
        confirm_ttl_secs: 120,
      };
    }
  });
</script>

{#if health?.degraded}
  <div
    class="fixed inset-x-0 top-0 z-40 flex items-start gap-2.5 border-b border-blocked bg-blocked-soft px-4 py-2 text-[12.5px] text-blocked backdrop-blur-sm"
    role="status"
    aria-live="polite"
    data-testid="classifier-degraded-banner"
  >
    <svg
      width="15"
      height="15"
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      stroke-width="2"
      class="mt-px shrink-0"
      aria-hidden="true"
    >
      <path d="M12 9v5" stroke-linecap="round" />
      <circle cx="12" cy="17.2" r="1" fill="currentColor" stroke="none" />
      <path d="M12 3l9 17H3z" stroke-linejoin="round" />
    </svg>
    <div>
      <div class="font-semibold text-text">Reduced privacy screening</div>
      <div class="leading-[1.5] text-text-2">
        The trained privacy classifier isn't loaded, so only the built-in
        pattern rules are checking what leaves this machine. Conversations set
        to <b>Auto</b> will stay on a local model instead of using a cloud
        provider until it's back.
        {#if health.reason}
          <button
            type="button"
            class="ml-1 underline decoration-dotted underline-offset-2"
            onclick={() => (showReason = !showReason)}
          >
            {showReason ? "Hide details" : "Details"}
          </button>
        {/if}
      </div>
      {#if showReason && health.reason}
        <div class="mt-1 font-mono text-[11.5px] text-text-3">{health.reason}</div>
      {/if}
    </div>
  </div>
{/if}
