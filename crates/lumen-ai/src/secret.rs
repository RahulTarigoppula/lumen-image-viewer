//! API key storage outside settings.json.
//!
//! The key goes to Windows Credential Manager (`CredWriteW`, generic
//! credential `Lumen/ai/<host>:<port>`, persisted per user on this machine).
//! The OS encrypts it with the user's DPAPI key, and the user can see and
//! delete it in Control Panel > Credential Manager. When `CredWriteW` fails
//! (locked-down profiles), keys are encrypted with `CryptProtectData` (user
//! scope) into `<data dir>/ai-key.bin`, a DPAPI blob of a small JSON map.
//!
//! In memory the key lives in [`ApiKey`], which zeroes itself on drop and
//! never prints its value.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::error::AiError;

/// Where a key is stored (`api_key_ref` in the settings).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyStore {
    Credman,
    Dpapi,
}

/// An API key that is wiped from memory when dropped.
#[derive(Clone)]
pub struct ApiKey(Zeroizing<String>);

impl ApiKey {
    /// Trims the text; `None` when it is empty.
    pub fn new(text: &str) -> Option<ApiKey> {
        let t = text.trim();
        (!t.is_empty()).then(|| ApiKey(Zeroizing::new(t.to_string())))
    }

    /// The secret itself. Only the HTTP client should call this.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApiKey(<redacted>)")
    }
}

/// Saves, loads and deletes keys by Credential Manager target name
/// (`Endpoint::credential_target`).
#[derive(Clone, Debug)]
pub struct SecretStore {
    fallback_file: PathBuf,
}

const FALLBACK_NAME: &str = "ai-key.bin";

impl SecretStore {
    /// `data_dir` is Lumen's settings directory (portable aware).
    pub fn new(data_dir: &Path) -> SecretStore {
        SecretStore { fallback_file: data_dir.join(FALLBACK_NAME) }
    }

    /// Stores the key, preferring Credential Manager. Returns where it went,
    /// to be saved as `api_key_ref`.
    pub fn save(&self, target: &str, key: &ApiKey) -> Result<KeyStore, AiError> {
        match sys::cred_write(target, key.expose()) {
            Ok(()) => {
                // Do not leave an older copy in the fallback file.
                let _ = self.dpapi_remove(target);
                Ok(KeyStore::Credman)
            }
            Err(e) => {
                log::warn!("Credential Manager unavailable ({e}), using the DPAPI file");
                self.dpapi_put(target, key)?;
                Ok(KeyStore::Dpapi)
            }
        }
    }

    pub fn save_to(&self, store: KeyStore, target: &str, key: &ApiKey) -> Result<(), AiError> {
        match store {
            KeyStore::Credman => sys::cred_write(target, key.expose()).map_err(AiError::Secret),
            KeyStore::Dpapi => self.dpapi_put(target, key),
        }
    }

    /// Loads the key from `store`, or from either store when `None`.
    pub fn load(&self, target: &str, store: Option<KeyStore>) -> Result<Option<ApiKey>, AiError> {
        let cred = || sys::cred_read(target).map_err(AiError::Secret);
        let file = || self.dpapi_map().map(|m| m.get(target).and_then(|k| ApiKey::new(k)));
        match store {
            Some(KeyStore::Credman) => cred(),
            Some(KeyStore::Dpapi) => file(),
            None => match cred()? {
                Some(k) => Ok(Some(k)),
                None => file(),
            },
        }
    }

    /// "Forget key": removes the key from both stores.
    pub fn delete(&self, target: &str) -> Result<(), AiError> {
        sys::cred_delete(target).map_err(AiError::Secret)?;
        self.dpapi_remove(target)
    }

    fn dpapi_map(&self) -> Result<BTreeMap<String, Zeroizing<String>>, AiError> {
        let blob = match std::fs::read(&self.fallback_file) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
            Err(e) => return Err(AiError::Secret(format!("read {}: {e}", self.fallback_file.display()))),
        };
        let plain = Zeroizing::new(sys::unprotect(&blob).map_err(AiError::Secret)?);
        let map: BTreeMap<String, String> =
            serde_json::from_slice(&plain).map_err(|e| AiError::Secret(format!("corrupt key file: {e}")))?;
        Ok(map.into_iter().map(|(k, v)| (k, Zeroizing::new(v))).collect())
    }

    fn dpapi_write(&self, map: &BTreeMap<String, Zeroizing<String>>) -> Result<(), AiError> {
        if map.is_empty() {
            return match std::fs::remove_file(&self.fallback_file) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(AiError::Secret(e.to_string())),
                _ => Ok(()),
            };
        }
        let plain: BTreeMap<&str, &str> = map.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let json = Zeroizing::new(serde_json::to_vec(&plain).map_err(|e| AiError::Secret(e.to_string()))?);
        let blob = sys::protect(&json).map_err(AiError::Secret)?;
        if let Some(dir) = self.fallback_file.parent() {
            std::fs::create_dir_all(dir).map_err(|e| AiError::Secret(e.to_string()))?;
        }
        let tmp = self.fallback_file.with_extension("bin.tmp");
        std::fs::write(&tmp, &blob).map_err(|e| AiError::Secret(e.to_string()))?;
        std::fs::rename(&tmp, &self.fallback_file).map_err(|e| AiError::Secret(e.to_string()))
    }

    fn dpapi_put(&self, target: &str, key: &ApiKey) -> Result<(), AiError> {
        let mut map = self.dpapi_map()?;
        map.insert(target.to_string(), Zeroizing::new(key.expose().to_string()));
        self.dpapi_write(&map)
    }

    fn dpapi_remove(&self, target: &str) -> Result<(), AiError> {
        if !self.fallback_file.exists() {
            return Ok(());
        }
        let mut map = self.dpapi_map()?;
        if map.remove(target).is_some() {
            self.dpapi_write(&map)?;
        }
        Ok(())
    }
}

#[cfg(windows)]
mod sys {
    use windows::core::{HSTRING, PWSTR};
    use windows::Win32::Foundation::{LocalFree, ERROR_NOT_FOUND, HLOCAL};
    use windows::Win32::Security::Credentials::{
        CredDeleteW, CredFree, CredReadW, CredWriteW, CREDENTIALW, CRED_MAX_CREDENTIAL_BLOB_SIZE,
        CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC,
    };
    use windows::Win32::Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };
    use zeroize::Zeroize;

    /// Mixed into DPAPI so other apps calling CryptUnprotectData on the file
    /// without it get nothing. Not a secret, just a namespace.
    const ENTROPY: &[u8] = b"Lumen.ai.key.v1";

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    fn not_found(e: &windows::core::Error) -> bool {
        e.code() == ERROR_NOT_FOUND.to_hresult()
    }

    pub fn cred_write(target: &str, secret: &str) -> Result<(), String> {
        if secret.len() > CRED_MAX_CREDENTIAL_BLOB_SIZE as usize {
            return Err("key too long".into());
        }
        let mut target_w = wide(target);
        let mut user_w = wide("lumen");
        let mut blob = secret.as_bytes().to_vec();
        let cred = CREDENTIALW {
            Type: CRED_TYPE_GENERIC,
            TargetName: PWSTR(target_w.as_mut_ptr()),
            CredentialBlobSize: blob.len() as u32,
            CredentialBlob: blob.as_mut_ptr(),
            Persist: CRED_PERSIST_LOCAL_MACHINE,
            UserName: PWSTR(user_w.as_mut_ptr()),
            ..Default::default()
        };
        // SAFETY: every pointer in `cred` points into a live local buffer.
        let r = unsafe { CredWriteW(&cred, 0) };
        blob.zeroize();
        r.map_err(|e| e.message())
    }

    pub fn cred_read(target: &str) -> Result<Option<super::ApiKey>, String> {
        let mut p: *mut CREDENTIALW = std::ptr::null_mut();
        // SAFETY: CredReadW allocates `p`; it is released with CredFree below.
        match unsafe { CredReadW(&HSTRING::from(target), CRED_TYPE_GENERIC, None, &mut p) } {
            Ok(()) => {}
            Err(e) if not_found(&e) => return Ok(None),
            Err(e) => return Err(e.message()),
        }
        // SAFETY: on success `p` is a valid CREDENTIALW whose blob holds
        // CredentialBlobSize bytes.
        let key = unsafe {
            let c = &*p;
            let bytes = if c.CredentialBlob.is_null() {
                &[][..]
            } else {
                std::slice::from_raw_parts(c.CredentialBlob, c.CredentialBlobSize as usize)
            };
            let text = zeroize::Zeroizing::new(String::from_utf8_lossy(bytes).into_owned());
            CredFree(p as *const _);
            super::ApiKey::new(&text)
        };
        Ok(key)
    }

    pub fn cred_delete(target: &str) -> Result<(), String> {
        // SAFETY: plain call with an owned wide string.
        match unsafe { CredDeleteW(&HSTRING::from(target), CRED_TYPE_GENERIC, None) } {
            Ok(()) => Ok(()),
            Err(e) if not_found(&e) => Ok(()),
            Err(e) => Err(e.message()),
        }
    }

    fn blob(data: &[u8]) -> CRYPT_INTEGER_BLOB {
        CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 }
    }

    /// Copies a DPAPI output blob and frees it.
    unsafe fn take(out: CRYPT_INTEGER_BLOB) -> Vec<u8> {
        // SAFETY: DPAPI returned `cbData` bytes at `pbData`, allocated with LocalAlloc.
        unsafe {
            let v = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
            std::ptr::write_bytes(out.pbData, 0, out.cbData as usize);
            let _ = LocalFree(Some(HLOCAL(out.pbData as *mut _)));
            v
        }
    }

    pub fn protect(data: &[u8]) -> Result<Vec<u8>, String> {
        let input = blob(data);
        let entropy = blob(ENTROPY);
        let mut out = CRYPT_INTEGER_BLOB::default();
        // SAFETY: input and entropy borrow live slices; out is filled by DPAPI.
        unsafe {
            CryptProtectData(
                &input,
                &HSTRING::from("Lumen AI key"),
                Some(&entropy),
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
            .map_err(|e| e.message())?;
            Ok(take(out))
        }
    }

    pub fn unprotect(data: &[u8]) -> Result<Vec<u8>, String> {
        let input = blob(data);
        let entropy = blob(ENTROPY);
        let mut out = CRYPT_INTEGER_BLOB::default();
        // SAFETY: as in `protect`.
        unsafe {
            CryptUnprotectData(&input, None, Some(&entropy), None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut out)
                .map_err(|e| e.message())?;
            Ok(take(out))
        }
    }
}

#[cfg(not(windows))]
mod sys {
    const MSG: &str = "secure key storage is only implemented on Windows";
    pub fn cred_write(_: &str, _: &str) -> Result<(), String> {
        Err(MSG.into())
    }
    pub fn cred_read(_: &str) -> Result<Option<super::ApiKey>, String> {
        Ok(None)
    }
    pub fn cred_delete(_: &str) -> Result<(), String> {
        Ok(())
    }
    pub fn protect(_: &[u8]) -> Result<Vec<u8>, String> {
        Err(MSG.into())
    }
    pub fn unprotect(_: &[u8]) -> Result<Vec<u8>, String> {
        Err(MSG.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_debug_is_redacted() {
        let k = ApiKey::new("  sk-secret ").unwrap();
        assert_eq!(k.expose(), "sk-secret");
        assert!(!format!("{k:?}").contains("secret"));
        assert!(ApiKey::new("   ").is_none());
    }

    #[cfg(windows)]
    #[test]
    fn dpapi_round_trip() {
        let p = sys::protect(b"hello").unwrap();
        assert_ne!(&p[..], b"hello");
        assert_eq!(sys::unprotect(&p).unwrap(), b"hello");
    }

    #[cfg(windows)]
    #[test]
    fn dpapi_file_store() {
        let dir = std::env::temp_dir().join(format!("lumen-ai-secret-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let s = SecretStore::new(&dir);
        let k = ApiKey::new("sk-file").unwrap();
        s.save_to(KeyStore::Dpapi, "Lumen/ai/test-a:8000", &k).unwrap();
        s.save_to(KeyStore::Dpapi, "Lumen/ai/test-b:8000", &ApiKey::new("sk-b").unwrap()).unwrap();
        let raw = std::fs::read(dir.join(FALLBACK_NAME)).unwrap();
        assert!(!raw.windows(7).any(|w| w == b"sk-file"), "file must be encrypted");
        let got = s.load("Lumen/ai/test-a:8000", Some(KeyStore::Dpapi)).unwrap().unwrap();
        assert_eq!(got.expose(), "sk-file");
        s.dpapi_remove("Lumen/ai/test-a:8000").unwrap();
        assert!(s.load("Lumen/ai/test-a:8000", Some(KeyStore::Dpapi)).unwrap().is_none());
        assert_eq!(s.load("Lumen/ai/test-b:8000", Some(KeyStore::Dpapi)).unwrap().unwrap().expose(), "sk-b");
        s.dpapi_remove("Lumen/ai/test-b:8000").unwrap();
        assert!(!dir.join(FALLBACK_NAME).exists(), "empty map removes the file");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn credential_manager_round_trip() {
        let dir = std::env::temp_dir().join(format!("lumen-ai-cred-{}", std::process::id()));
        let s = SecretStore::new(&dir);
        let target = format!("Lumen/ai/unit-test-{}:8000", std::process::id());
        let store = s.save(&target, &ApiKey::new("sk-cred").unwrap()).unwrap();
        assert_eq!(s.load(&target, Some(store)).unwrap().unwrap().expose(), "sk-cred");
        assert_eq!(s.load(&target, None).unwrap().unwrap().expose(), "sk-cred");
        s.delete(&target).unwrap();
        assert!(s.load(&target, None).unwrap().is_none());
        // Deleting a missing key is not an error.
        s.delete(&target).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
