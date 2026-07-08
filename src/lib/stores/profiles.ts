// Lost Harness — Profile state (Svelte stores).
//
// The four profiles (personal / work / school / developer) map to distinct
// model defaults, tool policies, and sensitivity routing. The M1 stub only
// tracks the list and active id; the real impl will read from the profile
// manager and watch for external switches (CLI flag, IPC, system event).

import { writable, derived, get, type Readable } from "svelte/store";
import * as api from "../api/tauri";

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
 * Switches the active profile. M1 stub: in-memory only and a no-op on the
 * backend. Real impl will call into the profile manager (notify the agent
 * loop, flush conversation caches, swap default model endpoints).
 */
export async function switchProfile(id: string): Promise<void> {
  if (!DEFAULT_PROFILES.some((p) => p.id === id)) {
    throw new Error(`Unknown profile: ${id}`);
  }
  activeProfileId.set(id);
  // Persist locally so the browser fallback (no Tauri) survives reloads.
  try {
    if (typeof localStorage !== "undefined") {
      localStorage.setItem("lh.activeProfile", id);
    }
  } catch {
    // localStorage may be unavailable (private mode, SSR); non-fatal.
  }
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
