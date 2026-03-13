use thiserror::Error;

#[derive(Error, Debug)]
pub enum CryptoError {
    #[error("Failed to decrypt: {0}")]
    DecryptionFailed(String),

    #[error("Unsupported encryption version: {0}")]
    UnsupportedVersion(String),

    #[cfg(target_os = "macos")]
    #[error("Keychain error: {0}")]
    KeychainError(String),

    #[cfg(target_os = "linux")]
    #[error("Secret service error: {0}")]
    SecretServiceError(String),

    #[cfg(target_os = "windows")]
    #[error("DPAPI error: {0}")]
    DpapiError(String),
}

pub type Result<T> = std::result::Result<T, CryptoError>;

#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(target_os = "linux")]
pub mod linux;

#[cfg(target_os = "windows")]
pub mod windows;

/// # Errors
///
/// Returns an error if decryption fails, the OS is unsupported, or the result is not valid UTF-8.
pub fn decrypt_cookie_value(encrypted_value: &[u8]) -> Result<String> {
    if encrypted_value.is_empty() {
        return Ok(String::new());
    }

    #[cfg(target_os = "macos")]
    let value = macos::decrypt(encrypted_value)?;

    #[cfg(target_os = "linux")]
    let value = linux::decrypt(encrypted_value)?;

    #[cfg(target_os = "windows")]
    let value = windows::decrypt(encrypted_value)?;

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    return Err(CryptoError::UnsupportedVersion(
        "Unsupported OS".to_string(),
    ));

    Ok(strip_chrome_value_prefix(&value))
}

/// Strip Chrome's cookie value format prefix (Chrome 130+).
///
/// Chrome stores cookie values with a prefix indicating the format:
/// - `[digit]t` prefix: plaintext value (e.g. "0tyes" → "yes")
/// - `[digit]e`` prefix: encoded value (e.g. "1e`token" → "token")
fn strip_chrome_value_prefix(value: &str) -> String {
    let bytes = value.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_digit() {
        if bytes[1] == b't' {
            return value[2..].to_string();
        }
        if bytes[1] == b'e' && bytes.len() >= 3 && bytes[2] == b'`' {
            return value[3..].to_string();
        }
    }
    value.to_string()
}
