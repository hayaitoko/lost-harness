// The Settings → Models quick-add presets.
//
// Extracted from Settings.svelte so the constraint below is testable. It was
// inline in the component, which meant nothing failed if the trap preset came
// back — the removal was held in place by a comment and good intentions.

import type { ProviderKind } from "$lib/stores/providers.svelte";

export interface QuickProviderPreset {
  id: string;
  name: string;
  baseUrl: string;
  kind: ProviderKind;
}

/**
 * One-click endpoint presets.
 *
 * # The rule every entry has to satisfy
 *
 * **A preset may only point at an endpoint this app can actually talk to.**
 * Lost Harness's model client speaks exactly one wire protocol
 * (`src-tauri/src/models/client.rs`): `GET {base_url}/models` and
 * `POST {base_url}/chat/completions`, authenticated with
 * `Authorization: Bearer`. There is no free-text model entry anywhere in the
 * UI, so a provider whose `/models` comes back empty or errors can never be
 * selected at all — it is a dead row the user can add and then never use.
 *
 * That is why there is **no Anthropic preset, and one must not be re-added**.
 * Anthropic's native API needs `x-api-key` + `anthropic-version` and rejects a
 * Bearer key, so `https://api.anthropic.com/v1/models` always came back empty.
 * Relabelling it would not have made the request work. (It also sorted first
 * alphabetically, which is how it became the endpoint every "just use the first
 * configured provider" path silently served — see the endpoint-routing spec.)
 *
 * `src/tests/provider-presets.test.ts` enforces this.
 */
export const QUICK_PROVIDER_PRESETS: QuickProviderPreset[] = [
  { id: "openai", name: "OpenAI", baseUrl: "https://api.openai.com/v1", kind: "cloud" },
  {
    id: "openrouter",
    name: "OpenRouter",
    baseUrl: "https://openrouter.ai/api/v1",
    kind: "cloud",
  },
  { id: "lmstudio", name: "LM Studio", baseUrl: "http://localhost:1234/v1", kind: "local" },
  { id: "ollama", name: "Ollama", baseUrl: "http://127.0.0.1:11434/v1", kind: "local" },
];

/**
 * Hosts known to require an auth scheme or request shape this app's model
 * client does not speak. A preset pointing at one of these is a trap: the user
 * can add it, it will list no models, and nothing about it will work.
 *
 * Kept as data rather than a single hard-coded assertion so a future
 * "let's add Anthropic/Vertex/Bedrock quickly" has something concrete to trip
 * over.
 */
export const INCOMPATIBLE_PRESET_HOSTS = [
  "api.anthropic.com",
  "generativelanguage.googleapis.com",
  "bedrock-runtime.us-east-1.amazonaws.com",
];
