import { useSyncExternalStore } from "react";

export type ThemeMode = "light" | "dark";

const STORAGE_KEY = "pismo-theme-mode";
// Pre-rebrand key. Read once as a fallback so existing users keep their mode.
const LEGACY_STORAGE_KEY = "tideline-theme-mode";

function readStored(): ThemeMode {
  try {
    const v = localStorage.getItem(STORAGE_KEY);
    if (v === "light" || v === "dark") return v;
    const legacy = localStorage.getItem(LEGACY_STORAGE_KEY);
    if (legacy === "light" || legacy === "dark") {
      localStorage.setItem(STORAGE_KEY, legacy);
      localStorage.removeItem(LEGACY_STORAGE_KEY);
      return legacy;
    }
  } catch {}
  return "dark";
}

let currentMode: ThemeMode = readStored();
const listeners = new Set<() => void>();

function applyToDom(mode: ThemeMode) {
  if (typeof document === "undefined") return;
  document.documentElement.setAttribute("data-mode", mode);
}

// Apply immediately on module load so the page paints with the right mode.
applyToDom(currentMode);

export function getMode(): ThemeMode {
  return currentMode;
}

export function setMode(next: ThemeMode) {
  if (next === currentMode) return;
  currentMode = next;
  try {
    localStorage.setItem(STORAGE_KEY, next);
  } catch {}
  applyToDom(next);
  listeners.forEach((l) => l());
}

export function toggleMode() {
  setMode(currentMode === "dark" ? "light" : "dark");
}

function subscribe(cb: () => void): () => void {
  listeners.add(cb);
  return () => listeners.delete(cb);
}

export function useThemeMode(): ThemeMode {
  return useSyncExternalStore(subscribe, getMode, getMode);
}
