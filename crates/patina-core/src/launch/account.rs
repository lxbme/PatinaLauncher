use md5::{Digest, Md5};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UserType {
    Msa,
    Legacy,
}

impl UserType {
    pub fn as_str(&self) -> &'static str {
        match self {
            UserType::Msa => "msa",
            UserType::Legacy => "legacy",
        }
    }
}

/// The identity a game is launched with. Fields and the Microsoft
/// constructor are crate-private, so outside this crate one comes only from
/// the account store; offline ones exist only with the `dev-offline` feature
/// (and test stand-ins, `test_account`, only with `test-support`).
#[derive(Clone)]
pub struct LaunchAccount {
    pub(crate) name: String,
    pub(crate) uuid: Uuid,
    pub(crate) access_token: String,
    pub(crate) user_type: UserType,
    pub(crate) xuid: Option<String>,
}

impl LaunchAccount {
    /// A signed-in Microsoft account. Crate-private: outside the crate a
    /// launch identity only comes from the account store, so a release build
    /// cannot fabricate one with an arbitrary token.
    #[cfg_attr(
        not(test),
        allow(dead_code, reason = "plan 3 builds MSA accounts from AccountStore")
    )]
    pub(crate) fn msa(name: &str, uuid: Uuid, access_token: &str, xuid: Option<String>) -> Self {
        LaunchAccount {
            name: name.to_string(),
            uuid,
            access_token: access_token.to_string(),
            user_type: UserType::Msa,
            xuid,
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn uuid(&self) -> Uuid {
        self.uuid
    }

    pub fn user_type(&self) -> UserType {
        self.user_type
    }
}

impl std::fmt::Debug for LaunchAccount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LaunchAccount")
            .field("name", &self.name)
            .field("uuid", &self.uuid)
            .field("access_token", &"<redacted>")
            .field("user_type", &self.user_type)
            .field("xuid", &self.xuid.as_ref().map(|_| "<present>"))
            .finish()
    }
}

/// Same as Java's `UUID.nameUUIDFromBytes(("OfflinePlayer:" + name).getBytes(UTF_8))`.
pub fn offline_uuid(name: &str) -> Uuid {
    let digest = Md5::digest(format!("OfflinePlayer:{name}").as_bytes());
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest);
    uuid::Builder::from_md5_bytes(bytes).into_uuid()
}

/// Development-only offline account; never available in default builds.
#[cfg(feature = "dev-offline")]
pub fn dev_offline_account(name: &str) -> LaunchAccount {
    LaunchAccount {
        name: name.to_string(),
        uuid: offline_uuid(name),
        access_token: "0".to_string(),
        user_type: UserType::Legacy,
        xuid: None,
    }
}

/// A stand-in launch identity for other crates' tests (an offline-shaped
/// player with a dummy token). Only with the `test-support` feature, which
/// release builds never enable.
#[cfg(any(test, feature = "test-support"))]
pub fn test_account(name: &str) -> LaunchAccount {
    LaunchAccount {
        name: name.to_string(),
        uuid: offline_uuid(name),
        access_token: "0".to_string(),
        user_type: UserType::Legacy,
        xuid: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offline_uuid_matches_java() {
        assert_eq!(
            offline_uuid("Notch").to_string(),
            "b50ad385-829d-3141-a216-7e7d7539ba7f"
        );
        assert_eq!(
            offline_uuid("Dev").to_string(),
            "380df991-f603-344c-a090-369bad2a924a"
        );
    }

    #[test]
    fn msa_constructor_shape() {
        let a = LaunchAccount::msa("Player", offline_uuid("x"), "s3cret", Some("xu".into()));
        assert_eq!(a.name(), "Player");
        assert_eq!(a.uuid(), offline_uuid("x"));
        assert_eq!(a.user_type(), UserType::Msa);
        assert_eq!(
            (a.access_token.as_str(), a.xuid.as_deref()),
            ("s3cret", Some("xu"))
        );
        let dbg = format!("{a:?}");
        assert!(!dbg.contains("s3cret") && !dbg.contains("\"xu\""), "{dbg}");
    }

    #[test]
    fn user_type_strings() {
        assert_eq!(UserType::Msa.as_str(), "msa");
        assert_eq!(UserType::Legacy.as_str(), "legacy");
    }

    #[cfg(feature = "dev-offline")]
    #[test]
    fn dev_account_shape() {
        let a = dev_offline_account("Dev");
        assert_eq!(a.uuid, offline_uuid("Dev"));
        assert_eq!(a.access_token, "0");
        assert_eq!(a.user_type, UserType::Legacy);
        assert!(a.xuid.is_none());
    }
}
