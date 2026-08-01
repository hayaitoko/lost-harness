/// <reference types="vitest" />

/**
 * The community-model provenance consent gate — driven at the real component,
 * not asserted against a hand-rolled object.
 *
 * `downloads.test.ts` only ever calls `downloadModel` directly and checks the
 * browser-fallback throw; nothing exercised the actual UI gate that decides
 * whether `acknowledge_community` is allowed to be `true`. That gate lives in
 * `Settings.svelte`'s `startModelDownload`: a community-provenance model's
 * first "Download" press only ARMS a per-(model,quant) confirmation (and
 * never calls the download IPC); a second, distinct press within the window
 * is what actually calls `downloadModel(..., true)`. A regression that
 * hardcodes the acknowledgement — either by dropping the arm/confirm gate or
 * by passing `true` unconditionally — would let a community model download
 * on the very first click, with both `downloads.test.ts` and the rest of the
 * suite staying green, because nothing else calls this code path.
 *
 * These tests mount the real `Settings.svelte` Models pane and drive the
 * actual button presses; only the network-facing trio (`searchModels`,
 * `getModelDetail`, `downloadModel`) are stubbed, so the confirm-gate logic
 * under test is 100% production code.
 */

import { describe, it, expect, vi, afterEach } from "vitest";
import { render, fireEvent, cleanup, waitFor } from "@testing-library/svelte";
import Settings from "$lib/design/screens/Settings.svelte";
import type { HfModelSummary, ModelDetailResponse } from "$lib/api/tauri";

// ── Fixtures ─────────────────────────────────────────────────────────────

const SPEC = {
  architecture: "llama",
  total_params_b: 7,
  active_params_b: 7,
  n_layers: 32,
  n_kv_heads: 8,
  head_dim: 128,
  native_context_len: 8192,
  kv_exact: true,
};

function quant(filename: string, quantName: string): ModelDetailResponse["quants"][number] {
  return {
    quant: quantName,
    total_size_bytes: 4_000_000_000,
    files: [
      {
        quant: quantName,
        filename,
        url: `https://huggingface.co/example/${filename}`,
        sha256: "0".repeat(64),
        size_bytes: 4_000_000_000,
        part: null,
      },
    ],
    complete: true,
  };
}

const COMMUNITY_A: ModelDetailResponse = {
  id: "acme/llama-community",
  publisher: "acme",
  provenance: "community",
  spec: SPEC,
  spec_notes: [],
  quants: [quant("llama-community.Q4_K_M.gguf", "Q4_K_M")],
};

const COMMUNITY_B: ModelDetailResponse = {
  id: "acme/mistral-community",
  publisher: "acme",
  provenance: "community",
  spec: SPEC,
  spec_notes: [],
  quants: [quant("mistral-community.Q4_K_M.gguf", "Q4_K_M")],
};

const CURATED: ModelDetailResponse = {
  id: "acme/verified-curated",
  publisher: "acme",
  provenance: "curated",
  spec: SPEC,
  spec_notes: [],
  quants: [quant("verified-curated.Q4_K_M.gguf", "Q4_K_M")],
};

const RESULTS: HfModelSummary[] = [COMMUNITY_A, COMMUNITY_B, CURATED].map((d) => ({
  id: d.id,
  publisher: d.publisher,
  downloads: 10,
  likes: 1,
  tags: [],
  provenance: d.provenance,
}));

const DETAIL_BY_ID: Record<string, ModelDetailResponse> = {
  [COMMUNITY_A.id]: COMMUNITY_A,
  [COMMUNITY_B.id]: COMMUNITY_B,
  [CURATED.id]: CURATED,
};

const downloadModelSpy = vi.fn(
  async (id: string, filename: string, _acknowledgeCommunity?: boolean) => ({
    id,
    name: filename,
    path: `/models/${filename}`,
  }),
);

vi.mock("$lib/api/tauri", async (importOriginal) => {
  const actual = await importOriginal<typeof import("$lib/api/tauri")>();
  return {
    ...actual,
    searchModels: vi.fn(async () => RESULTS),
    getModelDetail: vi.fn(async (id: string) => DETAIL_BY_ID[id] ?? null),
    downloadModel: (...args: [string, string, boolean?]) => downloadModelSpy(...args),
  };
});

afterEach(() => {
  cleanup();
  downloadModelSpy.mockClear();
});

/** Open the Models pane, search, and expand one result row. Returns the
 *  quant row's Download/Confirm button. */
async function openModelRow(modelId: string) {
  const screen = render(Settings);
  await fireEvent.click(screen.getByRole("button", { name: "Models" }));
  await fireEvent.click(screen.getByRole("button", { name: "Search" }));
  await screen.findByText(modelId);
  await fireEvent.click(screen.getByText(modelId));
  const button = await screen.findByRole("button", { name: /^(Download|Confirm publisher)$/ });
  return { screen, button };
}

describe("community-model download consent gate — driven at the real component", () => {
  it("a community model's first press only arms confirmation — the download IPC is never called", async () => {
    const { button } = await openModelRow(COMMUNITY_A.id);
    expect(button.textContent?.trim()).toBe("Download");

    await fireEvent.click(button);

    // The load-bearing assertion: the arm click must not have reached the
    // download IPC under ANY acknowledgement value.
    expect(downloadModelSpy).not.toHaveBeenCalled();
    await waitFor(() => expect(button.textContent?.trim()).toBe("Confirm publisher"));
  });

  it("a community model reaches the download IPC only on the explicit second press, with acknowledge_community true", async () => {
    const { button } = await openModelRow(COMMUNITY_A.id);

    await fireEvent.click(button); // arm
    expect(downloadModelSpy).not.toHaveBeenCalled();
    await waitFor(() => expect(button.textContent?.trim()).toBe("Confirm publisher"));

    await fireEvent.click(button); // explicit confirm
    await waitFor(() => expect(downloadModelSpy).toHaveBeenCalledTimes(1));
    expect(downloadModelSpy).toHaveBeenCalledWith(
      COMMUNITY_A.id,
      "llama-community.Q4_K_M.gguf",
      true,
    );
  });

  it("a curated model needs no acknowledgement and downloads on the first press with acknowledge_community false", async () => {
    const { button } = await openModelRow(CURATED.id);
    expect(button.textContent?.trim()).toBe("Download");

    await fireEvent.click(button);

    await waitFor(() => expect(downloadModelSpy).toHaveBeenCalledTimes(1));
    expect(downloadModelSpy).toHaveBeenCalledWith(
      CURATED.id,
      "verified-curated.Q4_K_M.gguf",
      false,
    );
  });

  it("the acknowledgement is per-action, not sticky: confirming one community model does not pre-arm the next", async () => {
    const screen = render(Settings);
    await fireEvent.click(screen.getByRole("button", { name: "Models" }));
    await fireEvent.click(screen.getByRole("button", { name: "Search" }));
    await screen.findByText(COMMUNITY_A.id);

    // Confirm model A fully (arm + confirm).
    await fireEvent.click(screen.getByText(COMMUNITY_A.id));
    const buttonA = await screen.findByRole("button", { name: /^(Download|Confirm publisher)$/ });
    await fireEvent.click(buttonA);
    await waitFor(() => expect(buttonA.textContent?.trim()).toBe("Confirm publisher"));
    await fireEvent.click(buttonA);
    await waitFor(() => expect(downloadModelSpy).toHaveBeenCalledTimes(1));
    expect(downloadModelSpy).toHaveBeenLastCalledWith(
      COMMUNITY_A.id,
      "llama-community.Q4_K_M.gguf",
      true,
    );

    // Switch to a SECOND, unrelated community model. If confirmation were
    // sticky (e.g. keyed globally instead of per model+quant, or the arm
    // state simply never cleared), this press would download immediately.
    await fireEvent.click(screen.getByText(COMMUNITY_B.id));
    const buttonB = await screen.findByRole("button", { name: /^(Download|Confirm publisher)$/ });
    expect(buttonB.textContent?.trim()).toBe("Download");

    await fireEvent.click(buttonB); // first press on B — must only arm, not download
    expect(downloadModelSpy).toHaveBeenCalledTimes(1); // still just A's call
    await waitFor(() => expect(buttonB.textContent?.trim()).toBe("Confirm publisher"));

    await fireEvent.click(buttonB); // explicit second press on B
    await waitFor(() => expect(downloadModelSpy).toHaveBeenCalledTimes(2));
    expect(downloadModelSpy).toHaveBeenLastCalledWith(
      COMMUNITY_B.id,
      "mistral-community.Q4_K_M.gguf",
      true,
    );
  });
});
