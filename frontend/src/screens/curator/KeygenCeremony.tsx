// Keygen step (SO-305, fixed in SO-307): run the two-round DKG, then FORCE an
// encrypted backup of the curator share before the ceremony can complete.
// Bluefin accounts cannot rotate keys, so this backup is load-bearing — the UI
// says so and blocks completion until the file is downloaded.
//
// Two failure modes drive the shape of this step:
//   * The hedge-signer persists ITS half at DKG round 2, so a refresh between
//     the ceremony and the backup used to destroy the curator half for good
//     (and every retry then 409s). The passphrase is therefore collected
//     BEFORE the ceremony, and the share is encrypted + cached the instant the
//     ceremony returns — before any download UI exists.
//   * Deriving the key takes seconds (PBKDF2, 600k iterations); a programmatic
//     `a.click()` after that await has lost the browser's transient user
//     activation and is silently dropped. The download is a real anchor the
//     curator clicks.

import { useEffect, useState } from "react";

import { HedgeSignerError, fetchFrostPubkey } from "../../api/hedgeSigner";
import { runKeygenCeremony } from "../../frost/ceremony";
import {
  cacheShare,
  cachedShare,
  clearCachedShare,
  encryptShare,
  shareBackupBlob,
  shareBackupFilename,
  type ShareBackup,
} from "../../frost/share";
import type { UnlockedShare } from "../../state/curatorBluefin";
import { CeremonyStatus, useCeremony } from "./ceremonyUi";
import { ShareUnlock } from "./ShareUnlock";
import { curatorFieldStyle } from "./styles";

type KeygenDraft = {
  keyPackageB64: string;
  publicKeyPackageB64: string;
  groupPublicKeyHex: string;
  parentAddress: string;
};

/** A ceremony already ran for this vault: either the service says so (409 on
 * round 1) or this browser still holds the encrypted share from a run that
 * was interrupted before the backup. Either way the curator must unlock the
 * existing share, not start a new ceremony. */
type ResumeState = { parentAddress: string; hasCache: boolean };

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
  const [backupUrl, setBackupUrl] = useState<string | null>(null);
  const [encrypting, setEncrypting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [resume, setResume] = useState<ResumeState | null>(() => {
    const cached = cachedShare(vaultId);
    return cached ? { parentAddress: cached.parentAddress, hasCache: true } : null;
  });

  const passphraseOk = passphrase.length >= 8 && passphrase === confirm;

  // The download anchor's href, minted when the encrypted backup lands.
  useEffect(() => {
    if (!backup) return;
    const url = URL.createObjectURL(shareBackupBlob(backup));
    setBackupUrl(url);
    return () => URL.revokeObjectURL(url);
  }, [backup]);

  const onKeygen = async () => {
    setError(null);
    const outcome = await run(async (onProgress) => {
      try {
        return await runKeygenCeremony(vaultId, onProgress);
      } catch (e) {
        // The service already holds a share for this vault — a second
        // ceremony would orphan the parent account, so resume instead.
        if (e instanceof HedgeSignerError && e.status === 409) {
          onProgress("Existing key share found — looking up the parent account…");
          const existing = await fetchFrostPubkey(vaultId);
          if (!existing) throw e;
          setResume({
            parentAddress: existing.suiAddress,
            hasCache: cachedShare(vaultId) !== null,
          });
          return null;
        }
        throw e;
      }
    }, "Key shares generated — encrypting your backup…");
    if (!outcome) return;

    setDraft(outcome);
    setEncrypting(true);
    try {
      const b = await encryptShare(outcome.keyPackageB64, passphrase, {
        vaultId,
        parentAddress: outcome.parentAddress,
        groupPublicKeyHex: outcome.groupPublicKeyHex,
        publicKeyPackageB64: outcome.publicKeyPackageB64,
      });
      // Cache unconditionally: past this point the service holds its half, so
      // a refresh must not be able to lose ours. An opt-out is honored at
      // "Continue to registration".
      cacheShare(b);
      setBackup(b);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setEncrypting(false);
    }
  };

  const onFinish = () => {
    if (!draft || !downloaded) return;
    if (!cache) clearCachedShare(vaultId);
    onComplete(
      { vaultId, ...draft },
      draft.parentAddress,
      draft.publicKeyPackageB64,
      draft.groupPublicKeyHex,
    );
  };

  if (resume && !draft) {
    return (
      <div className="vault-card__body">
        <div className="status-pill status-pill--note is-info" style={{ marginBottom: 10 }}>
          A key ceremony has already run for this vault — the protocol signer
          holds its half of the parent key. Unlock your existing share to
          continue; a fresh ceremony would orphan the account below.
        </div>
        <div className="vault-kv" style={{ marginBottom: 10 }}>
          <div className="vault-kv__row">
            <span>Parent account (Sui address)</span>
            <span className="mono-break" style={{ minWidth: 0 }} title={resume.parentAddress}>
              {resume.parentAddress}
            </span>
          </div>
        </div>
        <button
          className="vault-invest__tab"
          style={{ marginBottom: 10 }}
          onClick={() => void navigator.clipboard.writeText(resume.parentAddress)}
        >
          Copy parent address
        </button>
        <ShareUnlock
          vaultId={vaultId}
          expectedParentAddress={resume.parentAddress}
          onUnlocked={(share) =>
            onComplete(share, share.parentAddress, share.publicKeyPackageB64, share.groupPublicKeyHex)
          }
        />
        {!resume.hasCache && (
          <div className="status-pill status-pill--note is-danger" style={{ marginTop: 10 }}>
            ⚠ This browser has no cached share. Load the encrypted backup file
            you downloaded during the ceremony. If it is gone, the curator half
            is unrecoverable and the parent account above must be abandoned: an
            operator has to prune the signer's share
            (<code className="mono-break">hedge-signer prune-share {vaultId}</code>) before a new key
            ceremony can run.
          </div>
        )}
      </div>
    );
  }

  if (!draft) {
    return (
      <div className="vault-card__body">
        <div className="vault-prose__muted" style={{ fontSize: 12, marginBottom: 8 }}>
          Generate the vault's Bluefin parent key by a 2-of-2 distributed key
          ceremony with the protocol signer. Your half is created in your
          browser and never leaves it unencrypted. Choose its backup passphrase
          first — the share is encrypted the moment the ceremony finishes.
        </div>
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
        <label style={{ fontSize: 12, display: "flex", gap: 6, alignItems: "center", marginBottom: 4 }}>
          <input type="checkbox" checked={cache} onChange={(e) => setCache(e.target.checked)} />
          Also cache the encrypted share in this browser (unlock per session)
        </label>
        <div className="vault-prose__muted" style={{ fontSize: 11, marginBottom: 8 }}>
          The encrypted share is always cached during the ceremony so a refresh
          can't lose it; unchecked, it is cleared once you continue to
          registration.
        </div>
        <button
          className="vault-invest__cta"
          disabled={busy || !passphraseOk}
          onClick={onKeygen}
        >
          {busy ? "Running key ceremony…" : "Start key ceremony"}
        </button>
        {(passphrase.length > 0 || confirm.length > 0) && !passphraseOk && (
          <div className="vault-prose__muted" style={{ fontSize: 11, marginTop: 6 }}>
            {passphrase.length < 8
              ? "Use a passphrase of at least 8 characters."
              : "Passphrases do not match."}
          </div>
        )}
        <CeremonyStatus state={state} />
      </div>
    );
  }

  return (
    <div className="vault-card__body">
      <div className="vault-kv" style={{ marginBottom: 10 }}>
        <div className="vault-kv__row">
          <span>Parent account (Sui address)</span>
          <span className="mono-break" style={{ minWidth: 0 }} title={draft.parentAddress}>
            {draft.parentAddress}
          </span>
        </div>
      </div>
      <button
        className="vault-invest__tab"
        style={{ marginBottom: 10 }}
        onClick={() => void navigator.clipboard.writeText(draft.parentAddress)}
      >
        Copy parent address
      </button>

      <div className="status-pill status-pill--note is-danger" style={{ marginBottom: 10 }}>
        ⚠ Bluefin accounts cannot rotate keys. If you lose this share, the
        parent account — and any funds in it — are permanently stranded.
        Downloading the encrypted backup is mandatory.
      </div>

      {!backup || !backupUrl ? (
        <button className="vault-invest__cta" disabled>
          {encrypting ? "Encrypting backup…" : "Preparing backup…"}
        </button>
      ) : (
        <>
          <a
            className="vault-invest__cta"
            href={backupUrl}
            download={shareBackupFilename(backup)}
            onClick={() => setDownloaded(true)}
            style={{
              display: "block",
              boxSizing: "border-box",
              textAlign: "center",
              textDecoration: "none",
              marginBottom: 8,
            }}
          >
            {downloaded ? "Download again" : "Download encrypted backup"}
          </a>
          {downloaded ? (
            <>
              <div className="status-pill status-pill--note is-success" style={{ marginBottom: 10 }}>
                ✓ Backup downloaded{cache ? " and cached in this browser" : ""}. Store it somewhere safe.
              </div>
              <button className="vault-invest__cta" onClick={onFinish}>
                Continue to registration
              </button>
            </>
          ) : (
            <div className="vault-prose__muted" style={{ fontSize: 12 }}>
              Download the backup to continue. The encrypted share is already
              saved in this browser, but a browser cache is not a backup.
            </div>
          )}
        </>
      )}
      {error && (
        <div className="status-pill status-pill--note is-danger" style={{ marginTop: 8 }}>
          ⚠ {error}
        </div>
      )}
    </div>
  );
}
