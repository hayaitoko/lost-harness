// Shared design-system types, ported from ~/Desktop/lost-harness-ui/src/types.ts.
// These mirror the Rust core's routing vocabulary (agent::gate::Binding /
// GateDecision) so the UI and backend speak the same language.

/** Where a message/turn actually went. The product's core meaning-color signal. */
export type Route = "local" | "cloud" | "blocked";

/** A conversation's binding — the user's routing *intent*. */
export type Binding = "auto" | "public" | "private";

/** The top-level surfaces reachable from the left-rail section nav + chat/settings. */
export type ScreenId =
  | "main"
  | "empty"
  | "email"
  | "whiteboard"
  | "files"
  | "scheduled-jobs"
  | "editor"
  | "settings"
  | "onboarding";

export const SCREEN_IDS: ScreenId[] = [
  "main",
  "empty",
  "email",
  "whiteboard",
  "files",
  "scheduled-jobs",
  "editor",
  "settings",
  "onboarding",
];
