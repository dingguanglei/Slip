//! Identity keys and the `box-v1` sealed payload format.
//!
//! Cipher: NaCl `box` (X25519 + XSalsa20-Poly1305) via the RustCrypto
//! `crypto_box` crate. Sealed layout: `nonce(24) || ciphertext`. See
//! docs/PROTOCOL.md. No custom cryptography lives here — only key storage,
//! fingerprints, and the nonce-prefix framing.

use anyhow::{Context, Result, anyhow};
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use chacha20poly1305::{XChaCha20Poly1305, XNonce, aead::KeyInit};
use crypto_box::{
    PublicKey, SalsaBox, SecretKey,
    aead::{Aead, AeadCore, OsRng, rand_core::RngCore},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub const NONCE_LEN: usize = 24;
pub const KEY_LEN: usize = 32;
/// Env var holding the passphrase that encrypts the identity key at rest.
pub const PASSPHRASE_ENV: &str = "SLIP_PASSPHRASE";

/// This installation's long-term X25519 keypair.
#[derive(Clone)]
pub struct Identity {
    secret: SecretKey,
}

impl std::fmt::Debug for Identity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Identity")
            .field("fingerprint", &self.fingerprint())
            .finish()
    }
}

#[derive(Deserialize, Serialize)]
struct StoredIdentity {
    version: u32,
    /// Plaintext base64 secret key, present when not passphrase-protected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    secret: Option<String>,
    /// Passphrase-encrypted secret key, present when protected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    encrypted: Option<EncryptedSecret>,
    public: String,
    created_at: u64,
}

/// Argon2id-derived-key + XChaCha20-Poly1305 seal of the identity secret.
#[derive(Deserialize, Serialize)]
struct EncryptedSecret {
    kdf: String,
    salt: String,
    nonce: String,
    ciphertext: String,
}

impl Identity {
    /// Load the identity from `<dir>/identity.json`, creating one on first
    /// use. If `SLIP_PASSPHRASE` is set, the secret is stored encrypted; a
    /// plaintext identity is migrated to encrypted on next load, and vice
    /// versa when the passphrase is cleared.
    pub fn load_or_create(dir: &Path) -> Result<Self> {
        let path = dir.join("identity.json");
        if path.exists() {
            let bytes = std::fs::read(&path)
                .with_context(|| format!("read identity {}", path.display()))?;
            let stored: StoredIdentity = serde_json::from_slice(&bytes)
                .with_context(|| format!("parse identity {}", path.display()))?;
            let passphrase = env_passphrase();
            let secret = decode_stored_secret(&stored, passphrase.as_deref())?;
            let identity = Self { secret };
            // Re-save if the at-rest protection no longer matches the env.
            let was_encrypted = stored.encrypted.is_some();
            if was_encrypted != passphrase.is_some() {
                identity.save(&path)?;
            }
            return Ok(identity);
        }

        let identity = Self::generate();
        identity.save(&path)?;
        Ok(identity)
    }

    pub fn generate() -> Self {
        Self {
            secret: SecretKey::generate(&mut OsRng),
        }
    }

    fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let created_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or_default();
        let stored = match env_passphrase() {
            Some(passphrase) => StoredIdentity {
                version: 2,
                secret: None,
                encrypted: Some(encrypt_secret(&self.secret.to_bytes(), &passphrase)?),
                public: self.public_key_b64(),
                created_at,
            },
            None => StoredIdentity {
                version: 2,
                secret: Some(BASE64.encode(self.secret.to_bytes())),
                encrypted: None,
                public: self.public_key_b64(),
                created_at,
            },
        };
        write_secret_file(path, &serde_json::to_vec_pretty(&stored)?)
            .with_context(|| format!("write identity {}", path.display()))
    }

    pub fn public_key_b64(&self) -> String {
        BASE64.encode(self.secret.public_key().as_bytes())
    }

    pub fn fingerprint(&self) -> String {
        fingerprint_of_b64(&self.public_key_b64()).unwrap_or_default()
    }

    /// Seal `plain` for `their_public_b64`: random nonce prefixed to the box.
    pub fn seal(&self, their_public_b64: &str, plain: &[u8]) -> Result<Vec<u8>> {
        let their_public = decode_public_key(their_public_b64)?;
        let sealer = SalsaBox::new(&their_public, &self.secret);
        let nonce = SalsaBox::generate_nonce(&mut OsRng);
        let ciphertext = sealer
            .encrypt(&nonce, plain)
            .map_err(|_| anyhow!("box-v1 encryption failed"))?;
        let mut sealed = Vec::with_capacity(NONCE_LEN + ciphertext.len());
        sealed.extend_from_slice(&nonce);
        sealed.extend_from_slice(&ciphertext);
        Ok(sealed)
    }

    /// Open a `nonce || box` payload sealed by `their_public_b64` for us.
    pub fn open(&self, their_public_b64: &str, sealed: &[u8]) -> Result<Vec<u8>> {
        if sealed.len() <= NONCE_LEN {
            return Err(anyhow!("sealed payload too short"));
        }
        let their_public = decode_public_key(their_public_b64)?;
        let opener = SalsaBox::new(&their_public, &self.secret);
        let nonce = crypto_box::Nonce::from_slice(&sealed[..NONCE_LEN]);
        opener
            .decrypt(nonce, &sealed[NONCE_LEN..])
            .map_err(|_| anyhow!("box-v1 decryption failed (wrong key or corrupted payload)"))
    }
}

/// `hex(SHA-256(raw public key))[..16]`, the human-checkable key fingerprint.
pub fn fingerprint_of_b64(public_b64: &str) -> Result<String> {
    let raw = BASE64
        .decode(public_b64.trim())
        .context("public key is not valid base64")?;
    if raw.len() != KEY_LEN {
        return Err(anyhow!("public key must be {KEY_LEN} bytes"));
    }
    let digest = Sha256::digest(&raw);
    Ok(hex::encode(&digest[..8]))
}

/// A 16-hex fingerprint split into four space-separated groups, easier to
/// read aloud when comparing keys out of band ("1a2b 3c4d 5e6f 7a8b").
pub fn grouped_fingerprint(fingerprint: &str) -> String {
    fingerprint
        .as_bytes()
        .chunks(4)
        .map(|chunk| String::from_utf8_lossy(chunk).into_owned())
        .collect::<Vec<_>>()
        .join(" ")
}

// -- identity-at-rest encryption -------------------------------------------

fn env_passphrase() -> Option<String> {
    std::env::var(PASSPHRASE_ENV)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Derive a 32-byte key from a passphrase and salt using Argon2id.
fn derive_key(passphrase: &str, salt: &[u8]) -> Result<[u8; KEY_LEN]> {
    use argon2::{Algorithm, Argon2, Params, Version};
    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, Params::default());
    let mut key = [0u8; KEY_LEN];
    argon2
        .hash_password_into(passphrase.as_bytes(), salt, &mut key)
        .map_err(|err| anyhow!("key derivation failed: {err}"))?;
    Ok(key)
}

fn encrypt_secret(secret: &[u8], passphrase: &str) -> Result<EncryptedSecret> {
    let mut salt = [0u8; 16];
    let mut nonce = [0u8; 24];
    OsRng.fill_bytes(&mut salt);
    OsRng.fill_bytes(&mut nonce);
    let key = derive_key(passphrase, &salt)?;
    let cipher = XChaCha20Poly1305::new((&key).into());
    let ciphertext = cipher
        .encrypt(XNonce::from_slice(&nonce), secret)
        .map_err(|_| anyhow!("identity encryption failed"))?;
    Ok(EncryptedSecret {
        kdf: "argon2id".to_string(),
        salt: BASE64.encode(salt),
        nonce: BASE64.encode(nonce),
        ciphertext: BASE64.encode(ciphertext),
    })
}

fn decrypt_secret(enc: &EncryptedSecret, passphrase: &str) -> Result<[u8; KEY_LEN]> {
    let salt = BASE64
        .decode(enc.salt.trim())
        .context("bad identity salt")?;
    let nonce = BASE64
        .decode(enc.nonce.trim())
        .context("bad identity nonce")?;
    let ciphertext = BASE64
        .decode(enc.ciphertext.trim())
        .context("bad identity ciphertext")?;
    if nonce.len() != 24 {
        return Err(anyhow!("bad identity nonce length"));
    }
    let key = derive_key(passphrase, &salt)?;
    let cipher = XChaCha20Poly1305::new((&key).into());
    let plain = cipher
        .decrypt(XNonce::from_slice(&nonce), ciphertext.as_ref())
        .map_err(|_| anyhow!("wrong passphrase or corrupted identity"))?;
    plain
        .as_slice()
        .try_into()
        .map_err(|_| anyhow!("decrypted identity key has wrong length"))
}

fn decode_stored_secret(stored: &StoredIdentity, passphrase: Option<&str>) -> Result<SecretKey> {
    if let Some(enc) = &stored.encrypted {
        let passphrase = passphrase.ok_or_else(|| {
            anyhow!("identity is passphrase-protected; set {PASSPHRASE_ENV} to unlock it")
        })?;
        return Ok(SecretKey::from(decrypt_secret(enc, passphrase)?));
    }
    let plaintext = stored
        .secret
        .as_deref()
        .ok_or_else(|| anyhow!("identity file has neither a secret nor an encrypted secret"))?;
    decode_secret_key(plaintext)
}

/// Validate and normalize a base64 X25519 public key received on the wire.
pub fn normalize_public_key_b64(value: &str) -> Result<String> {
    let raw = BASE64
        .decode(value.trim())
        .context("public key is not valid base64")?;
    if raw.len() != KEY_LEN {
        return Err(anyhow!("public key must be {KEY_LEN} bytes"));
    }
    Ok(BASE64.encode(raw))
}

/// 128-bit random lowercase-hex id for `X-Slip-Id`.
pub fn random_id() -> String {
    // SecretKey::generate is our vetted randomness source; hashing its public
    // key yields uniform bytes without adding another RNG dependency.
    let seed = SecretKey::generate(&mut OsRng);
    let digest = Sha256::digest(seed.to_bytes());
    hex::encode(&digest[..16])
}

fn decode_secret_key(value: &str) -> Result<SecretKey> {
    let raw = BASE64
        .decode(value.trim())
        .context("secret key is not valid base64")?;
    let bytes: [u8; KEY_LEN] = raw
        .as_slice()
        .try_into()
        .map_err(|_| anyhow!("secret key must be {KEY_LEN} bytes"))?;
    Ok(SecretKey::from(bytes))
}

fn decode_public_key(value: &str) -> Result<PublicKey> {
    let raw = BASE64
        .decode(value.trim())
        .context("public key is not valid base64")?;
    let bytes: [u8; KEY_LEN] = raw
        .as_slice()
        .try_into()
        .map_err(|_| anyhow!("public key must be {KEY_LEN} bytes"))?;
    Ok(PublicKey::from(bytes))
}

#[cfg(unix)]
fn write_secret_file(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    Ok(())
}

#[cfg(not(unix))]
fn write_secret_file(path: &Path, bytes: &[u8]) -> Result<()> {
    std::fs::write(path, bytes)?;
    Ok(())
}
