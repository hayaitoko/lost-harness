<script lang="ts">
  // Editor — the built-in document editor: Obsidian-style live-markdown (click a
  // line to edit its source), syntax-highlighted Rust with line numbers, and a
  // PDF viewer that stays hard-blocked (health) — viewable locally, never cloud.
  // Ported from the React screen (templates/editor/Editor.dc.html).
  import { onDestroy } from "svelte";
  import { nav } from "$lib/design/nav.svelte";
  import Sidebar from "../components/Sidebar.svelte";
  import AppStatusBar from "../components/AppStatusBar.svelte";
  import IconButton from "../components/IconButton.svelte";

  type Tab = "md" | "rs" | "pdf";

  const INITIAL_LINES = [
    "# Reply to Marcus",
    "",
    "**To:** Marcus Webb · **Re:** Heater repair — Unit 4B",
    "",
    "Hi Marcus,",
    "",
    "Thursday 9–12 works — I will be home. Please confirm with the technician, and thanks for the quick turnaround.",
    "",
    "## Before he arrives",
    "- Clear the closet in front of the unit",
    "- Ask for a copy of the invoice",
    "",
    "> Drafted locally by Qwen3-14B — nothing has been sent.",
  ];

  /* --- markdown inline parsing (ported from the DC's DCLogic) --- */
  type Seg = { kind: "text" | "b" | "code" | "em"; s: string };
  function inline(t: string): Seg[] {
    const out: Seg[] = [];
    let last = 0;
    let m: RegExpExecArray | null;
    const re = /(\*\*[^*]+\*\*|\*[^*]+\*|`[^`]+`)/g;
    while ((m = re.exec(t))) {
      if (m.index > last) out.push({ kind: "text", s: t.slice(last, m.index) });
      const s = m[0];
      if (s.startsWith("**")) out.push({ kind: "b", s: s.slice(2, -2) });
      else if (s.startsWith("`")) out.push({ kind: "code", s: s.slice(1, -1) });
      else out.push({ kind: "em", s: s.slice(1, -1) });
      last = m.index + s.length;
    }
    if (last < t.length) out.push({ kind: "text", s: t.slice(last) });
    return out;
  }

  /* active-line <input> classes — mirror activeStyle() per prefix */
  function activeCls(t: string): string {
    const base =
      "w-full border-0 bg-transparent p-0 text-text outline-none [caret-color:var(--accent)]";
    if (t.startsWith("# ")) return base + " text-[23px] font-bold tracking-[-0.015em]";
    if (t.startsWith("## ")) return base + " text-[17.5px] font-[650]";
    if (t.startsWith("### ")) return base + " text-[15px] font-semibold";
    if (t.startsWith("> ")) return base + " italic text-text-2";
    return base;
  }

  /* --- Rust syntax highlight (literal, document-specific colors) --- */
  type Tok = { s: string; c?: string; i?: boolean };
  const KW = "#82a7e8";
  const FN = "#d8c07a";
  const TY = "#6fc2b5";
  const NUM = "#c9a179";
  const CMT = "#8f8f99";
  const CODE: { n: number; toks: Tok[]; hi?: boolean }[] = [
    { n: 1, toks: [{ s: "async fn", c: KW }, { s: " " }, { s: "with_retry", c: FN }, { s: "<" }, { s: "F", c: TY }, { s: ">(op: " }, { s: "F", c: TY }, { s: ") -> " }, { s: "Result", c: TY }, { s: "<" }, { s: "Receipt", c: TY }, { s: ">" }] },
    { n: 2, toks: [{ s: "where", c: KW }, { s: " " }, { s: "F", c: TY }, { s: ": " }, { s: "Fn", c: TY }, { s: "() -> " }, { s: "Fut", c: TY }, { s: "<" }, { s: "Receipt", c: TY }, { s: "> {" }] },
    { n: 3, toks: [{ s: "    " }, { s: "// Give transient failures a chance to clear.", c: CMT, i: true }] },
    { n: 4, toks: [{ s: "    " }, { s: "for", c: KW }, { s: " attempt " }, { s: "in", c: KW }, { s: " " }, { s: "0", c: NUM }, { s: ".." }, { s: "MAX_RETRIES", c: TY }, { s: " {" }] },
    { n: 5, hi: true, toks: [{ s: "        " }, { s: "match", c: KW }, { s: " op()." }, { s: "await", c: FN }, { s: " {" }] },
    { n: 6, toks: [{ s: "            " }, { s: "Ok", c: TY }, { s: "(r) => " }, { s: "return", c: KW }, { s: " " }, { s: "Ok", c: TY }, { s: "(r)," }] },
    { n: 7, toks: [{ s: "            " }, { s: "Err", c: TY }, { s: "(e) " }, { s: "if", c: KW }, { s: " e." }, { s: "retryable", c: FN }, { s: "() => " }, { s: "backoff", c: FN }, { s: "(attempt)." }, { s: "await", c: FN }, { s: "," }] },
    { n: 8, toks: [{ s: "            " }, { s: "Err", c: TY }, { s: "(e) => " }, { s: "return", c: KW }, { s: " " }, { s: "Err", c: TY }, { s: "(e)," }] },
    { n: 9, toks: [{ s: "        }" }] },
    { n: 10, toks: [{ s: "    }" }] },
    { n: 11, toks: [{ s: "    " }, { s: "Err", c: TY }, { s: "(" }, { s: "Error", c: TY }, { s: "::" }, { s: "Exhausted", c: TY }, { s: ")" }] },
    { n: 12, toks: [{ s: "}" }] },
  ];

  const TABS: { id: Tab; label: string }[] = [
    { id: "md", label: "heater-reply.md" },
    { id: "rs", label: "retry_helper.rs" },
    { id: "pdf", label: "lab-results.pdf" },
  ];

  const HEADINGS: { label: string; fs: number; fw: number }[] = [
    { label: "Paragraph", fs: 13, fw: 400 },
    { label: "Heading 1", fs: 17, fw: 700 },
    { label: "Heading 2", fs: 15, fw: 650 },
    { label: "Heading 3", fs: 13.5, fw: 600 },
  ];

  const LAB: { a: string; r: string; ref: string; bold?: boolean }[] = [
    { a: "Total cholesterol", r: "178 mg/dL", ref: "< 200" },
    { a: "HDL", r: "62 mg/dL", ref: "> 40" },
    { a: "LDL (calc)", r: "98 mg/dL", ref: "< 130" },
    { a: "Triglycerides", r: "91 mg/dL", ref: "< 150" },
    { a: "Vitamin D, 25-OH", r: "24 ng/mL ↓", ref: "30 – 100", bold: true },
  ];

  /* --- local state --- */
  let tab = $state<Tab>("md");
  let lines = $state<string[]>([...INITIAL_LINES]);
  let active = $state<number | null>(null);
  let fileMenuOpen = $state(false);
  let headingMenuOpen = $state(false);
  let headingLabel = $state("Paragraph");
  let toastMsg = $state<string | null>(null);
  let toastTimer: ReturnType<typeof setTimeout> | undefined;

  onDestroy(() => clearTimeout(toastTimer));

  function toast(msg: string) {
    fileMenuOpen = false;
    toastMsg = msg;
    clearTimeout(toastTimer);
    toastTimer = setTimeout(() => (toastMsg = null), 2600);
  }

  let words = $derived(
    lines.join(" ").replace(/[#>*`\-]/g, " ").split(/\s+/).filter(Boolean).length,
  );
  let activeFile = $derived(TABS.find((t) => t.id === tab)!.label);

  function focus(node: HTMLInputElement) {
    node.focus();
  }

  function onLineKey(e: KeyboardEvent, i: number) {
    const t = lines[i];
    if (e.key === "Enter") {
      const next = lines.slice();
      next.splice(i + 1, 0, "");
      lines = next;
      active = i + 1;
    } else if (e.key === "Backspace" && t === "" && lines.length > 1) {
      e.preventDefault();
      const next = lines.slice();
      next.splice(i, 1);
      lines = next;
      active = Math.max(0, i - 1);
    } else if (e.key === "ArrowUp" && i > 0) active = i - 1;
    else if (e.key === "ArrowDown" && i < lines.length - 1) active = i + 1;
    else if (e.key === "Escape") active = null;
  }

  const tbBtn =
    "inline-flex items-center justify-center gap-[5px] rounded-[var(--r)] border-0 bg-transparent text-text-2 cursor-pointer transition-[background-color,color] duration-100 hover:bg-surface-hover hover:text-text";
  const menuBtn =
    "flex w-full items-center justify-between gap-2 rounded-[var(--r)] border-0 bg-transparent px-[9px] py-[7px] text-left text-[12.5px] text-text cursor-pointer hover:bg-surface-hover";
</script>

{#snippet segs(parts: Seg[])}
  {#each parts as p}
    {#if p.kind === "b"}<b>{p.s}</b>
    {:else if p.kind === "code"}<code
        class="rounded-[var(--r-sm)] bg-surface-2 px-[5px] py-px text-[12px] [font-family:ui-monospace,Menlo,monospace]"
        >{p.s}</code
      >
    {:else if p.kind === "em"}<em>{p.s}</em>
    {:else}{p.s}{/if}
  {/each}
{/snippet}

{#snippet sep()}
  <span class="mx-[3px] h-[18px] w-px shrink-0 bg-border"></span>
{/snippet}

{#snippet caret()}
  <svg width="10" height="10" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" class="text-text-3">
    <path d="M6 9l6 6 6-6" />
  </svg>
{/snippet}

<div class="grid h-screen grid-cols-[260px_1fr] bg-bg text-text">
  <Sidebar active="files" />

  <main class="flex min-h-0 min-w-0 flex-col">
    <!-- Top bar -->
    <div class="flex h-12 flex-shrink-0 items-center gap-3 border-b border-border pl-[18px] pr-[14px]">
      <div class="flex min-w-0 items-center gap-[6px] overflow-hidden text-ellipsis whitespace-nowrap text-[13.5px] font-semibold">
        <button
          type="button"
          onclick={() => nav.go("files")}
          class="cursor-pointer border-0 bg-transparent p-0 text-text-2"
        >
          Files
        </button>
        <span class="text-text-3">/</span>
        <span class="text-text-3">workspace</span>
        <span class="text-text-3">/</span>
        {activeFile}
      </div>
      <div class="flex-1"></div>
      <span class="inline-flex items-center gap-[7px] text-[11.5px] text-text-3">
        <span class="h-[7px] w-[7px] rounded-full bg-local"></span>
        Saved · never left this Mac
      </span>
      <div class="flex flex-shrink-0 items-center gap-1">
        <IconButton label="Settings" onclick={() => nav.go("settings")}>
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
            <path d="M4 7h16M4 12h16M4 17h16" />
            <circle cx="9" cy="7" r="2" fill="var(--surface)" />
            <circle cx="15" cy="12" r="2" fill="var(--surface)" />
            <circle cx="8" cy="17" r="2" fill="var(--surface)" />
          </svg>
        </IconButton>
      </div>
    </div>

    <!-- Tab bar -->
    <div class="flex flex-shrink-0 items-center gap-[2px] border-b border-border px-[12px] pt-[6px]">
      {#each TABS as t (t.id)}
        <button
          type="button"
          onclick={() => (tab = t.id)}
          class="cursor-pointer border-0 border-b-2 bg-transparent px-[12px] pb-[8px] pt-[7px] text-[12.5px] {tab ===
          t.id
            ? 'border-accent font-semibold text-text'
            : 'border-transparent font-medium text-text-2'}"
        >
          {t.label}
        </button>
      {/each}
    </div>

    <!-- Toolbar (per tab) -->
    <div class="flex flex-shrink-0 flex-wrap items-center gap-[5px] border-b border-border px-[12px] py-[6px]">
      <div class="relative flex-none">
        <button
          type="button"
          aria-haspopup="true"
          onclick={() => {
            fileMenuOpen = !fileMenuOpen;
            headingMenuOpen = false;
          }}
          class="{tbBtn} h-[28px] px-[10px] text-[12.5px] font-[550] text-text"
        >
          File
          {@render caret()}
        </button>
        {#if fileMenuOpen}
          <div class="absolute left-0 top-[34px] z-20 w-[210px] rounded-[var(--r-lg)] border border-border-strong bg-surface p-[5px] shadow-[var(--shadow-pop)]">
            <button type="button" onclick={() => toast("New file created")} class={menuBtn}
              >New file<span class="text-[11px] text-text-3">⌘N</span></button
            >
            <button
              type="button"
              onclick={() => {
                fileMenuOpen = false;
                nav.go("files");
              }}
              class={menuBtn}>Open…<span class="text-[11px] text-text-3">⌘O</span></button
            >
            <button
              type="button"
              onclick={() => toast("Saved locally — never left this Mac")}
              class={menuBtn}>Save<span class="text-[11px] text-text-3">⌘S</span></button
            >
            <div class="mx-[4px] my-[5px] h-px bg-border"></div>
            <button type="button" onclick={() => toast("Exported to workspace")} class="{menuBtn} justify-start"
              >Export as…</button
            >
            <button type="button" onclick={() => toast("Sent to printer")} class="{menuBtn} justify-start"
              >Print…</button
            >
          </div>
        {/if}
      </div>
      {@render sep()}

      {#if tab === "md"}
        <div class="relative flex-none">
          <button
            type="button"
            onclick={() => {
              headingMenuOpen = !headingMenuOpen;
              fileMenuOpen = false;
            }}
            class="{tbBtn} h-[28px] px-[9px] text-[12.5px]"
          >
            {headingLabel}
            {@render caret()}
          </button>
          {#if headingMenuOpen}
            <div class="absolute left-0 top-[34px] z-20 w-[150px] rounded-[var(--r-lg)] border border-border-strong bg-surface p-[5px] shadow-[var(--shadow-pop)]">
              {#each HEADINGS as h (h.label)}
                <button
                  type="button"
                  onclick={() => {
                    headingLabel = h.label;
                    headingMenuOpen = false;
                  }}
                  class="w-full rounded-[var(--r)] border-0 bg-transparent px-[9px] py-[6px] text-left text-text cursor-pointer hover:bg-surface-hover"
                  style="font-size:{h.fs}px;font-weight:{h.fw}"
                >
                  {h.label}
                </button>
              {/each}
            </div>
          {/if}
        </div>
        {@render sep()}
        <button type="button" title="Bold ⌘B" aria-label="Bold" class="{tbBtn} h-[28px] w-[28px] text-[13px] font-bold">B</button>
        <button type="button" title="Italic ⌘I" aria-label="Italic" class="{tbBtn} h-[28px] w-[28px] text-[13px] italic [font-family:Georgia,serif]">I</button>
        <button type="button" title="Strikethrough" aria-label="Strikethrough" class="{tbBtn} h-[28px] w-[28px] text-[13px] line-through">S</button>
        <button type="button" title="Inline code" aria-label="Inline code" class="{tbBtn} h-[28px] w-[30px] text-[12px] [font-family:ui-monospace,Menlo,monospace]">&lt;/&gt;</button>
        {@render sep()}
        <button type="button" title="Insert link ⌘K" aria-label="Insert link" class="{tbBtn} h-[28px] w-[28px]">
          <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><path d="M9 15l6-6" /><path d="M11 6l1-1a4 4 0 0 1 6 6l-1 1" /><path d="M13 18l-1 1a4 4 0 0 1-6-6l1-1" /></svg>
        </button>
        <button type="button" title="Highlight" aria-label="Highlight" class="{tbBtn} h-[28px] w-[28px]">
          <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><path d="M4 20h16" /><path d="M6 16l8-8 3 3-8 8H6z" /><rect x="9" y="5" width="6" height="4" rx="1" fill="var(--warn)" stroke="none" /></svg>
        </button>
        <button type="button" title="Text color" aria-label="Text color" class="{tbBtn} h-[28px] w-[28px] flex-col gap-px">
          <span class="text-[12px] font-bold leading-none">A</span>
          <span class="h-[3px] w-[14px] rounded-[2px] bg-accent"></span>
        </button>
        {@render sep()}
        <button type="button" title="Bulleted list" aria-label="Bulleted list" class="{tbBtn} h-[28px] w-[28px]">
          <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><circle cx="5" cy="7" r="1.3" fill="currentColor" /><circle cx="5" cy="12" r="1.3" fill="currentColor" /><circle cx="5" cy="17" r="1.3" fill="currentColor" /><path d="M9 7h11M9 12h11M9 17h11" /></svg>
        </button>
        <button type="button" title="Checklist" aria-label="Checklist" class="{tbBtn} h-[28px] w-[28px]">
          <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><rect x="3" y="4" width="7" height="7" rx="1.5" /><path d="M4.5 7.5L6 9l3-3.5" /><rect x="3" y="14" width="7" height="7" rx="1.5" /><path d="M13 7h8M13 17h8" /></svg>
        </button>
        <button type="button" title="Quote" aria-label="Quote" class="{tbBtn} h-[28px] w-[28px]">
          <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><path d="M7 7H4v6h4l-2 4" /><path d="M17 7h-3v6h4l-2 4" /></svg>
        </button>
        <div class="flex-1"></div>
        <span class="flex-none text-[11.5px] text-text-3">{words} words</span>
      {/if}

      {#if tab === "rs"}
        <button type="button" onclick={() => toast("Saved locally — never left this Mac")} class="{tbBtn} h-[28px] px-[10px] text-[12.5px]">
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><path d="M5 3h11l3 3v15H5z" /><path d="M8 3v6h7" /><path d="M8 21v-6h8v6" /></svg>Save
        </button>
        <button type="button" class="{tbBtn} h-[28px] px-[10px] text-[12.5px]">
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><path d="M4 7h6l2 2h8v10H4z" /></svg>Format
        </button>
        <button type="button" class="{tbBtn} h-[28px] px-[10px] text-[12.5px]">
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><path d="M4 7h16M4 12h10l3 3M4 17h6" /></svg>Wrap
        </button>
        {@render sep()}
        <button type="button" class="{tbBtn} h-[28px] px-[10px] text-[12.5px]">
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><circle cx="11" cy="11" r="7" /><path d="M20 20l-4-4" /></svg>Find
        </button>
        <div class="flex-1"></div>
        <span class="flex-none text-[11.5px] text-text-3">Rust · spaces: 4</span>
      {/if}

      {#if tab === "pdf"}
        <button type="button" class="{tbBtn} h-[28px] px-[10px] text-[12.5px]">
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><path d="M4 7h6l2 2h8v10H4z" /></svg>Open
        </button>
        <button type="button" onclick={() => toast("Exported to workspace")} class="{tbBtn} h-[28px] px-[10px] text-[12.5px]">
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><path d="M12 3v12M7 10l5 5 5-5" /><path d="M4 21h16" /></svg>Export
        </button>
        <button type="button" onclick={() => toast("Sent to printer")} class="{tbBtn} h-[28px] px-[10px] text-[12.5px]">
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><path d="M6 9V3h12v6" /><rect x="4" y="9" width="16" height="8" rx="1" /><path d="M7 17h10v4H7z" /></svg>Print
        </button>
        <div class="flex-1"></div>
        <span class="flex-none text-[11.5px] text-text-3">Read-only · hard-blocked</span>
      {/if}
    </div>

    <!-- Content -->
    <div class="min-h-0 flex-1 overflow-y-auto">
      {#if tab === "md"}
        <div class="mx-auto mt-[14px] flex max-w-[720px] items-center gap-[10px] px-[28px] text-[11px] text-text-3">
          <span>Markdown · live preview</span>
          <span>·</span>
          <span>click a line to edit its source, Obsidian-style</span>
        </div>
        <!-- svelte-ignore a11y_no_static_element_interactions -->
        <!-- svelte-ignore a11y_click_events_have_key_events -->
        <div
          class="mx-auto mt-[10px] max-w-[720px] cursor-text px-[28px] pb-[60px] pt-[12px] text-[13.5px] leading-[1.65]"
          onclick={(e) => {
            if (e.target === e.currentTarget) active = lines.length - 1;
          }}
        >
          {#each lines as t, i (i)}
            {#if i === active}
              <input
                use:focus
                value={t}
                oninput={(e) => {
                  const next = lines.slice();
                  next[i] = e.currentTarget.value;
                  lines = next;
                }}
                onblur={() => (active = null)}
                onkeydown={(e) => onLineKey(e, i)}
                class={activeCls(t)}
              />
            {:else}
              <div
                role="button"
                tabindex="0"
                class="min-h-[20px] rounded-[var(--r-sm)]"
                onclick={() => (active = i)}
                onkeydown={(e) => {
                  if (e.key === "Enter" || e.key === " ") {
                    e.preventDefault();
                    active = i;
                  }
                }}
              >
                {#if t.startsWith("# ")}
                  <div class="mb-[8px] mt-[4px] text-[23px] font-bold tracking-[-0.015em]">{@render segs(inline(t.slice(2)))}</div>
                {:else if t.startsWith("## ")}
                  <div class="mb-[4px] mt-[10px] text-[17.5px] font-[650]">{@render segs(inline(t.slice(3)))}</div>
                {:else if t.startsWith("### ")}
                  <div class="mb-[2px] mt-[8px] text-[15px] font-semibold">{@render segs(inline(t.slice(4)))}</div>
                {:else if t.startsWith("- ")}
                  <div class="flex gap-[9px] pl-[6px]">
                    <span class="text-text-3">•</span>
                    <span>{@render segs(inline(t.slice(2)))}</span>
                  </div>
                {:else if t.startsWith("> ")}
                  <div class="my-[4px] border-l-[3px] border-border-strong pl-[11px] italic text-text-2">{@render segs(inline(t.slice(2)))}</div>
                {:else if t === ""}
                  <div class="h-[12px]"></div>
                {:else}
                  <div>{@render segs(inline(t))}</div>
                {/if}
              </div>
            {/if}
          {/each}
        </div>
      {/if}

      {#if tab === "rs"}
        <div class="mx-auto mb-[60px] mt-[18px] max-w-[860px] px-[24px]">
          <div class="overflow-hidden rounded-[var(--r-lg)] border border-border bg-surface">
            <div class="flex items-center gap-[10px] border-b border-border px-[14px] py-[8px] text-[11px] text-text-3">
              <span>Rust</span><span>·</span><span>14 lines</span>
              <div class="flex-1"></div>
              <span>Drafted with Cloud · Opus 4.8 — code left this Mac on Jul 11</span>
            </div>
            <div class="py-[12px] text-[12.5px] leading-[1.75] [font-family:ui-monospace,SFMono-Regular,Menlo,monospace]">
              {#each CODE as row (row.n)}
                <div class="flex {row.hi ? 'bg-surface-2' : ''}">
                  <span class="w-[44px] flex-none select-none pr-[16px] text-right {row.hi ? 'text-text-2' : 'text-text-3'}">{row.n}</span>
                  <span
                    >{#each row.toks as tk}{#if tk.c}<span style="color:{tk.c}{tk.i ? ';font-style:italic' : ''}">{tk.s}</span>{:else}{tk.s}{/if}{/each}</span
                  >
                </div>
              {/each}
            </div>
          </div>
        </div>
      {/if}

      {#if tab === "pdf"}
        <div class="mx-auto mb-[60px] mt-[14px] max-w-[860px] px-[24px]">
          <div class="flex items-center gap-[10px] rounded-[var(--r-lg)] border border-border bg-surface px-[12px] py-[8px] text-[11.5px] text-text-2">
            <span class="inline-flex items-center gap-[7px]">
              <span class="h-[7px] w-[7px] rounded-full bg-blocked"></span>Hard-blocked · health
            </span>
            <span class="text-text-3">— viewable and searchable locally; can never be sent to a cloud model</span>
            <div class="flex-1"></div>
            <span>Page 1 of 2</span>
            <span class="text-text-3">·</span>
            <button type="button" aria-label="Zoom out" class="{tbBtn} h-[22px] w-[22px]">
              <svg width="11" height="11" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M5 12h14" /></svg>
            </button>
            <span>100%</span>
            <button type="button" aria-label="Zoom in" class="{tbBtn} h-[22px] w-[22px]">
              <svg width="11" height="11" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M12 5v14M5 12h14" /></svg>
            </button>
          </div>
          <div class="grid place-items-center py-[22px]">
            <div
              class="w-[600px] rounded-[2px] px-[56px] py-[52px] text-[12.5px] leading-[1.6]"
              style="aspect-ratio:8.5/11;background:#fff;color:#26251f;box-shadow:0 8px 30px rgba(0,0,0,.45)"
            >
              <div class="flex items-baseline justify-between pb-[10px]" style="border-bottom:2px solid #26251f">
                <span class="text-[16px] font-bold tracking-[-0.01em]">Riverside Medical Group</span>
                <span class="text-[11px]" style="color:#6d6b60">Laboratory Report</span>
              </div>
              <div class="mt-[14px] flex gap-[26px] text-[11px]" style="color:#54524a">
                <span>Patient: A. Tanaka</span><span>Collected: Jul 6, 2026</span><span>Ordering: Dr. E. Chen</span>
              </div>
              <div class="mt-[24px] text-[11px] font-bold uppercase tracking-[0.06em]" style="color:#54524a">Lipid panel</div>
              <div class="mt-[8px] grid gap-x-[14px] gap-y-[4px] text-[12px]" style="grid-template-columns:1fr 90px 130px">
                <span class="text-[10.5px] font-semibold uppercase tracking-[0.05em]" style="color:#54524a">Analyte</span>
                <span class="text-[10.5px] font-semibold uppercase tracking-[0.05em]" style="color:#54524a">Result</span>
                <span class="text-[10.5px] font-semibold uppercase tracking-[0.05em]" style="color:#54524a">Reference</span>
                {#each LAB as row (row.a)}
                  <span class="contents">
                    <span>{row.a}</span>
                    <span>{#if row.bold}<b>{row.r}</b>{:else}{row.r}{/if}</span>
                    <span style="color:#54524a">{row.ref}</span>
                  </span>
                {/each}
              </div>
              <div class="mt-[26px] rounded-[4px] px-[12px] py-[10px] text-[11px]" style="background:#f4f3ee;color:#54524a">
                Vitamin D slightly below reference range. All other values unremarkable. Discuss supplementation at next visit.
              </div>
            </div>
          </div>
        </div>
      {/if}
    </div>

    <AppStatusBar session="0:12" />
  </main>
</div>

{#if toastMsg}
  <div class="fixed bottom-[44px] left-1/2 z-[90] -translate-x-1/2">
    <div class="flex max-w-[340px] items-center gap-2 rounded-[var(--r)] border border-border-strong bg-surface px-[14px] py-[9px] text-[12.5px] text-text shadow-[var(--shadow-pop)]">
      <span class="h-[6px] w-[6px] shrink-0 rounded-full bg-accent"></span>
      {toastMsg}
    </div>
  </div>
{/if}
