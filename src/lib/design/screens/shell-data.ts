// Shared shell sample-data — lifted verbatim from the DC templates' renderVals().
// Consumed by the left rail (Sidebar) and the model picker. Local sample data
// only; no backend wiring this pass.

export type Model = { name: string; kind: "local" | "cloud"; group: string };

export const MODELS: Model[] = [
  { name: "Qwen3-14B", kind: "local", group: "On this Mac" },
  { name: "Llama 3.3 8B", kind: "local", group: "On this Mac" },
  { name: "Claude Opus 4.8", kind: "cloud", group: "Anthropic" },
  { name: "Claude Sonnet 5", kind: "cloud", group: "Anthropic" },
];

export const PROFILES = [
  { name: "Personal", sub: "Memory wall · strict", avatar: "P" },
  { name: "Work", sub: "Memory wall · standard", avatar: "W" },
];
