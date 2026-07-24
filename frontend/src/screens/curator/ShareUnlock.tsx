// Unlock the curator's FROST share for the session (SO-305): decrypt the
// encrypted backup (from the localStorage cache or an uploaded file) with
// the passphrase. The plaintext key package lives only in memory
// (useCuratorBluefin), never on disk.

import { useState } from "react";

import { loadFrost } from "../../frost/frost";
import {
  cachedShare,
  decryptShare,
  parseShareBackup,
  type ShareBackup,
} from "../../frost/share";
import type { UnlockedShare } from "../../state/curatorBluefin";
import { curatorFieldStyle } from "./styles";

export function ShareUnlock({
  vaultId,
  expectedParentAddress,
  onUnlocked,
}: {
  vaultId: string;
  /** The vault's known parent address, to reject a mismatched share file. */
  expectedParentAddress?: string;
  onUnlocked: (share: UnlockedShare) => void;
}) {
  const [backup, setBackup] = useState<ShareBackup | null>(() => cachedShare(vaultId));
  const [passphrase, setPassphrase] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const onFile = async (file: File) => {
    setError(null);
    try {
      const parsed = parseShareBackup(await file.text());
      if (parsed.vaultId !== vaultId) {
        setError("This share file is for a different vault.");
        return;
      }
      setBackup(parsed);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  };

  const onUnlock = async () => {
    if (!backup) return;
    setBusy(true);
    setError(null);
    try {
      const keyPackageB64 = await decryptShare(backup, passphrase);
      // Confirm the decrypted material really derives the vault's parent —
      // a valid-but-wrong share would sabotage every ceremony downstream.
      const frost = await loadFrost();
      const derived = frost.group_identity(backup.publicKeyPackageB64);
      const parent = expectedParentAddress ?? backup.parentAddress;
      if (derived.sui_address !== parent) {
        setError("Share does not match this vault's parent account.");
        return;
      }
      onUnlocked({
        vaultId,
        keyPackageB64,
        publicKeyPackageB64: backup.publicKeyPackageB64,
        groupPublicKeyHex: backup.groupPublicKeyHex,
        parentAddress: backup.parentAddress,
      });
      setPassphrase("");
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="vault-card__body">
      <div className="vault-prose__muted" style={{ fontSize: 12, marginBottom: 8 }}>
        {backup
          ? "Unlock your key share for this session to co-sign ceremonies."
          : "Load your encrypted key-share backup file to co-sign ceremonies."}
      </div>
      {!backup && (
        <input
          type="file"
          accept="application/json"
          onChange={(e) => e.target.files?.[0] && onFile(e.target.files[0])}
          style={{ fontSize: 12, marginBottom: 8 }}
        />
      )}
      {backup && (
        <>
          <input
            type="password"
            placeholder="Share passphrase"
            value={passphrase}
            onChange={(e) => setPassphrase(e.target.value)}
            style={{ ...curatorFieldStyle, marginBottom: 8 }}
          />
          <div style={{ display: "flex", gap: 8 }}>
            <button
              className="vault-invest__cta"
              disabled={busy || passphrase.length === 0}
              onClick={onUnlock}
            >
              {busy ? "Unlocking…" : "Unlock share"}
            </button>
            <button className="vault-invest__tab" onClick={() => setBackup(null)}>
              Use a different file
            </button>
          </div>
        </>
      )}
      {error && (
        <div className="status-pill is-danger" style={{ display: "block", marginTop: 8, fontSize: 12 }}>
          ⚠ {error}
        </div>
      )}
    </div>
  );
}
