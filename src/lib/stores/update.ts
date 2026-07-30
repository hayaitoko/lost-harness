// Round-2 item 3 — the one slot holding "an update was found".
//
// Two things can fill it: the launch-time check (Rust emits `update:available`,
// which `UpdateBanner` listens for) and the Settings → About "Check for
// updates" button. Both write here so the banner is the SINGLE place an
// available update is ever offered — Settings never grows its own install
// button, and there is therefore only one path to `install_update`.
//
// Note what is deliberately absent: nothing in this module fetches anything.
// Discovering an update is Rust's job, behind the launch toggle.

import { writable } from "svelte/store";
import type { UpdateInfo } from "$lib/api/tauri";

/** The newer version we know about, or `null` when there is nothing to offer. */
export const availableUpdate = writable<UpdateInfo | null>(null);

/** Record a found update (from either the launch check or a manual check). */
export function setAvailableUpdate(info: UpdateInfo): void {
  availableUpdate.set(info);
}

/** Clear the slot — the user dismissed the banner, or a check came back current. */
export function clearAvailableUpdate(): void {
  availableUpdate.set(null);
}
