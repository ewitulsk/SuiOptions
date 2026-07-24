// Keygen step (SO-305): run the two-round DKG, then FORCE an encrypted
// backup of the curator share before the ceremony can complete. Bluefin
// accounts cannot rotate keys, so this backup is load-bearing — the UI says
// so and blocks completion until the file is downloaded.

import { useState } from "react";

import { runKeygenCeremony } from "../../frost/ceremony";
import {
  cacheShare,
  downloadShareBackup,
  encryptShare,
  type ShareBackup,
} from "../../frost/share";
import type { UnlockedShare } from "../../state/curatorBluefin";
import { CeremonyStatus, useCeremony } from "./ceremonyUi";
import { curatorFieldStyle } from "./styles";

type KeygenDraft = {
  keyPackageB64: string;
  publicKeyPackageB64: string;
  groupPublicKeyHex: string;
  parentAddress: string;
};

export function KeygenCeremony({
  vaultId,
  onComplete,
}: {
  vaultId: string;
  onComplete: (share: UnlockedShare, parentAddress: string, publicKeyPackageB64: string, groupPublicKeyHex: string) => void;
}) {
  const { state, run, busy } = useCeremony();
  const [draft, setDraft] = useState<KeygenDraft | null>(null);
  const [passphrase, setPassphrase] = useState("");
  const [confirm, setConfirm] = useState("");
  const [cache, setCache] = useState(true);
  const [downloaded, setDownloaded] = useState(false);
  const [backup, setBackup] = useState<ShareBackup | null>(null);
  const [error, setError] = useState<string | null>(null);

  const onKeygen = async () => {
    const outcome = await run(
      (onProgress) => runKeygenCeremony(vaultId, onProgress),
      "Key shares generated — back up your share now.",
    );
    if (outcome) {
      setDraft({
        keyPackageB64: outcome.keyPackageB64,
        publicKeyPackageB64: outcome.publicKeyPackageB64,
        groupPublicKeyHex: outcome.groupPublicKeyHex,
        parentAddress: outcome.parentAddress,
      });
    }
  };

  const onBackup = async () => {
    if (!draft) return;
    setError(null);
    if (passphrase.length < 8) {
      setError("Use a passphrase of at least 8 characters.");
      return;
    }
    if (passphrase !== confirm) {
      setError("Passphrases do not match.");
      return;
    }
    try {
      const b = await encryptShare(draft.keyPackageB64, passphrase, {
        vaultId,
        parentAddress: draft.parentAddress,
        groupPublicKeyHex: draft.groupPublicKeyHex,
        publicKeyPackageB64: draft.publicKeyPackageB64,
      });
      downloadShareBackup(b);
      if (cache) cacheShare(b);
      setBackup(b);
      setDownloaded(true);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  };

  const onFinish = () => {
    if (!draft || !downloaded) return;
    onComplete(
      {
        vaultId,
        keyPackageB64: draft.keyPackageB64,
        publicKeyPackageB64: draft.publicKeyPackageB64,
        groupPublicKeyHex: draft.groupPublicKeyHex,
        parentAddress: draft.parentAddress,
      },
      draft.parentAddress,
      draft.publicKeyPackageB64,
      draft.groupPublicKeyHex,
    );
  };

  if (!draft) {
    return (
      <div className="vault-card__body">
        <div className="vault-prose__muted" style={{ fontSize: 12, marginBottom: 8 }}>
          Generate the vault's Bluefin parent key by a 2-of-2 distributed key
          ceremony with the protocol signer. Your half is created in your
          browser and never leaves it unencrypted.
        </div>
        <button className="vault-invest__cta" disabled={busy} onClick={onKeygen}>
          {busy ? "Running key ceremony…" : "Start key ceremony"}
        </button>
        <CeremonyStatus state={state} />
      </div>
    );
  }

  return (
    <div className="vault-card__body">
      <div className="vault-kv" style={{ marginBottom: 10 }}>
        <div className="vault-kv__row">
          <span>Parent account (Sui address)</span>
          <span title={draft.parentAddress}>{draft.parentAddress}</span>
        </div>
      </div>
      <button
        className="vault-invest__tab"
        style={{ marginBottom: 10 }}
        onClick={() => void navigator.clipboard.writeText(draft.parentAddress)}
      >
        Copy parent address
      </button>

      <div
        className="status-pill is-danger"
        style={{ display: "block", fontSize: 12, lineHeight: 1.5, padding: "6px 10px", marginBottom: 10 }}
      >
        ⚠ Bluefin accounts cannot rotate keys. If you lose this share, the
        parent account — and any funds in it — are permanently stranded.
        Downloading the encrypted backup is mandatory.
      </div>

      {!downloaded ? (
        <>
          <input
            type="password"
            placeholder="Backup passphrase (min 8 chars)"
            value={passphrase}
            onChange={(e) => setPassphrase(e.target.value)}
            style={{ ...curatorFieldStyle, marginBottom: 8 }}
          />
          <input
            type="password"
            placeholder="Confirm passphrase"
            value={confirm}
            onChange={(e) => setConfirm(e.target.value)}
            style={{ ...curatorFieldStyle, marginBottom: 8 }}
          />
          <label style={{ fontSize: 12, display: "flex", gap: 6, alignItems: "center", marginBottom: 8 }}>
            <input type="checkbox" checked={cache} onChange={(e) => setCache(e.target.checked)} />
            Also cache the encrypted share in this browser (unlock per session)
          </label>
          <button className="vault-invest__cta" onClick={onBackup}>
            Download encrypted backup
          </button>
          {error && (
            <div className="status-pill is-danger" style={{ display: "block", marginTop: 8, fontSize: 12 }}>
              ⚠ {error}
            </div>
          )}
        </>
      ) : (
        <>
          <div className="status-pill is-success" style={{ display: "block", fontSize: 12, marginBottom: 10 }}>
            ✓ Backup downloaded{backup && cache ? " and cached" : ""}. Store it somewhere safe.
          </div>
          <button
            className="vault-invest__tab"
            style={{ marginBottom: 8 }}
            onClick={() => backup && downloadShareBackup(backup)}
          >
            Download again
          </button>
          <button className="vault-invest__cta" onClick={onFinish}>
            Continue to registration
          </button>
        </>
      )}
    </div>
  );
}
