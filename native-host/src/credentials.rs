//! Per-user protocol-v5 identity and paired-phone storage.

use std::fmt;

use serde::{Deserialize, Serialize};
use snow::{Builder, params::NoiseParams};
use zeroize::Zeroize;

const STORE_VERSION: u8 = 1;
const NOISE_NAME: &str = "Noise_XX_25519_ChaChaPoly_BLAKE2s";

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Credentials {
    version: u8,
    private_key: [u8; 32],
    public_key: [u8; 32],
    paired_phone_public_key: Option<[u8; 32]>,
    paired_phone_name: Option<String>,
}

impl Credentials {
    pub fn private_key(&self) -> &[u8; 32] {
        &self.private_key
    }

    pub fn public_key(&self) -> &[u8; 32] {
        &self.public_key
    }

    pub fn paired_phone_public_key(&self) -> Option<&[u8; 32]> {
        self.paired_phone_public_key.as_ref()
    }

    pub fn paired_phone_name(&self) -> Option<&str> {
        self.paired_phone_name.as_deref()
    }

    pub fn is_paired(&self) -> bool {
        self.paired_phone_public_key.is_some()
    }

    pub fn authorize_phone(&mut self, public_key: [u8; 32], name: String) {
        self.paired_phone_public_key = Some(public_key);
        self.paired_phone_name = Some(name);
    }

    pub fn forget_phone(&mut self) {
        if let Some(mut key) = self.paired_phone_public_key.take() {
            key.zeroize();
        }
        self.paired_phone_name = None;
    }

    fn generate() -> Result<Self, CredentialError> {
        let params: NoiseParams = NOISE_NAME
            .parse()
            .map_err(|error| CredentialError::Crypto(format!("Noise parameters: {error}")))?;
        let keypair = Builder::new(params)
            .generate_keypair()
            .map_err(|error| CredentialError::Crypto(format!("X25519 identity: {error}")))?;
        Ok(Self {
            version: STORE_VERSION,
            private_key: keypair
                .private
                .try_into()
                .map_err(|_| CredentialError::Corrupt("private key length".to_owned()))?,
            public_key: keypair
                .public
                .try_into()
                .map_err(|_| CredentialError::Corrupt("public key length".to_owned()))?,
            paired_phone_public_key: None,
            paired_phone_name: None,
        })
    }

    fn validate(&self) -> Result<(), CredentialError> {
        if self.version != STORE_VERSION {
            return Err(CredentialError::Corrupt(format!(
                "unsupported credential version {}",
                self.version
            )));
        }
        if self.private_key.iter().all(|byte| *byte == 0)
            || self.public_key.iter().all(|byte| *byte == 0)
        {
            return Err(CredentialError::Corrupt(
                "identity contains a null key".to_owned(),
            ));
        }
        Ok(())
    }
}

impl Drop for Credentials {
    fn drop(&mut self) {
        self.private_key.zeroize();
    }
}

pub fn load() -> Result<Option<Credentials>, CredentialError> {
    let Some(bytes) = platform::load()? else {
        return Ok(None);
    };
    let credentials: Credentials = serde_json::from_slice(&bytes)
        .map_err(|error| CredentialError::Corrupt(error.to_string()))?;
    credentials.validate()?;
    Ok(Some(credentials))
}

pub fn load_or_create() -> Result<Credentials, CredentialError> {
    if let Some(credentials) = load()? {
        return Ok(credentials);
    }
    let credentials = Credentials::generate()?;
    save(&credentials)?;
    Ok(credentials)
}

pub fn save(credentials: &Credentials) -> Result<(), CredentialError> {
    credentials.validate()?;
    let mut bytes = serde_json::to_vec(credentials)
        .map_err(|error| CredentialError::Corrupt(error.to_string()))?;
    let result = platform::save(&bytes);
    bytes.zeroize();
    result
}

pub fn is_paired() -> Result<bool, CredentialError> {
    Ok(load()?.is_some_and(|credentials| credentials.is_paired()))
}

pub fn forget_phone() -> Result<(), CredentialError> {
    let Some(mut credentials) = load()? else {
        return Ok(());
    };
    credentials.forget_phone();
    save(&credentials)
}

#[derive(Debug)]
pub enum CredentialError {
    Unavailable(String),
    Corrupt(String),
    Crypto(String),
}

impl fmt::Display for CredentialError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(message) => write!(formatter, "secure credential store: {message}"),
            Self::Corrupt(message) => {
                write!(formatter, "stored v5 credentials are invalid: {message}")
            }
            Self::Crypto(message) => write!(formatter, "v5 identity setup failed: {message}"),
        }
    }
}

impl std::error::Error for CredentialError {}

#[cfg(windows)]
mod platform {
    use std::fs;
    use std::io::Write;
    use std::path::PathBuf;
    use std::ptr::{null, null_mut};
    use std::slice;

    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
    };
    use zeroize::Zeroize;

    use super::CredentialError;

    const ENTROPY: &[u8] = b"holodori-phone-trackpad-v5-dpapi";

    pub fn load() -> Result<Option<Vec<u8>>, CredentialError> {
        let path = credential_path()?;
        let encrypted = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(unavailable(format!(
                    "could not read {}: {error}",
                    path.display()
                )));
            }
        };
        unprotect(&encrypted).map(Some)
    }

    pub fn save(plaintext: &[u8]) -> Result<(), CredentialError> {
        let path = credential_path()?;
        let directory = path
            .parent()
            .ok_or_else(|| unavailable("credential path has no parent"))?;
        fs::create_dir_all(directory).map_err(|error| {
            unavailable(format!("could not create {}: {error}", directory.display()))
        })?;
        let mut protected = protect(plaintext)?;
        let write_result = (|| {
            let mut file = fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&path)?;
            file.write_all(&protected)?;
            file.sync_all()
        })();
        protected.zeroize();
        write_result
            .map_err(|error| unavailable(format!("could not write {}: {error}", path.display())))
    }

    fn credential_path() -> Result<PathBuf, CredentialError> {
        let base = std::env::var_os("LOCALAPPDATA")
            .filter(|value| !value.is_empty())
            .ok_or_else(|| unavailable("LOCALAPPDATA is unavailable"))?;
        Ok(PathBuf::from(base)
            .join("Holodori")
            .join("doritrack-v5.credentials"))
    }

    fn protect(plaintext: &[u8]) -> Result<Vec<u8>, CredentialError> {
        crypt(plaintext, true)
    }

    fn unprotect(ciphertext: &[u8]) -> Result<Vec<u8>, CredentialError> {
        crypt(ciphertext, false)
    }

    fn crypt(input: &[u8], protect: bool) -> Result<Vec<u8>, CredentialError> {
        if input.len() > u32::MAX as usize {
            return Err(unavailable("credential record is too large"));
        }
        let input_blob = CRYPT_INTEGER_BLOB {
            cbData: input.len() as u32,
            pbData: input.as_ptr().cast_mut(),
        };
        let entropy_blob = CRYPT_INTEGER_BLOB {
            cbData: ENTROPY.len() as u32,
            pbData: ENTROPY.as_ptr().cast_mut(),
        };
        let mut output = CRYPT_INTEGER_BLOB::default();
        let ok = unsafe {
            if protect {
                CryptProtectData(
                    &input_blob,
                    null(),
                    &entropy_blob,
                    null(),
                    null(),
                    CRYPTPROTECT_UI_FORBIDDEN,
                    &mut output,
                )
            } else {
                CryptUnprotectData(
                    &input_blob,
                    null_mut(),
                    &entropy_blob,
                    null(),
                    null(),
                    CRYPTPROTECT_UI_FORBIDDEN,
                    &mut output,
                )
            }
        };
        if ok == 0 {
            return Err(unavailable(format!(
                "DPAPI operation failed: {}",
                std::io::Error::last_os_error()
            )));
        }
        let result =
            unsafe { slice::from_raw_parts(output.pbData, output.cbData as usize) }.to_vec();
        unsafe {
            LocalFree(output.pbData.cast());
        }
        Ok(result)
    }

    fn unavailable(message: impl Into<String>) -> CredentialError {
        CredentialError::Unavailable(message.into())
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use std::collections::HashMap;

    use secret_service::{EncryptionType, blocking::SecretService};

    use super::CredentialError;

    const LABEL: &str = "doritrack protocol v5 identity";

    pub fn load() -> Result<Option<Vec<u8>>, CredentialError> {
        let service = connect()?;
        let collection = service.get_default_collection().map_err(unavailable)?;
        if collection.is_locked().map_err(unavailable)? {
            collection.unlock().map_err(unavailable)?;
        }
        let mut items = collection.search_items(attributes()).map_err(unavailable)?;
        if items.len() > 1 {
            return Err(CredentialError::Corrupt(
                "multiple Secret Service identity records exist".to_owned(),
            ));
        }
        let Some(item) = items.pop() else {
            return Ok(None);
        };
        if item.is_locked().map_err(unavailable)? {
            item.unlock().map_err(unavailable)?;
        }
        item.get_secret().map(Some).map_err(unavailable)
    }

    pub fn save(plaintext: &[u8]) -> Result<(), CredentialError> {
        let service = connect()?;
        let collection = service.get_default_collection().map_err(unavailable)?;
        if collection.is_locked().map_err(unavailable)? {
            collection.unlock().map_err(unavailable)?;
        }
        collection
            .create_item(
                LABEL,
                attributes(),
                plaintext,
                true,
                "application/octet-stream",
            )
            .map(|_| ())
            .map_err(unavailable)
    }

    fn connect() -> Result<SecretService<'static>, CredentialError> {
        SecretService::connect(EncryptionType::Dh).map_err(unavailable)
    }

    fn attributes() -> HashMap<&'static str, &'static str> {
        HashMap::from([
            ("application", "doritrack"),
            ("protocol", "5"),
            ("kind", "identity"),
        ])
    }

    fn unavailable(error: impl fmt::Display) -> CredentialError {
        CredentialError::Unavailable(error.to_string())
    }

    use std::fmt;
}

#[cfg(not(any(windows, target_os = "linux")))]
mod platform {
    use super::CredentialError;

    pub fn load() -> Result<Option<Vec<u8>>, CredentialError> {
        Err(CredentialError::Unavailable(
            "protocol v5 credentials are supported only by DPAPI and Secret Service".to_owned(),
        ))
    }

    pub fn save(_plaintext: &[u8]) -> Result<(), CredentialError> {
        Err(CredentialError::Unavailable(
            "protocol v5 credentials are supported only by DPAPI and Secret Service".to_owned(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_json_rejects_unknown_or_wrong_version_data() {
        let credentials = Credentials {
            version: STORE_VERSION,
            private_key: [1; 32],
            public_key: [2; 32],
            paired_phone_public_key: Some([3; 32]),
            paired_phone_name: Some("Phone".to_owned()),
        };
        let encoded = serde_json::to_vec(&credentials).unwrap();
        let decoded: Credentials = serde_json::from_slice(&encoded).unwrap();
        assert!(decoded.validate().is_ok());

        let mut wrong = decoded;
        wrong.version = 99;
        assert!(wrong.validate().is_err());
    }

    #[test]
    fn forgetting_peer_preserves_installation_identity() {
        let mut credentials = Credentials {
            version: STORE_VERSION,
            private_key: [1; 32],
            public_key: [2; 32],
            paired_phone_public_key: Some([3; 32]),
            paired_phone_name: Some("Phone".to_owned()),
        };
        credentials.forget_phone();
        assert!(!credentials.is_paired());
        assert_eq!(credentials.private_key(), &[1; 32]);
        assert_eq!(credentials.public_key(), &[2; 32]);
    }
}
