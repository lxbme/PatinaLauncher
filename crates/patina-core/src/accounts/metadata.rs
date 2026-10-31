//! Durable account metadata, distinct from the legacy rename-only public save.
use super::{AccountStore, AccountsFile, credentials::validate_reference, error::AuthError};
use crate::{paths::DataDirs, platform::credentials};
use std::collections::HashSet;

impl AccountStore {
    pub(super) fn load_durable(data: &DataDirs) -> Result<Self, AuthError> {
        let bytes =
            credentials::read_account_metadata(data).map_err(super::credentials::read_error)?;
        let file: AccountsFile = match bytes {
            None => AccountsFile::default(),
            Some(bytes) => toml::from_str(
                std::str::from_utf8(&bytes).map_err(|_| AuthError::CredentialCorrupt)?,
            )
            .map_err(|_| AuthError::CredentialCorrupt)?,
        };
        let mut ids = HashSet::new();
        let mut references = HashSet::new();
        for account in &file.accounts {
            if !ids.insert(&account.id) || account.uuid.is_nil() {
                return Err(AuthError::CredentialCorrupt);
            }
            if let Some(reference) = &account.token_ref {
                validate_reference(reference).map_err(super::credentials::read_error)?;
                if !references.insert(reference) {
                    return Err(AuthError::CredentialCorrupt);
                }
            }
            if account
                .verification
                .as_ref()
                .is_some_and(|v| v.profile_uuid != account.uuid)
                || account
                    .skin_hash
                    .as_ref()
                    .is_some_and(|h| h.is_empty() || !h.bytes().all(|b| b.is_ascii_hexdigit()))
            {
                return Err(AuthError::CredentialCorrupt);
            }
        }
        Ok(Self::from_file(data.root().join("accounts.toml"), file))
    }
    pub(super) fn save_durable(
        &self,
        data: &DataDirs,
        #[cfg(test)] fail: Option<credentials::WriteStage>,
    ) -> Result<(), AuthError> {
        let text = toml::to_string(&self.as_file()).map_err(|_| AuthError::CredentialWrite)?;
        credentials::write_account_metadata(
            data,
            text.as_bytes(),
            #[cfg(test)]
            fail,
        )
        .map_err(super::credentials::write_error)
    }
    pub(super) fn references(&self) -> HashSet<String> {
        self.accounts
            .iter()
            .chain(&self.hidden)
            .filter_map(|a| a.token_ref.clone())
            .collect()
    }
}
