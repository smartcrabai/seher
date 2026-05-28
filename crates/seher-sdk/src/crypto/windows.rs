use crate::crypto::{CryptoError, Result};
use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, KeyInit},
};
use std::fs;
use std::path::Path;

pub fn decrypt(encrypted_value: &[u8]) -> Result<String> {
    if encrypted_value.len() < 3 {
        return Ok(String::new());
    }

    let version = &encrypted_value[..3];
    match version {
        b"v10" => {
            let encrypted = &encrypted_value[3..];
            decrypt_aes_gcm(encrypted)
        }
        _ => String::from_utf8(encrypted_value.to_vec())
            .map_err(|e| CryptoError::DecryptionFailed(e.to_string())),
    }
}

fn decrypt_aes_gcm(encrypted: &[u8]) -> Result<String> {
    if encrypted.len() < 12 {
        return Err(CryptoError::DecryptionFailed(
            "Encrypted data too short".to_string(),
        ));
    }

    let key = get_encryption_key()?;
    let nonce_bytes = &encrypted[..12];
    let ciphertext = &encrypted[12..];

    let cipher = Aes256Gcm::new_from_slice(&key)
        .map_err(|e| CryptoError::DecryptionFailed(format!("Invalid key length: {}", e)))?;

    let nonce = Nonce::from_slice(nonce_bytes);

    let plaintext = cipher
        .decrypt(nonce, ciphertext)
        .map_err(|e| CryptoError::DecryptionFailed(format!("AES-GCM decryption failed: {}", e)))?;

    String::from_utf8(plaintext)
        .map_err(|e| CryptoError::DecryptionFailed(format!("UTF-8 conversion failed: {}", e)))
}

fn get_encryption_key() -> Result<Vec<u8>> {
    let local_state_path = get_local_state_path()?;
    let content = fs::read_to_string(&local_state_path)
        .map_err(|e| CryptoError::DpapiError(format!("Failed to read Local State: {}", e)))?;

    let json: serde_json::Value = serde_json::from_str(&content)
        .map_err(|e| CryptoError::DpapiError(format!("Failed to parse JSON: {}", e)))?;

    let encrypted_key_base64 = json["os_crypt"]["encrypted_key"]
        .as_str()
        .ok_or_else(|| CryptoError::DpapiError("encrypted_key not found".to_string()))?;

    let encrypted_key = base64::decode(encrypted_key_base64)
        .map_err(|e| CryptoError::DpapiError(format!("Base64 decode failed: {}", e)))?;

    if encrypted_key.len() < 5 || &encrypted_key[..5] != b"DPAPI" {
        return Err(CryptoError::DpapiError("Invalid DPAPI prefix".to_string()));
    }

    let encrypted_key_data = &encrypted_key[5..];
    dpapi_decrypt(encrypted_key_data)
}

fn dpapi_decrypt(data: &[u8]) -> Result<Vec<u8>> {
    use windows::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptUnprotectData,
    };

    let mut data_in = CRYPT_INTEGER_BLOB {
        cbData: data.len() as u32,
        pbData: data.as_ptr() as *mut u8,
    };

    let mut data_out = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };

    unsafe {
        let result = CryptUnprotectData(
            &mut data_in,
            None,
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut data_out,
        );

        if result.is_err() {
            return Err(CryptoError::DpapiError(
                "DPAPI decryption failed".to_string(),
            ));
        }

        let decrypted =
            std::slice::from_raw_parts(data_out.pbData, data_out.cbData as usize).to_vec();

        // Free the memory allocated by CryptUnprotectData
        if !data_out.pbData.is_null() {
            windows::Win32::System::Memory::LocalFree(data_out.pbData as isize);
        }

        Ok(decrypted)
    }
}

fn get_local_state_path() -> Result<std::path::PathBuf> {
    let local_app_data = std::env::var("LOCALAPPDATA")
        .map_err(|_| CryptoError::DpapiError("LOCALAPPDATA not set".to_string()))?;

    let paths = [
        "Google\\Chrome\\User Data\\Local State",
        "Microsoft\\Edge\\User Data\\Local State",
        "BraveSoftware\\Brave-Browser\\User Data\\Local State",
    ];

    for path in &paths {
        let full_path = Path::new(&local_app_data).join(path);
        if full_path.exists() {
            return Ok(full_path);
        }
    }

    Err(CryptoError::DpapiError(
        "Local State file not found".to_string(),
    ))
}
