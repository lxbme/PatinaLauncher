//! Historical ownership is independent of current credentials and wall-clock time.
use super::{Account, AccountKind, AccountStore, OfflineOrigin, error::AuthError};

pub(super) fn offline_allowed(accounts: &[Account]) -> bool {
    accounts.iter().any(|account| {
        account.kind == AccountKind::Msa
            && !account.uuid.is_nil()
            && account
                .verification
                .as_ref()
                .is_some_and(|proof| proof.profile_uuid == account.uuid)
    })
}

pub(super) fn is_launchable(account: &Account, accounts: &[Account]) -> bool {
    match account.kind {
        AccountKind::Msa => account.token_ref.is_some(),
        AccountKind::Offline => match account.offline_origin() {
            Some(OfflineOrigin::Entitled) => offline_allowed(accounts),
            Some(OfflineOrigin::Development) => cfg!(feature = "dev-offline"),
            None => false,
        },
    }
}

impl AccountStore {
    /// Repair policy-invalid selection without overwriting a hidden stored choice.
    pub(super) fn recompute_current(&mut self) {
        if self
            .current()
            .is_some_and(|a| is_launchable(a, &self.accounts))
        {
            return;
        }
        let next = self
            .accounts
            .iter()
            .find(|a| is_launchable(a, &self.accounts))
            .map(|a| a.id.clone());
        if self.current != next {
            self.current = next;
            if !self.stored_current_is_hidden() {
                self.current_changed = true;
            }
        }
    }

    pub(super) fn stored_current_is_hidden(&self) -> bool {
        self.hidden
            .iter()
            .any(|a| Some(&a.id) == self.stored_current.as_ref())
    }

    pub(super) fn insert_offline(
        &mut self,
        name: &str,
        origin: OfflineOrigin,
    ) -> Result<Account, AuthError> {
        if origin == OfflineOrigin::Entitled && !offline_allowed(&self.accounts) {
            return Err(AuthError::OfflineNotAllowed);
        }
        if !(3..=16).contains(&name.len())
            || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            return Err(AuthError::Account(super::AccountError::InvalidName(
                name.to_owned(),
            )));
        }
        if let Some(account) = self
            .accounts
            .iter()
            .chain(&self.hidden)
            .find(|a| a.kind == AccountKind::Offline && a.username.eq_ignore_ascii_case(name))
        {
            return if account.offline_origin() == Some(origin) {
                Ok(account.clone())
            } else {
                Err(AuthError::OfflineConflict)
            };
        }
        let uuid = crate::launch::offline_uuid(name);
        let account = Account {
            id: format!("offline-{}", uuid.simple()),
            kind: AccountKind::Offline,
            username: name.to_owned(),
            uuid,
            token_ref: None,
            verification: None,
            skin_hash: None,
            offline_origin: Some(origin),
        };
        self.accounts.push(account.clone());
        self.recompute_current();
        Ok(account)
    }
}
