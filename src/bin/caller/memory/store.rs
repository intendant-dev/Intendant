//! The P1.5 Gate-B-lite custody adapter: a production durable store
//! for the Memory plane on the primary OS (macOS), plus the crash
//! battery that is its terminating acceptance.
//!
//! **The write bar holds — this module is a de-risking ARTIFACT.**
//! Nothing here is wired under `MemoryService`/`MemoryHandle`: the
//! live plane stays ephemeral until the ratified sequence completes
//! (custody subset → P0.5 checkpoint replacement → tombed cutover →
//! P1.8 durable writes). The adapter exists so durable writes, when
//! they unlock, land on proven machinery in the spec's own format.
//!
//! On-disk shape = spec §6.2 verbatim (owner ruling D-35):
//!
//! - `ctrl.iplog`   — `IPLOG2` header (kind 1) ‖ one `0x01` frame
//!   carrying the plaintext genesis ceremony operation.
//! - `tenant.iplog` — `IPLOG2` header (kind 0) ‖ `0x11` ITEM_COMMIT
//!   frames. **Plaintext tenant operations never touch disk** (§6.3):
//!   each op seals via the vendored §5.1 pipeline (`seal_item` under a
//!   fresh DEK; DEK wrapped under `wrap_key(KEK, item_addr)`) into the
//!   registered `Itemcommit` shape.
//! - `custody.v1.json` — the custody sidecar (0600): the ceremony's
//!   secrets ([`PlaneCustody`]) + its random identifiers
//!   ([`PlaneResume`]). File custody follows the daemon's shipped
//!   posture (the `access-certs` precedent); OS-keystore custody
//!   (Keychain) is a distinguishable production concern the asset's
//!   lane plan leaves at full Gate B.
//! - `plane.lock` — §6.2 L3: ONE exclusive advisory lock per store;
//!   losers get the named `lock-denied`.
//!
//! Durability = §6.2 L1: an append is ACKED only after the frame's
//! bytes are flushed (`sync_all`, fail-pointed for the battery); a
//! flush failure surfaces the named `storage-io` and FREEZES the
//! writer; recovery truncates a torn tail and durably flushes the
//! truncation BEFORE any new append; a complete final frame with a
//! bad CRC — or any mid-log corruption — QUARANTINES the store
//! read-only with the named `log-corrupt` (never silent truncation).
//! Recovery semantics ride the STAMPED walker
//! (`owner_plane_reducer::edge::walk`) — never a third
//! implementation — and admission of the recovered set rides the
//! stamped fold via [`EphemeralPlane::resume`].

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write as IoWrite};
use std::path::{Path, PathBuf};

use owner_plane_core::cbor;
use owner_plane_core::keyschedule::{item_addr, open_item, seal_item, unwrap_dek, wrap_dek};
use owner_plane_core::shapes::journal::{Itemcommit, Itemcore, Itemwrap};
use owner_plane_core::shapes::ToValue;
use owner_plane_reducer::edge::{walk, HEADER_LEN};
use owner_plane_reducer::envelope::parse_op;

use super::plane::{EphemeralPlane, PlaneCustody, PlaneResume};
use super::types::MemoryError;

const SYNC: &[u8; 4] = b"IPLR";
const FRAME_CTRL_OP: u8 = 0x01;
const FRAME_ITEM_COMMIT: u8 = 0x11;

/// Named store outcomes (§6.2 vocabulary; §C.2 discipline — a denial
/// is a named outcome, never a silent proceed or downgrade).
#[derive(Debug, thiserror::Error)]
pub(crate) enum StoreError {
    /// §6.2 L3: another live handle holds the plane store.
    #[error("rejected: lock-denied (another process holds this plane store)")]
    LockDenied,
    /// D-35 quarantine: corruption evidence in `{file}` — the store is
    /// read-only until rebuilt; never silently truncated.
    #[error("rejected: log-corrupt ({file}: {why}) — store quarantined read-only")]
    LogCorrupt { file: &'static str, why: String },
    /// §6.2 L1: flush/write failure — the writer froze; no partial
    /// frame was exposed as committed.
    #[error("rejected: storage-io ({0}) — writer frozen")]
    StorageIo(String),
    #[error("custody sidecar: {0}")]
    Custody(String),
    /// Recovered-set admission failed under the stamped fold.
    #[error("recovered set: {0}")]
    Recovery(#[from] MemoryError),
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex_exact<const N: usize>(s: &str, what: &'static str) -> Result<[u8; N], StoreError> {
    let bytes = (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2).unwrap_or("zz"), 16))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| StoreError::Custody(format!("{what}: {e}")))?;
    bytes
        .as_slice()
        .try_into()
        .map_err(|_| StoreError::Custody(format!("{what}: wrong length")))
}

/// CRC32C (Castagnoli, RFC 3720 convention) — the §6.2 trailer. The
/// stamped walker validates every frame we write, so a divergence here
/// cannot ship: `frames_walk_back_byte_identical` would go red.
fn crc32c(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0x82F6_3B78 & mask);
        }
    }
    crc ^ 0xFFFF_FFFF
}

fn file_header(kind: u8, plane_id: &[u8; 32], zone_id: &[u8; 16]) -> Vec<u8> {
    let mut h = Vec::with_capacity(HEADER_LEN);
    h.extend_from_slice(b"IPLOG2");
    h.push(2);
    h.push(kind);
    h.extend_from_slice(plane_id);
    h.extend_from_slice(zone_id);
    h
}

fn frame(frame_type: u8, payload: &[u8]) -> Vec<u8> {
    let len = (payload.len() + 1) as u32; // counts type + payload
    let nlen = !len;
    let mut body = Vec::with_capacity(8 + 1 + payload.len());
    body.extend_from_slice(&len.to_le_bytes());
    body.extend_from_slice(&nlen.to_le_bytes());
    body.push(frame_type);
    body.extend_from_slice(payload);
    let crc = crc32c(&body);
    let mut out = Vec::with_capacity(4 + body.len() + 4);
    out.extend_from_slice(SYNC);
    out.extend_from_slice(&body);
    out.extend_from_slice(&crc.to_le_bytes());
    out
}

/// The battery's flush failpoint (the storage lane's own technique):
/// with `INTENDANT_P15_FAIL_SYNC_AFTER=k` set, the k-th flush of this
/// process fails — proving the ACK is COUPLED to the flush (a build
/// that dropped the `sync_all` call would stop failing here and the
/// coupling test goes red).
fn sync_seam(f: &File) -> std::io::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SYNCS: AtomicU64 = AtomicU64::new(0);
    let n = SYNCS.fetch_add(1, Ordering::SeqCst) + 1;
    if let Ok(k) = std::env::var("INTENDANT_P15_FAIL_SYNC_AFTER") {
        if k.parse::<u64>().is_ok_and(|k| n >= k) {
            return Err(std::io::Error::other("failpoint: sync refused"));
        }
    }
    f.sync_all()
}

/// fsync the directory itself (file creation durability, §6.2 L1 —
/// Unix semantics; Windows custody is full-Gate-B and out of scope).
fn sync_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        File::open(dir)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

fn durable_write(path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    let tmp = path.with_extension("tmp");
    let io = |e: std::io::Error| StoreError::StorageIo(e.to_string());
    let mut f = File::create(&tmp).map_err(io)?;
    f.write_all(bytes).map_err(io)?;
    sync_seam(&f).map_err(io)?;
    std::fs::rename(&tmp, path).map_err(io)?;
    sync_dir(path.parent().expect("store files live in the store dir")).map_err(io)?;
    Ok(())
}

#[cfg(unix)]
fn restrict_mode(path: &Path) -> Result<(), StoreError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| StoreError::Custody(format!("chmod 0600: {e}")))
}
#[cfg(not(unix))]
fn restrict_mode(_path: &Path) -> Result<(), StoreError> {
    Ok(())
}

/// The custody sidecar, serialized as hex-field JSON (store-internal
/// v1; secrets tier — 0600, atomic-replace + flush).
#[derive(serde::Serialize, serde::Deserialize)]
struct SidecarV1 {
    v: u32,
    root_seed: String,
    recovery_seed: String,
    sig_seed: String,
    kem_ikm: String,
    kek_epoch1: String,
    plane_id: String,
    zone_id: String,
    home_space: String,
    audit_space: String,
    device_id: String,
    lineage: String,
    evidence_hash: String,
    revocation_id: String,
    grant_id: String,
    genesis_hash: String,
}

impl SidecarV1 {
    fn from_parts(r: &PlaneResume) -> SidecarV1 {
        SidecarV1 {
            v: 1,
            root_seed: hex(&r.custody.root_seed),
            recovery_seed: hex(&r.custody.recovery_seed),
            sig_seed: hex(&r.custody.sig_seed),
            kem_ikm: hex(&r.custody.kem_ikm),
            kek_epoch1: hex(&r.custody.kek_epoch1),
            plane_id: hex(&r.plane_id),
            zone_id: hex(&r.zone_id),
            home_space: hex(&r.home_space),
            audit_space: hex(&r.audit_space),
            device_id: hex(&r.device_id),
            lineage: hex(&r.lineage),
            evidence_hash: hex(&r.evidence_hash),
            revocation_id: hex(&r.revocation_id),
            grant_id: hex(&r.grant_id),
            genesis_hash: hex(&r.genesis_hash),
        }
    }

    fn into_parts(self) -> Result<PlaneResume, StoreError> {
        Ok(PlaneResume {
            custody: PlaneCustody {
                root_seed: unhex_exact(&self.root_seed, "root_seed")?,
                recovery_seed: unhex_exact(&self.recovery_seed, "recovery_seed")?,
                sig_seed: unhex_exact(&self.sig_seed, "sig_seed")?,
                kem_ikm: unhex_exact(&self.kem_ikm, "kem_ikm")?,
                kek_epoch1: unhex_exact(&self.kek_epoch1, "kek_epoch1")?,
            },
            plane_id: unhex_exact(&self.plane_id, "plane_id")?,
            zone_id: unhex_exact(&self.zone_id, "zone_id")?,
            home_space: unhex_exact(&self.home_space, "home_space")?,
            audit_space: unhex_exact(&self.audit_space, "audit_space")?,
            device_id: unhex_exact(&self.device_id, "device_id")?,
            lineage: unhex_exact(&self.lineage, "lineage")?,
            evidence_hash: unhex_exact(&self.evidence_hash, "evidence_hash")?,
            revocation_id: unhex_exact(&self.revocation_id, "revocation_id")?,
            grant_id: unhex_exact(&self.grant_id, "grant_id")?,
            genesis_hash: unhex_exact(&self.genesis_hash, "genesis_hash")?,
        })
    }
}

/// The durable plane store: lock + logs + custody. Composed with the
/// writable plane by [`DurablePlane`].
pub(crate) struct DurableStore {
    dir: PathBuf,
    /// Held for the process lifetime — dropping releases the L3 lock.
    _lock: File,
    tenant: File,
    resume: PlaneResume,
    /// §6.2 L1: a flush failure freezes the writer permanently (this
    /// handle); reopening re-runs recovery and decides afresh.
    frozen: bool,
}

impl DurableStore {
    fn lock_store(dir: &Path) -> Result<File, StoreError> {
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(dir.join("plane.lock"))
            .map_err(|e| StoreError::StorageIo(e.to_string()))?;
        match lock.try_lock() {
            Ok(()) => Ok(lock),
            Err(std::fs::TryLockError::WouldBlock) => Err(StoreError::LockDenied),
            Err(std::fs::TryLockError::Error(e)) => Err(StoreError::StorageIo(e.to_string())),
        }
    }

    /// Create a fresh store from a just-minted ceremony. Layout note:
    /// logs and sidecar are written and flushed BEFORE the caller ever
    /// acks anything.
    fn create(
        dir: &Path,
        resume: PlaneResume,
        genesis_bytes: &[u8],
    ) -> Result<DurableStore, StoreError> {
        std::fs::create_dir_all(dir).map_err(|e| StoreError::StorageIo(e.to_string()))?;
        let lock = Self::lock_store(dir)?;

        let sidecar_path = dir.join("custody.v1.json");
        let sidecar = serde_json::to_vec_pretty(&SidecarV1::from_parts(&resume))
            .map_err(|e| StoreError::Custody(e.to_string()))?;
        durable_write(&sidecar_path, &sidecar)?;
        restrict_mode(&sidecar_path)?;

        let mut ctrl = file_header(1, &resume.plane_id, &resume.zone_id);
        ctrl.extend_from_slice(&frame(FRAME_CTRL_OP, genesis_bytes));
        durable_write(&dir.join("ctrl.iplog"), &ctrl)?;

        let tenant_path = dir.join("tenant.iplog");
        durable_write(
            &tenant_path,
            &file_header(0, &resume.plane_id, &resume.zone_id),
        )?;
        let tenant = OpenOptions::new()
            .append(true)
            .open(&tenant_path)
            .map_err(|e| StoreError::StorageIo(e.to_string()))?;

        Ok(DurableStore {
            dir: dir.to_path_buf(),
            _lock: lock,
            tenant,
            resume,
            frozen: false,
        })
    }

    fn read_log(dir: &Path, name: &'static str) -> Result<Vec<u8>, StoreError> {
        let mut bytes = Vec::new();
        File::open(dir.join(name))
            .and_then(|mut f| f.read_to_end(&mut bytes))
            .map_err(|e| StoreError::StorageIo(format!("{name}: {e}")))?;
        Ok(bytes)
    }

    /// Walk one log per D-35: corruption quarantines; a torn tail is
    /// truncated and the truncation flushed BEFORE anything appends.
    fn recover_log(
        dir: &Path,
        name: &'static str,
        expect_plane: &[u8; 32],
    ) -> Result<Vec<Vec<u8>>, StoreError> {
        let stream = Self::read_log(dir, name)?;
        let Some((frames, durable_end)) = walk(&stream) else {
            return Err(StoreError::LogCorrupt {
                file: name,
                why: "corruption evidence in the frame walk".into(),
            });
        };
        if stream[8..40] != expect_plane[..] {
            return Err(StoreError::LogCorrupt {
                file: name,
                why: "header names a different plane".into(),
            });
        }
        if durable_end < stream.len() {
            // Torn tail: truncate to the durable prefix, flush the
            // truncation, only then is the store appendable.
            let f = OpenOptions::new()
                .write(true)
                .open(dir.join(name))
                .map_err(|e| StoreError::StorageIo(e.to_string()))?;
            f.set_len(durable_end as u64)
                .map_err(|e| StoreError::StorageIo(e.to_string()))?;
            sync_seam(&f).map_err(|e| StoreError::StorageIo(e.to_string()))?;
        }
        Ok(frames
            .into_iter()
            .map(|(a, b)| stream[a..b].to_vec())
            .collect())
    }

    /// Decode one §6.2 frame we walked: `(type, payload)`.
    fn split_frame(frame: &[u8]) -> (u8, &[u8]) {
        (frame[12], &frame[13..frame.len() - 4])
    }

    fn append_frame(&mut self, frame_type: u8, payload: &[u8]) -> Result<(), StoreError> {
        if self.frozen {
            return Err(StoreError::StorageIo(
                "writer frozen by an earlier flush failure".into(),
            ));
        }
        let bytes = frame(frame_type, payload);
        let outcome = self
            .tenant
            .write_all(&bytes)
            .and_then(|()| sync_seam(&self.tenant));
        if let Err(e) = outcome {
            self.frozen = true;
            return Err(StoreError::StorageIo(e.to_string()));
        }
        Ok(())
    }
}

/// What [`DurableStore::open`] recovers: the store handle plus the
/// custody/identity record and the decrypted op set, ready for
/// [`EphemeralPlane::resume`].
pub(crate) struct RecoveredStore {
    pub store: DurableStore,
    pub resume: PlaneResume,
    pub items: BTreeMap<String, Vec<u8>>,
}

impl DurableStore {
    /// Create a fresh durable store from a just-minted ceremony
    /// (P1.8: called by `MemoryService::new_durable`).
    pub(crate) fn create_from_ceremony(
        dir: &Path,
        plane: &EphemeralPlane,
        custody: PlaneCustody,
    ) -> Result<DurableStore, StoreError> {
        let genesis_bytes = plane
            .held_items()
            .values()
            .next()
            .expect("a bootstrapped plane holds its genesis")
            .clone();
        let genesis_hash = parse_op(&genesis_bytes)
            .map_err(|e| StoreError::Custody(format!("genesis re-parse: {e:?}")))?
            .op_hash();
        let resume = PlaneResume {
            custody,
            plane_id: plane.plane_id,
            zone_id: plane.zone_id,
            home_space: plane.home_space,
            audit_space: plane.audit_space,
            device_id: plane.dev.device_id,
            lineage: plane.dev.lineage,
            evidence_hash: plane.dev.cert.evidence_hash,
            revocation_id: plane.dev.cert.revocation_id,
            grant_id: plane.grant.grant_id,
            genesis_hash,
        };
        DurableStore::create(dir, resume, &genesis_bytes)
    }

    /// Reopen after a restart or crash: sidecar → recover both logs
    /// (stamped walker) → open every item (§5.1/§5.3 inverses). The
    /// caller folds the recovered set via [`EphemeralPlane::resume`].
    pub(crate) fn open(dir: &Path) -> Result<RecoveredStore, StoreError> {
        let lock = DurableStore::lock_store(dir)?;
        let sidecar_bytes = std::fs::read(dir.join("custody.v1.json"))
            .map_err(|e| StoreError::Custody(format!("sidecar: {e}")))?;
        let sidecar: SidecarV1 = serde_json::from_slice(&sidecar_bytes)
            .map_err(|e| StoreError::Custody(format!("sidecar: {e}")))?;
        if sidecar.v != 1 {
            return Err(StoreError::Custody(format!(
                "sidecar shape v{} is newer than this build",
                sidecar.v
            )));
        }
        let resume = sidecar.into_parts()?;

        let ctrl_frames = DurableStore::recover_log(dir, "ctrl.iplog", &resume.plane_id)?;
        let mut items: BTreeMap<String, Vec<u8>> = BTreeMap::new();
        let mut n = 0u32;
        let mut push = |items: &mut BTreeMap<String, Vec<u8>>, bytes: Vec<u8>| {
            n += 1;
            items.insert(format!("op-{n:06}"), bytes);
        };
        for f in &ctrl_frames {
            let (ty, payload) = DurableStore::split_frame(f);
            if ty != FRAME_CTRL_OP {
                return Err(StoreError::LogCorrupt {
                    file: "ctrl.iplog",
                    why: format!("unexpected frame type 0x{ty:02x}"),
                });
            }
            push(&mut items, payload.to_vec());
        }
        let genesis_ok = ctrl_frames.len() == 1
            && parse_op(&items["op-000001"]).is_ok_and(|op| op.op_hash() == resume.genesis_hash);
        if !genesis_ok {
            return Err(StoreError::LogCorrupt {
                file: "ctrl.iplog",
                why: "control log does not hold exactly the ceremony genesis".into(),
            });
        }

        for f in DurableStore::recover_log(dir, "tenant.iplog", &resume.plane_id)? {
            let (ty, payload) = DurableStore::split_frame(&f);
            if ty != FRAME_ITEM_COMMIT {
                return Err(StoreError::LogCorrupt {
                    file: "tenant.iplog",
                    why: format!("unexpected frame type 0x{ty:02x}"),
                });
            }
            let op_bytes = open_item_frame(payload, &resume)?;
            push(&mut items, op_bytes);
        }

        let tenant = OpenOptions::new()
            .append(true)
            .open(dir.join("tenant.iplog"))
            .map_err(|e| StoreError::StorageIo(e.to_string()))?;
        Ok(RecoveredStore {
            store: DurableStore {
                dir: dir.to_path_buf(),
                _lock: lock,
                tenant,
                resume,
                frozen: false,
            },
            resume: DurableStore::reread_resume(dir)?,
            items,
        })
    }

    fn reread_resume(dir: &Path) -> Result<PlaneResume, StoreError> {
        let sidecar_bytes = std::fs::read(dir.join("custody.v1.json"))
            .map_err(|e| StoreError::Custody(format!("sidecar: {e}")))?;
        let sidecar: SidecarV1 = serde_json::from_slice(&sidecar_bytes)
            .map_err(|e| StoreError::Custody(format!("sidecar: {e}")))?;
        sidecar.into_parts()
    }

    /// Seal + append one admitted op (§6.2 L1: the caller acks only
    /// on Ok). P1.8's service path.
    pub(crate) fn append_sealed_op(&mut self, op_bytes: &[u8]) -> Result<(), StoreError> {
        let resume = &self.resume;
        let payload = seal_item_frame(op_bytes, resume)?;
        self.append_frame(FRAME_ITEM_COMMIT, &payload)
    }

    pub(crate) fn is_frozen(&self) -> bool {
        self.frozen
    }
}

/// The composed durable plane — the crash battery's harness shape
/// (production wiring composes the same pieces inside
/// `MemoryService`).
pub(crate) struct DurablePlane {
    pub plane: EphemeralPlane,
    store: DurableStore,
}

impl DurablePlane {
    pub(crate) fn create(dir: &Path) -> Result<DurablePlane, StoreError> {
        let (plane, custody) = EphemeralPlane::bootstrap_with_custody()?;
        let store = DurableStore::create_from_ceremony(dir, &plane, custody)?;
        Ok(DurablePlane { plane, store })
    }

    pub(crate) fn open(dir: &Path) -> Result<DurablePlane, StoreError> {
        let recovered = DurableStore::open(dir)?;
        let plane = EphemeralPlane::resume(&recovered.resume, recovered.items)?;
        Ok(DurablePlane {
            plane,
            store: recovered.store,
        })
    }

    pub(crate) fn open_or_create(dir: &Path) -> Result<DurablePlane, StoreError> {
        if dir.join("custody.v1.json").exists() {
            Self::open(dir)
        } else {
            Self::create(dir)
        }
    }

    /// Mint + admit a Memory claim, then persist it as a sealed item;
    /// ACKED only after the frame is flushed (§6.2 L1). On a
    /// persistence failure the in-memory admission is retracted so
    /// nothing unpersisted is ever exposed, and the writer freezes.
    pub(crate) fn append_claim_op(&mut self, statement: &str) -> Result<[u8; 32], StoreError> {
        use owner_plane_core::shapes::envelope::ActorKind;
        use owner_plane_core::shapes::memory::Mclaim;
        use owner_plane_core::shapes::{Class, Kind};
        if self.store.is_frozen() {
            return Err(StoreError::StorageIo(
                "writer frozen by an earlier flush failure".into(),
            ));
        }
        let body = Mclaim {
            kind: Kind::Observation,
            statement: statement.to_string(),
            sensitivity: Class::Private,
            observed_at_ms: Some(1),
            valid_from_ms: None,
            valid_until_ms: None,
            expires_at_ms: None,
            session: None,
            project: None,
            model: None,
            evidence: vec![],
            supersedes: None,
            labels: None,
        };
        let op_hash =
            self.plane
                .tenant_op(ActorKind::Daemon, None, Mclaim::OP_TYPE, body.to_value())?;
        let op_bytes = self
            .plane
            .held_items()
            .values()
            .last()
            .expect("the op just admitted is held")
            .clone();
        if let Err(e) = self.store.append_sealed_op(&op_bytes) {
            self.plane.retract_unpersisted(&op_hash)?;
            return Err(e);
        }
        Ok(op_hash)
    }

    pub(crate) fn store_dir(&self) -> &Path {
        &self.store.dir
    }
}

/// Seal one op into the registered `Itemcommit` shape (§5.1 DEK seal,
/// §5.3 DEK wrap under the epoch-1 KEK).
fn seal_item_frame(op_bytes: &[u8], r: &PlaneResume) -> Result<Vec<u8>, StoreError> {
    let mut dek = [0u8; 32];
    let mut nonce = [0u8; 12];
    {
        use rand::RngCore;
        rand::rngs::OsRng.fill_bytes(&mut dek);
        rand::rngs::OsRng.fill_bytes(&mut nonce);
    }
    let core = seal_item(&dek, nonce, &r.plane_id, &r.zone_id, op_bytes);
    let addr = item_addr(&core);
    let wrapped_dek = wrap_dek(
        &r.custody.kek_epoch1,
        &r.plane_id,
        &r.zone_id,
        1,
        &addr,
        &dek,
    );
    let op = parse_op(op_bytes).map_err(|e| StoreError::Custody(format!("op re-parse: {e:?}")))?;
    let commit = Itemcommit {
        core,
        wrap: Itemwrap {
            item_addr: addr,
            key_wrap_epoch: 1,
            wrapped_dek,
        },
        lineage: r.lineage,
        gen: 1,
        seq: op.header.writer_sequence,
    };
    cbor::encode(&commit.to_value()).map_err(|e| StoreError::Custody(format!("encode: {e:?}")))
}

/// Inverse: decode an ITEM_COMMIT payload, unwrap its DEK, open the
/// item, verify the address binding. Every failure is `log-corrupt` —
/// the D-35 quarantine class, never a skip.
fn open_item_frame(payload: &[u8], r: &PlaneResume) -> Result<Vec<u8>, StoreError> {
    let corrupt = |why: &str| StoreError::LogCorrupt {
        file: "tenant.iplog",
        why: why.to_string(),
    };
    let node = owner_plane_reducer::cbor::decode(payload)
        .map_err(|_| corrupt("undecodable item frame"))?;
    let core_node = node
        .get("core")
        .ok_or_else(|| corrupt("itemcommit.core missing"))?;
    let nonce: [u8; 12] = core_node
        .get("nonce")
        .and_then(|v| v.as_bytes())
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| corrupt("itemcore.nonce"))?;
    let ct = core_node
        .get("ct")
        .and_then(|v| v.as_bytes())
        .ok_or_else(|| corrupt("itemcore.ct"))?
        .to_vec();
    let wrap_node = node
        .get("wrap")
        .ok_or_else(|| corrupt("itemcommit.wrap missing"))?;
    let stored_addr: [u8; 32] = wrap_node
        .get("item_addr")
        .and_then(|v| v.as_bytes())
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| corrupt("itemwrap.item_addr"))?;
    let wrapped_dek: [u8; 48] = wrap_node
        .get("wrapped_dek")
        .and_then(|v| v.as_bytes())
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| corrupt("itemwrap.wrapped_dek"))?;

    let core = Itemcore { nonce, ct };
    let addr = item_addr(&core);
    if addr != stored_addr {
        return Err(corrupt("item_addr does not bind the item core"));
    }
    let dek = unwrap_dek(
        &r.custody.kek_epoch1,
        &r.plane_id,
        &r.zone_id,
        1,
        &addr,
        &wrapped_dek,
    )
    .ok_or_else(|| corrupt("DEK unwrap failed (wrong KEK or tamper)"))?;
    open_item(&dek, &r.plane_id, &r.zone_id, &core)
        .ok_or_else(|| corrupt("item AEAD open failed (tamper)"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The battery is process-global by nature — child processes of the
    /// TEST BINARY and env-var failpoints — so its tests serialize on
    /// one lock. Under plain `cargo test` (threads in one process, the
    /// CI legs) a concurrently spawned child briefly duplicates the
    /// whole fd table in Linux's fork→exec window; a `plane.lock`
    /// dropped by another test thread in that window survives in the
    /// child until exec's CLOEXEC sweep, and that test's reopen sees a
    /// spurious `lock-denied` (live: two different store tests ejected
    /// PR #405's queue entry and flaked a Dell repro loop; macOS never
    /// reproduces — its posix_spawn is a true syscall with no fork
    /// window). Serializing removes the fd-inheritance overlap; nextest
    /// (process-per-test) was never exposed.
    static BATTERY: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn battery_guard() -> std::sync::MutexGuard<'static, ()> {
        match BATTERY.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn acks_path(dir: &Path) -> PathBuf {
        dir.join("acks.log")
    }

    /// Child-process work loop for the crash battery: append claims,
    /// journal each ACK to `acks.log` (append + flush per line — the
    /// parent's ground truth), and die at the scripted point. A no-op
    /// unless the battery env is set; `#[ignore]` keeps it out of
    /// normal runs — parents invoke it by exact name.
    #[test]
    #[ignore]
    fn crash_child_entrypoint() {
        let Ok(dir) = std::env::var("INTENDANT_P15_CHILD_DIR") else {
            return;
        };
        let dir = PathBuf::from(dir);
        let total: u64 = std::env::var("INTENDANT_P15_TOTAL")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(4);
        let abort_at: Option<u64> = std::env::var("INTENDANT_P15_ABORT_AT")
            .ok()
            .and_then(|v| v.parse().ok());
        let mut plane = DurablePlane::open_or_create(&dir).expect("child open_or_create");
        let mut acks = OpenOptions::new()
            .create(true)
            .append(true)
            .open(acks_path(&dir))
            .expect("acks journal");
        for i in 0..total {
            match plane.append_claim_op(&format!("crash battery claim {i}")) {
                Ok(hash) => {
                    // Journal the ack durably BEFORE anything else —
                    // the ground truth must never claim more than the
                    // store acked.
                    acks.write_all(format!("{}\n", hex(&hash)).as_bytes())
                        .and_then(|()| acks.sync_all())
                        .expect("journal the ack");
                }
                Err(e @ StoreError::StorageIo(_)) => {
                    // The flush failpoint (or real IO trouble): record
                    // that the coupling fired and stop — the writer is
                    // frozen by contract.
                    std::fs::write(dir.join("failpoint-hit"), e.to_string())
                        .expect("failpoint marker");
                    return;
                }
                Err(other) => panic!("unexpected append error: {other}"),
            }
            if abort_at == Some(i + 1) {
                // SIGABRT with no unwinding, no drops, no flush of
                // anything the store didn't already flush itself.
                std::process::abort();
            }
        }
    }

    fn spawn_child(
        dir: &Path,
        total: u64,
        abort_at: Option<u64>,
        fail_sync_after: Option<u64>,
    ) -> std::process::Child {
        let exe = std::env::current_exe().expect("test binary path");
        let mut cmd = std::process::Command::new(exe);
        cmd.args([
            "memory::store::tests::crash_child_entrypoint",
            "--exact",
            "--ignored",
            "--nocapture",
        ])
        .env("INTENDANT_P15_CHILD_DIR", dir)
        .env("INTENDANT_P15_TOTAL", total.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
        if let Some(n) = abort_at {
            cmd.env("INTENDANT_P15_ABORT_AT", n.to_string());
        }
        if let Some(k) = fail_sync_after {
            cmd.env("INTENDANT_P15_FAIL_SYNC_AFTER", k.to_string());
        }
        cmd.spawn().expect("spawn crash child")
    }

    fn acked_hashes(dir: &Path) -> Vec<String> {
        std::fs::read_to_string(acks_path(dir))
            .unwrap_or_default()
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect()
    }

    fn held_op_hashes(plane: &DurablePlane) -> Vec<String> {
        plane
            .plane
            .held_items()
            .values()
            .filter_map(|raw| parse_op(raw).ok().map(|op| hex(&op.op_hash())))
            .collect()
    }

    /// Round-trip + chain resumption: everything acked before a clean
    /// shutdown is present after reopen, byte-identical under the
    /// stamped fold, and the writer chain CONTINUES (post-reopen
    /// appends admit — the rebuilt cert/grant hash back to the
    /// ceremony's citations or the fold would reject them).
    #[test]
    fn durable_roundtrip_and_chain_resume() {
        let _battery = battery_guard();
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("plane");
        let mut acked = Vec::new();
        {
            let mut plane = DurablePlane::create(&dir).unwrap();
            for i in 0..3 {
                acked.push(hex(&plane.append_claim_op(&format!("claim {i}")).unwrap()));
            }
        }
        let mut plane = DurablePlane::open(&dir).unwrap();
        let held = held_op_hashes(&plane);
        for hash in &acked {
            assert!(held.contains(hash), "acked claim {hash} lost across reopen");
        }
        // The chain resumes: two more appends admit under the fold.
        acked.push(hex(&plane.append_claim_op("post-reopen claim A").unwrap()));
        acked.push(hex(&plane.append_claim_op("post-reopen claim B").unwrap()));
        drop(plane);
        let plane = DurablePlane::open(&dir).unwrap();
        let held = held_op_hashes(&plane);
        for hash in &acked {
            assert!(held.contains(hash), "claim {hash} lost after second reopen");
        }
        assert_eq!(
            plane.plane.held_items().len(),
            1 + 5,
            "genesis + five claims"
        );
    }

    /// The core crash matrix: a child killed by SIGABRT at every
    /// append position leaves a store whose reopen holds EVERY acked
    /// op and still accepts new appends. (§6.2 L1: acked ⊆ recovered;
    /// an unacked trailing frame may legitimately survive — the
    /// invariant is no acked loss, plus recovered-set admission.)
    #[test]
    fn crash_battery_kill_at_every_point() {
        let _battery = battery_guard();
        for abort_at in 1..=4u64 {
            let tmp = tempfile::tempdir().unwrap();
            let dir = tmp.path().join("plane");
            let status = spawn_child(&dir, 4, Some(abort_at), None)
                .wait()
                .expect("child exit");
            assert!(!status.success(), "the child must die by abort");
            let acked = acked_hashes(&dir);
            assert_eq!(
                acked.len() as u64,
                abort_at,
                "child acked up to the abort point"
            );

            let mut plane = DurablePlane::open(&dir)
                .unwrap_or_else(|e| panic!("reopen after abort@{abort_at}: {e}"));
            let held = held_op_hashes(&plane);
            for hash in &acked {
                assert!(
                    held.contains(hash),
                    "abort@{abort_at}: acked claim {hash} lost"
                );
            }
            plane
                .append_claim_op("post-crash append")
                .expect("the recovered store accepts new appends");
        }
    }

    /// The timing-nondeterministic torture: SIGKILL mid-run, wherever
    /// the child happens to be — including possibly mid-write. Reopen
    /// must hold every journaled ack; a torn trailing frame truncates.
    #[test]
    fn crash_battery_sigkill_mid_run() {
        let _battery = battery_guard();
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("plane");
        let mut child = spawn_child(&dir, 200, None, None);
        // Kill only once the child demonstrably runs (store created and
        // at least one ack journaled) — a fixed sleep raced slow starts
        // under full-suite load and killed before creation.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while std::time::Instant::now() < deadline {
            if acks_path(&dir).exists() && !acked_hashes(&dir).is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        std::thread::sleep(std::time::Duration::from_millis(60));
        child.kill().expect("SIGKILL the child");
        let _ = child.wait();

        let acked = acked_hashes(&dir);
        let mut plane = DurablePlane::open(&dir).expect("reopen after SIGKILL");
        let held = held_op_hashes(&plane);
        for hash in &acked {
            assert!(held.contains(hash), "SIGKILL: acked claim {hash} lost");
        }
        plane
            .append_claim_op("post-sigkill append")
            .expect("the recovered store accepts new appends");
    }

    /// The flush-coupling proof (the storage lane's R6 technique): a
    /// failing flush means the claim is NOT acked, the writer freezes
    /// with the named `storage-io`, and reopen agrees the claim never
    /// happened. The failpoint marker doubles as the proof that the
    /// append path actually CALLS the sync seam — deleting the
    /// `sync_all` call site turns this red.
    #[test]
    fn ack_is_coupled_to_flush() {
        let _battery = battery_guard();
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("plane");
        {
            let mut plane = DurablePlane::create(&dir).unwrap();
            plane.append_claim_op("pre-failpoint claim").unwrap();
        }
        // Child opens (no create-time syncs) and appends under a
        // first-flush failpoint: the very first append must freeze.
        let status = spawn_child(&dir, 3, None, Some(1))
            .wait()
            .expect("child exit");
        assert!(status.success(), "the frozen child exits cleanly");
        assert!(
            dir.join("failpoint-hit").exists(),
            "the sync failpoint never fired — the append path lost its flush"
        );
        let marker = std::fs::read_to_string(dir.join("failpoint-hit")).unwrap();
        assert!(marker.contains("storage-io"), "named outcome: {marker}");
        assert_eq!(
            acked_hashes(&dir).len(),
            0,
            "nothing may be acked past a failed flush (child journal)"
        );
        // The refused-flush frame was WRITTEN but never acked: with no
        // OS crash the page cache may still land it, so reopen may hold
        // 2 ops (genesis + the acked claim) or 3 — both legal. The L1
        // contract is acked ⊆ recovered and no ack past a failed flush,
        // never that unacked bytes must vanish.
        let plane = DurablePlane::open(&dir).expect("reopen after freeze");
        let held = plane.plane.held_items().len();
        assert!(
            (2..=3).contains(&held),
            "genesis + acked claim (+ at most the unacked frame), got {held}"
        );
    }

    /// D-35 recovery: a torn trailing frame (EOF inside the frame)
    /// truncates cleanly — the durable prefix survives and the store
    /// stays writable.
    #[test]
    fn torn_tail_truncates_and_recovers() {
        let _battery = battery_guard();
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("plane");
        {
            let mut plane = DurablePlane::create(&dir).unwrap();
            plane.append_claim_op("claim one").unwrap();
            plane.append_claim_op("claim two").unwrap();
        }
        let log = dir.join("tenant.iplog");
        let clean_len = std::fs::metadata(&log).unwrap().len();
        let mut f = OpenOptions::new().append(true).open(&log).unwrap();
        f.write_all(b"IPLR\x99\x00").unwrap(); // a torn frame prefix
        f.sync_all().unwrap();
        drop(f);

        let mut plane = DurablePlane::open(&dir).expect("torn tail recovers");
        assert_eq!(plane.plane.held_items().len(), 3, "genesis + both claims");
        assert_eq!(
            std::fs::metadata(&log).unwrap().len(),
            clean_len,
            "the torn tail was truncated back to the durable prefix"
        );
        plane.append_claim_op("post-truncation append").unwrap();
    }

    /// D-35 quarantine: a COMPLETE final frame with a bad CRC is
    /// ambiguous (torn vs corrupted committed data) — the store
    /// refuses with the named `log-corrupt`, never silent truncation.
    #[test]
    fn corrupt_final_frame_quarantines() {
        let _battery = battery_guard();
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("plane");
        {
            let mut plane = DurablePlane::create(&dir).unwrap();
            plane.append_claim_op("claim one").unwrap();
            plane.append_claim_op("claim two").unwrap();
        }
        let log = dir.join("tenant.iplog");
        let mut bytes = std::fs::read(&log).unwrap();
        let mid_last_frame = bytes.len() - 24;
        bytes[mid_last_frame] ^= 0x01;
        std::fs::write(&log, &bytes).unwrap();

        match DurablePlane::open(&dir).map(|_| "opened") {
            Err(e @ StoreError::LogCorrupt { .. }) => {
                assert!(e.to_string().contains("log-corrupt"), "named outcome: {e}");
            }
            other => panic!("expected the log-corrupt quarantine, got {other:?}"),
        }
    }

    /// D-35: a bad frame FOLLOWED by valid frames is media corruption —
    /// quarantine, never a resync-and-skip.
    #[test]
    fn midlog_corruption_quarantines() {
        let _battery = battery_guard();
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("plane");
        {
            let mut plane = DurablePlane::create(&dir).unwrap();
            plane.append_claim_op("claim one").unwrap();
            plane.append_claim_op("claim two").unwrap();
        }
        let log = dir.join("tenant.iplog");
        let mut bytes = std::fs::read(&log).unwrap();
        let inside_first_frame = HEADER_LEN + 20;
        bytes[inside_first_frame] ^= 0x01;
        std::fs::write(&log, &bytes).unwrap();

        match DurablePlane::open(&dir).map(|_| "opened") {
            Err(StoreError::LogCorrupt { .. }) => {}
            other => panic!("expected the log-corrupt quarantine, got {other:?}"),
        }
    }

    /// §6.2 L3: one exclusive lock per plane store; a second open gets
    /// the named `lock-denied` while the first handle lives.
    #[test]
    fn second_open_is_lock_denied() {
        let _battery = battery_guard();
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("plane");
        let plane = DurablePlane::create(&dir).unwrap();
        match DurablePlane::open(&dir).map(|_| "opened") {
            Err(e @ StoreError::LockDenied) => {
                assert!(e.to_string().contains("lock-denied"));
            }
            other => panic!("expected lock-denied, got {other:?}"),
        }
        drop(plane);
        DurablePlane::open(&dir).expect("the lock releases with its handle");
    }

    /// The custody sidecar carries the plane's secrets: 0600, owner
    /// read/write only (the daemon's shipped file-custody posture).
    #[cfg(unix)]
    #[test]
    fn sidecar_is_owner_only() {
        let _battery = battery_guard();
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("plane");
        let _plane = DurablePlane::create(&dir).unwrap();
        let mode = std::fs::metadata(dir.join("custody.v1.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "custody sidecar must be 0600");
    }

    /// Our frame writer round-trips through the STAMPED walker
    /// byte-identically — the §6.2 encoding is the walker's, not ours.
    #[test]
    fn frames_walk_back_byte_identical() {
        let _battery = battery_guard();
        let plane_id = [7u8; 32];
        let zone_id = [9u8; 16];
        let mut stream = file_header(0, &plane_id, &zone_id);
        let f1 = frame(FRAME_ITEM_COMMIT, b"payload-one");
        let f2 = frame(FRAME_ITEM_COMMIT, b"payload-two-longer");
        stream.extend_from_slice(&f1);
        stream.extend_from_slice(&f2);
        let (frames, end) = walk(&stream).expect("stamped walker accepts our frames");
        assert_eq!(end, stream.len(), "no torn tail");
        assert_eq!(frames.len(), 2);
        assert_eq!(&stream[frames[0].0..frames[0].1], f1.as_slice());
        assert_eq!(&stream[frames[1].0..frames[1].1], f2.as_slice());
    }
}
