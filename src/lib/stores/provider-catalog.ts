// Lost Harness — Provider model catalog (M1 stub).
//
// The Electron prototype pulls models live from each provider's `/models`
// endpoint via the native bridge. The Tauri product's bridge isn't wired
// yet, so we ship a small baked-in list per known provider id to drive
// the UI. Custom / local / unknown providers fall through to a freeform
// entry so the picker still works.

import type { Provider } from "./providers.svelte";

export interface ModelPreset {
  id: string;
  name: string;
}

const KNOWN_MODELS: Record<string, ModelPreset[]> = {
  openai: [
    { id: "gpt-4o", name: "gpt-4o" },
    { id: "gpt-4o-mini", name: "gpt-4o-mini" },
    { id: "gpt-4.1", name: "gpt-4.1" },
    { id: "gpt-4.1-mini", name: "gpt-4.1-mini" },
    { id: "o3-mini", name: "o3-mini" },
    { id: "o4-mini", name: "o4-mini" },
  ],
  anthropic: [
    { id: "claude-opus-4-7", name: "claude-opus-4-7" },
    { id: "claude-sonnet-4-5", name: "claude-sonnet-4-5" },
    { id: "claude-haiku-4-5", name: "claude-haiku-4-5" },
  ],
  openrouter: [
    { id: "anthropic/claude-sonnet-4", name: "anthropic/claude-sonnet-4" },
    { id: "openai/gpt-4o", name: "openai/gpt-4o" },
    { id: "google/gemini-2.5-pro", name: "google/gemini-2.5-pro" },
  ],
  gemini: [
    { id: "gemini-2.5-pro", name: "gemini-2.5-pro" },
    { id: "gemini-2.5-flash", name: "gemini-2.5-flash" },
  ],
  groq: [
    { id: "llama-3.3-70b-versatile", name: "llama-3.3-70b-versatile" },
    { id: "mixtral-8x7b-32768", name: "mixtral-8x7b-32768" },
  ],
  mistral: [
    { id: "mistral-large-latest", name: "mistral-large-latest" },
    { id: "mistral-small-latest", name: "mistral-small-latest" },
  ],
  together: [
    { id: "meta-llama/Llama-3.3-70B-Instruct-Turbo", name: "Llama-3.3-70B-Instruct-Turbo" },
  ],
  deepseek: [
    { id: "deepseek-chat", name: "deepseek-chat" },
    { id: "deepseek-reasoner", name: "deepseek-reasoner" },
  ],
  xai: [
    { id: "grok-3", name: "grok-3" },
    { id: "grok-3-mini", name: "grok-3-mini" },
  ],
  perplexity: [
    { id: "sonar-pro", name: "sonar-pro" },
  ],
  fireworks: [
    { id: "accounts/fireworks/models/llama-v3p3-70b-instruct", name: "llama-v3p3-70b-instruct" },
  ],
  cohere: [
    { id: "command-r-plus", name: "command-r-plus" },
  ],
  // Local providers have no baked list — the user types the model name
  // their server is serving.
  lmstudio: [],
  ollama: [],
  vllm: [],
  llamacpp: [],
};

const FALLBACK: ModelPreset[] = [{ id: "default", name: "default" }];

/** Return the catalog of models for a provider. Empty for local providers. */
export function modelsForProvider(provider: Provider): ModelPreset[] {
  if (KNOWN_MODELS[provider.id]) return KNOWN_MODELS[provider.id];
  // Local / custom endpoints serve whatever the user loaded — show the
  // fallback so the picker isn't blank.
  if (provider.kind === "local" || provider.kind === "custom") return FALLBACK;
  // Unknown cloud provider → empty list (user can freeform-type a name).
  return [];
}
