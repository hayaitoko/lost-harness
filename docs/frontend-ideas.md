# Lost Harness — Frontend / UI idea dump (raw notes)

Raw capture of every UI idea that's come up, to feed the frontend mockup/spec.
**Not a spec** — Lukas owns that. The top sections are ideas raised directly in
conversation; the "From the broader design" section at the end is UI-relevant
stuff already decided in PLAN.md, included so the spec is complete.

Last updated: 2026-07-10.

---

## 1. The privacy filter / classifier (the biggest chunk we talked about)

- **Dedicated classifier settings page.** Let the user fine-tune the filter's
  behavior: how paranoid it is (strictness / threshold), the "uncertainty band,"
  redaction on/off, and the hard-block behavior for proprietary/health content.
  Ships with sensible defaults — the page is for people who want to adjust.
  Likely per-profile.
- **Censorship is surfaced, never silent.** When the filter censors, redacts, or
  blocks a message, the user gets a **non-blocking alert** — not a silent swap.
  They always know it happened.
- **"What tripped it" review in the right sidebar.** The alert carries a button
  that opens the **right sidebar** showing your *original* message annotated with
  exactly what set the filter off — which spans, which category (SSN / health /
  proprietary / API key / …), and which layer fired (deterministic rules vs. the
  model). The data for this already exists (the classifier returns exact span
  offsets + categories), so the view is "free" to build.
- **Redaction / partial delegation.** For a private message, black out just the
  sensitive spans, send the safe remainder to the cloud, and stitch the answer
  back together. UI implication: show what got redacted vs. what actually left
  the machine (probably inline in that annotated view).

## 2. The tool-approval flow (built a working v1 this session)

- **Approval dialog** pops when the agent wants to run a tool that needs the OK.
  It shows the tool name **and the exact command + arguments** — so you can vet
  what it's about to do, not just "a tool wants to run."
- Buttons: **Deny · Allow once · Allow for this session.** "Allow once" is the
  primary/highlighted button (the *narrowest* grant — a hurried click gives away
  the least). **Esc = deny.**
- Shows a **"N more waiting"** counter when approvals queue up.
- **Risk badges.** Tools carry a risk class (safe / write / dangerous). Surface
  it as a badge in the dialog (and the tool list) so a risky action *reads* as
  risky at a glance.
- Later: an **"Always allow"** option (persists across restarts — currently held
  back until the settings store lands).

## 3. The chat view (the main surface — needs the most love)

- Today it's: left sidebar (conversations) + chat panel + a top bar with a
  settings gear. Functional, not designed — this is where "the interface needs
  quite a bit of work" bites hardest.
- **Privacy indicator** — a chip/banner on each message showing how it was routed
  (went to the cloud vs. kept local). This is load-bearing, not decoration —
  routing *is* the product's soul — so it should be legible and trustworthy at a
  glance, maybe the most prominent single signal in the chat.

## 4. Sidebars (left vs. right)

- **Left sidebar** = conversations / session list (exists).
- **Right sidebar** = the "why did this happen" detail panel. Introduced for the
  annotated-censorship view; natural home for routing details, redactions, and
  tool activity too.

## 5. Profiles

- A profile switcher (personal / work / school / developer) — floated as a
  "cycle chip."
- Each profile has its own settings surfaces (below), including its own
  memory-privacy and skills-autonomy toggles.

## 6. Settings (the pages we've named)

- **Memory privacy toggle** (per profile) — shared vs. walled ("keep this
  profile's memory private" → its own separate store).
- **Classifier settings page** (see §1).
- **Provider / model settings** (exists) + future **model "seat" assignment**
  (bind a model to Writer / Reviewer / Coding roles).
- **Skills autonomy toggle** (per profile) — approve-first vs. autonomous.

---

## From the broader design (UI-relevant, already decided in PLAN.md)

Not necessarily raised out loud this session, but these all need a UI eventually
— listed so the mockup can leave room for them:

- **Command palette** (M2) — replaces slash-commands as the main "do a thing"
  entry point.
- **"What I taught myself" feed** — a reviewable list of skills the agent created,
  with edit/delete; plus the approve-first skill review (draft shown to
  approve / edit / reject before it's trusted).
- **Memory you can actually read/edit** — the curated summary + daily notes kept
  as plain markdown files, openable in-app (and Obsidian-compatible). Implies an
  in-app memory view and, later, a file-explorer panel.
- **Cost / usage** — visible per-profile spend; a "flying blind" flag when cost is
  unknown; a budget cap surface for unattended server work.
- **Notifications / away-summary** — OS notifications, and a rollup ("…and 42 more
  while you were away") instead of a flood after time away.
- **Baton / multi-device** — a read-only banner: "your desktop is working on this
  — open read-only, or ask it to hand over."
- **Server pairing flow** — enter a code / scan a QR once to pair a server
  (no passwords).
- **Onboarding / first-run** (M8) — hardware detection, a curated model catalog to
  download from, and seat assignment as part of setup.
- **Voice** (M6) — a settings toggle; barge-in (interrupt mid-response); a voice
  mode surface.
- **OS-citizen polish** (M9) — tray icon, menu bar, global hotkeys, notification
  center integration.
