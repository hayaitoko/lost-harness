# Ultracode goal-anchored resume prompt (2026-07-17)

Supersedes the plain trigger in `2026-07-17-ultracode-full-build.md` for **resuming** the
build. Uses `/goal` to make the true stop condition the *entire manifest drained* — so the
session keeps driving toward completion (and auto-resumes across context boundaries) instead
of stopping at the first clean boundary. Paste both blocks, in order, into a fresh session at
the repo root (`/Users/hayai/Desktop/lost-harness-product`).

The prior run completed Wave 1 + part of Wave 2 (permission modes, session_search,
system_status) and stopped cleanly — no blocker. The lowest undrained item is **Wave 2.1's
remaining tools** (ask-human, headless browser, delegate, cron). This prompt resumes there.

---

### 1. Set the goal (the true stop condition)

```
/goal Drive docs/BUILD-MANIFEST.md to FULL completion — every item in Waves 1 through 7 implemented, tested, adversarially reviewed, and committed to main, with each item's gates green (cargo test --lib passing; cargo build --lib --no-default-features clean; cargo clippy --lib at 0 errors; npm run build + npm run check clean for any frontend change) and docs/ROADMAP.md + HANDOFF.md updated to match. This goal is NOT met while any manifest item remains unbuilt. Running low on context or tokens is NOT completion — commit cleanly, record exact progress in ROADMAP + HANDOFF, and resume toward this goal. Stop only when the manifest is fully drained, or to surface a genuine blocker that needs Lukas (a decision not in the specs, a missing credential/endpoint, a spec contradiction).
```

### 2. Start the build

```
ultracode — Resume and complete the Lost Harness build, working to the /goal set above.

A prior run finished Wave 1 and part of Wave 2 of docs/BUILD-MANIFEST.md, then stopped
cleanly at a context boundary — NOT a blocker. Drive the rest of the backlog to the goal:
every remaining item, all the way through Wave 7. Do not declare completion until the
manifest is fully drained; "ran low on context" is not done — resume and keep going.

BEFORE YOU START, read in this order: HANDOFF.md (current state, gotchas, the last session
logs), docs/ROADMAP.md (the "Stage" block names the lowest undrained item — right now Wave
2.1's remaining tools: ask-human, headless browser, delegate, cron management), then
docs/BUILD-MANIFEST.md (your work queue) and docs/PLAN.md (source of truth). Read each
item's spec pointer before building it; never re-derive a design that already exists.

EXECUTION MODEL
- Resume at the lowest undrained item and go wave by wave, 2 → 7. A wave assumes every
  earlier wave is merged. Don't skip ahead.
- Within a wave, fan out the items marked "∥ parallel" concurrently (orchestrate with
  workflows / parallel subagents); respect the "⇢ after N" dependencies.
- Tier A items: implement → adversarial multi-lens review (fresh context) → verify →
  commit, in small coherent commits.
- Tier B items (the Wave 5 flagships + 7.3 signing): the FIRST deliverable is a design doc
  in docs/plans/ in the house format (see the existing plans there), reviewed and landed,
  THEN build against it. Never fan a swarm straight at a Tier B item.
- Keep going across items and waves without stopping to check in. Stop ONLY on a genuine
  blocker that needs Lukas: surface it clearly, then continue with everything not blocked
  by it. Honor any deferral already recorded in ROADMAP (e.g. the UserPromptSubmit hook,
  the 1.6 rename) unless building it is trivial and unblocks nothing else.

NON-NEGOTIABLE INVARIANTS (every item, every wave — these outrank velocity)
- The privacy filter is load-bearing; the hardline danger-floor is non-overridable; ALL
  untrusted content (tool output, web pages, recalled memory, screen/clipboard) is
  guard-wrapped before it can reach the model as anything but data.
- Local-first: the app never *requires* a model download or a server. After every item,
  `cargo build --lib --no-default-features` must stay green. A cloud call flagged
  must-stay-local fails loud, never silently degrades.
- Per-profile isolation and the memory sensitivity wall hold; no private-local data
  crosses a profile or endpoint boundary. Permission modes stay matrix-bounded — a mode
  can never widen External/Dangerous.

VERIFICATION (hard gates — no commit without them)
- Per item, before commit: cargo test --lib green, cargo build --lib --no-default-features
  clean, cargo clippy --lib 0 errors; for frontend changes also npm run build + npm run
  check clean. Run cargo from src-tauri/. Add tests for every new behavior; never let the
  suite regress (current baseline: 399 passing).
- Per wave, once it lands: update docs/ROADMAP.md (stage line, milestone board, the wave's
  checklist) and add a HANDOFF.md session-log entry (what shipped, commits, test count,
  anything deferred + why). Then start the next wave.
- Commit to main in small coherent units; conventional-commit messages; end each body with
  the Co-Authored-By: Claude trailer.

CONTEXT DISCIPLINE (so the goal actually gets reached)
- As a context boundary approaches: finish the item in flight, commit it, make ROADMAP +
  HANDOFF reflect the exact resume point, then continue / resume — the /goal is unmet until
  the manifest is drained, so treat a boundary as a checkpoint, not an ending.

OUT OF SCOPE: the "prove it works" end-to-end dogfooding/QA campaign — that's the NEXT
directive after this backlog is fully drained. Keep per-item verification tight, but don't
divert into a live-usage campaign.

WHEN THE MANIFEST IS FULLY DRAINED (goal met), write a final report: what shipped per wave,
final test count, anything deferred or blocked and why, and a punch-list for the "prove it
works" phase to target.
```

---

## Notes for Lukas (not part of the prompt)

- **What `/goal` buys you here:** it pins the stop condition to *"manifest fully drained,"*
  not *"agent decided it reached a good place."* The last run stopped cleanly at a context
  boundary because nothing told it the boundary wasn't the end. This does — it treats a
  boundary as a checkpoint and keeps driving.
- **It still can't defy context limits in a single stretch** — it will checkpoint and resume
  rather than finish 40+ items in one unbroken run. But the goal + the "resume at the lowest
  undrained item" discipline means each re-engagement continues toward completion instead of
  re-deciding scope.
- **This is the whole product (M4→M10 + server twin).** If you'd rather gate at a review
  point, change the `/goal` to "…Waves 2 through 4 (finish M4)…" and it'll stop cleanly after
  skills & agents, before the flagships/server where a wrong call is expensive.
