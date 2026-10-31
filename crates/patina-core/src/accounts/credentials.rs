//! Protected records and per-reference backend priority. Call on a blocking pool.
use super::{error::AuthError, login::StorageKind, msa::MinecraftSession, secret::SecretString};
use crate::{
    paths::DataDirs,
    platform::credentials::{CredentialLock, PrivateCredentialDir},
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use zeroize::Zeroizing;

pub(crate) const MAX_RECORD_BYTES: usize = 128 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialError {
    Unavailable,
    Corrupt,
    UnsafePath,
    Io,
    Ambiguous,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CredentialRecord {
    version: u8,
    refresh_token: SecretString,
    minecraft: Option<MinecraftSession>,
    reauth_required: bool,
}
impl CredentialRecord {
    #[allow(
        dead_code,
        reason = "Task 4 converts verified login into protected record"
    )]
    pub(crate) fn new(
        refresh_token: SecretString,
        minecraft: Option<MinecraftSession>,
        reauth_required: bool,
    ) -> Result<Self, CredentialError> {
        let record = Self {
            version: 1,
            refresh_token,
            minecraft,
            reauth_required,
        };
        record.validate()?;
        Ok(record)
    }
    fn validate(&self) -> Result<(), CredentialError> {
        if self.version != 1
            || self.refresh_token.expose().is_empty()
            || self.refresh_token.expose().len() > 32_768
            || self
                .minecraft
                .as_ref()
                .is_some_and(|s| !s.valid_persisted())
            || (self.reauth_required && self.minecraft.is_some())
        {
            return Err(CredentialError::Corrupt);
        }
        Ok(())
    }
    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, CredentialError> {
        if bytes.len() > MAX_RECORD_BYTES {
            return Err(CredentialError::Corrupt);
        }
        let record: Self = serde_json::from_slice(bytes).map_err(|_| CredentialError::Corrupt)?;
        record.validate()?;
        Ok(record)
    }
    pub(crate) fn encode(&self) -> Result<Zeroizing<Vec<u8>>, CredentialError> {
        self.validate()?;
        let bytes = Zeroizing::new(serde_json::to_vec(self).map_err(|_| CredentialError::Corrupt)?);
        if bytes.len() > MAX_RECORD_BYTES {
            return Err(CredentialError::Corrupt);
        }
        Ok(bytes)
    }
    #[allow(dead_code, reason = "Task 4 refresh reads protected record")]
    pub(crate) fn refresh_token(&self) -> &SecretString {
        &self.refresh_token
    }
    #[allow(dead_code, reason = "Task 4 validates cached session")]
    pub(crate) fn minecraft(&self) -> Option<&MinecraftSession> {
        self.minecraft.as_ref()
    }
    #[allow(dead_code, reason = "Task 4 refuses cached launch after invalid_grant")]
    pub(crate) fn reauth_required(&self) -> bool {
        self.reauth_required
    }
}

pub(crate) fn validate_reference(reference: &str) -> Result<(), CredentialError> {
    let id = uuid::Uuid::parse_str(reference).map_err(|_| CredentialError::Corrupt)?;
    if id.is_nil() || id.hyphenated().to_string() != reference {
        return Err(CredentialError::Corrupt);
    }
    Ok(())
}

pub(crate) trait CredentialBackend: Send + Sync {
    fn read(&self, reference: &str) -> Result<Option<CredentialRecord>, CredentialError>;
    fn write(&self, reference: &str, record: &CredentialRecord) -> Result<(), CredentialError>;
    fn delete(&self, reference: &str) -> Result<(), CredentialError>;
}

pub struct CredentialStore {
    data: DataDirs,
    keyring: Arc<dyn CredentialBackend>,
    files: Arc<dyn CredentialBackend>,
}
impl CredentialStore {
    #[allow(dead_code, reason = "Task 4 constructs service lazily")]
    pub(crate) fn production(data: DataDirs) -> Self {
        Self::new(
            data.clone(),
            Arc::new(crate::platform::keyring::Keyring::new()),
            Arc::new(crate::platform::credentials::FileCredentials::new(data)),
        )
    }
    pub(crate) fn new(
        data: DataDirs,
        keyring: Arc<dyn CredentialBackend>,
        files: Arc<dyn CredentialBackend>,
    ) -> Self {
        Self {
            data,
            keyring,
            files,
        }
    }
    #[allow(dead_code, reason = "Task 4 reads credentials under operation lock")]
    pub(crate) fn read(&self, reference: &str) -> Result<CredentialRecord, AuthError> {
        validate_reference(reference).map_err(read_error)?;
        if let Some(record) = self.files.read(reference).map_err(read_error)? {
            return Ok(record);
        }
        self.keyring
            .read(reference)
            .map_err(read_error)?
            .ok_or(AuthError::CredentialMissing)
    }
    /// Failure does not imply rollback: a local rename or remote mutation may
    /// already have occurred before its durability/acknowledgement failed.
    #[allow(dead_code, reason = "Task 4 persists full login and rotated records")]
    pub(crate) fn write(
        &self,
        reference: &str,
        record: &CredentialRecord,
    ) -> Result<StorageKind, AuthError> {
        validate_reference(reference).map_err(read_error)?;
        record.validate().map_err(read_error)?;
        if self.files.read(reference).map_err(read_error)?.is_some() {
            self.files.write(reference, record).map_err(write_error)?;
            return Ok(StorageKind::LocalFile);
        }
        match self.keyring.write(reference, record) {
            Ok(()) => Ok(StorageKind::Keyring),
            Err(CredentialError::Unavailable) => {
                self.files.write(reference, record).map_err(write_error)?;
                Ok(StorageKind::LocalFile)
            }
            Err(e) => Err(write_error(e)),
        }
    }
    #[allow(
        dead_code,
        reason = "Task 4 reconciles journal and deletes both stores"
    )]
    pub(crate) fn delete(&self, reference: &str) -> Result<(), AuthError> {
        validate_reference(reference).map_err(read_error)?;
        let key = self.keyring.delete(reference);
        let file = self.files.delete(reference);
        key.and(file).map_err(write_error)
    }
    #[allow(dead_code, reason = "Task 4 bounded cancellable lock retry")]
    pub(crate) fn try_lock(&self) -> Result<Option<CredentialLock>, AuthError> {
        PrivateCredentialDir::open(&self.data)
            .and_then(|d| d.try_lock())
            .map_err(write_error)
    }
    #[allow(dead_code, reason = "Task 4 recovery reads nonsecret cleanup intents")]
    pub(crate) fn journal(&self) -> Result<Vec<String>, AuthError> {
        let dir = PrivateCredentialDir::open(&self.data).map_err(read_error)?;
        let Some(bytes) = dir.read("cleanup.json").map_err(read_error)? else {
            return Ok(vec![]);
        };
        let refs: Vec<String> =
            serde_json::from_slice(&bytes).map_err(|_| AuthError::CredentialCorrupt)?;
        for reference in &refs {
            validate_reference(reference).map_err(read_error)?;
        }
        Ok(refs)
    }
    #[allow(dead_code, reason = "Task 4 owns intent/reconciliation policy")]
    pub(crate) fn write_journal(&self, references: &[String]) -> Result<(), AuthError> {
        for reference in references {
            validate_reference(reference).map_err(read_error)?;
        }
        let bytes = serde_json::to_vec(references).map_err(|_| AuthError::CredentialWrite)?;
        PrivateCredentialDir::open(&self.data)
            .and_then(|d| d.write("cleanup.json", &bytes))
            .map_err(write_error)
    }
}
fn read_error(e: CredentialError) -> AuthError {
    match e {
        CredentialError::Unavailable | CredentialError::Io => AuthError::StorageUnavailable,
        CredentialError::UnsafePath => AuthError::UnsafeCredentialPath,
        CredentialError::Corrupt | CredentialError::Ambiguous => AuthError::CredentialCorrupt,
    }
}
fn write_error(e: CredentialError) -> AuthError {
    match e {
        CredentialError::UnsafePath => AuthError::UnsafeCredentialPath,
        CredentialError::Corrupt | CredentialError::Ambiguous => AuthError::CredentialCorrupt,
        CredentialError::Unavailable => AuthError::StorageUnavailable,
        CredentialError::Io => AuthError::CredentialWrite,
    }
}

/// Isolated byte-scripted backend; never chooses a native service.
#[cfg(any(test, feature = "test-support"))]
pub mod testing {
    use super::*;
    use std::{collections::HashMap, sync::Mutex};
    #[derive(Default)]
    pub struct MemoryCredentials {
        records: Mutex<HashMap<String, Zeroizing<Vec<u8>>>>,
        error: Mutex<Option<CredentialError>>,
        write_error: Mutex<Option<CredentialError>>,
    }
    impl MemoryCredentials {
        pub fn set_write_error(&self, error: Option<CredentialError>) {
            *self.write_error.lock().expect("fixture mutex") = error;
        }
        pub fn set_error(&self, error: Option<CredentialError>) {
            *self.error.lock().expect("fixture mutex") = error;
        }
        pub fn seed(&self, reference: &str, bytes: &[u8]) -> Result<(), CredentialError> {
            validate_reference(reference)?;
            CredentialRecord::decode(bytes)?;
            self.records
                .lock()
                .expect("fixture mutex")
                .insert(reference.into(), Zeroizing::new(bytes.to_vec()));
            Ok(())
        }
    }
    impl CredentialBackend for MemoryCredentials {
        fn read(&self, r: &str) -> Result<Option<CredentialRecord>, CredentialError> {
            if let Some(e) = *self.error.lock().expect("fixture mutex") {
                return Err(e);
            }
            self.records
                .lock()
                .expect("fixture mutex")
                .get(r)
                .map(|b| CredentialRecord::decode(b))
                .transpose()
        }
        fn write(&self, r: &str, record: &CredentialRecord) -> Result<(), CredentialError> {
            if let Some(e) = *self.write_error.lock().expect("fixture mutex") {
                return Err(e);
            }
            if let Some(e) = *self.error.lock().expect("fixture mutex") {
                return Err(e);
            }
            self.records
                .lock()
                .expect("fixture mutex")
                .insert(r.into(), record.encode()?);
            Ok(())
        }
        fn delete(&self, r: &str) -> Result<(), CredentialError> {
            if let Some(e) = *self.error.lock().expect("fixture mutex") {
                return Err(e);
            }
            self.records.lock().expect("fixture mutex").remove(r);
            Ok(())
        }
    }
    pub fn credential_store(data: DataDirs, keyring: Arc<MemoryCredentials>) -> CredentialStore {
        CredentialStore::new(
            data.clone(),
            keyring,
            Arc::new(crate::platform::credentials::FileCredentials::new(data)),
        )
    }
}
