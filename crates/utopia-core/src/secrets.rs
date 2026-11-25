//! Encryption at rest for credentials (sealing).
//!
//! The LLM API keys, question-answering connection strings, tokens in source configs, and push secrets of api sources stored in the database used to be
//! plaintext: anyone who can read the database (or a backup) gets all external credentials. Now these values are sealed with
//! AES-256-GCM before being written, and the key is not in the database: the environment variable `UTOPIA_SECRET_KEY`, or the
//! `secret.key` generated on first start in the data directory. The threat model is "a database leak is not a credential leak": a pg_dump or a
//! read-only database account yields only ciphertext. Someone who can read both the data directory and the database (who has gotten onto the server) is not covered here:
//! that is a matter of server access control.
//!
//! **Format**: `enc:v1:` + base64(nonce(12) ‖ ciphertext+tag). Values without the prefix are read as plaintext (rows written
//! before the upgrade); at startup the callers of [`crate::secrets`] (the store's backfill) seal them.
//! Sealing is idempotent: sealing an already sealed value again returns it unchanged, so neither the read nor the write path needs to check first.
//!
//! **One key per process**: [`init`] is called once when the service starts; when not initialized, [`seal`] returns its input unchanged and
//! [`open`] only accepts plaintext. Unit tests and tools without a service take this path.
//!
//! Key rotation is not in this version: changing the key = read out with the old key, write back with the new, an explicit migration.

use aes_gcm::aead::{Aead, KeyInit, OsRng};
use aes_gcm::{AeadCore, Aes256Gcm, Key, Nonce};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use std::sync::OnceLock;

pub const PREFIX: &str = "enc:v1:";
const NONCE_LEN: usize = 12;

static SEALER: OnceLock<Aes256Gcm> = OnceLock::new();

/// Install the key. Only the first call counts; later calls return false and the key does not change
pub fn init(key: [u8; 32]) -> bool {
    SEALER
        .set(Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key)))
        .is_ok()
}

pub fn is_ready() -> bool {
    SEALER.get().is_some()
}

/// Generate a new key (32 bytes from a CSPRNG)
pub fn generate_key() -> [u8; 32] {
    Aes256Gcm::generate_key(OsRng).into()
}

/// Text form of the key: 64 hex characters or 44 base64 characters, both accepted
pub fn parse_key(text: &str) -> Option<[u8; 32]> {
    let t = text.trim();
    let bytes: Vec<u8> = if t.len() == 64 && t.bytes().all(|b| b.is_ascii_hexdigit()) {
        (0..64)
            .step_by(2)
            .map(|i| u8::from_str_radix(&t[i..i + 2], 16).ok())
            .collect::<Option<Vec<u8>>>()?
    } else {
        B64.decode(t).ok()?
    };
    bytes.try_into().ok()
}

pub fn key_to_hex(key: &[u8; 32]) -> String {
    key.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn is_sealed(stored: &str) -> bool {
    stored.starts_with(PREFIX)
}

/// Seal. Idempotent: an already sealed value is returned as is; with no key installed it is returned as is (stored in plaintext, as before)
pub fn seal(plain: &str) -> String {
    if is_sealed(plain) {
        return plain.to_string();
    }
    let Some(cipher) = SEALER.get() else {
        return plain.to_string();
    };
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
    let mut out = cipher
        .encrypt(&nonce, plain.as_bytes())
        .expect("AES-GCM encryption does not fail on in-memory input");
    let mut bytes = nonce.to_vec();
    bytes.append(&mut out);
    format!("{PREFIX}{}", B64.encode(bytes))
}

/// Open. A value without the prefix is returned as plaintext (old rows from before the upgrade); with the prefix but a wrong key or a tampered value, it errors:
/// silently returning garbage would make downstream feed it to external APIs, failing much further away
pub fn open(stored: &str) -> anyhow::Result<String> {
    let Some(body) = stored.strip_prefix(PREFIX) else {
        return Ok(stored.to_string());
    };
    let Some(cipher) = SEALER.get() else {
        anyhow::bail!("a sealed secret was read before the secret key was loaded");
    };
    let bytes = B64
        .decode(body)
        .map_err(|_| anyhow::anyhow!("a sealed secret is not valid base64"))?;
    if bytes.len() < NONCE_LEN {
        anyhow::bail!("a sealed secret is too short");
    }
    let (nonce, ct) = bytes.split_at(NONCE_LEN);
    let plain = cipher
        .decrypt(Nonce::from_slice(nonce), ct)
        .map_err(|_| anyhow::anyhow!("a sealed secret does not open with this key (wrong UTOPIA_SECRET_KEY, or the value was tampered with)"))?;
    String::from_utf8(plain).map_err(|_| anyhow::anyhow!("a sealed secret is not UTF-8"))
}

pub fn seal_opt(plain: Option<&str>) -> Option<String> {
    plain.map(seal)
}

pub fn open_opt(stored: Option<&str>) -> anyhow::Result<Option<String>> {
    stored.map(open).transpose()
}

/// Seal the string values of the given keys in a JSON object (in place). Non-string values are left alone
pub fn seal_json_keys(value: &mut serde_json::Value, keys: &[&str]) {
    if let Some(obj) = value.as_object_mut() {
        for key in keys {
            if let Some(serde_json::Value::String(s)) = obj.get(*key) {
                let sealed = seal(s);
                obj.insert((*key).to_string(), serde_json::Value::String(sealed));
            }
        }
    }
}

/// The inverse of [`seal_json_keys`]
pub fn open_json_keys(value: &mut serde_json::Value, keys: &[&str]) -> anyhow::Result<()> {
    if let Some(obj) = value.as_object_mut() {
        for key in keys {
            if let Some(serde_json::Value::String(s)) = obj.get(*key) {
                let opened = open(s)?;
                obj.insert((*key).to_string(), serde_json::Value::String(opened));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready() {
        // Only installed once in the test binary; later tests share the same key
        let _ = init([7u8; 32]);
    }

    #[test]
    fn a_secret_round_trips_and_never_repeats() {
        ready();
        let a = seal("sk-live-123");
        let b = seal("sk-live-123");
        assert!(is_sealed(&a));
        assert_ne!(a, b, "a fresh nonce every time");
        assert_eq!(open(&a).unwrap(), "sk-live-123");
        assert_eq!(open(&b).unwrap(), "sk-live-123");
    }

    #[test]
    fn sealing_is_idempotent_and_plaintext_passes_through() {
        ready();
        let once = seal("token");
        assert_eq!(seal(&once), once, "already sealed stays as it is");
        assert_eq!(open("legacy-plain").unwrap(), "legacy-plain");
    }

    #[test]
    fn a_tampered_value_does_not_open() {
        ready();
        let sealed = seal("secret");
        let mut broken = sealed.clone();
        broken.pop();
        broken.push(if sealed.ends_with('A') { 'B' } else { 'A' });
        assert!(open(&broken).is_err());
        assert!(open("enc:v1:not-base64!").is_err());
    }

    #[test]
    fn json_keys_are_sealed_in_place() {
        ready();
        let mut cfg = serde_json::json!({ "token": "t0k", "url": "https://x", "n": 3 });
        seal_json_keys(&mut cfg, &["token", "url_missing", "n"]);
        assert!(is_sealed(cfg["token"].as_str().unwrap()));
        assert_eq!(cfg["url"], "https://x");
        assert_eq!(cfg["n"], 3);
        open_json_keys(&mut cfg, &["token"]).unwrap();
        assert_eq!(cfg["token"], "t0k");
    }

    #[test]
    fn keys_parse_from_hex_and_base64() {
        let key = generate_key();
        assert_eq!(parse_key(&key_to_hex(&key)), Some(key));
        assert_eq!(parse_key(&B64.encode(key)), Some(key));
        assert_eq!(parse_key("too-short"), None);
    }
}
