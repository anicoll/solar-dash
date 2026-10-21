//! Request/response encryption for the iSolarCloud OpenAPI.
//!
//! Each request body is encrypted with AES using a fresh random key. That
//! key is itself encrypted with the server's RSA public key and sent in the
//! `x-random-secret-key` header, so the server can decrypt the body.

use aes::Aes128;
use aes::cipher::block_padding::Pkcs7;
use aes::cipher::{BlockModeDecrypt, BlockModeEncrypt, KeyInit};
use anyhow::{Context, Result, anyhow};
use base64::Engine;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use rand::RngExt;
use rand::distr::Alphanumeric;
use rsa::pkcs8::DecodePublicKey;
use rsa::traits::PublicKeyParts;
use rsa::{Pkcs1v15Encrypt, RsaPublicKey};

/// PKCS#1 v1.5 padding adds at least 11 bytes to every chunk.
const RSA_PKCS1_OVERHEAD: usize = 11;

type Aes128EcbEnc = ecb::Encryptor<Aes128>;
type Aes128EcbDec = ecb::Decryptor<Aes128>;

/// URL-safe base64 that accepts input with or without `=` padding.
const LENIENT_URL_SAFE: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::URL_SAFE,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

/// Standard-alphabet base64 that accepts input with or without padding.
const LENIENT_STANDARD: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::STANDARD,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

/// `len` random characters from `[A-Za-z0-9]`.
fn random_alphanumeric(len: usize) -> String {
    rand::rng()
        .sample_iter(Alphanumeric)
        .take(len)
        .map(char::from)
        .collect()
}

/// The 16-character AES key for one request.
pub fn random_key() -> String {
    random_alphanumeric(16)
}

/// A 32-character nonce.
pub fn nonce() -> String {
    random_alphanumeric(32)
}

/// Turns a key string into exactly 16 bytes: pad with spaces, then truncate.
/// This mirrors Python's `key.ljust(16)[:16]`.
fn aes_key_bytes(key: &str) -> [u8; 16] {
    let mut bytes = [b' '; 16];
    let src = key.as_bytes();
    let n = src.len().min(16);
    bytes[..n].copy_from_slice(&src[..n]);
    bytes
}

/// AES-128-ECB with PKCS7 padding. Returns uppercase hex.
pub fn aes_encrypt(plaintext: &str, key: &str) -> String {
    let cipher = Aes128EcbEnc::new(&aes_key_bytes(key).into());
    let encrypted = cipher.encrypt_padded_vec::<Pkcs7>(plaintext.as_bytes());
    hex::encode_upper(encrypted)
}

/// The reverse of [`aes_encrypt`]. Accepts upper or lower case hex.
pub fn aes_decrypt(hex_ciphertext: &str, key: &str) -> Result<String> {
    let bytes = hex::decode(hex_ciphertext.trim()).context("ciphertext is not valid hex")?;
    let cipher = Aes128EcbDec::new(&aes_key_bytes(key).into());
    let decrypted = cipher
        .decrypt_padded_vec::<Pkcs7>(&bytes)
        // The padding error carries no detail, so we describe it ourselves.
        .map_err(|_| anyhow!("bad padding (wrong key or corrupt ciphertext)"))?;
    String::from_utf8(decrypted).context("decrypted data is not valid UTF-8")
}

/// Decodes base64 in either alphabet, with or without padding.
fn decode_base64_lenient(input: &str) -> Result<Vec<u8>> {
    let input = input.trim();
    LENIENT_URL_SAFE
        .decode(input)
        .or_else(|_| LENIENT_STANDARD.decode(input))
        .context("public key is not valid base64")
}

/// Encrypts `data` with RSA PKCS#1 v1.5. The public key is base64 of a DER
/// SubjectPublicKeyInfo (no PEM headers). Long input is split into chunks
/// that fit the key size; the encrypted chunks are concatenated and returned
/// as URL-safe base64 with padding.
pub fn rsa_encrypt_key(data: &str, public_key_b64: &str) -> Result<String> {
    let der = decode_base64_lenient(public_key_b64)?;
    let public_key =
        RsaPublicKey::from_public_key_der(&der).context("public key is not a valid DER key")?;

    let chunk_size = public_key.size() - RSA_PKCS1_OVERHEAD;
    let mut rng = rsa::rand_core::OsRng;
    let mut output = Vec::new();
    for chunk in data.as_bytes().chunks(chunk_size) {
        let encrypted = public_key
            .encrypt(&mut rng, Pkcs1v15Encrypt, chunk)
            .context("RSA encryption failed")?;
        output.extend_from_slice(&encrypted);
    }
    Ok(base64::engine::general_purpose::URL_SAFE.encode(output))
}

/// One request's encryption state: a random AES key, plus that key encrypted
/// for the server.
pub struct Envelope {
    random_key: String,
    /// Value for the `x-random-secret-key` header.
    pub encrypted_key: String,
}

impl Envelope {
    pub fn new(public_key_b64: &str) -> Result<Self> {
        let random_key = random_key();
        let encrypted_key = rsa_encrypt_key(&random_key, public_key_b64)?;
        Ok(Self {
            random_key,
            encrypted_key,
        })
    }

    /// Encrypts a JSON request body.
    pub fn seal(&self, json: &str) -> String {
        aes_encrypt(json, &self.random_key)
    }

    /// Decrypts a response body.
    pub fn open(&self, hex: &str) -> Result<String> {
        aes_decrypt(hex, &self.random_key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsa::RsaPrivateKey;
    use rsa::pkcs8::EncodePublicKey;

    /// Makes a throwaway 2048-bit key pair; the public half is URL-safe base64 DER.
    fn test_keys() -> (RsaPrivateKey, String) {
        let private = RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 2048).unwrap();
        let der = private.to_public_key().to_public_key_der().unwrap();
        let b64 = base64::engine::general_purpose::URL_SAFE.encode(der.as_bytes());
        (private, b64)
    }

    /// Decrypts the output of `rsa_encrypt_key`, chunk by chunk.
    fn rsa_decrypt_all(private: &RsaPrivateKey, b64: &str) -> String {
        let bytes = base64::engine::general_purpose::URL_SAFE
            .decode(b64)
            .unwrap();
        let mut plain = Vec::new();
        for chunk in bytes.chunks(private.size()) {
            plain.extend(private.decrypt(Pkcs1v15Encrypt, chunk).unwrap());
        }
        String::from_utf8(plain).unwrap()
    }

    #[test]
    fn aes_known_answer() {
        // Expected value computed with Python's `cryptography` package.
        let out = aes_encrypt(r#"{"a":1}"#, "abcdefghijklmnop");
        assert_eq!(out, "5548A15286C1D0F2FC6BF75865E475E3");
    }

    #[test]
    fn aes_round_trip() {
        let long_json = format!(r#"{{"data":"{}"}}"#, "x".repeat(500));
        // Includes a short key (space-padded) and a too-long key (truncated).
        for key in ["abcdefghijklmnop", "short", "0123456789abcdefEXTRA"] {
            for text in [r#"{"a":1}"#, long_json.as_str(), ""] {
                let encrypted = aes_encrypt(text, key);
                assert_eq!(aes_decrypt(&encrypted, key).unwrap(), text);
            }
        }
    }

    #[test]
    fn aes_decrypt_accepts_lowercase_and_whitespace() {
        let encrypted = aes_encrypt("hello", "k").to_lowercase();
        let padded = format!("  {encrypted}\n");
        assert_eq!(aes_decrypt(&padded, "k").unwrap(), "hello");
    }

    #[test]
    fn aes_decrypt_reports_errors() {
        let err = aes_decrypt("not hex", "k").unwrap_err();
        assert!(err.to_string().contains("hex"));

        // A block of zeros encrypted without padding always decrypts to a
        // final byte of 0, which is never valid PKCS7.
        use aes::cipher::block_padding::NoPadding;
        let cipher = Aes128EcbEnc::new(&aes_key_bytes("k").into());
        let encrypted = hex::encode(cipher.encrypt_padded_vec::<NoPadding>(&[0u8; 16]));
        let err = aes_decrypt(&encrypted, "k").unwrap_err();
        assert!(err.to_string().contains("padding"));
    }

    #[test]
    fn rsa_round_trip() {
        let (private, public_b64) = test_keys();
        let key = random_key();
        let encrypted = rsa_encrypt_key(&key, &public_b64).unwrap();
        assert_eq!(rsa_decrypt_all(&private, &encrypted), key);
    }

    #[test]
    fn rsa_round_trip_multiple_chunks() {
        let (private, public_b64) = test_keys();
        let data = "A".repeat(300);
        let encrypted = rsa_encrypt_key(&data, &public_b64).unwrap();
        // 300 bytes need 2 chunks of 245; each encrypts to 256 bytes.
        let raw = base64::engine::general_purpose::URL_SAFE
            .decode(&encrypted)
            .unwrap();
        assert_eq!(raw.len(), 2 * 256);
        assert_eq!(rsa_decrypt_all(&private, &encrypted), data);
    }

    #[test]
    fn rsa_accepts_other_base64_forms() {
        let (private, public_b64) = test_keys();
        let standard = base64::engine::general_purpose::STANDARD
            .encode(LENIENT_URL_SAFE.decode(&public_b64).unwrap());
        let unpadded = public_b64.trim_end_matches('=');
        for form in [standard, unpadded.to_string(), format!(" {public_b64}\n")] {
            let encrypted = rsa_encrypt_key("secret", &form).unwrap();
            assert_eq!(rsa_decrypt_all(&private, &encrypted), "secret");
        }
    }

    #[test]
    fn rsa_rejects_garbage_key() {
        assert!(rsa_encrypt_key("x", "!!!").is_err());
        assert!(rsa_encrypt_key("x", "AAAA").is_err());
    }

    #[test]
    fn random_values_are_alphanumeric() {
        let key = random_key();
        assert_eq!(key.len(), 16);
        assert!(key.chars().all(|c| c.is_ascii_alphanumeric()));
        let n = nonce();
        assert_eq!(n.len(), 32);
        assert!(n.chars().all(|c| c.is_ascii_alphanumeric()));
    }

    #[test]
    fn envelope_round_trip() {
        let (private, public_b64) = test_keys();
        let envelope = Envelope::new(&public_b64).unwrap();
        let sealed = envelope.seal(r#"{"hello":"world"}"#);
        assert_eq!(envelope.open(&sealed).unwrap(), r#"{"hello":"world"}"#);
        // The server would recover the AES key from the header value.
        let key = rsa_decrypt_all(&private, &envelope.encrypted_key);
        assert_eq!(aes_decrypt(&sealed, &key).unwrap(), r#"{"hello":"world"}"#);
    }
}
