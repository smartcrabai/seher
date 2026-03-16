use crate::crypto::{CryptoError, Result};
use aes::cipher::{BlockDecryptMut, KeyIvInit};
use security_framework::os::macos::keychain::SecKeychain;

type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;

const SALT: &[u8] = b"saltysalt";
const IV: &[u8] = b"                "; // 16 spaces
const KEY_LENGTH: usize = 16;
const ITERATIONS: u32 = 1003;

/// # Errors
///
/// Returns an error if the encryption version is unsupported, decryption fails, or
/// the result is not valid UTF-8.
pub fn decrypt(encrypted_value: &[u8]) -> Result<String> {
    if encrypted_value.len() < 3 {
        return Ok(String::new());
    }

    let version = &encrypted_value[..3];
    match version {
        b"v10" | b"v11" => {
            let encrypted = &encrypted_value[3..];
            let key = get_encryption_key()?;
            decrypt_aes_cbc(&key, encrypted)
        }
        _ => String::from_utf8(encrypted_value.to_vec())
            .map_err(|e| CryptoError::DecryptionFailed(e.to_string())),
    }
}

fn get_encryption_key() -> Result<Vec<u8>> {
    let password = get_chrome_password()?;

    let mut key = vec![0u8; KEY_LENGTH];
    pbkdf2::pbkdf2_hmac::<sha1::Sha1>(&password, SALT, ITERATIONS, &mut key);

    Ok(key)
}

fn get_chrome_password() -> Result<Vec<u8>> {
    // Prefer `security` CLI: its code signature is stable, so "Always Allow"
    // in the Keychain dialog persists across rebuilds of our binary.
    if let Ok(pw) = get_password_from_cli() {
        return Ok(pw);
    }

    // Fallback: security-framework API (prompts per-binary)
    get_password_from_keychain()
}

fn get_password_from_keychain() -> Result<Vec<u8>> {
    let keychain = SecKeychain::default()
        .map_err(|e| CryptoError::KeychainError(format!("Failed to access keychain: {e}")))?;

    let (password_data, _item) = keychain
        .find_generic_password("Chrome Safe Storage", "Chrome")
        .map_err(|e| {
            CryptoError::KeychainError(format!("Failed to find Chrome Safe Storage: {e}"))
        })?;

    Ok(password_data.as_ref().to_vec())
}

fn get_password_from_cli() -> Result<Vec<u8>> {
    let output = std::process::Command::new("security")
        .args([
            "find-generic-password",
            "-s",
            "Chrome Safe Storage",
            "-a",
            "Chrome",
            "-w",
        ])
        .output()
        .map_err(|e| CryptoError::KeychainError(format!("Failed to run security command: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(CryptoError::KeychainError(format!(
            "security command failed: {}",
            stderr.trim()
        )));
    }

    let password = String::from_utf8_lossy(&output.stdout);
    Ok(password.trim_end_matches('\n').as_bytes().to_vec())
}

fn decrypt_aes_cbc(key: &[u8], encrypted: &[u8]) -> Result<String> {
    let cipher = Aes128CbcDec::new(key.into(), IV.into());

    let mut buffer = encrypted.to_vec();
    let decrypted = cipher
        .decrypt_padded_mut::<aes::cipher::block_padding::Pkcs7>(&mut buffer)
        .map_err(|e| CryptoError::DecryptionFailed(format!("AES-CBC decryption failed: {e:?}")))?;

    // Newer Chrome prepends a 32-byte binary header (31 bytes + 0x60 separator)
    // before the actual cookie value. Scan for valid UTF-8 and strip the header.
    if let Ok(s) = String::from_utf8(decrypted.to_vec()) {
        return Ok(s);
    }
    for offset in 1..decrypted.len().min(64) {
        if let Ok(s) = std::str::from_utf8(&decrypted[offset..]) {
            // Skip the 0x60 separator byte that Chrome uses as header terminator
            let s = s.strip_prefix('`').unwrap_or(s);
            return Ok(s.to_string());
        }
    }

    Err(CryptoError::DecryptionFailed(format!(
        "UTF-8 conversion failed (first 32 bytes: {:02x?})",
        &decrypted[..decrypted.len().min(32)]
    )))
}
