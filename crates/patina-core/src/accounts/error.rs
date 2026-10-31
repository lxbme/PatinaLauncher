//! Typed authentication failures. Never retain remote text or external error sources.
use super::AccountError;
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthStage {
    DeviceCode,
    MicrosoftToken,
    XboxLive,
    Xsts,
    MinecraftToken,
    Entitlement,
    Profile,
    Skin,
}

impl AuthStage {
    pub fn display_name(self) -> String {
        match self {
            Self::DeviceCode => rust_i18n::t!("auth.stage.device_code").to_string(),
            Self::MicrosoftToken => rust_i18n::t!("auth.stage.microsoft_token").to_string(),
            Self::XboxLive => rust_i18n::t!("auth.stage.xbox_live").to_string(),
            Self::Xsts => rust_i18n::t!("auth.stage.xsts").to_string(),
            Self::MinecraftToken => rust_i18n::t!("auth.stage.minecraft_token").to_string(),
            Self::Entitlement => rust_i18n::t!("auth.stage.entitlement").to_string(),
            Self::Profile => rust_i18n::t!("auth.stage.profile").to_string(),
            Self::Skin => rust_i18n::t!("auth.stage.skin").to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XboxRestriction {
    AccountRequired,
    Region,
    AdultVerification,
    Family,
    Unknown,
}

#[derive(Clone, PartialEq, Eq)]
pub enum AuthError {
    Cancelled,
    Expired,
    Denied,
    Network { stage: AuthStage },
    Http { stage: AuthStage, status: u16 },
    InvalidResponse { stage: AuthStage },
    XboxRestriction(XboxRestriction),
    NoEntitlement,
    ProfileMissing,
    IdentityMismatch,
    ReauthenticationRequired,
    StorageUnavailable,
    CredentialMissing,
    CredentialCorrupt,
    CredentialWrite,
    UnsafeCredentialPath,
    Account(AccountError),
    OfflineNotAllowed,
    OfflineConflict,
    Busy,
    SessionExpired,
}

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::Cancelled => rust_i18n::t!("errors.auth.cancelled"),
            Self::Expired => rust_i18n::t!("errors.auth.expired"),
            Self::Denied => rust_i18n::t!("errors.auth.denied"),
            Self::NoEntitlement => rust_i18n::t!("errors.auth.no_entitlement"),
            Self::ProfileMissing => rust_i18n::t!("errors.auth.profile_missing"),
            Self::IdentityMismatch => rust_i18n::t!("errors.auth.identity_mismatch"),
            Self::ReauthenticationRequired => {
                rust_i18n::t!("errors.auth.reauthentication_required")
            }
            Self::StorageUnavailable => rust_i18n::t!("errors.auth.storage_unavailable"),
            Self::CredentialMissing => rust_i18n::t!("errors.auth.credential_missing"),
            Self::CredentialCorrupt => rust_i18n::t!("errors.auth.credential_corrupt"),
            Self::CredentialWrite => rust_i18n::t!("errors.auth.credential_write"),
            Self::UnsafeCredentialPath => rust_i18n::t!("errors.auth.unsafe_credential_path"),
            Self::OfflineNotAllowed => rust_i18n::t!("errors.auth.offline_not_allowed"),
            Self::OfflineConflict => rust_i18n::t!("errors.auth.offline_conflict"),
            Self::Busy => rust_i18n::t!("errors.auth.busy"),
            Self::SessionExpired => rust_i18n::t!("errors.auth.session_expired"),
            Self::Network { stage } => {
                rust_i18n::t!("errors.auth.network", stage = stage.display_name())
            }
            Self::Http { stage, status } => rust_i18n::t!(
                "errors.auth.http",
                stage = stage.display_name(),
                status = status
            ),
            Self::InvalidResponse { stage } => {
                rust_i18n::t!("errors.auth.invalid_response", stage = stage.display_name())
            }
            // Legacy AccountError carries paths, names and raw parser/I/O strings.
            // Keep it for typed handling, but never delegate formatting or source().
            Self::Account(_) => rust_i18n::t!("errors.auth.account"),
            Self::XboxRestriction(restriction) => match restriction {
                XboxRestriction::AccountRequired => {
                    rust_i18n::t!("errors.auth.xbox.account_required")
                }
                XboxRestriction::Region => rust_i18n::t!("errors.auth.xbox.region"),
                XboxRestriction::AdultVerification => {
                    rust_i18n::t!("errors.auth.xbox.adult_verification")
                }
                XboxRestriction::Family => rust_i18n::t!("errors.auth.xbox.family"),
                XboxRestriction::Unknown => rust_i18n::t!("errors.auth.xbox.unknown"),
            },
        };
        f.write_str(&message)
    }
}

impl fmt::Debug for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Display contains only local translations, closed stages and numeric status.
        write!(f, "AuthError({self})")
    }
}

impl std::error::Error for AuthError {}
