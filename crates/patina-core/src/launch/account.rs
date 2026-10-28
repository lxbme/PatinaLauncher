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

#[derive(Clone)]
pub struct LaunchAccount {
    pub name: String,
    pub uuid: Uuid,
    pub access_token: String,
    pub user_type: UserType,
    pub xuid: Option<String>,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offline_uuid_matches_java() {
        assert_eq!(offline_uuid("Notch").to_string(), "b50ad385-829d-3141-a216-7e7d7539ba7f");
        assert_eq!(offline_uuid("Dev").to_string(), "380df991-f603-344c-a090-369bad2a924a");
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
