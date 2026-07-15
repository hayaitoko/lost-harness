<script lang="ts">
  // Settings § Server pairing (global) — this device's pairing code (with a QR
  // placeholder, no QR lib), a code-entry field to pair this app to a server,
  // and the list of already-paired servers. No password fields — pairing only.
  // Composes SettingRow / Button / IconButton and the existing `.qr` chip; the
  // code display and entry field are new structure (styles/server-pairing.css).
  import Button from "./Button.svelte";
  import IconButton from "./IconButton.svelte";
  import SettingRow from "./SettingRow.svelte";

  /** A server this app is already paired to. */
  interface PairedServer {
    /** Display name, e.g. "friday". */
    name: string;
    /** Address/host shown under the name, e.g. "10.0.0.200". */
    address: string;
    /** Reachability of the paired server. Defaults to `true` (online). */
    online?: boolean;
  }

  interface Props {
    /** This device's pairing code — shown large, monospace, and copyable. */
    code: string;
    /** Called with the trimmed code the user typed when they press "Pair". */
    onpair: (enteredCode: string) => void;
    /** Already-paired server(s). Omit or pass empty when nothing is paired. */
    paired?: PairedServer[];
  }

  let { code, onpair, paired = [] }: Props = $props();

  let entered = $state("");
  let copied = $state(false);
  let copyTimer: ReturnType<typeof setTimeout> | undefined;

  function handleCopy() {
    navigator.clipboard?.writeText(code).catch(() => {});
    copied = true;
    clearTimeout(copyTimer);
    copyTimer = setTimeout(() => (copied = false), 1600);
  }

  function handlePair() {
    const trimmed = entered.trim();
    if (!trimmed) return;
    onpair(trimmed);
    entered = "";
  }
</script>

<div>
  <h2 class="mb-[3px] text-[15px] font-semibold">Server pairing</h2>
  <p class="mb-4 text-[12.5px] text-text-3">
    Pair a second machine — your always-on "second brain" — with a code or QR. No
    passwords.
  </p>

  <div class="mb-5">
    <h3
      class="mb-2 text-[11px] font-[650] uppercase tracking-[0.05em] text-text-3"
    >
      This device
    </h3>
    <div
      class="mb-1.5 flex items-center gap-4 rounded-[var(--r)] border border-border bg-surface px-3 py-2.5"
    >
      <div class="flex shrink-0 flex-col items-center gap-1.5">
        <div
          class="grid h-[120px] w-[120px] place-items-center rounded-[var(--r)] border border-border bg-surface-2 text-text-3"
        >
          <svg
            width="34"
            height="34"
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            stroke-width="1.6"
          >
            <rect x="4" y="4" width="6" height="6" />
            <rect x="14" y="4" width="6" height="6" />
            <rect x="4" y="14" width="6" height="6" />
            <path d="M14 14h3v3M20 14v6M14 20h3" />
          </svg>
        </div>
        <div class="max-w-[120px] text-center text-[11.5px] leading-[1.4] text-text-3">
          Scan on the other device
        </div>
      </div>
      <div class="min-w-0 flex-1">
        <div class="text-[13px] font-[550]">Pairing code</div>
        <div class="mb-2 text-[11.5px] leading-[1.4] text-text-3">
          Enter this on the server, or scan the code above.
        </div>
        <div class="flex items-center gap-2">
          <code
            class="select-all rounded-[var(--r)] border border-border bg-surface px-[14px] py-2 font-mono text-[18px] font-semibold tracking-[2.5px] text-text"
            >{code}</code
          >
          <IconButton
            label={copied ? "Copied" : "Copy code"}
            onclick={handleCopy}
          >
            {#if copied}
              <svg
                width="14"
                height="14"
                viewBox="0 0 24 24"
                fill="none"
                stroke="currentColor"
                stroke-width="2.5"
              >
                <path d="M5 12l4 4L19 6" />
              </svg>
            {:else}
              <svg
                width="14"
                height="14"
                viewBox="0 0 24 24"
                fill="none"
                stroke="currentColor"
                stroke-width="1.9"
              >
                <rect x="8" y="8" width="12" height="12" rx="2" />
                <path d="M16 8V6a2 2 0 0 0-2-2H6a2 2 0 0 0-2 2v8a2 2 0 0 0 2 2h2" />
              </svg>
            {/if}
          </IconButton>
        </div>
      </div>
    </div>
  </div>

  <div class="mb-5">
    <h3
      class="mb-2 text-[11px] font-[650] uppercase tracking-[0.05em] text-text-3"
    >
      Pair this app to a server
    </h3>
    <div class="flex items-center gap-2">
      <input
        class="pairing-input min-w-0 flex-1 rounded-[var(--r)] border border-border bg-surface-2 px-[10px] py-[7px] font-mono text-[13px] tracking-[1px] text-text outline-none"
        bind:value={entered}
        onkeydown={(e) => {
          if (e.key === "Enter") handlePair();
        }}
        placeholder="Enter code, e.g. 7F2K-9QX4"
        aria-label="Pairing code"
        autocomplete="off"
        autocorrect="off"
        spellcheck={false}
      />
      <Button variant="primary" onclick={handlePair} disabled={!entered.trim()}>
        Pair
      </Button>
    </div>
  </div>

  <div class="mb-5 last:mb-0">
    <h3
      class="mb-2 text-[11px] font-[650] uppercase tracking-[0.05em] text-text-3"
    >
      Paired
    </h3>
    {#if paired.length === 0}
      <p class="m-0 text-[12.5px] text-text-3">No servers paired yet.</p>
    {:else}
      {#each paired as s (s.name + s.address)}
        <SettingRow
          title={s.name}
          desc={s.address}
          dotColor={s.online === false ? "var(--text-3)" : "var(--local)"}
          tag={s.online === false
            ? { label: "offline", color: "var(--text-3)", bg: "var(--surface-2)" }
            : { label: "paired", color: "var(--local)", bg: "var(--local-soft)" }}
        />
      {/each}
    {/if}
  </div>
</div>

<style>
  /* Irreducible: placeholder overrides + focus border uses color-mix, neither of
     which Tailwind can express cleanly. Mirrors `.pairing-input` in the design. */
  .pairing-input::placeholder {
    color: var(--text-3);
    letter-spacing: normal;
    font-family: inherit;
  }
  .pairing-input:focus {
    border-color: color-mix(in srgb, var(--accent) 45%, var(--border-strong));
  }
</style>
