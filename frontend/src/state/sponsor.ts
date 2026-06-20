// Sponsor-toggle store. Mirrors `theme.ts`: a module-level store exposed via
// `useSyncExternalStore`, persisted to localStorage.
//
// Default behaviour: ON. If the user has never explicitly toggled it
// (`userPref === null`), the effective state follows the gas station's health
// — when the sponsor balance is below threshold the toggle defaults OFF. Once
// the user toggles explicitly, their choice wins regardless of balance.

import { useSyncExternalStore } from "react";

import { fetchGasStationBalance, type GasStationBalance } from "../api/sponsor";

const STORAGE_KEY = "tideline-sponsor-pref";

function readPref(): boolean | null {
  try {
    const v = localStorage.getItem(STORAGE_KEY);
    if (v === "on") return true;
    if (v === "off") return false;
  } catch {}
  return null; // no explicit choice yet
}

// User's explicit preference, or null when they've never toggled.
let userPref: boolean | null = readPref();
// Latest gas-station health, or null until the first fetch resolves.
let health: GasStationBalance | null = null;

const listeners = new Set<() => void>();
function emit() {
  listeners.forEach((l) => l());
}

/** Effective on/off: explicit user choice wins; otherwise follow balance. */
export function isSponsorEnabled(): boolean {
  if (userPref !== null) return userPref;
  return health?.healthy ?? true; // default ON until we know otherwise
}

export function getSponsorHealth(): GasStationBalance | null {
  return health;
}

export function setSponsorEnabled(next: boolean) {
  userPref = next;
  try {
    localStorage.setItem(STORAGE_KEY, next ? "on" : "off");
  } catch {}
  emit();
}

/** Fetch the gas-station balance once at startup to seed the default. */
export async function initSponsorHealth(): Promise<void> {
  try {
    health = await fetchGasStationBalance();
  } catch {
    // Gas station unreachable: leave health null. With no explicit user pref
    // the toggle stays ON, but a sponsorship attempt will fail and surface an
    // error prompting the user to turn off the toggle (see useSubmitTransaction).
    health = null;
  }
  emit();
}

function subscribe(cb: () => void): () => void {
  listeners.add(cb);
  return () => listeners.delete(cb);
}

export function useSponsorEnabled(): boolean {
  return useSyncExternalStore(subscribe, isSponsorEnabled, isSponsorEnabled);
}

export function useSponsorHealth(): GasStationBalance | null {
  return useSyncExternalStore(subscribe, getSponsorHealth, getSponsorHealth);
}
