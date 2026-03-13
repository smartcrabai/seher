use crate::crypto::{CryptoError, Result};
use aes::cipher::{BlockDecryptMut, KeyIvInit};

type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;

const SALT: &[u8] = b"saltysalt";
const IV: &[u8] = b"                "; // 16 spaces
const KEY_LENGTH: usize = 16;
const ITERATIONS: u32 = 1;

/// # Errors
///
/// Returns an error if decryption fails or if the value is not valid UTF-8.
pub fn decrypt(encrypted_value: &[u8]) -> Result<String> {
    if encrypted_value.len() < 3 {
        return Ok(String::new());
    }

    let version = &encrypted_value[..3];
    match version {
        b"v10" | b"v11" => {
            let encrypted = &encrypted_value[3..];
            let key = get_encryption_key();
            decrypt_aes_cbc(&key, encrypted)
        }
        _ => String::from_utf8(encrypted_value.to_vec())
            .map_err(|e| CryptoError::DecryptionFailed(e.to_string())),
    }
}

fn get_encryption_key() -> Vec<u8> {
    if let Ok(key) = get_key_from_secret_service() {
        key
    } else {
        // Fallback to default password "peanuts"
        let mut key = vec![0u8; KEY_LENGTH];
        pbkdf2::pbkdf2_hmac::<sha1::Sha1>(b"peanuts", SALT, ITERATIONS, &mut key);
        key
    }
}

fn get_key_from_secret_service() -> Result<Vec<u8>> {
    use secret_service::blocking::SecretService;

    let service = SecretService::connect(secret_service::EncryptionType::Dh)
        .map_err(|e| CryptoError::SecretServiceError(format!("Failed to connect: {e}")))?;

    let collection = service
        .get_default_collection()
        .map_err(|e| CryptoError::SecretServiceError(format!("Failed to get collection: {e}")))?;

    let items = collection
        .search_items(std::collections::HashMap::from([("application", "chrome")]))
        .map_err(|e| CryptoError::SecretServiceError(format!("Failed to search items: {e}")))?;

    if let Some(item) = items.first() {
        let password = item
            .get_secret()
            .map_err(|e| CryptoError::SecretServiceError(format!("Failed to get secret: {e}")))?;

        let mut key = vec![0u8; KEY_LENGTH];
        pbkdf2::pbkdf2_hmac::<sha1::Sha1>(&password, SALT, ITERATIONS, &mut key);
        return Ok(key);
    }

    Err(CryptoError::SecretServiceError(
        "Chrome password not found".to_string(),
    ))
}

fn decrypt_aes_cbc(key: &[u8], encrypted: &[u8]) -> Result<String> {
    let cipher = Aes128CbcDec::new(key.into(), IV.into());

    let mut buffer = encrypted.to_vec();
    let decrypted = cipher
        .decrypt_padded_mut::<aes::cipher::block_padding::Pkcs7>(&mut buffer)
        .map_err(|e| CryptoError::DecryptionFailed(format!("AES-CBC decryption failed: {e:?}")))?;

    String::from_utf8(decrypted.to_vec())
        .map_err(|e| CryptoError::DecryptionFailed(format!("UTF-8 conversion failed: {e}")))
}
