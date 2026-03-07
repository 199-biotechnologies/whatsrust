//! Poll crypto: HKDF-SHA256 key derivation + AES-256-GCM encrypt/decrypt for poll votes.
//!
//! WhatsApp encrypts poll votes so only the poll creator can decrypt them.
//! Each poll has a random 32-byte `enc_key`. Votes are encrypted with:
//!   - Key: HKDF-SHA256(enc_key, info = "Poll Vote", no salt) → 32 bytes
//!   - AAD: "{poll_id}\0{voter_jid}"
//!   - AES-256-GCM with random 12-byte IV
//!
//! The encrypted vote payload is a serialized protobuf `PollVoteMessage`
//! containing selected option SHA-256 hashes.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use hkdf::Hkdf;
use prost::Message as ProstMessage;
use sha2::Sha256;
use waproto::whatsapp as wa;

/// Generate a random 32-byte poll encryption key.
pub fn generate_poll_key() -> [u8; 32] {
    let mut key = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut key);
    key
}

/// SHA-256 hash of a poll option name (used as the option identifier in votes).
pub fn hash_poll_option(name: &str) -> Vec<u8> {
    use sha2::Digest;
    let mut hasher = Sha256::new();
    hasher.update(name.as_bytes());
    hasher.finalize().to_vec()
}

/// Derive the AES-256-GCM key from the poll's enc_key using HKDF-SHA256.
fn derive_key(enc_key: &[u8]) -> [u8; 32] {
    let hk = Hkdf::<Sha256>::new(None, enc_key);
    let mut okm = [0u8; 32];
    hk.expand(b"Poll Vote", &mut okm)
        .expect("HKDF expand failed");
    okm
}

/// Build the AAD (additional authenticated data) for poll vote encryption.
fn build_aad(poll_id: &str, voter_jid: &str) -> Vec<u8> {
    let mut aad = Vec::with_capacity(poll_id.len() + 1 + voter_jid.len());
    aad.extend_from_slice(poll_id.as_bytes());
    aad.push(0);
    aad.extend_from_slice(voter_jid.as_bytes());
    aad
}

/// Decrypt a poll vote. Returns the list of selected option hashes.
pub fn decrypt_poll_vote(
    enc_key: &[u8],
    poll_id: &str,
    voter_jid: &str,
    ciphertext: &[u8],
    iv: &[u8],
) -> anyhow::Result<Vec<Vec<u8>>> {
    let key = derive_key(enc_key);
    let cipher = Aes256Gcm::new_from_slice(&key)?;
    let nonce = Nonce::from_slice(iv);
    let aad = build_aad(poll_id, voter_jid);
    let payload = Payload {
        msg: ciphertext,
        aad: &aad,
    };
    let plaintext = cipher
        .decrypt(nonce, payload)
        .map_err(|e| anyhow::anyhow!("poll vote decrypt failed: {e}"))?;

    let vote = wa::message::PollVoteMessage::decode(plaintext.as_slice())?;
    Ok(vote.selected_options)
}

/// Encrypt a poll vote (for testing / future outbound vote support).
#[cfg(test)]
pub fn encrypt_poll_vote(
    enc_key: &[u8],
    poll_id: &str,
    voter_jid: &str,
    selected_hashes: &[Vec<u8>],
) -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
    let key = derive_key(enc_key);
    let cipher = Aes256Gcm::new_from_slice(&key)?;
    let mut iv = [0u8; 12];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut iv);
    let nonce = Nonce::from_slice(&iv);
    let aad = build_aad(poll_id, voter_jid);

    let vote = wa::message::PollVoteMessage {
        selected_options: selected_hashes.to_vec(),
    };
    let mut plaintext = Vec::new();
    vote.encode(&mut plaintext)?;

    let payload = Payload {
        msg: &plaintext,
        aad: &aad,
    };
    let ciphertext = cipher
        .encrypt(nonce, payload)
        .map_err(|e| anyhow::anyhow!("poll vote encrypt failed: {e}"))?;

    Ok((ciphertext, iv.to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_roundtrip_poll_vote() {
        let enc_key = generate_poll_key();
        let poll_id = "3EB0ABCD1234";
        let voter_jid = "5511999999999@s.whatsapp.net";

        let opt_a = hash_poll_option("Red");
        let opt_b = hash_poll_option("Blue");
        let selected = vec![opt_a.clone(), opt_b.clone()];

        let (ct, iv) = encrypt_poll_vote(&enc_key, poll_id, voter_jid, &selected).unwrap();
        let decrypted = decrypt_poll_vote(&enc_key, poll_id, voter_jid, &ct, &iv).unwrap();

        assert_eq!(decrypted.len(), 2);
        assert_eq!(decrypted[0], opt_a);
        assert_eq!(decrypted[1], opt_b);
    }

    #[test]
    fn test_hash_poll_option() {
        let h = hash_poll_option("Red");
        assert_eq!(h.len(), 32);
        // Same input → same hash
        assert_eq!(h, hash_poll_option("Red"));
        // Different input → different hash
        assert_ne!(h, hash_poll_option("Blue"));
    }

    #[test]
    fn test_wrong_key_fails() {
        let enc_key = generate_poll_key();
        let wrong_key = generate_poll_key();
        let poll_id = "3EB0ABCD1234";
        let voter_jid = "5511999999999@s.whatsapp.net";
        let selected = vec![hash_poll_option("Yes")];

        let (ct, iv) = encrypt_poll_vote(&enc_key, poll_id, voter_jid, &selected).unwrap();
        let result = decrypt_poll_vote(&wrong_key, poll_id, voter_jid, &ct, &iv);
        assert!(result.is_err());
    }
}
