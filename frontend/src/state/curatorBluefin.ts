// Curator dashboard state (SO-305): resumable setup-wizard progress
// (vault-keyed localStorage) and the in-session unlocked FROST share.

import { useCallback, useState } from "react";

import type { ShareBackup } from "../frost/share";

/** Setup steps, in order. `done` means the wizard can advance past it. */
export type WizardStep =
  | "keygen"
  | "register"
  | "fund"
  | "deposit"
  | "authorize"
  | "complete";

export const WIZARD_STEPS: WizardStep[] = [
  "keygen",
  "register",
  "fund",
  "deposit",
  "authorize",
  "complete",
];

/** Persisted, non-secret wizard progress for one vault. The curator share
 * itself is stored separately (encrypted) by frost/share.ts. */
export type WizardProgress = {
  vaultId: string;
  step: WizardStep;
  /** The FROST parent (group) address, once keygen has run. */
  parentAddress?: string;
  groupPublicKeyHex?: string;
  publicKeyPackageB64?: string;
  /** True once the on-chain external account matches the parent address. */
  registered?: boolean;
  /** True once the Bluefin account is materialized (deposit landed). */
  deposited?: boolean;
  /** True once the curator wallet is an authorized trader. */
  authorized?: boolean;
  updatedAtMs: number;
};

const WIZARD_PREFIX = "curator-wizard:";

export function loadWizardProgress(vaultId: string): WizardProgress | null {
  const raw = localStorage.getItem(WIZARD_PREFIX + vaultId);
  if (!raw) return null;
  try {
    return JSON.parse(raw) as WizardProgress;
  } catch {
    return null;
  }
}

export function saveWizardProgress(p: WizardProgress): void {
  localStorage.setItem(
    WIZARD_PREFIX + p.vaultId,
    JSON.stringify({ ...p, updatedAtMs: Date.now() }),
  );
}

/** The curator's FROST share unlocked for this browser session (in-memory
 * only — never persisted in the clear). */
export type UnlockedShare = {
  vaultId: string;
  keyPackageB64: string;
  publicKeyPackageB64: string;
  groupPublicKeyHex: string;
  parentAddress: string;
};

/** Vault-scoped wizard state + the unlocked share for the session. */
export function useCuratorBluefin(vaultId: string) {
  const [progress, setProgress] = useState<WizardProgress | null>(() =>
    loadWizardProgress(vaultId),
  );
  const [share, setShare] = useState<UnlockedShare | null>(null);

  const update = useCallback(
    (patch: Partial<WizardProgress>) => {
      setProgress((prev) => {
        const next: WizardProgress = {
          vaultId,
          step: "keygen",
          ...prev,
          ...patch,
          updatedAtMs: Date.now(),
        };
        saveWizardProgress(next);
        return next;
      });
    },
    [vaultId],
  );

  const unlock = useCallback(
    (backup: ShareBackup, keyPackageB64: string) => {
      setShare({
        vaultId,
        keyPackageB64,
        publicKeyPackageB64: backup.publicKeyPackageB64,
        groupPublicKeyHex: backup.groupPublicKeyHex,
        parentAddress: backup.parentAddress,
      });
    },
    [vaultId],
  );

  const setUnlocked = useCallback((s: UnlockedShare) => setShare(s), []);

  return { progress, share, update, unlock, setUnlocked };
}
