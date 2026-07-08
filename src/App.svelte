<script lang="ts">
  // M0 bootstrap: minimal Svelte 5 runes-mode page.
  // Proves the webview renders end-to-end.
  let title = $state("Lost Harness");
  let subtitle = $state("Rust + Tauri + Svelte 5 — M0 bootstrap");
</script>

<main class="flex h-full flex-col items-center justify-center text-center">
  <div class="space-y-4 px-6">
    <h1 class="text-5xl font-semibold tracking-tight" style="color: var(--accent)">
      {title}
    </h1>
    <p class="text-lg text-neutral-300">{subtitle}</p>
    <p class="text-sm text-neutral-500">
      v0.1.0 · M0 — empty shell, no business logic
    </p>
  </div>
</main>
