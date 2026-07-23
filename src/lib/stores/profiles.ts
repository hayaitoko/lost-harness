// Lost Harness — Profile state (Svelte stores).
//
// The four profiles (personal / work / school / developer) map to distinct
// model defaults, tool policies, and sensitivity routing. The M1 stub only
// tracks the list and active id; the real impl will read from the profile
// manager and watch for external switches (CLI flag, IPC, system event).

import { writable, derived, get, type Readable } from "svelte/store";
import * as api from "../api/tauri";
import {
  activeConversationId,
  conversations,
  hydrateConversations,
} from "./chat";

export interface Profile {
  id: string;
  name: string;
  /** Single-character or short emoji used in the sidebar / chip. */
  icon: string;
}

/** Display names for the four profile ids from the spec. */
const DEFAULT_PROFILES: Profile[] = [
  { id: "personal", name: "Personal", icon: "🏠" },
  { id: "work", name: "Work", icon: "💼" },
  { id: "school", name: "School", icon: "🎓" },
  { id: "developer", name: "Developer", icon: "🛠" },
];

export const profiles = writable<Profile[]>(DEFAULT_PROFILES);
export const activeProfileId = writable<string>("personal");

/** Derived: the full Profile object for the active id. */
export const activeProfile: Readable<Profile | null> = derived(
  [profiles, activeProfileId],
  ([$profiles, $activeId]) =>
    $profiles.find((p) => p.id === $activeId) ?? null,
);

/**
 * Switches the active profile and reloads the conversation stores from the
 * new profile's DB. Backend-wise this is still a no-op by design: every data
 * command (`list_conversations`, `send_message`, …) takes an explicit
 * `profile` arg, so the switch is purely frontend state.
 */
export async function switchProfile(id: string): Promise<void> {
  if (!DEFAULT_PROFILES.some((p) => p.id === id)) {
    throw new Error(`Unknown profile: ${id}`);
  }
  if (get(activeProfileId) === id) return;
  activeProfileId.set(id);
  // Persist locally so the browser fallback (no Tauri) survives reloads.
  try {
    if (typeof localStorage !== "undefined") {
      localStorage.setItem("lh.activeProfile", id);
    }
  } catch {
    // localStorage may be unavailable (private mode, SSR); non-fatal.
  }
  // The chat stores still hold the previous profile's conversations. Clear
  // them BEFORE rehydrating: hydrateConversations() merges rather than
  // replaces (see the invariant in docs/codebase/frontend-svelte.md), so a
  // bare re-call would keep the old profile's rows as "local-only" entries —
  // and a stale activeConversationId would make the next send_message hit a
  // conversation id that doesn't exist in the new profile's DB.
  activeConversationId.set(null);
  conversations.set([]);
  await hydrateConversations();
}

/**
 * Hydrates the active profile from the Rust core (or browser fallback).
 * Call once on app start. Idempotent.
 */
export async function hydrate(): Promise<void> {
  try {
    const [remoteProfiles, remoteActive] = await Promise.all([
      api.listProfiles(),
      api.getActiveProfile(),
    ]);
    if (remoteProfiles.length > 0) {
      profiles.set(
        remoteProfiles.map((id) => {
          const known = DEFAULT_PROFILES.find((p) => p.id === id);
          return known ?? { id, name: id, icon: "•" };
        }),
      );
    }
    if (remoteActive) {
      activeProfileId.set(remoteActive);
    }
  } catch (err) {
    // Backend unreachable (e.g. browser fallback in an isolated iframe) —
    // fall back to defaults already in the stores.
    if (typeof localStorage !== "undefined") {
      const stored = localStorage.getItem("lh.activeProfile");
      if (stored) activeProfileId.set(stored);
    }
  }
}

// Re-export so a UI can reflect the current value without subscribing.
export function getActiveProfileId(): string {
  return get(activeProfileId);
}
