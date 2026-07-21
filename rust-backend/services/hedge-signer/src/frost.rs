//! FROST 2-of-2 threshold-ed25519 substrate (doc 03 §2/§3b).
//!
//! Each trading vault's Bluefin **parent account** key is a 2-of-2
//! threshold-ed25519 key: one share held by this service, one by the
//! curator. Externally it is a plain ed25519 pubkey + plain signatures — the
//! aggregated signature verifies under the group key with a stock ed25519
//! verifier, so Bluefin's Move verifier and Sui's transaction auth both
//! accept it. Neither party can sign alone.
//!
//! **Keygen: two-round distributed key generation (DKG), not trusted
//! dealer.** The frost crate's DKG (`frost_ed25519::keys::dkg`
//! part1/part2/part3) maps cleanly onto two HTTP round-trips with the
//! service as responder, so no party — and no dealer — ever holds the full
//! private key:
//!
//! ```text
//! curator part1 ──POST /frost/keygen/round1 {curator r1 pkg}──▶ service part1
//!               ◀──────────── {service r1 pkg} ────────────────
//! curator part2 ──POST /frost/keygen/round2 {curator r2 pkg}──▶ service part2+part3
//!               ◀── {service r2 pkg, group pubkey, address} ─── (share persisted)
//! curator part3 → same group pubkey
//! ```
//!
//! Round-2 packages must travel a confidential, authenticated channel
//! (nginx TLS in every deployed env). Keygen endpoints are otherwise
//! unauthenticated, like the rest of this service's surface — the group
//! address only becomes load-bearing once ops registers it as the vault's
//! hedge address on-chain, and that registration must verify the curator
//! really holds the counterpart share (out-of-band signing check). A vault
//! that already has a share refuses re-keygen: rotating a Bluefin parent
//! key is impossible (doc 03 §3b key-loss posture), so silently
//! regenerating one would orphan funds.
//!
//! Participant identifiers are fixed: curator = 1, service = 2.
//!
//! **Signing: standard two-round FROST.** Round 1 runs the payload policy
//! ([`crate::policy::bluefin`]) and, on approval, returns nonce commitments
//! and a session id; the session stores the approved 32-byte digest. Round 2
//! accepts the curator-built `SigningPackage`, refuses it unless its message
//! equals the stored digest byte-for-byte, and returns the service's
//! signature share. Sessions are in-memory, single-use, and expire.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use frost_ed25519 as frost;
use frost::keys::dkg;
use frost::keys::{KeyPackage, PublicKeyPackage};
use frost::Identifier;
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use sui_types::base_types::SuiAddress;

use crate::audit::now_ms;

/// Curator's fixed FROST participant identifier.
pub const CURATOR_ID: u16 = 1;
/// This service's fixed FROST participant identifier.
pub const SERVICE_ID: u16 = 2;

/// Keygen sessions die after this long without their round 2.
const KEYGEN_TTL: Duration = Duration::from_secs(10 * 60);
/// Signing sessions die after this long without their round 2.
const SIGN_TTL: Duration = Duration::from_secs(5 * 60);

pub fn curator_id() -> Identifier {
    Identifier::try_from(CURATOR_ID).expect("nonzero identifier")
}

pub fn service_id() -> Identifier {
    Identifier::try_from(SERVICE_ID).expect("nonzero identifier")
}

/// Sui address of the group ed25519 key: `blake2b256( 0x00 flag || pubkey )`.
pub fn group_sui_address(pubkeys: &PublicKeyPackage) -> Result<SuiAddress> {
    use blake2::digest::consts::U32;
    use blake2::{Blake2b, Digest};
    let pk = pubkeys
        .verifying_key()
        .serialize()
        .map_err(|e| anyhow!("serializing group verifying key: {e}"))?;
    let mut h = Blake2b::<U32>::new();
    h.update([0x00u8]); // ed25519 scheme flag
    h.update(&pk);
    let digest: [u8; 32] = h.finalize().into();
    SuiAddress::from_bytes(digest).context("deriving group Sui address")
}

// --------------------------------------------------------------------- store

/// One vault's persisted share material.
pub struct VaultShare {
    pub key_package: KeyPackage,
    pub public_key_package: PublicKeyPackage,
}

impl VaultShare {
    pub fn group_public_key_hex(&self) -> Result<String> {
        Ok(hex::encode(
            self.public_key_package
                .verifying_key()
                .serialize()
                .map_err(|e| anyhow!("serializing group verifying key: {e}"))?,
        ))
    }
}

/// On-disk record (hex of the frost binary serializations).
#[derive(Serialize, Deserialize)]
struct ShareRecord {
    key_package: String,
    public_key_package: String,
    created_ms: u64,
}

#[derive(Serialize, Deserialize, Default)]
struct StoreFile {
    #[serde(default)]
    vaults: BTreeMap<String, ShareRecord>,
}

/// TOML-file-backed share store, mirroring how the `[sui]` signing key lives
/// in a TOML the service reads at boot — but in its own file: deployed
/// `secrets.toml` is re-rendered from AWS on every deploy, while shares are
/// generated at runtime and must survive (the deployed path sits on the
/// service's persistent data volume). The file is chmod 0600 and gitignored.
pub struct ShareStore {
    path: PathBuf,
    shares: Mutex<HashMap<String, VaultShare>>,
}

impl ShareStore {
    /// Load the store (missing file → empty store). Fatal at boot on a
    /// present-but-unreadable file: never boot blind to existing shares.
    pub fn open(path: &Path) -> Result<Self> {
        let mut shares = HashMap::new();
        if path.exists() {
            let raw = std::fs::read_to_string(path)
                .with_context(|| format!("reading frost shares {}", path.display()))?;
            let file: StoreFile = toml::from_str(&raw)
                .with_context(|| format!("parsing frost shares {}", path.display()))?;
            for (vault_id, rec) in file.vaults {
                let key_package = KeyPackage::deserialize(
                    &hex::decode(&rec.key_package).context("decoding key_package hex")?,
                )
                .map_err(|e| anyhow!("deserializing key_package for {vault_id}: {e}"))?;
                let public_key_package = PublicKeyPackage::deserialize(
                    &hex::decode(&rec.public_key_package)
                        .context("decoding public_key_package hex")?,
                )
                .map_err(|e| anyhow!("deserializing public_key_package for {vault_id}: {e}"))?;
                shares.insert(
                    vault_id,
                    VaultShare {
                        key_package,
                        public_key_package,
                    },
                );
            }
        }
        Ok(Self {
            path: path.to_path_buf(),
            shares: Mutex::new(shares),
        })
    }

    pub fn get<T>(&self, vault_id: &str, f: impl FnOnce(&VaultShare) -> T) -> Option<T> {
        self.shares.lock().unwrap().get(vault_id).map(f)
    }

    pub fn contains(&self, vault_id: &str) -> bool {
        self.shares.lock().unwrap().contains_key(vault_id)
    }

    /// Insert and persist a fresh share. Errors if the vault already has one
    /// (re-keygen would orphan the existing parent account).
    pub fn insert(&self, vault_id: &str, share: VaultShare) -> Result<()> {
        let mut shares = self.shares.lock().unwrap();
        if shares.contains_key(vault_id) {
            bail!("vault {vault_id} already has a FROST share; refusing to overwrite");
        }
        shares.insert(vault_id.to_string(), share);
        self.persist(&shares)
            .with_context(|| format!("persisting frost shares {}", self.path.display()))
    }

    /// Serialize every share to TOML, write-then-rename for atomicity.
    fn persist(&self, shares: &HashMap<String, VaultShare>) -> Result<()> {
        let mut file = StoreFile::default();
        for (vault_id, share) in shares {
            file.vaults.insert(
                vault_id.clone(),
                ShareRecord {
                    key_package: hex::encode(
                        share
                            .key_package
                            .serialize()
                            .map_err(|e| anyhow!("serializing key_package: {e}"))?,
                    ),
                    public_key_package: hex::encode(
                        share
                            .public_key_package
                            .serialize()
                            .map_err(|e| anyhow!("serializing public_key_package: {e}"))?,
                    ),
                    created_ms: now_ms(),
                },
            );
        }
        let body = toml::to_string_pretty(&file).context("encoding frost shares TOML")?;
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let tmp = self.path.with_extension("toml.tmp");
        std::fs::write(&tmp, body)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

// ------------------------------------------------------------------ sessions

struct KeygenSession {
    round1_secret: dkg::round1::SecretPackage,
    curator_round1: dkg::round1::Package,
    created: Instant,
}

pub struct SignSession {
    pub vault_id: String,
    /// The policy-approved digest; round 2 refuses any other message.
    pub message: [u8; 32],
    pub nonces: frost::round1::SigningNonces,
    /// Audit fields carried from round-1 classification.
    pub kind: crate::policy::bluefin::PayloadKind,
    pub description: String,
    pub is_exit: bool,
    pub tx_digest: Option<String>,
    created: Instant,
}

/// In-memory ceremony state: keygen sessions (keyed by vault), signing
/// sessions (keyed by session id). Same shape as [`crate::state::AppState`]'s
/// per-boot maps: built once, mutated under a lock, nothing persisted except
/// completed keygens (via [`ShareStore`]).
pub struct Ceremonies {
    pub store: ShareStore,
    keygen: Mutex<HashMap<String, KeygenSession>>,
    signing: Mutex<HashMap<String, SignSession>>,
}

impl Ceremonies {
    pub fn new(store: ShareStore) -> Self {
        Self {
            store,
            keygen: Mutex::new(HashMap::new()),
            signing: Mutex::new(HashMap::new()),
        }
    }

    /// Service side of DKG round 1: run part1, remember the curator's
    /// round-1 package, return ours (serialized).
    pub fn keygen_round1(&self, vault_id: &str, curator_round1: &[u8]) -> Result<Vec<u8>> {
        if self.store.contains(vault_id) {
            bail!("vault {vault_id} already has a FROST share; refusing re-keygen");
        }
        let curator_round1 = dkg::round1::Package::deserialize(curator_round1)
            .map_err(|e| anyhow!("curator round1 package does not deserialize: {e}"))?;
        let (round1_secret, round1_package) = dkg::part1(service_id(), 2, 2, OsRng)
            .map_err(|e| anyhow!("dkg part1: {e}"))?;
        let out = round1_package
            .serialize()
            .map_err(|e| anyhow!("serializing service round1 package: {e}"))?;
        let mut keygen = self.keygen.lock().unwrap();
        keygen.retain(|_, s| s.created.elapsed() < KEYGEN_TTL);
        keygen.insert(
            vault_id.to_string(),
            KeygenSession {
                round1_secret,
                curator_round1,
                created: Instant::now(),
            },
        );
        Ok(out)
    }

    /// Service side of DKG round 2: part2 (produce our round-2 package for
    /// the curator) then part3 (finalize + persist the share). Returns
    /// (service round-2 package, group pubkey hex, group Sui address).
    pub fn keygen_round2(
        &self,
        vault_id: &str,
        curator_round2: &[u8],
    ) -> Result<(Vec<u8>, String, SuiAddress)> {
        let session = {
            let mut keygen = self.keygen.lock().unwrap();
            keygen
                .remove(vault_id)
                .ok_or_else(|| anyhow!("no keygen round1 session for vault {vault_id}"))?
        };
        if session.created.elapsed() >= KEYGEN_TTL {
            bail!("keygen session for vault {vault_id} expired; restart at round1");
        }
        let curator_round2 = dkg::round2::Package::deserialize(curator_round2)
            .map_err(|e| anyhow!("curator round2 package does not deserialize: {e}"))?;

        let round1_packages = BTreeMap::from([(curator_id(), session.curator_round1)]);
        let (round2_secret, round2_packages) =
            dkg::part2(session.round1_secret, &round1_packages)
                .map_err(|e| anyhow!("dkg part2: {e}"))?;
        let service_round2 = round2_packages
            .get(&curator_id())
            .ok_or_else(|| anyhow!("dkg part2 produced no package for the curator"))?
            .serialize()
            .map_err(|e| anyhow!("serializing service round2 package: {e}"))?;

        let round2_received = BTreeMap::from([(curator_id(), curator_round2)]);
        let (key_package, public_key_package) =
            dkg::part3(&round2_secret, &round1_packages, &round2_received)
                .map_err(|e| anyhow!("dkg part3: {e}"))?;

        let address = group_sui_address(&public_key_package)?;
        let share = VaultShare {
            key_package,
            public_key_package,
        };
        let pubkey_hex = share.group_public_key_hex()?;
        self.store.insert(vault_id, share)?;
        Ok((service_round2, pubkey_hex, address))
    }

    /// Signing round 1 for an already-policy-approved payload: generate
    /// nonces + commitments, stash the session. Returns
    /// (session_id, serialized commitments).
    pub fn sign_round1(
        &self,
        vault_id: &str,
        approved: crate::policy::bluefin::ApprovedPayload,
    ) -> Result<(String, Vec<u8>)> {
        let share = self
            .store
            .get(vault_id, |s| *s.key_package.signing_share())
            .ok_or_else(|| anyhow!("vault {vault_id} has no FROST share"))?;
        let (nonces, commitments) = frost::round1::commit(&share, &mut OsRng);
        let out = commitments
            .serialize()
            .map_err(|e| anyhow!("serializing signing commitments: {e}"))?;
        let session = SignSession {
            vault_id: vault_id.to_string(),
            message: approved.message,
            nonces,
            kind: approved.kind,
            description: approved.description,
            is_exit: approved.is_exit,
            tx_digest: approved.tx_digest,
            created: Instant::now(),
        };
        let id = uuid::Uuid::new_v4().to_string();
        let mut signing = self.signing.lock().unwrap();
        signing.retain(|_, s| s.created.elapsed() < SIGN_TTL);
        signing.insert(id.clone(), session);
        Ok((id, out))
    }

    /// Take (and consume) a signing session. Sessions are single-use: a
    /// mismatching round 2 burns the session and the nonces with it.
    pub fn take_sign_session(&self, session_id: &str) -> Option<SignSession> {
        let mut signing = self.signing.lock().unwrap();
        signing.retain(|_, s| s.created.elapsed() < SIGN_TTL);
        signing.remove(session_id)
    }

    /// Signing round 2: verify the curator-built `SigningPackage` carries
    /// exactly the policy-approved message, then produce our share.
    pub fn sign_round2(
        &self,
        session: &SignSession,
        signing_package: &[u8],
    ) -> Result<Vec<u8>> {
        let package = frost::SigningPackage::deserialize(signing_package)
            .map_err(|e| anyhow!("SigningPackage does not deserialize: {e}"))?;
        if package.message().as_slice() != &session.message[..] {
            bail!(
                "SigningPackage message {} is not the policy-approved digest {}",
                hex::encode(package.message()),
                hex::encode(session.message)
            );
        }
        let share = self
            .store
            .get(&session.vault_id, |s| s.key_package.clone())
            .ok_or_else(|| anyhow!("vault {} has no FROST share", session.vault_id))?;
        let signature_share = frost::round2::sign(&package, &session.nonces, &share)
            .map_err(|e| anyhow!("frost round2 sign: {e}"))?;
        Ok(signature_share.serialize())
    }
}
