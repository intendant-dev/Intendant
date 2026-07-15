//! Write-only vault deposits: the CLI lane that gets a secret INTO the
//! vault without the plaintext ever riding a web UI — and without this
//! daemon being able to read it afterwards.
//!
//! The vault blob is a single sealed body (`32-vault-custody.js`): nothing
//! outside a browser holding the master key can append inside it. So a
//! deposit is a sidecar: an ECIES envelope to the vault's deposit public
//! key, queued as one file per record on this daemon's disk, folded into
//! the blob by the next unlocked dashboard, then consumed (deleted) once
//! the re-wrapped blob has been published.
//!
//! Wire/crypto format (v1) — the browser mirror lives in
//! `32-vault-custody.js` (`vaultConsumeDeposits`), and
//! `scripts/vault-deposit-parity.cjs` cross-checks the two
//! implementations against each other with real WebCrypto:
//!
//! - recipient key: P-256, raw uncompressed point (65 bytes, base64url),
//!   published by an unlocked dashboard into `vault-deposit-key.pub.json`
//!   under the daemon state root. Public material — world-readable is fine.
//! - seal: ephemeral P-256 → ECDH → HKDF-SHA256 with empty salt and
//!   `info = "intendant-vault-deposit-v1" || eph_pub_raw ||
//!   recipient_pub_raw || label_utf8` → AES-256-GCM key; random 96-bit
//!   nonce; `AAD = "intendant-vault-deposit-v1:" || label_utf8` (binds the
//!   label the depositor typed to the ciphertext).
//! - record: one JSON file per deposit under `vault-deposits.d/` —
//!   atomic create, so a depositing CLI and a consuming dashboard never
//!   race on a shared file.
//!
//! Trust notes: the deposit CLI trusts the local pubkey file exactly as
//! far as it trusts the machine it runs on (same boundary as the daemon
//! state root itself). A malicious daemon could swap the deposit key and
//! capture FUTURE deposits — it cannot read existing vault entries, and
//! the dashboard's fold step surfaces every consumed deposit visibly.

use ring::aead;
use ring::agreement;
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const DEPOSIT_VERSION_INFO: &[u8] = b"intendant-vault-deposit-v1";
const DEPOSIT_AAD_PREFIX: &[u8] = b"intendant-vault-deposit-v1:";
const DEPOSIT_KEY_ALG: &str = "ECDH-P256";
const DEPOSIT_SEAL_ALG: &str = "ECIES-P256-HKDF-SHA256-A256GCM";
/// Uncompressed SEC1 P-256 point: 0x04 || X (32) || Y (32).
const P256_POINT_LEN: usize = 65;

/// The vault's deposit public key, as published by an unlocked dashboard.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DepositKey {
    pub alg: String,
    pub pub_raw_b64u: String,
    #[serde(default)]
    pub published_unix_ms: u64,
}

/// One sealed deposit, queued for the next unlocked dashboard.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DepositRecord {
    pub id: String,
    pub label: String,
    pub created_unix_ms: u64,
    pub alg: String,
    pub eph_pub_raw_b64u: String,
    pub nonce_b64u: String,
    pub ct_b64u: String,
}

pub fn deposit_key_path() -> PathBuf {
    crate::platform::intendant_home().join("vault-deposit-key.pub.json")
}

pub fn deposits_dir() -> PathBuf {
    crate::platform::intendant_home().join("vault-deposits.d")
}

fn b64u(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn from_b64u(text: &str) -> Result<Vec<u8>, String> {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(text.trim())
        .map_err(|e| format!("invalid base64url: {e}"))
}

fn now_unix_ms() -> u64 {
    crate::access::client_key::now_unix_ms().max(0) as u64
}

fn format_unix_ms(ms: u64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms as i64)
        .map(|dt| dt.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|| ms.to_string())
}

// ── deposit key file ──

pub fn save_deposit_key_in(path: &Path, key: &DepositKey) -> Result<(), String> {
    let point = from_b64u(&key.pub_raw_b64u)?;
    if key.alg != DEPOSIT_KEY_ALG {
        return Err(format!("unsupported deposit key alg {}", key.alg));
    }
    if point.len() != P256_POINT_LEN || point[0] != 0x04 {
        return Err("deposit key must be an uncompressed P-256 point".to_string());
    }
    let text = serde_json::to_string_pretty(key).map_err(|e| e.to_string())?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("finalize {}: {e}", path.display()))
}

pub fn load_deposit_key_in(path: &Path) -> Result<Option<DepositKey>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("read {}: {e}", path.display())),
    };
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|e| format!("parse {}: {e}", path.display()))
}

// ── sealing (the CLI side; the browser opens) ──

fn hkdf_aes256_key(shared_secret: &[u8], info_parts: &[&[u8]]) -> Result<[u8; 32], String> {
    struct Len32;
    impl ring::hkdf::KeyType for Len32 {
        fn len(&self) -> usize {
            32
        }
    }
    let salt = ring::hkdf::Salt::new(ring::hkdf::HKDF_SHA256, &[]);
    let prk = salt.extract(shared_secret);
    let okm = prk
        .expand(info_parts, Len32)
        .map_err(|_| "HKDF expand failed".to_string())?;
    let mut key = [0u8; 32];
    okm.fill(&mut key)
        .map_err(|_| "HKDF fill failed".to_string())?;
    Ok(key)
}

fn aead_key(key_bytes: &[u8; 32]) -> Result<aead::LessSafeKey, String> {
    let unbound = aead::UnboundKey::new(&aead::AES_256_GCM, key_bytes)
        .map_err(|_| "AEAD key init failed".to_string())?;
    Ok(aead::LessSafeKey::new(unbound))
}

fn deposit_aad(label: &str) -> Vec<u8> {
    let mut aad = DEPOSIT_AAD_PREFIX.to_vec();
    aad.extend_from_slice(label.as_bytes());
    aad
}

/// Seal `secret` to the vault's deposit public key. The ephemeral private
/// key never leaves this function; the daemon (and any later reader of
/// the record) holds only ciphertext.
pub fn seal_deposit(
    recipient_pub_raw: &[u8],
    label: &str,
    secret: &[u8],
) -> Result<DepositRecord, String> {
    if recipient_pub_raw.len() != P256_POINT_LEN || recipient_pub_raw[0] != 0x04 {
        return Err("recipient key must be an uncompressed P-256 point".to_string());
    }
    let rng = SystemRandom::new();
    let eph = agreement::EphemeralPrivateKey::generate(&agreement::ECDH_P256, &rng)
        .map_err(|_| "ephemeral key generation failed".to_string())?;
    let eph_pub = eph
        .compute_public_key()
        .map_err(|_| "ephemeral public key failed".to_string())?;
    let eph_pub_raw = eph_pub.as_ref().to_vec();
    let recipient = agreement::UnparsedPublicKey::new(&agreement::ECDH_P256, recipient_pub_raw);

    let key_bytes = agreement::agree_ephemeral(eph, &recipient, |shared| {
        hkdf_aes256_key(
            shared,
            &[
                DEPOSIT_VERSION_INFO,
                &eph_pub_raw,
                recipient_pub_raw,
                label.as_bytes(),
            ],
        )
    })
    .map_err(|_| "ECDH agreement failed (malformed recipient key?)".to_string())??;

    let key = aead_key(&key_bytes)?;
    let mut nonce_bytes = [0u8; 12];
    rng.fill(&mut nonce_bytes)
        .map_err(|_| "nonce generation failed".to_string())?;
    let nonce = aead::Nonce::assume_unique_for_key(nonce_bytes);
    let mut in_out = secret.to_vec();
    key.seal_in_place_append_tag(nonce, aead::Aad::from(deposit_aad(label)), &mut in_out)
        .map_err(|_| "seal failed".to_string())?;

    let mut id_bytes = [0u8; 12];
    rng.fill(&mut id_bytes)
        .map_err(|_| "id generation failed".to_string())?;
    Ok(DepositRecord {
        id: format!("dep-{}", b64u(&id_bytes)),
        label: label.to_string(),
        created_unix_ms: now_unix_ms(),
        alg: DEPOSIT_SEAL_ALG.to_string(),
        eph_pub_raw_b64u: b64u(&eph_pub_raw),
        nonce_b64u: b64u(&nonce_bytes),
        ct_b64u: b64u(&in_out),
    })
}

// ── the per-record queue ──

pub fn store_deposit_in(dir: &Path, record: &DepositRecord) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let path = dir.join(format!("{}.json", record.id));
    let tmp = dir.join(format!("{}.json.tmp", record.id));
    let text = serde_json::to_string_pretty(record).map_err(|e| e.to_string())?;
    std::fs::write(&tmp, text).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    restrict_file(&tmp);
    std::fs::rename(&tmp, &path).map_err(|e| format!("finalize {}: {e}", path.display()))?;
    Ok(path)
}

pub fn list_deposits_in(dir: &Path) -> Result<Vec<DepositRecord>, String> {
    let mut records = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(records),
        Err(e) => return Err(format!("read {}: {e}", dir.display())),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        match std::fs::read_to_string(&path)
            .map_err(|e| e.to_string())
            .and_then(|text| {
                serde_json::from_str::<DepositRecord>(&text).map_err(|e| e.to_string())
            }) {
            Ok(record) => records.push(record),
            Err(e) => eprintln!("[vault-deposits] skipping {}: {e}", path.display()),
        }
    }
    records.sort_by_key(|r| (r.created_unix_ms, r.id.clone()));
    Ok(records)
}

/// Delete consumed records. Only called after the dashboard reports the
/// re-wrapped blob was published; missing files are fine (another
/// dashboard may have consumed concurrently).
pub fn consume_deposits_in(dir: &Path, ids: &[String]) -> usize {
    let mut removed = 0;
    for id in ids {
        // Ids are self-minted (`dep-<b64url>`); refuse anything that
        // could traverse.
        if id.is_empty() || id.contains(['/', '\\', '.']) {
            continue;
        }
        let path = dir.join(format!("{id}.json"));
        if std::fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

fn restrict_file(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(metadata) = std::fs::metadata(path) {
            let mut perms = metadata.permissions();
            perms.set_mode(0o600);
            let _ = std::fs::set_permissions(path, perms);
        }
    }
}

// ── CLI: `intendant vault deposit <label>` ──

pub async fn run_vault_cli(args: Vec<String>) -> i32 {
    match args.first().map(String::as_str) {
        Some("deposit") => cli_deposit(&args[1..]),
        Some("status") => cli_status(),
        _ => {
            print_vault_usage();
            2
        }
    }
}

fn print_vault_usage() {
    println!("Usage:");
    println!("    intendant vault deposit <label>   # seal a secret INTO the vault (write-only)");
    println!("    intendant vault status            # deposit key + pending deposit count");
    println!();
    println!("  `deposit` reads the secret from stdin. Pipe it to keep it out of the");
    println!("  terminal: `pbpaste | intendant vault deposit gh-token`. The secret is");
    println!("  sealed to the vault's deposit key on this machine; only an unlocked");
    println!("  dashboard can read it, and this CLI cannot read anything back out.");
}

fn cli_status() -> i32 {
    match load_deposit_key_in(&deposit_key_path()) {
        Ok(Some(key)) => {
            println!(
                "deposit key: present ({}, published {})",
                key.alg,
                format_unix_ms(key.published_unix_ms)
            );
        }
        Ok(None) => {
            println!("deposit key: none — unlock the vault in a dashboard on this daemon once");
        }
        Err(e) => {
            eprintln!("deposit key: unreadable: {e}");
            return 1;
        }
    }
    match list_deposits_in(&deposits_dir()) {
        Ok(records) => {
            println!("pending deposits: {}", records.len());
            for record in records {
                println!(
                    "  {}  {}  ({})",
                    record.id,
                    record.label,
                    format_unix_ms(record.created_unix_ms)
                );
            }
            0
        }
        Err(e) => {
            eprintln!("pending deposits: unreadable: {e}");
            1
        }
    }
}

fn cli_deposit(args: &[String]) -> i32 {
    let Some(label) = args.first().map(|s| s.trim()).filter(|s| !s.is_empty()) else {
        eprintln!("usage: intendant vault deposit <label>   (secret on stdin)");
        return 2;
    };
    let key = match load_deposit_key_in(&deposit_key_path()) {
        Ok(Some(key)) => key,
        Ok(None) => {
            eprintln!("No vault deposit key on this daemon yet.");
            eprintln!("Unlock the vault once in a dashboard connected to this daemon —");
            eprintln!("it publishes the deposit public key here, after which deposits work");
            eprintln!("even while the vault stays locked.");
            return 1;
        }
        Err(e) => {
            eprintln!("deposit key unreadable: {e}");
            return 1;
        }
    };
    let recipient = match from_b64u(&key.pub_raw_b64u) {
        Ok(bytes) => bytes,
        Err(e) => {
            eprintln!("deposit key corrupt: {e}");
            return 1;
        }
    };

    use std::io::IsTerminal;
    use std::io::Read;
    if std::io::stdin().is_terminal() {
        eprintln!("Reading the secret from stdin. NOTE: typed input echoes in this");
        eprintln!("terminal — prefer piping (`pbpaste | intendant vault deposit {label}`).");
        eprintln!("End with Ctrl-D on an empty line.");
    }
    let mut secret = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut secret) {
        eprintln!("failed to read secret from stdin: {e}");
        return 1;
    }
    let trimmed = secret.trim_end_matches(['\r', '\n']);
    if trimmed.is_empty() {
        eprintln!("empty secret; nothing deposited");
        return 1;
    }

    match seal_deposit(&recipient, label, trimmed.as_bytes())
        .and_then(|record| store_deposit_in(&deposits_dir(), &record).map(|path| (record, path)))
    {
        Ok((record, _path)) => {
            println!("Sealed deposit {} ({label}).", record.id);
            println!("It will appear in the vault the next time an owner unlocks it in a");
            println!("dashboard connected to this daemon. This machine holds only ciphertext.");
            0
        }
        Err(e) => {
            eprintln!("deposit failed: {e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Recipient side, test-only: ring's agreement API is ephemeral-only
    /// by design, so the "vault" here is a second ephemeral keypair —
    /// which is exactly the browser's math (ECDH is symmetric in the two
    /// key roles).
    fn open_deposit(
        recipient_priv: agreement::EphemeralPrivateKey,
        recipient_pub_raw: &[u8],
        record: &DepositRecord,
    ) -> Result<Vec<u8>, String> {
        let eph_pub_raw = from_b64u(&record.eph_pub_raw_b64u)?;
        let eph_pub = agreement::UnparsedPublicKey::new(&agreement::ECDH_P256, &eph_pub_raw);
        let key_bytes = agreement::agree_ephemeral(recipient_priv, &eph_pub, |shared| {
            hkdf_aes256_key(
                shared,
                &[
                    DEPOSIT_VERSION_INFO,
                    &eph_pub_raw,
                    recipient_pub_raw,
                    record.label.as_bytes(),
                ],
            )
        })
        .map_err(|_| "agree failed".to_string())??;
        let key = aead_key(&key_bytes)?;
        let nonce_bytes: [u8; 12] = from_b64u(&record.nonce_b64u)?
            .try_into()
            .map_err(|_| "bad nonce".to_string())?;
        let mut ct = from_b64u(&record.ct_b64u)?;
        let plain = key
            .open_in_place(
                aead::Nonce::assume_unique_for_key(nonce_bytes),
                aead::Aad::from(deposit_aad(&record.label)),
                &mut ct,
            )
            .map_err(|_| "open failed".to_string())?;
        Ok(plain.to_vec())
    }

    fn test_recipient() -> (agreement::EphemeralPrivateKey, Vec<u8>) {
        let rng = SystemRandom::new();
        let priv_key =
            agreement::EphemeralPrivateKey::generate(&agreement::ECDH_P256, &rng).unwrap();
        let pub_raw = priv_key.compute_public_key().unwrap().as_ref().to_vec();
        (priv_key, pub_raw)
    }

    #[test]
    fn seal_round_trips_against_an_independent_recipient_implementation() {
        let (recipient_priv, recipient_pub) = test_recipient();
        let record = seal_deposit(&recipient_pub, "gh-token", b"hunter2-but-long").unwrap();
        assert_eq!(record.alg, DEPOSIT_SEAL_ALG);
        let plain = open_deposit(recipient_priv, &recipient_pub, &record).unwrap();
        assert_eq!(plain, b"hunter2-but-long");
    }

    #[test]
    fn tampered_label_fails_the_aad_binding() {
        let (recipient_priv, recipient_pub) = test_recipient();
        let mut record = seal_deposit(&recipient_pub, "gh-token", b"secret").unwrap();
        record.label = "prod-db-password".to_string();
        assert!(open_deposit(recipient_priv, &recipient_pub, &record).is_err());
    }

    #[test]
    fn queue_stores_lists_and_consumes_per_record_files() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("vault-deposits.d");
        assert!(list_deposits_in(&dir).unwrap().is_empty());

        let (_priv1, pub1) = test_recipient();
        let a = seal_deposit(&pub1, "a", b"1").unwrap();
        let b = seal_deposit(&pub1, "b", b"2").unwrap();
        store_deposit_in(&dir, &a).unwrap();
        store_deposit_in(&dir, &b).unwrap();
        let listed = list_deposits_in(&dir).unwrap();
        assert_eq!(listed.len(), 2);

        assert_eq!(consume_deposits_in(&dir, &[a.id.clone()]), 1);
        // Missing + traversal-shaped ids are ignored, not errors.
        assert_eq!(
            consume_deposits_in(&dir, &[a.id.clone(), "../etc/passwd".to_string()]),
            0
        );
        let listed = list_deposits_in(&dir).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, b.id);
    }

    #[test]
    fn deposit_key_file_round_trips_and_validates_the_point() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("vault-deposit-key.pub.json");
        assert!(load_deposit_key_in(&path).unwrap().is_none());

        let (_priv1, pub1) = test_recipient();
        let key = DepositKey {
            alg: DEPOSIT_KEY_ALG.to_string(),
            pub_raw_b64u: b64u(&pub1),
            published_unix_ms: 1,
        };
        save_deposit_key_in(&path, &key).unwrap();
        let loaded = load_deposit_key_in(&path).unwrap().unwrap();
        assert_eq!(loaded.pub_raw_b64u, key.pub_raw_b64u);

        // A compressed/garbage point is refused at save time.
        let bad = DepositKey {
            alg: DEPOSIT_KEY_ALG.to_string(),
            pub_raw_b64u: b64u(&[0x02; 33]),
            published_unix_ms: 1,
        };
        assert!(save_deposit_key_in(&path, &bad).is_err());
    }
}
