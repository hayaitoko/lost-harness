/// <reference types="vitest" />

/**
 * Provenance wire-contract test (Workstream D — provenance label truth-up).
 *
 * The backend's `Provenance` enum (src-tauri/src/models/hf_search.rs) derives
 * `#[serde(rename_all = "snake_case")]`, so its wire values are the string
 * literals "curated" and "community" — pinned on the Rust side by
 * `manifest_state_wire_shape_is_stable_for_the_ui`
 * (src-tauri/src/models/hf_search.rs, in the `models::hf_search::tests`
 * module). This file is the frontend half of that same contract: it pins the
 * identical literals in the `Provenance` type (`src/lib/api/tauri.ts`) AND
 * mounts the REAL `Settings.svelte` search-results badge to prove the
 * "curated" branch renders the muted trust badge and the "community" branch
 * renders the orange community-warning badge — not the other way around, and
 * not neither.
 *
 * Before this fix the frontend still compared against the dead `"trusted"`
 * value (an artifact of the manifest work renaming `Trusted` -> `Curated`
 * before shipping), so every row silently fell into the `community` branch.
 * A future rename on either side must fail HERE, not by an inverted trust
 * badge in production — see review-fixes/progress/P09.md follow-up #1 and
 * lost-harness-fixes/PROGRESS-MAP.md open follow-up #8.
 */

import { describe, it, expect, vi, afterEach } from "vitest";
import { render, fireEvent, cleanup } from "@testing-library/svelte";
import { within } from "@testing-library/dom";
import Settings from "$lib/design/screens/Settings.svelte";
import type { HfModelSummary, Provenance } from "$lib/api/tauri";

// ── Wire-literal pin ─────────────────────────────────────────────────────────
//
// These are written as bare string literals, NOT re-exported from tauri.ts,
// so that if a future rename changes the `Provenance` union (either side) but
// forgets to update this file, `npm run check` fails here instead of the
// mismatch shipping silently. Cross-reference: hf_search.rs's
// `manifest_state_wire_shape_is_stable_for_the_ui` pins the same two literals
// via `serde_json::to_value(Provenance::Curated/Community)`.

const CURATED: Provenance = "curated";
const COMMUNITY: Provenance = "community";

describe("Provenance — wire literal contract", () => {
  it("pins the exact wire strings the backend's snake_case serde emits", () => {
    // Deliberately redundant with the type check above: also assert at
    // runtime, so a `.test.ts` change alone (without touching tauri.ts)
    // still shows the intended values in the failure diff.
    expect(CURATED).toBe("curated");
    expect(COMMUNITY).toBe("community");
    expect(CURATED).not.toBe("trusted");
  });
});

// ── Mock the search IPC so the component renders fixture rows ──────────────

const FIXTURE_RESULTS: HfModelSummary[] = [
  {
    id: "acme/curated-model",
    publisher: "acme",
    downloads: 100,
    likes: 5,
    tags: [],
    provenance: CURATED,
  },
  {
    id: "acme/community-model",
    publisher: "acme",
    downloads: 50,
    likes: 2,
    tags: [],
    provenance: COMMUNITY,
  },
];

vi.mock("$lib/api/tauri", async (importOriginal) => {
  const actual = await importOriginal<typeof import("$lib/api/tauri")>();
  return {
    ...actual,
    searchModels: vi.fn(async () => FIXTURE_RESULTS),
  };
});

afterEach(() => {
  cleanup();
});

describe("Settings — model search provenance badge", () => {
  it("renders 'curated' (not 'community') for a curated result, and vice versa", async () => {
    // `vi.mock` above is hoisted above this file's imports (vitest/esbuild
    // hoist all `vi.mock` calls, same as Jest), so Settings.svelte's static
    // `import { searchModels } from "$lib/api/tauri"` already resolves to
    // the mock by the time it's rendered here.
    const { getByRole, getByText, findByText } = render(Settings);

    // Models is a section in Settings' own submenu nav (not the header label,
    // which is a <span>) — scope to the button role to disambiguate.
    await fireEvent.click(getByRole("button", { name: "Models" }));
    await fireEvent.click(getByRole("button", { name: "Search" }));

    // The mocked searchModels resolves asynchronously; wait for a result row.
    await findByText("acme/curated-model");
    await findByText("acme/community-model");

    const curatedRow = getByText("acme/curated-model").closest("button");
    const communityRow = getByText("acme/community-model").closest("button");
    expect(curatedRow).toBeTruthy();
    expect(communityRow).toBeTruthy();

    // The curated row shows the muted "curated" badge and NEVER the orange
    // community-warning badge.
    const curatedBadge = within(curatedRow!).getByText("curated");
    expect(curatedBadge.className).toContain("bg-local-soft");
    expect(curatedBadge.className).toContain("text-local");
    expect(within(curatedRow!).queryByText("community")).not.toBeInTheDocument();
    expect(within(curatedRow!).queryByText("trusted")).not.toBeInTheDocument();

    // The community row shows the orange community-warning badge and never
    // the curated one.
    const communityBadge = within(communityRow!).getByText("community");
    expect(communityBadge.className).toContain("bg-warn-soft");
    expect(communityBadge.className).toContain("text-warn");
    expect(within(communityRow!).queryByText("curated")).not.toBeInTheDocument();
  });
});
