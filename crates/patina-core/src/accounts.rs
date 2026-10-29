//! Persistent account store (`<data>/accounts.toml`).
//!
//! Release builds only expose Microsoft accounts. Offline accounts exist only
//! behind the `dev-offline` feature; without it they are kept aside and
//! written back verbatim on save so a dev build's data is never lost.

use crate::launch::LaunchAccount;
use crate::paths::DataDirs;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AccountKind {
    Msa,
    Offline,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    pub id: String,
    pub kind: AccountKind,
    pub username: String,
    pub uuid: Uuid,
    /// Reserved for the keyring reference (plan 3); always `None` for now.
    #[serde(default)]
    pub token_ref: Option<String>,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum AccountError {
    #[error("{}", rust_i18n::t!("errors.accounts.not_found", id = .0))]
    NotFound(String),
    #[error("{}", rust_i18n::t!("errors.accounts.invalid_name", name = .0))]
    InvalidName(String),
    #[error("{}", rust_i18n::t!("errors.accounts.msa_unavailable"))]
    MsaUnavailable,
    #[error("{}", rust_i18n::t!("errors.accounts.io", path = .path.display(), error = .error))]
    Io { path: PathBuf, error: String },
    #[error("{}", rust_i18n::t!("errors.accounts.parse", error = .0))]
    Parse(String),
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct AccountsFile {
    #[serde(default)]
    current: Option<String>,
    #[serde(default)]
    accounts: Vec<Account>,
}

#[derive(Debug)]
pub struct AccountStore {
    path: PathBuf,
    /// Accounts visible in this build.
    accounts: Vec<Account>,
    /// Accounts hidden in this build, written back verbatim after the visible ones.
    hidden: Vec<Account>,
    current: Option<String>,
    /// `current` as read from the file.
    stored_current: Option<String>,
    /// Whether the user changed the current account in this session.
    current_changed: bool,
}

fn visible_in_this_build(a: &Account) -> bool {
    cfg!(feature = "dev-offline") || a.kind != AccountKind::Offline
}

impl AccountStore {
    pub fn load(data: &DataDirs) -> Result<AccountStore, AccountError> {
        let path = data.root().join("accounts.toml");
        let file: AccountsFile = match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).map_err(|e| AccountError::Parse(e.to_string()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => AccountsFile::default(),
            Err(e) => {
                return Err(AccountError::Io {
                    path,
                    error: e.to_string(),
                });
            }
        };
        let mut seen = std::collections::HashSet::new();
        let unique: Vec<Account> = file
            .accounts
            .into_iter()
            .filter(|a| {
                let first = seen.insert(a.id.clone());
                if !first {
                    tracing::warn!("ignoring a duplicate account entry with id {}", a.id);
                }
                first
            })
            .collect();
        let (accounts, hidden): (Vec<_>, Vec<_>) =
            unique.into_iter().partition(visible_in_this_build);
        let current = file
            .current
            .as_ref()
            .filter(|id| accounts.iter().any(|a| &a.id == *id))
            .cloned()
            .or_else(|| accounts.first().map(|a| a.id.clone()));
        Ok(AccountStore {
            path,
            accounts,
            hidden,
            current,
            stored_current: file.current,
            current_changed: false,
        })
    }

    pub fn save(&self) -> Result<(), AccountError> {
        let file = AccountsFile {
            current: if self.current_changed {
                self.current.clone()
            } else {
                // Keep the stored current (possibly a hidden account) unless
                // it names no account at all.
                self.stored_current
                    .clone()
                    .filter(|id| {
                        self.accounts
                            .iter()
                            .chain(&self.hidden)
                            .any(|a| &a.id == id)
                    })
                    .or_else(|| self.current.clone())
            },
            accounts: self
                .accounts
                .iter()
                .chain(self.hidden.iter())
                .cloned()
                .collect(),
        };
        let text = toml::to_string(&file).map_err(|e| AccountError::Parse(e.to_string()))?;
        let io = |e: std::io::Error| AccountError::Io {
            path: self.path.clone(),
            error: e.to_string(),
        };
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(io)?;
        }
        crate::fsutil::write_atomic(&self.path, text.as_bytes()).map_err(io)
    }

    pub fn accounts(&self) -> &[Account] {
        &self.accounts
    }

    pub fn current(&self) -> Option<&Account> {
        let id = self.current.as_ref()?;
        self.accounts.iter().find(|a| &a.id == id)
    }

    pub fn set_current(&mut self, id: &str) -> Result<(), AccountError> {
        if !self.accounts.iter().any(|a| a.id == id) {
            return Err(AccountError::NotFound(id.to_string()));
        }
        self.current = Some(id.to_string());
        self.current_changed = true;
        Ok(())
    }

    pub fn remove(&mut self, id: &str) -> Result<(), AccountError> {
        let pos = self
            .accounts
            .iter()
            .position(|a| a.id == id)
            .ok_or_else(|| AccountError::NotFound(id.to_string()))?;
        self.accounts.remove(pos);
        if self.current.as_deref() == Some(id) {
            self.current = self.accounts.first().map(|a| a.id.clone());
            self.current_changed = true;
        }
        Ok(())
    }

    /// Makes `idx` the current account when there is none yet.
    #[cfg(any(test, feature = "dev-offline"))]
    fn adopt_current(&mut self, idx: usize) {
        if self.current.is_none() {
            self.current = Some(self.accounts[idx].id.clone());
            self.current_changed = true;
        }
    }

    #[cfg(feature = "dev-offline")]
    pub fn add_offline(&mut self, username: &str) -> Result<&Account, AccountError> {
        let valid = (3..=16).contains(&username.len())
            && username
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_');
        if !valid {
            return Err(AccountError::InvalidName(username.to_string()));
        }
        let idx = match self.accounts.iter().position(|a| {
            a.kind == AccountKind::Offline && a.username.eq_ignore_ascii_case(username)
        }) {
            Some(i) => i,
            None => {
                let uuid = crate::launch::offline_uuid(username);
                self.accounts.push(Account {
                    id: format!("offline-{}", uuid.simple()),
                    kind: AccountKind::Offline,
                    username: username.to_string(),
                    uuid,
                    token_ref: None,
                });
                self.accounts.len() - 1
            }
        };
        self.adopt_current(idx);
        Ok(&self.accounts[idx])
    }

    /// Entry point for plan 3's sign-in flow; only tests use it for now.
    #[cfg(test)]
    pub(crate) fn add_msa(
        &mut self,
        username: &str,
        uuid: Uuid,
        token_ref: Option<String>,
    ) -> &Account {
        let id = uuid.simple().to_string();
        let idx = match self.accounts.iter().position(|a| a.id == id) {
            Some(i) => {
                self.accounts[i].username = username.to_string();
                self.accounts[i].token_ref = token_ref;
                i
            }
            None => {
                self.accounts.push(Account {
                    id,
                    kind: AccountKind::Msa,
                    username: username.to_string(),
                    uuid,
                    token_ref,
                });
                self.accounts.len() - 1
            }
        };
        self.adopt_current(idx);
        &self.accounts[idx]
    }

    pub fn launch_account(&self, id: &str) -> Result<LaunchAccount, AccountError> {
        let acct = self
            .accounts
            .iter()
            .find(|a| a.id == id)
            .ok_or_else(|| AccountError::NotFound(id.to_string()))?;
        match acct.kind {
            AccountKind::Msa => Err(AccountError::MsaUnavailable),
            #[cfg(feature = "dev-offline")]
            AccountKind::Offline => Ok(crate::launch::account::dev_offline_account(&acct.username)),
            #[cfg(not(feature = "dev-offline"))]
            AccountKind::Offline => Err(AccountError::NotFound(id.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dirs() -> (tempfile::TempDir, DataDirs) {
        let t = tempfile::tempdir().unwrap();
        let d = DataDirs::new(t.path());
        (t, d)
    }

    fn msa(n: u128) -> (String, Uuid) {
        (format!("Player{n}"), Uuid::from_u128(n))
    }

    #[test]
    fn missing_file_is_empty_and_garbage_is_parse_error() {
        let (_t, d) = dirs();
        let s = AccountStore::load(&d).unwrap();
        assert!(s.accounts().is_empty());
        assert!(s.current().is_none());
        std::fs::write(d.root().join("accounts.toml"), "not [valid").unwrap();
        assert!(matches!(
            AccountStore::load(&d),
            Err(AccountError::Parse(_))
        ));
        assert_eq!(
            std::fs::read_to_string(d.root().join("accounts.toml")).unwrap(),
            "not [valid"
        );
    }

    #[cfg(feature = "dev-offline")]
    #[test]
    fn add_offline_and_set_current_persist() {
        let (_t, d) = dirs();
        let mut s = AccountStore::load(&d).unwrap();
        let a = s.add_offline("Dev_One").unwrap().clone();
        assert_eq!(a.kind, AccountKind::Offline);
        assert_eq!(a.id, format!("offline-{}", a.uuid.simple()));
        assert_eq!(a.uuid, crate::launch::offline_uuid("Dev_One"));
        let b = s.add_offline("Other").unwrap().clone();
        // Duplicate name (case-insensitive) returns the existing account.
        assert_eq!(s.add_offline("dev_one").unwrap().id, a.id);
        assert_eq!(s.accounts().len(), 2);
        s.set_current(&b.id).unwrap();
        s.save().unwrap();

        let s2 = AccountStore::load(&d).unwrap();
        assert_eq!(s2.accounts().len(), 2);
        assert_eq!(s2.current().unwrap().id, b.id);
        let la = s2.launch_account(&a.id).unwrap();
        assert_eq!(la.name, "Dev_One");
        assert_eq!(la.uuid, a.uuid);
        assert!(matches!(
            s2.launch_account("nope"),
            Err(AccountError::NotFound(_))
        ));
    }

    #[cfg(feature = "dev-offline")]
    #[test]
    fn invalid_offline_names_rejected() {
        let (_t, d) = dirs();
        let mut s = AccountStore::load(&d).unwrap();
        for n in [
            "",
            "ab",
            "a".repeat(17).as_str(),
            "bad name",
            "名字名字",
            "a-b",
        ] {
            assert!(
                matches!(s.add_offline(n), Err(AccountError::InvalidName(_))),
                "{n}"
            );
        }
        assert!(s.add_offline("abc").is_ok());
        assert!(s.add_offline(&"a".repeat(16)).is_ok());
    }

    #[test]
    fn remove_current_moves_current() {
        let (_t, d) = dirs();
        let mut s = AccountStore::load(&d).unwrap();
        let (n1, u1) = msa(1);
        let (n2, u2) = msa(2);
        let (n3, u3) = msa(3);
        let a = s.add_msa(&n1, u1, None).id.clone();
        let b = s.add_msa(&n2, u2, None).id.clone();
        let c = s.add_msa(&n3, u3, None).id.clone();
        assert_eq!(a, u1.simple().to_string());
        s.set_current(&b).unwrap();
        s.remove(&b).unwrap();
        assert_eq!(s.current().unwrap().id, a);
        s.remove(&a).unwrap();
        assert_eq!(s.current().unwrap().id, c);
        s.save().unwrap();
        let mut s2 = AccountStore::load(&d).unwrap();
        assert_eq!(s2.current().unwrap().id, c);
        s2.remove(&c).unwrap();
        assert!(s2.current().is_none());
        assert!(matches!(s2.remove(&c), Err(AccountError::NotFound(_))));
        assert!(matches!(
            s2.set_current("zzz"),
            Err(AccountError::NotFound(_))
        ));
    }

    fn raw_file(d: &DataDirs) -> AccountsFile {
        toml::from_str(&std::fs::read_to_string(d.root().join("accounts.toml")).unwrap()).unwrap()
    }

    fn msa_entry(n: u128, name: &str) -> String {
        let u = Uuid::from_u128(n);
        format!(
            "[[accounts]]\nid = \"{}\"\nkind = \"msa\"\nusername = \"{name}\"\nuuid = \"{u}\"\n\n",
            u.simple()
        )
    }

    #[test]
    fn duplicate_ids_keep_the_first_entry() {
        let (_t, d) = dirs();
        std::fs::write(
            d.root().join("accounts.toml"),
            format!(
                "{}{}{}",
                msa_entry(1, "First"),
                msa_entry(2, "B"),
                msa_entry(1, "Dup")
            ),
        )
        .unwrap();
        let s = AccountStore::load(&d).unwrap();
        let names: Vec<_> = s.accounts().iter().map(|a| a.username.as_str()).collect();
        assert_eq!(names, ["First", "B"]);
        s.save().unwrap();
        assert_eq!(raw_file(&d).accounts.len(), 2);
    }

    #[test]
    fn dangling_stored_current_is_not_written_back() {
        let (_t, d) = dirs();
        std::fs::write(
            d.root().join("accounts.toml"),
            format!("current = \"ghost\"\n\n{}", msa_entry(1, "Only")),
        )
        .unwrap();
        let s = AccountStore::load(&d).unwrap();
        let id = Uuid::from_u128(1).simple().to_string();
        assert_eq!(s.current().unwrap().id, id);
        s.save().unwrap();
        assert_eq!(raw_file(&d).current.as_deref(), Some(id.as_str()));
    }

    #[test]
    fn add_msa_updates_an_existing_account() {
        let (_t, d) = dirs();
        let mut s = AccountStore::load(&d).unwrap();
        let (n1, u1) = msa(1);
        let (n2, u2) = msa(2);
        let a = s.add_msa(&n1, u1, None).id.clone();
        s.add_msa(&n2, u2, None);
        let again = s.add_msa("Renamed", u1, Some("ref-1".into())).clone();
        assert_eq!(again.id, a);
        assert_eq!(again.username, "Renamed");
        assert_eq!(again.token_ref.as_deref(), Some("ref-1"));
        assert_eq!(s.accounts().len(), 2);
        assert_eq!(s.current().unwrap().id, a, "updating keeps the current");
    }

    #[test]
    fn msa_launch_unavailable() {
        let (_t, d) = dirs();
        let mut s = AccountStore::load(&d).unwrap();
        let (n, u) = msa(7);
        let id = s.add_msa(&n, u, None).id.clone();
        assert!(matches!(
            s.launch_account(&id),
            Err(AccountError::MsaUnavailable)
        ));
        assert!(!AccountError::MsaUnavailable.to_string().is_empty());
    }

    #[cfg(not(feature = "dev-offline"))]
    #[test]
    fn release_build_hides_but_preserves_offline_accounts() {
        let (_t, d) = dirs();
        let off = Uuid::from_u128(0xabc);
        let msa_id = Uuid::from_u128(5).simple().to_string();
        let off_id = format!("offline-{}", off.simple());
        std::fs::write(
            d.root().join("accounts.toml"),
            format!(
                "current = \"{off_id}\"\n\n[[accounts]]\nid = \"{off_id}\"\nkind = \"offline\"\nusername = \"Dev\"\nuuid = \"{off}\"\n\n[[accounts]]\nid = \"{msa_id}\"\nkind = \"msa\"\nusername = \"Real\"\nuuid = \"{}\"\n",
                Uuid::from_u128(5)
            ),
        )
        .unwrap();

        let mut s = AccountStore::load(&d).unwrap();
        assert_eq!(s.accounts().len(), 1);
        assert_eq!(s.accounts()[0].id, msa_id);
        // Hidden current falls back to the first visible account.
        assert_eq!(s.current().unwrap().id, msa_id);
        // Offline accounts are hidden from every operation.
        assert!(matches!(
            s.launch_account(&off_id),
            Err(AccountError::NotFound(_))
        ));
        assert!(matches!(
            s.set_current(&off_id),
            Err(AccountError::NotFound(_))
        ));
        assert!(matches!(s.remove(&off_id), Err(AccountError::NotFound(_))));

        // Saving without a change keeps the original current and the offline entry.
        s.save().unwrap();
        let raw: AccountsFile =
            toml::from_str(&std::fs::read_to_string(d.root().join("accounts.toml")).unwrap())
                .unwrap();
        assert_eq!(raw.current.as_deref(), Some(off_id.as_str()));
        assert_eq!(raw.accounts.len(), 2);
        assert!(raw.accounts.iter().any(|a| a.id == off_id
            && a.kind == AccountKind::Offline
            && a.username == "Dev"
            && a.uuid == off));

        // Adding a Microsoft account keeps the offline one too.
        s.add_msa("Second", Uuid::from_u128(6), None);
        s.save().unwrap();
        let raw: AccountsFile =
            toml::from_str(&std::fs::read_to_string(d.root().join("accounts.toml")).unwrap())
                .unwrap();
        assert_eq!(raw.accounts.len(), 3);
        assert_eq!(raw.current.as_deref(), Some(off_id.as_str()));

        // An explicit set_current overrides the stored hidden current.
        s.set_current(&msa_id).unwrap();
        s.save().unwrap();
        let raw: AccountsFile =
            toml::from_str(&std::fs::read_to_string(d.root().join("accounts.toml")).unwrap())
                .unwrap();
        assert_eq!(raw.current.as_deref(), Some(msa_id.as_str()));
        assert!(raw.accounts.iter().any(|a| a.id == off_id));
    }
}
