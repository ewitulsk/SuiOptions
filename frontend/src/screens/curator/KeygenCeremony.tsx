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
//
// SO-309 adds the mobile half of that gate: iOS Safari ignores `download` on a
// blob anchor (it opens a viewer), so the anchor's click told us nothing. On a
// touch device that can share files the save goes through `navigator.share`
// and only a RESOLVED share counts; and on any touch device the curator must
// additionally confirm the file landed somewhere outside this browser. Desktop
// keeps the SO-307 anchor and its click-means-downloaded behavior.

import { useEffect, useMemo, useState } from "react";

import { HedgeSignerError, fetchFrostPubkey } from "../../api/hedgeSigner";
import { runKeygenCeremony } from "../../frost/ceremony";
import { loadFrost } from "../../frost/frost";
import {
  cacheShare,
  cachedShare,
  clearCachedShare,
  encryptShare,
  shareBackupBlob,
  shareBackupFile,
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
  const [shared, setShared] = useState(false);
  const [confirmedSaved, setConfirmedSaved] = useState(false);
  const [shareNote, setShareNote] = useState<string | null>(null);
  const [backup, setBackup] = useState<ShareBackup | null>(null);
  const [backupUrl, setBackupUrl] = useState<string | null>(null);
  const [encrypting, setEncrypting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [resume, setResume] = useState<ResumeState | null>(() => {
    const cached = cachedShare(vaultId);
    return cached ? { parentAddress: cached.parentAddress, hasCache: true } : null;
  });
  const [coarsePointer] = useState(() => window.matchMedia?.("(pointer: coarse)").matches ?? false);

  const passphraseOk = passphrase.length >= 8 && passphrase === confirm;

  // Warm the wasm module while the curator is still typing a passphrase: its
  // first load is multi-second on cellular, and today that lands mid-ceremony.
  useEffect(() => {
    void loadFrost();
  }, []);

  // The download anchor's href, minted when the encrypted backup lands.
  // Revoke-on-unmount is safe: `backup` lives in this component too, so an
  // unmount loses the backup itself and the flow restarts on the resume path.
  useEffect(() => {
    if (!backup) return;
    const url = URL.createObjectURL(shareBackupBlob(backup));
    setBackupUrl(url);
    return () => URL.revokeObjectURL(url);
  }, [backup]);

  // Web Share with files, on a touch device: the share sheet actually writes
  // the file (Files/Drive/…) and its promise reports whether that happened.
  // Gated on the pointer too, because desktop Chrome also advertises
  // canShare({files}) and its anchor download works — SO-307 keeps that path.
  const backupFile = useMemo(() => (backup ? shareBackupFile(backup) : null), [backup]);
  const shareFile = useMemo(
    () =>
      coarsePointer &&
      backupFile !== null &&
      typeof navigator.share === "function" &&
      (navigator.canShare?.({ files: [backupFile] }) ?? false),
    [backupFile, coarsePointer],
  );

  // Touch devices attest regardless of which save path they got: a share sheet
  // can end in a chat app, and a mobile "download" can be a preview that was
  // never saved.
  const needsConfirm = coarsePointer;
  const saved = shareFile ? shared : downloaded;
  const canContinue = saved && (!needsConfirm || confirmedSaved);

  const onShare = async () => {
    if (!backupFile) return;
    setShareNote(null);
    try {
      // No await before this call: the payload is already in memory, so the
      // user activation from this click is still live.
      await navigator.share({ files: [backupFile], title: backupFile.name });
      setShared(true);
    } catch {
      // Cancelled, or the target refused the file — the gate stays closed.
      setShareNote(
        "Backup not saved yet — choose a destination that stores the file (Files, Drive, …).",
      );
    }
  };

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
    if (!draft || !canContinue) return;
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
          registration. Mobile browsers (iOS especially) evict site storage
          after about a week without a visit, so the cache is never a backup —
          the file is.
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
        <>
          <button className="vault-invest__cta" disabled>
            {encrypting ? "Encrypting backup…" : "Preparing backup…"}
          </button>
          {encrypting && (
            <div
              className="vault-prose__muted"
              role="status"
              style={{ fontSize: 12, marginTop: 8, display: "flex", alignItems: "center", gap: 8 }}
            >
              <span
                className="modal__spinner"
                style={{ width: 14, height: 14, borderWidth: 2, margin: 0, flex: "none" }}
              />
              Deriving the encryption key — a few seconds on a phone.
            </div>
          )}
        </>
      ) : (
        <>
          {shareFile ? (
            // iOS/Android: `<a download>` on a blob URL may just open a viewer,
            // so the click proves nothing. The share sheet's promise does.
            <button
              className="vault-invest__cta"
              style={{ marginBottom: 8 }}
              onClick={onShare}
            >
              {shared ? "Save backup file again…" : "Save backup file…"}
            </button>
          ) : (
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
          )}
          {shareNote && (
            <div className="status-pill status-pill--note is-info" style={{ marginBottom: 8 }}>
              {shareNote}
            </div>
          )}
          {saved ? (
            <>
              <div className="status-pill status-pill--note is-success" style={{ marginBottom: 10 }}>
                ✓ Backup {shareFile ? "saved" : "downloaded"}
                {cache ? " and cached in this browser" : ""}. Store it somewhere safe.
              </div>
              {needsConfirm && (
                <label
                  style={{
                    fontSize: 12,
                    display: "flex",
                    gap: 6,
                    alignItems: "flex-start",
                    marginBottom: 10,
                  }}
                >
                  <input
                    type="checkbox"
                    checked={confirmedSaved}
                    onChange={(e) => setConfirmedSaved(e.target.checked)}
                  />
                  I have verified the backup file is saved outside this browser.
                </label>
              )}
              <button className="vault-invest__cta" disabled={!canContinue} onClick={onFinish}>
                Continue to registration
              </button>
            </>
          ) : (
            <div className="vault-prose__muted" style={{ fontSize: 12 }}>
              {shareFile ? "Save the backup file" : "Download the backup"} to
              continue. The encrypted share is already saved in this browser,
              but a browser cache is not a backup.
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
