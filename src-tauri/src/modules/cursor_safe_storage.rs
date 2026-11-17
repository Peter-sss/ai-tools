//! Cursor 3.27+ seals desktop auth tokens as `cursor-safe-storage:v1:` plus
//! Electron safeStorage ciphertext. Web JWTs stay plaintext.

use std::path::Path;
use std::process::Command;

use base64::Engine;

#[cfg(not(target_os = "windows"))]
use aes::Aes128;
#[cfg(target_os = "windows")]
use aes_gcm::aead::generic_array::GenericArray;
#[cfg(target_os = "windows")]
use aes_gcm::aead::{Aead, OsRng};
#[cfg(target_os = "windows")]
use aes_gcm::{AeadCore, Aes256Gcm, KeyInit, Nonce};
#[cfg(not(target_os = "windows"))]
use cbc::cipher::block_padding::Pkcs7;
#[cfg(not(target_os = "windows"))]
use cbc::cipher::{BlockDecryptMut, BlockEncryptMut, KeyIvInit};
#[cfg(not(target_os = "windows"))]
use pbkdf2::pbkdf2_hmac;
#[cfg(not(target_os = "windows"))]
use sha1::Sha1;

#[cfg(target_os = "windows")]
use windows::Win32::Foundation::{LocalFree, HLOCAL};
#[cfg(target_os = "windows")]
use windows::Win32::Security::Cryptography::{CryptUnprotectData, CRYPT_INTEGER_BLOB};

#[cfg(not(target_os = "windows"))]
type Aes128CbcEnc = cbc::Encryptor<Aes128>;
#[cfg(not(target_os = "windows"))]
type Aes128CbcDec = cbc::Decryptor<Aes128>;

const ENVELOPE_PREFIX: &str = "cursor-safe-storage:v1:";
const V10_PREFIX: &[u8] = b"v10";
const V11_PREFIX: &[u8] = b"v11";
#[cfg(not(target_os = "windows"))]
const CBC_IV: [u8; 16] = [b' '; 16];
#[cfg(not(target_os = "windows"))]
const SALT: &[u8] = b"saltysalt";

pub fn is_cursor_safe_storage(value: &str) -> bool {
    value.trim().starts_with(ENVELOPE_PREFIX)
}

pub fn open_stored_token(stored: &str, data_dir: &Path) -> Result<String, String> {
    let stored = stored.trim();
    if !is_cursor_safe_storage(stored) {
        return Ok(stored.to_string());
    }
    let raw = decode_envelope(stored)?;
    let plain = decrypt_safe_storage_bytes(&raw, data_dir)?;
    String::from_utf8(plain).map_err(|_| "Cursor safe storage 解密结果不是 UTF-8".to_string())
}

pub fn seal_stored_token(plaintext: &str, data_dir: &Path) -> Result<String, String> {
    let raw = encrypt_safe_storage_bytes(plaintext.as_bytes(), data_dir)?;
    Ok(format!(
        "{}{}",
        ENVELOPE_PREFIX,
        base64::engine::general_purpose::STANDARD.encode(raw)
    ))
}

fn decode_envelope(stored: &str) -> Result<Vec<u8>, String> {
    let b64 = stored
        .trim()
        .strip_prefix(ENVELOPE_PREFIX)
        .ok_or_else(|| "缺少 cursor-safe-storage 前缀".to_string())?;
    base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .map_err(|err| format!("Cursor safe storage Base64 无法解析: {}", err))
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn open_v10_cbc_with_password(stored: &str, password: &str) -> Result<String, String> {
    let raw = decode_envelope(stored)?;
    let key = pbkdf2_sha1_key(password, 1003);
    let plain = decrypt_cbc_prefixed(&raw, V10_PREFIX, &key)?;
    String::from_utf8(plain).map_err(|_| "Cursor safe storage 解密结果不是 UTF-8".to_string())
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn seal_v10_cbc_with_password(plaintext: &str, password: &str) -> Result<String, String> {
    let key = pbkdf2_sha1_key(password, 1003);
    let raw = encrypt_cbc_prefixed(V10_PREFIX, &key, plaintext.as_bytes())?;
    Ok(format!(
        "{}{}",
        ENVELOPE_PREFIX,
        base64::engine::general_purpose::STANDARD.encode(raw)
    ))
}

#[cfg(not(target_os = "windows"))]
fn decrypt_safe_storage_bytes(raw: &[u8], _data_dir: &Path) -> Result<Vec<u8>, String> {
    let password = cursor_safe_storage_password()?;
    if raw.starts_with(V11_PREFIX) {
        let key = pbkdf2_sha1_key(&password, 1);
        return decrypt_cbc_prefixed(raw, V11_PREFIX, &key);
    }
    let key = pbkdf2_sha1_key(&password, 1003);
    decrypt_cbc_prefixed(raw, V10_PREFIX, &key)
}

#[cfg(not(target_os = "windows"))]
fn encrypt_safe_storage_bytes(plaintext: &[u8], _data_dir: &Path) -> Result<Vec<u8>, String> {
    let password = cursor_safe_storage_password()?;
    let key = pbkdf2_sha1_key(&password, 1003);
    encrypt_cbc_prefixed(V10_PREFIX, &key, plaintext)
}

#[cfg(not(target_os = "windows"))]
fn cursor_safe_storage_password() -> Result<String, String> {
    let mut errors = Vec::new();
    for account in ["Cursor Key", "Cursor"] {
        match read_secret_password(account) {
            Ok(password) => return Ok(password),
            Err(err) => errors.push(err),
        }
    }
    Err(format!(
        "未找到 Cursor Safe Storage 密钥: {}",
        errors.join("; ")
    ))
}

#[cfg(target_os = "macos")]
fn read_secret_password(account: &str) -> Result<String, String> {
    let output = Command::new("security")
        .args([
            "find-generic-password",
            "-s",
            "Cursor Safe Storage",
            "-a",
            account,
            "-w",
        ])
        .output()
        .map_err(|err| format!("读取 Keychain 失败: {}", err))?;
    if !output.status.success() {
        return Err(format!("Keychain 没有账号 {}", account));
    }
    let password = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if password.is_empty() {
        return Err(format!("Keychain 账号 {} 为空", account));
    }
    Ok(password)
}

#[cfg(target_os = "linux")]
fn read_secret_password(account: &str) -> Result<String, String> {
    let output = Command::new("secret-tool")
        .args([
            "lookup",
            "service",
            "Cursor Safe Storage",
            "account",
            account,
        ])
        .output()
        .map_err(|err| format!("读取 secret-tool 失败: {}", err))?;
    if !output.status.success() {
        return Err(format!("secret-tool 没有账号 {}", account));
    }
    let password = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if password.is_empty() {
        return Err(format!("secret-tool 账号 {} 为空", account));
    }
    Ok(password)
}

#[cfg(not(target_os = "windows"))]
fn pbkdf2_sha1_key(password: &str, iterations: u32) -> [u8; 16] {
    let mut key = [0u8; 16];
    pbkdf2_hmac::<Sha1>(password.as_bytes(), SALT, iterations, &mut key);
    key
}

#[cfg(not(target_os = "windows"))]
fn decrypt_cbc_prefixed(
    encrypted: &[u8],
    expected_prefix: &[u8],
    key: &[u8; 16],
) -> Result<Vec<u8>, String> {
    if !encrypted.starts_with(expected_prefix) {
        return Err("Cursor safe storage 密文前缀不匹配".to_string());
    }
    let raw = &encrypted[expected_prefix.len()..];
    let mut buf = raw.to_vec();
    let cipher = Aes128CbcDec::new_from_slices(key, &CBC_IV)
        .map_err(|err| format!("初始化 AES-CBC 失败: {}", err))?;
    cipher
        .decrypt_padded_mut::<Pkcs7>(&mut buf)
        .map(|plain| plain.to_vec())
        .map_err(|err| format!("AES-CBC 解密失败: {}", err))
}

#[cfg(not(target_os = "windows"))]
fn encrypt_cbc_prefixed(prefix: &[u8], key: &[u8; 16], plaintext: &[u8]) -> Result<Vec<u8>, String> {
    let cipher = Aes128CbcEnc::new_from_slices(key, &CBC_IV)
        .map_err(|err| format!("初始化 AES-CBC 失败: {}", err))?;
    let mut buf = plaintext.to_vec();
    let msg_len = buf.len();
    let pad_len = 16 - (msg_len % 16);
    buf.resize(msg_len + pad_len, 0);
    let ciphertext = cipher
        .encrypt_padded_mut::<Pkcs7>(&mut buf, msg_len)
        .map_err(|err| format!("AES-CBC 加密失败: {}", err))?
        .to_vec();
    let mut result = Vec::with_capacity(prefix.len() + ciphertext.len());
    result.extend_from_slice(prefix);
    result.extend_from_slice(&ciphertext);
    Ok(result)
}

#[cfg(target_os = "windows")]
fn decrypt_safe_storage_bytes(raw: &[u8], data_dir: &Path) -> Result<Vec<u8>, String> {
    let key = windows_safe_storage_key(data_dir)?;
    decrypt_windows_gcm_v10(&key, raw)
}

#[cfg(target_os = "windows")]
fn encrypt_safe_storage_bytes(plaintext: &[u8], data_dir: &Path) -> Result<Vec<u8>, String> {
    let key = windows_safe_storage_key(data_dir)?;
    encrypt_windows_gcm_v10(&key, plaintext)
}

#[cfg(target_os = "windows")]
fn windows_safe_storage_key(data_dir: &Path) -> Result<Vec<u8>, String> {
    let path = data_dir.join("Local State");
    let content = std::fs::read_to_string(&path)
        .map_err(|err| format!("读取 Cursor Local State 失败: {}", err))?;
    let json: serde_json::Value = serde_json::from_str(&content)
        .map_err(|err| format!("解析 Cursor Local State 失败: {}", err))?;
    let encrypted_key_b64 = json["os_crypt"]["encrypted_key"]
        .as_str()
        .ok_or_else(|| "Cursor Local State 缺少 os_crypt.encrypted_key".to_string())?;
    let encrypted_key_bytes = base64::engine::general_purpose::STANDARD
        .decode(encrypted_key_b64)
        .map_err(|err| format!("Cursor Local State 密钥 Base64 无法解析: {}", err))?;
    if encrypted_key_bytes.len() < 6 || &encrypted_key_bytes[..5] != b"DPAPI" {
        return Err("Cursor Local State 密钥不是 DPAPI".to_string());
    }
    let key = dpapi_decrypt(&encrypted_key_bytes[5..])?;
    if key.len() != 32 {
        return Err(format!("Cursor DPAPI 密钥长度异常: {}", key.len()));
    }
    Ok(key)
}

#[cfg(target_os = "windows")]
fn dpapi_decrypt(encrypted: &[u8]) -> Result<Vec<u8>, String> {
    unsafe {
        let mut input = CRYPT_INTEGER_BLOB {
            cbData: encrypted.len() as u32,
            pbData: encrypted.as_ptr() as *mut u8,
        };
        let mut output = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: std::ptr::null_mut(),
        };
        CryptUnprotectData(&mut input, None, None, None, None, 0, &mut output)
            .map_err(|_| "DPAPI 解密 Cursor Safe Storage 密钥失败".to_string())?;
        let result = std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
        LocalFree(HLOCAL(output.pbData as *mut _));
        Ok(result)
    }
}

#[cfg(target_os = "windows")]
fn decrypt_windows_gcm_v10(key: &[u8], encrypted: &[u8]) -> Result<Vec<u8>, String> {
    if encrypted.len() < 31 || &encrypted[..3] != V10_PREFIX {
        return Err("不是 Windows v10 safe storage 密文".to_string());
    }
    let cipher = Aes256Gcm::new(GenericArray::from_slice(key));
    let nonce = Nonce::from_slice(&encrypted[3..15]);
    cipher
        .decrypt(nonce, &encrypted[15..])
        .map_err(|_| "AES-GCM 解密 Cursor token 失败".to_string())
}

#[cfg(target_os = "windows")]
fn encrypt_windows_gcm_v10(key: &[u8], plaintext: &[u8]) -> Result<Vec<u8>, String> {
    let cipher = Aes256Gcm::new(GenericArray::from_slice(key));
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
    let ciphertext = cipher
        .encrypt(&nonce, plaintext)
        .map_err(|_| "AES-GCM 加密 Cursor token 失败".to_string())?;
    let mut result = Vec::with_capacity(3 + nonce.len() + ciphertext.len());
    result.extend_from_slice(V10_PREFIX);
    result.extend_from_slice(nonce.as_slice());
    result.extend_from_slice(&ciphertext);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn safe_storage_v10_round_trip_with_password() {
        let sealed = seal_v10_cbc_with_password("desktop-session-token", "cursor-key").expect("seal");
        assert!(is_cursor_safe_storage(&sealed));
        let opened = open_v10_cbc_with_password(&sealed, "cursor-key").expect("open");
        assert_eq!(opened, "desktop-session-token");
    }

    #[test]
    fn plaintext_token_is_not_an_envelope() {
        assert!(!is_cursor_safe_storage("eyJhbGciOiJIUzI1NiJ9.e30.sig"));
    }
}
