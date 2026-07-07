use crate::error::{Result, TrelaneError};
use crate::models::Message;
use chrono::Utc;
use digest::KeyInit;
use hmac::{Hmac, Mac};
use serde_json::Value;
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

pub fn now_iso() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

pub fn new_id(prefix: &str) -> String {
    let stamp = Utc::now().format("%Y%m%dT%H%M%SZ");
    let hex: String = (0..3)
        .map(|_| format!("{:02x}", rand::random::<u8>()))
        .collect();
    format!("{prefix}-{stamp}-{hex}")
}

pub fn random_hex(bytes: usize) -> String {
    (0..bytes)
        .map(|_| format!("{:02x}", rand::random::<u8>()))
        .collect()
}

pub fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

pub fn canonical(msg: &Message) -> Vec<u8> {
    let mut value = serde_json::to_value(msg).expect("message must serialize to json");
    if let Value::Object(ref mut map) = value {
        map.remove("sig");
    }
    serde_json::to_vec(&value).expect("canonical json must serialize")
}

pub fn sign(secret: &[u8], msg: &mut Message) {
    let can = canonical(msg);
    let mut mac = HmacSha256::new_from_slice(secret).expect("hmac key");
    mac.update(&can);
    msg.sig = hex_encode(&mac.finalize().into_bytes());
}

pub fn verify(secret: &[u8], msg: &Message) -> bool {
    if msg.sig.is_empty() {
        return false;
    }
    let can = canonical(msg);
    let mut mac: HmacSha256 = match HmacSha256::new_from_slice(secret) {
        Ok(m) => m,
        Err(_) => return false,
    };
    mac.update(&can);
    let decoded = hex::decode(&msg.sig).unwrap_or_default();
    mac.verify_slice(&decoded).is_ok()
}

pub fn generate_secret() -> String {
    random_hex(32)
}

mod hex {
    pub fn decode(s: &str) -> Result<Vec<u8>, ()> {
        if !s.len().is_multiple_of(2) {
            return Err(());
        }
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|_| ()))
            .collect()
    }
}

pub fn load_secret(trelane_dir: &std::path::Path) -> Result<Vec<u8>> {
    let path = trelane_dir.join("secret");
    if !path.exists() {
        // Self-heal: a missing secret means no message has ever been signed in
        // this project (init was skipped or the session dir was created
        // implicitly, e.g. by a biplane apply opening the DB directly), so
        // generating one now is always safe and unblocks the signing path.
        std::fs::create_dir_all(trelane_dir)?;
        let secret = generate_secret();
        std::fs::write(&path, &secret).map_err(|e| {
            TrelaneError::Msg(format!("cannot create secret at {}: {e}", path.display()))
        })?;
        return Ok(secret.trim().as_bytes().to_vec());
    }
    let data = std::fs::read_to_string(&path)
        .map_err(|e| TrelaneError::Msg(format!("cannot read secret at {}: {e}", path.display())))?;
    Ok(data.trim().as_bytes().to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_message() -> Message {
        Message::new(
            "msg-test".to_string(),
            "alpha".to_string(),
            "beta".to_string(),
            "question".to_string(),
            "normal".to_string(),
            "what shape?".to_string(),
            "need the schema".to_string(),
            None,
            None,
            vec![],
            "2026-07-03T00:00:00Z".to_string(),
        )
    }

    #[test]
    fn sign_and_verify_roundtrip() {
        let secret = b"test-secret-key";
        let mut msg = make_test_message();
        sign(secret, &mut msg);
        assert!(!msg.sig.is_empty());
        assert!(verify(secret, &msg));
    }

    #[test]
    fn verify_rejects_wrong_secret() {
        let secret = b"correct-key";
        let wrong = b"wrong-key";
        let mut msg = make_test_message();
        sign(secret, &mut msg);
        assert!(!verify(wrong, &msg));
    }

    #[test]
    fn verify_rejects_tampered_message() {
        let secret = b"test-secret-key";
        let mut msg = make_test_message();
        sign(secret, &mut msg);
        msg.subject = "tampered subject".to_string();
        assert!(!verify(secret, &msg));
    }

    #[test]
    fn verify_rejects_empty_sig() {
        let secret = b"test-secret-key";
        let msg = make_test_message();
        assert!(!verify(secret, &msg));
    }

    #[test]
    fn canonical_is_deterministic() {
        let msg = make_test_message();
        let can1 = canonical(&msg);
        let can2 = canonical(&msg);
        assert_eq!(can1, can2);
    }

    #[test]
    fn new_id_has_correct_prefix() {
        let id = new_id("msg");
        assert!(id.starts_with("msg-"));
        assert!(id.len() > "msg-".len() + 14); // timestamp + hex
    }

    #[test]
    fn load_secret_self_heals_when_missing() {
        let dir = std::env::temp_dir().join(format!("trelane-secret-test-{}", new_id("t")));
        // dir intentionally does not exist yet: load_secret must create it,
        // generate a secret, persist it, and return it.
        let first = load_secret(&dir).expect("self-heal generates a secret");
        assert!(!first.is_empty());
        assert!(dir.join("secret").exists());
        // Subsequent reads return the same persisted secret, so signatures
        // made after the heal keep verifying.
        let second = load_secret(&dir).expect("second read");
        assert_eq!(first, second);
        std::fs::remove_dir_all(&dir).ok();
    }
}
