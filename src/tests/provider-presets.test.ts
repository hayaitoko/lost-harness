/// <reference types="vitest" />

/**
 * The quick-add presets may only offer endpoints this app can actually use.
 *
 * The Anthropic preset was removed on this branch because it was a trap: the
 * model client speaks only the OpenAI-compatible surface (`GET /models`,
 * `POST /chat/completions`, `Authorization: Bearer` —
 * src-tauri/src/models/client.rs), Anthropic's native API rejects a Bearer key
 * and needs `x-api-key` + `anthropic-version`, and with no free-text model
 * entry in the UI a provider that lists no models can never be selected. It
 * also sorted first alphabetically, which is how it became the endpoint that
 * every "just use the first configured provider" path silently served.
 *
 * Nothing pinned that removal — it was held in place by a code comment, so
 * re-adding the preset would have failed no test. These do.
 */

import { describe, it, expect } from "vitest";
import {
  QUICK_PROVIDER_PRESETS,
  INCOMPATIBLE_PRESET_HOSTS,
} from "$lib/design/provider-presets";

const hostOf = (url: string) => new URL(url).host;

describe("quick-add provider presets", () => {
  it("offers no preset pointing at an endpoint this app cannot talk to", () => {
    const offending = QUICK_PROVIDER_PRESETS.filter((p) =>
      INCOMPATIBLE_PRESET_HOSTS.includes(hostOf(p.baseUrl)),
    );
    expect(
      offending.map((p) => `${p.name} → ${p.baseUrl}`),
      "these hosts do not speak the OpenAI-compatible API this app requires, so the preset would add a provider that lists no models and can never be selected",
    ).toEqual([]);
  });

  it("has no Anthropic preset, by host and by name", () => {
    // Named explicitly: this is the one that shipped, and the one most likely
    // to be re-added by someone reading the provider list and noticing a gap.
    expect(QUICK_PROVIDER_PRESETS.map((p) => hostOf(p.baseUrl))).not.toContain(
      "api.anthropic.com",
    );
    expect(
      QUICK_PROVIDER_PRESETS.some((p) => /anthropic/i.test(p.name)),
    ).toBe(false);
    expect(QUICK_PROVIDER_PRESETS.some((p) => /anthropic/i.test(p.id))).toBe(
      false,
    );
  });

  it("keeps the presets well-formed and distinct", () => {
    expect(QUICK_PROVIDER_PRESETS.length).toBeGreaterThan(0);
    const ids = QUICK_PROVIDER_PRESETS.map((p) => p.id);
    expect(new Set(ids).size).toBe(ids.length);
    for (const preset of QUICK_PROVIDER_PRESETS) {
      expect(() => new URL(preset.baseUrl)).not.toThrow();
      expect(preset.name.trim()).not.toBe("");
      expect(["local", "cloud", "custom"]).toContain(preset.kind);
    }
  });
});
