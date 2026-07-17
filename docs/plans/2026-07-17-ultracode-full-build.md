# Ultracode full-build trigger prompt (2026-07-17)

Paste the block below into a fresh Claude Code session at the repo root
(`/Users/hayai/Desktop/lost-harness-product`) to fire the entire build backlog.
Reusable — if a run stops partway, re-paste; it re-reads the manifest + ROADMAP and
resumes at the lowest undrained wave.

---

```
ultracode — Build the Lost Harness product to completion.

Execute the entire build backlog in docs/BUILD-MANIFEST.md, start to finish,
autonomously, wave by wave, until every item is built, verified, and committed.
This is the standing "build everything spec'd, then prove it" directive — you are
doing the BUILD phase; the prove-it-works phase is explicitly out of scope here.

BEFORE YOU START, read in this order: HANDOFF.md (current state + toolchain
gotchas), docs/BUILD-MANIFEST.md (your work queue), docs/PLAN.md (source of truth).
The manifest indexes into PLAN.md and the tool-system docs for each item's real
spec — read the pointer before building anything; never re-derive a design that
already exists.

EXECUTION MODEL
- Go wave by wave, 1 → 7. A wave assumes every earlier wave is merged. Don't skip ahead.
- Within a wave, fan out the items marked "∥ parallel" concurrently (orchestrate with
  workflows / parallel subagents); respect the "⇢ after N" dependencies.
- Tier A items: implement → adversarial multi-lens review (fresh context) → verify →
  commit. Small, coherent commits.
- Tier B items (the Wave 5 flagships + 7.3 signing): the FIRST deliverable is a design
  doc in docs/plans/ in the house format (see the existing plan there), reviewed and
  landed, THEN build against it. Never fan a swarm straight at a Tier B item.
- Keep going across waves without stopping to check in. Stop ONLY on a genuine blocker
  that needs Lukas (a decision not in the specs, a missing credential/endpoint, a spec
  contradiction): surface it clearly, then continue with everything not blocked by it.

NON-NEGOTIABLE INVARIANTS (every item, every wave — these outrank velocity)
- The privacy filter is load-bearing; the hardline danger-floor is non-overridable; ALL
  untrusted content (tool output, web pages, recalled memory, screen/clipboard) is
  guard-wrapped before it can reach the model as anything but data.
- Local-first: the app never *requires* a model download or a server. After every item,
  `cargo build --lib --no-default-features` must stay green (rules-only / embedder-absent
  fallback). A cloud call flagged must-stay-local fails loud, never silently degrades.
- Per-profile isolation and the memory sensitivity wall hold; no private-local data
  crosses a profile or endpoint boundary.

VERIFICATION (hard gates — no commit without them)
- Per item, before commit: `cargo test --lib` green, `cargo build --lib
  --no-default-features` clean, `cargo clippy --lib` 0 errors; for frontend changes also
  `npm run build` + `npm run check` clean. Run cargo from src-tauri/. Add tests for every
  new behavior; keep the existing suite green (baseline: 385 passing).
- Per wave, once it lands: update docs/ROADMAP.md (stage line, milestone board, the wave's
  checklist) and add a HANDOFF.md session-log entry (what shipped, commits, test count,
  anything deferred). Then start the next wave.
- Work on main (or a feature branch you keep merged forward). Conventional-commit messages;
  end each commit body with the Co-Authored-By: Claude trailer.

WHEN THE MANIFEST IS FULLY DRAINED, write a final report: what shipped per wave, final
test count, anything deferred or blocked and why, and a punch-list for the "prove it works"
phase (the next directive) to target.
```

---

## Notes for Lukas (not part of the prompt)

- **The "in one shot" honesty:** this backlog is the entire remaining product (M4→M10 +
  the server twin). Even at full ultracode fan-out it's a very large run; expect it to work
  through waves over a long session and to *pause on genuine blockers* rather than finish
  everything untouched-by-human. That's by design — the prompt tells it to surface blockers
  and keep going on everything else.
- **Throttled alternative:** to scope the first fire to a clean M4 finish, change the first
  paragraph's "Execute the entire build backlog" to "Execute Waves 1–4 of
  docs/BUILD-MANIFEST.md" and it'll stop cleanly after skills & agents, leaving the
  flagships + server for a later fire.
