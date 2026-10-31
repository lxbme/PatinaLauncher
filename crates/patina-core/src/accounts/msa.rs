//! Private Microsoft -> Xbox -> Minecraft protocol. Persistence and terminal
//! login events belong to AccountService, not this client.
use super::error::{AuthError, AuthStage, XboxRestriction};
use super::http::{AuthHttp, AuthRequest, AuthResponse, MicrosoftGrant, cancelled};
use super::login::{CancelToken, DeviceCode, LoginEvent};
use super::secret::SecretString;
use futures::future::BoxFuture;
use serde::Deserialize;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc::UnboundedSender;
use tokio::time::Instant;
use uuid::Uuid;

/// Wall-clock token expiry; polling uses Tokio's monotonic clock instead.
/// Public only through the test-support facade for deterministic collaborators.
pub trait AuthClock: Send + Sync {
    fn unix_seconds(&self) -> u64;
}
pub(crate) struct SystemClock;
impl AuthClock for SystemClock {
    fn unix_seconds(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }
}

#[derive(Debug)]
pub(crate) struct MicrosoftTokens {
    access_token: SecretString,
    refresh_token: SecretString,
    expires_at: u64,
}
impl MicrosoftTokens {
    #[allow(
        dead_code,
        reason = "Task 4 persists rotation before complete_minecraft"
    )]
    pub(crate) fn refresh_token(&self) -> &SecretString {
        &self.refresh_token
    }
    #[allow(dead_code, reason = "Task 4 inspects Microsoft token validity")]
    pub(crate) fn expires_at(&self) -> u64 {
        self.expires_at
    }
}

#[derive(Debug)]
pub(crate) struct MinecraftSession {
    access_token: SecretString,
    expires_at: u64,
}
impl MinecraftSession {
    #[allow(
        dead_code,
        reason = "Task 3 serializes the protected Minecraft session"
    )]
    pub(crate) fn access_token(&self) -> &SecretString {
        &self.access_token
    }
    #[allow(dead_code, reason = "Task 3 persists and Task 4 checks session expiry")]
    pub(crate) fn expires_at(&self) -> u64 {
        self.expires_at
    }
}

#[derive(Debug)]
pub(crate) struct MinecraftProfile {
    uuid: Uuid,
    username: String,
    xuid: Option<String>,
    skin_hash: Option<String>,
}
impl MinecraftProfile {
    #[allow(dead_code, reason = "Task 4 commits verified profile metadata")]
    pub(crate) fn uuid(&self) -> Uuid {
        self.uuid
    }
    #[allow(dead_code, reason = "Task 4 commits verified profile metadata")]
    pub(crate) fn username(&self) -> &str {
        &self.username
    }
    #[allow(dead_code, reason = "Task 4 commits verified profile metadata")]
    pub(crate) fn xuid(&self) -> Option<&str> {
        self.xuid.as_deref()
    }
    #[allow(dead_code, reason = "Task 4 commits safe skin metadata")]
    pub(crate) fn skin_hash(&self) -> Option<&str> {
        self.skin_hash.as_deref()
    }
}

#[derive(Debug)]
pub(crate) struct VerifiedLogin {
    profile: MinecraftProfile,
    refresh_token: SecretString,
    session: MinecraftSession,
    verified_at: u64,
}
impl VerifiedLogin {
    #[allow(
        dead_code,
        reason = "Task 4 transfers the verified result into durable records"
    )]
    pub(crate) fn into_parts(self) -> (MinecraftProfile, SecretString, MinecraftSession, u64) {
        (
            self.profile,
            self.refresh_token,
            self.session,
            self.verified_at,
        )
    }

    #[allow(dead_code, reason = "Task 4 commits verified profile metadata")]
    pub(crate) fn profile(&self) -> &MinecraftProfile {
        &self.profile
    }
    #[allow(dead_code, reason = "Task 4 commits the protected refresh token")]
    pub(crate) fn refresh_token(&self) -> &SecretString {
        &self.refresh_token
    }
    #[allow(dead_code, reason = "Task 3/4 persist the protected Minecraft session")]
    pub(crate) fn session(&self) -> &MinecraftSession {
        &self.session
    }
    #[allow(dead_code, reason = "Task 4 records historical verification time")]
    pub(crate) fn verified_at(&self) -> u64 {
        self.verified_at
    }
}

/// No public token or verified-result constructor. Test support only injects
/// the real request-level transport and a deterministic clock.
#[derive(Clone)]
pub struct MsaClient {
    http: AuthHttp,
    clock: Arc<dyn AuthClock>,
}
impl MsaClient {
    #[allow(dead_code, reason = "Task 4 constructs the production AccountService")]
    pub(crate) fn production() -> Result<Self, AuthError> {
        Ok(Self::new(AuthHttp::production()?, Arc::new(SystemClock)))
    }
    pub(crate) fn new(http: AuthHttp, clock: Arc<dyn AuthClock>) -> Self {
        Self { http, clock }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn for_test(http: AuthHttp, clock: Arc<dyn AuthClock>) -> Self {
        Self::new(http, clock)
    }

    #[allow(dead_code, reason = "Task 4 owns login commit and terminal events")]
    pub(crate) fn sign_in(
        &self,
        events: UnboundedSender<LoginEvent>,
        mut cancel: CancelToken,
    ) -> BoxFuture<'static, Result<VerifiedLogin, AuthError>> {
        let client = self.clone();
        Box::pin(async move {
            let request_cancel = cancel.clone();
            tokio::select! {
                biased;
                _ = cancelled(&mut cancel) => Err(AuthError::Cancelled),
                _ = events.closed() => Err(AuthError::Cancelled),
                result = client.sign_in_inner(&events, request_cancel) => result,
            }
        })
    }

    async fn sign_in_inner(
        &self,
        events: &UnboundedSender<LoginEvent>,
        cancel: CancelToken,
    ) -> Result<VerifiedLogin, AuthError> {
        let stage = AuthStage::DeviceCode;
        let response = self
            .http
            .send(AuthRequest::DeviceCode, cancel.clone())
            .await?;
        let code: DeviceResponse = success(&response, stage)?;
        nonempty(code.device_code.expose(), stage)?;
        nonempty(&code.user_code, stage)?;
        if !matches!(
            code.verification_uri.as_str(),
            "https://microsoft.com/link"
                | "https://www.microsoft.com/link"
                | "https://www.microsoft.com/devicelogin"
        ) {
            return Err(invalid(stage));
        }
        let expires_in = self.duration(code.expires_in, stage)?;
        let mut interval = self.duration(code.interval.unwrap_or(5), stage)?;
        let deadline = Instant::now()
            .checked_add(expires_in)
            .ok_or_else(|| invalid(stage))?;
        events
            .send(LoginEvent::Code(DeviceCode {
                user_code: code.user_code,
                verification_uri: code.verification_uri,
                expires_in,
            }))
            .map_err(|_| AuthError::Cancelled)?;
        events
            .send(LoginEvent::Waiting)
            .map_err(|_| AuthError::Cancelled)?;
        let mut wait = interval;
        let tokens = loop {
            // Both sleeps and in-flight requests are bounded by the same deadline.
            // Success leaves this deadline behind before Xbox/Minecraft validation.
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(AuthError::Expired);
            }
            tokio::time::sleep(wait.min(remaining)).await;
            if Instant::now() >= deadline {
                return Err(AuthError::Expired);
            }
            let result = tokio::select! {
                biased;
                _ = tokio::time::sleep_until(deadline) => return Err(AuthError::Expired),
                result = self.http.send(AuthRequest::MicrosoftToken(MicrosoftGrant::DeviceCode(code.device_code.clone())), cancel.clone()) => result,
            };
            let response = match result {
                Err(AuthError::Timeout { .. }) => {
                    interval = interval
                        .saturating_mul(2)
                        .min(deadline.saturating_duration_since(Instant::now()));
                    wait = interval;
                    continue;
                }
                other => other?,
            };
            if response.status() == 429 {
                wait = response
                    .retry_after()
                    .unwrap_or(interval)
                    .max(interval)
                    .min(deadline.saturating_duration_since(Instant::now()));
                continue;
            }
            if response.status() == 400 {
                let error: OAuthError = parse(&response, AuthStage::MicrosoftToken)?;
                match error.error.as_str() {
                    "authorization_pending" => {}
                    "slow_down" => interval = interval.saturating_add(Duration::from_secs(5)),
                    "access_denied" | "authorization_declined" => return Err(AuthError::Denied),
                    "expired_token" => return Err(AuthError::Expired),
                    _ => return Err(invalid(AuthStage::MicrosoftToken)),
                }
                wait = interval;
                continue;
            }
            break self.microsoft_tokens(&response, None)?;
        };
        self.complete_minecraft(tokens, cancel).await
    }

    #[allow(
        dead_code,
        reason = "Task 4 persists refresh rotation before downstream verification"
    )]
    pub(crate) fn refresh_microsoft(
        &self,
        refresh: SecretString,
        cancel: CancelToken,
    ) -> BoxFuture<'static, Result<MicrosoftTokens, AuthError>> {
        let client = self.clone();
        Box::pin(async move {
            let response = client
                .http
                .send(
                    AuthRequest::MicrosoftToken(MicrosoftGrant::Refresh(refresh.clone())),
                    cancel,
                )
                .await?;
            if response.status() == 400 {
                let error: OAuthError = parse(&response, AuthStage::MicrosoftToken)?;
                return Err(if error.error == "invalid_grant" {
                    AuthError::ReauthenticationRequired
                } else {
                    invalid(AuthStage::MicrosoftToken)
                });
            }
            client.microsoft_tokens(&response, Some(refresh))
        })
    }

    fn microsoft_tokens(
        &self,
        response: &AuthResponse,
        prior: Option<SecretString>,
    ) -> Result<MicrosoftTokens, AuthError> {
        let stage = AuthStage::MicrosoftToken;
        let tokens: MicrosoftResponse = success(response, stage)?;
        nonempty(tokens.access_token.expose(), stage)?;
        let refresh_token = tokens
            .refresh_token
            .or(prior)
            .ok_or_else(|| invalid(stage))?;
        nonempty(refresh_token.expose(), stage)?;
        Ok(MicrosoftTokens {
            access_token: tokens.access_token,
            refresh_token,
            expires_at: self.expiry(tokens.expires_in, stage)?,
        })
    }

    pub(crate) fn complete_minecraft(
        &self,
        tokens: MicrosoftTokens,
        mut cancel: CancelToken,
    ) -> BoxFuture<'static, Result<VerifiedLogin, AuthError>> {
        let client = self.clone();
        Box::pin(async move {
            let request_cancel = cancel.clone();
            tokio::select! {
                biased;
                _ = cancelled(&mut cancel) => Err(AuthError::Cancelled),
                result = client.complete_minecraft_inner(tokens, request_cancel) => result,
            }
        })
    }

    async fn complete_minecraft_inner(
        &self,
        tokens: MicrosoftTokens,
        cancel: CancelToken,
    ) -> Result<VerifiedLogin, AuthError> {
        if tokens.expires_at <= self.clock.unix_seconds() {
            return Err(AuthError::SessionExpired);
        }
        let response = self
            .http
            .send(
                AuthRequest::XboxLive {
                    access_token: tokens.access_token,
                },
                cancel.clone(),
            )
            .await?;
        let xbox = self.xbox(&response, AuthStage::XboxLive)?;
        let response = self
            .http
            .send(
                AuthRequest::Xsts {
                    xbox_token: xbox.token,
                },
                cancel.clone(),
            )
            .await?;
        if !(200..300).contains(&response.status()) {
            let restriction = serde_json::from_slice::<XstsError>(response.body())
                .ok()
                .and_then(|e| match e.code {
                    Some(2148916233) => Some(XboxRestriction::AccountRequired),
                    Some(2148916235) => Some(XboxRestriction::Region),
                    Some(2148916236 | 2148916237) => Some(XboxRestriction::AdultVerification),
                    Some(2148916238) => Some(XboxRestriction::Family),
                    _ => None,
                });
            return Err(restriction
                .map(AuthError::XboxRestriction)
                .unwrap_or(AuthError::Http {
                    stage: AuthStage::Xsts,
                    status: response.status(),
                }));
        }
        let xsts = self.xbox(&response, AuthStage::Xsts)?;
        if xbox.user_hash.expose() != xsts.user_hash.expose() {
            return Err(AuthError::IdentityMismatch);
        }
        let response = self
            .http
            .send(
                AuthRequest::MinecraftToken {
                    user_hash: xsts.user_hash,
                    xsts_token: xsts.token,
                },
                cancel.clone(),
            )
            .await?;
        let mc: MinecraftResponse = success(&response, AuthStage::MinecraftToken)?;
        nonempty(mc.access_token.expose(), AuthStage::MinecraftToken)?;
        if !mc.token_type.eq_ignore_ascii_case("bearer") {
            return Err(invalid(AuthStage::MinecraftToken));
        }
        let session = MinecraftSession {
            access_token: mc.access_token,
            expires_at: self.expiry(mc.expires_in, AuthStage::MinecraftToken)?,
        };
        let response = self
            .http
            .send(
                AuthRequest::Entitlement {
                    access_token: session.access_token.clone(),
                },
                cancel.clone(),
            )
            .await?;
        let entitlement: Entitlements = success(&response, AuthStage::Entitlement)?;
        if !entitlement
            .items
            .iter()
            .any(|item| matches!(item.name.as_str(), "game_minecraft" | "product_minecraft"))
        {
            return Err(AuthError::NoEntitlement);
        }
        let response = self
            .http
            .send(
                AuthRequest::Profile {
                    access_token: session.access_token.clone(),
                },
                cancel,
            )
            .await?;
        if response.status() == 404 {
            return Err(AuthError::ProfileMissing);
        }
        let profile: ProfileResponse = success(&response, AuthStage::Profile)?;
        let uuid = strict_uuid(&profile.id).ok_or_else(|| invalid(AuthStage::Profile))?;
        if !(3..=16).contains(&profile.name.len())
            || !profile
                .name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            return Err(invalid(AuthStage::Profile));
        }
        let skin_hash = profile.skins.as_array().and_then(|skins| {
            skins.iter().find_map(|skin| {
                if skin.get("state")?.as_str()? != "ACTIVE" {
                    return None;
                }
                let hash = skin
                    .get("url")?
                    .as_str()?
                    .strip_prefix("https://textures.minecraft.net/texture/")?;
                (!hash.is_empty() && hash.bytes().all(|b| b.is_ascii_hexdigit()))
                    .then(|| hash.to_ascii_lowercase())
            })
        });
        Ok(VerifiedLogin {
            profile: MinecraftProfile {
                uuid,
                username: profile.name,
                xuid: xsts.xuid.or(xbox.xuid),
                skin_hash,
            },
            refresh_token: tokens.refresh_token,
            session,
            verified_at: self.clock.unix_seconds(),
        })
    }

    fn xbox(&self, response: &AuthResponse, stage: AuthStage) -> Result<XboxToken, AuthError> {
        let body: XboxResponse = success(response, stage)?;
        nonempty(body.token.expose(), stage)?;
        let expiry = chrono::DateTime::parse_from_rfc3339(&body.not_after)
            .map_err(|_| invalid(stage))?
            .timestamp();
        if expiry < 0 || expiry as u64 <= self.clock.unix_seconds() {
            return Err(invalid(stage));
        }
        let claim = body
            .claims
            .xui
            .into_iter()
            .next()
            .ok_or_else(|| invalid(stage))?;
        nonempty(claim.uhs.expose(), stage)?;
        let xuid = claim
            .xid
            .or(claim.xuid)
            .filter(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()));
        Ok(XboxToken {
            token: body.token,
            user_hash: claim.uhs,
            xuid,
        })
    }
    fn duration(&self, seconds: u64, stage: AuthStage) -> Result<Duration, AuthError> {
        let duration = Duration::from_secs(seconds);
        if seconds == 0
            || self.clock.unix_seconds().checked_add(seconds).is_none()
            || Instant::now().checked_add(duration).is_none()
        {
            return Err(invalid(stage));
        }
        Ok(duration)
    }
    fn expiry(&self, seconds: u64, stage: AuthStage) -> Result<u64, AuthError> {
        self.duration(seconds, stage)?;
        self.clock
            .unix_seconds()
            .checked_add(seconds)
            .ok_or_else(|| invalid(stage))
    }
}

fn invalid(stage: AuthStage) -> AuthError {
    AuthError::InvalidResponse { stage }
}
fn nonempty(value: &str, stage: AuthStage) -> Result<(), AuthError> {
    if value.trim().is_empty() {
        Err(invalid(stage))
    } else {
        Ok(())
    }
}
fn parse<T: serde::de::DeserializeOwned>(
    response: &AuthResponse,
    stage: AuthStage,
) -> Result<T, AuthError> {
    serde_json::from_slice(response.body()).map_err(|_| invalid(stage))
}
fn success<T: serde::de::DeserializeOwned>(
    response: &AuthResponse,
    stage: AuthStage,
) -> Result<T, AuthError> {
    if !(200..300).contains(&response.status()) {
        return Err(AuthError::Http {
            stage,
            status: response.status(),
        });
    }
    parse(response, stage)
}
fn strict_uuid(id: &str) -> Option<Uuid> {
    let valid = match id.len() {
        32 => id.bytes().all(|b| b.is_ascii_hexdigit()),
        36 => id.bytes().enumerate().all(|(i, b)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        }),
        _ => false,
    };
    valid.then(|| Uuid::parse_str(id).ok()).flatten()
}

// These DTOs deliberately have no Debug and ignore remote diagnostics/messages.
#[derive(Deserialize)]
struct DeviceResponse {
    device_code: SecretString,
    user_code: String,
    verification_uri: String,
    expires_in: u64,
    interval: Option<u64>,
}
#[derive(Deserialize)]
struct MicrosoftResponse {
    access_token: SecretString,
    refresh_token: Option<SecretString>,
    expires_in: u64,
}
#[derive(Deserialize)]
struct OAuthError {
    error: String,
}
#[derive(Deserialize)]
struct XstsError {
    #[serde(rename = "XErr")]
    code: Option<u64>,
}
#[derive(Deserialize)]
struct XboxResponse {
    #[serde(rename = "Token")]
    token: SecretString,
    #[serde(rename = "NotAfter")]
    not_after: String,
    #[serde(rename = "DisplayClaims")]
    claims: XboxClaims,
}
#[derive(Deserialize)]
struct XboxClaims {
    xui: Vec<XboxClaim>,
}
#[derive(Deserialize)]
struct XboxClaim {
    uhs: SecretString,
    xid: Option<String>,
    xuid: Option<String>,
}
struct XboxToken {
    token: SecretString,
    user_hash: SecretString,
    xuid: Option<String>,
}
#[derive(Deserialize)]
struct MinecraftResponse {
    access_token: SecretString,
    expires_in: u64,
    token_type: String,
}
#[derive(Deserialize)]
struct Entitlements {
    items: Vec<Entitlement>,
}
#[derive(Deserialize)]
struct Entitlement {
    name: String,
}
#[derive(Deserialize)]
struct ProfileResponse {
    id: String,
    name: String,
    #[serde(default)]
    skins: serde_json::Value,
}
