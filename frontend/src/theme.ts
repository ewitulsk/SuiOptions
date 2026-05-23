import { useSyncExternalStore } from "react";

export type ThemeMode = "light" | "dark";

const STORAGE_KEY = "tideline-theme-mode";

function readStored(): ThemeMode {
  try {
    const v = localStorage.getItem(STORAGE_KEY);
    if (v === "light" || v === "dark") return v;
  } catch {}
  return "light";
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
