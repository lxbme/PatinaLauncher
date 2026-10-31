//! Authentication-only transport. No caller-supplied clients or production URLs.
use super::error::{AuthError, AuthStage};
use super::login::CancelToken;
use super::secret::SecretString;
use futures::future::BoxFuture;
use reqwest::{Client, Request, Url, header, redirect::Policy};
use std::time::Duration;
use zeroize::Zeroizing;

pub const DEFAULT_MSA_CLIENT_ID: &str = "e8d9cf1c-b85b-4413-876d-190d5e552218";
pub fn msa_client_id() -> &'static str {
    option_env!("PATINA_MSA_CLIENT_ID").unwrap_or(DEFAULT_MSA_CLIENT_ID)
}

const ENDPOINTS: [(AuthStage, &str); 7] = [
    (
        AuthStage::DeviceCode,
        "https://login.microsoftonline.com/consumers/oauth2/v2.0/devicecode",
    ),
    (
        AuthStage::MicrosoftToken,
        "https://login.microsoftonline.com/consumers/oauth2/v2.0/token",
    ),
    (
        AuthStage::XboxLive,
        "https://user.auth.xboxlive.com/user/authenticate",
    ),
    (
        AuthStage::Xsts,
        "https://xsts.auth.xboxlive.com/xsts/authorize",
    ),
    (
        AuthStage::MinecraftToken,
        "https://api.minecraftservices.com/authentication/login_with_xbox",
    ),
    (
        AuthStage::Entitlement,
        "https://api.minecraftservices.com/entitlements/mcstore",
    ),
    (
        AuthStage::Profile,
        "https://api.minecraftservices.com/minecraft/profile",
    ),
];
pub(crate) const MSA_SCOPE: &str = "XboxLive.signin offline_access";
const MAX_BODY: usize = 1024 * 1024;

#[derive(Debug)]
#[allow(dead_code, reason = "Closed requests are consumed by Task 2 MsaClient")]
pub(crate) enum MicrosoftGrant {
    DeviceCode(SecretString),
    Refresh(SecretString),
}

#[derive(Debug)]
#[allow(dead_code, reason = "Closed requests are consumed by Task 2 MsaClient")]
pub(crate) enum AuthRequest {
    DeviceCode,
    MicrosoftToken(MicrosoftGrant),
    XboxLive {
        access_token: SecretString,
    },
    Xsts {
        xbox_token: SecretString,
    },
    MinecraftToken {
        user_hash: SecretString,
        xsts_token: SecretString,
    },
    Entitlement {
        access_token: SecretString,
    },
    Profile {
        access_token: SecretString,
    },
}

impl AuthRequest {
    fn stage(&self) -> AuthStage {
        match self {
            Self::DeviceCode => AuthStage::DeviceCode,
            Self::MicrosoftToken(_) => AuthStage::MicrosoftToken,
            Self::XboxLive { .. } => AuthStage::XboxLive,
            Self::Xsts { .. } => AuthStage::Xsts,
            Self::MinecraftToken { .. } => AuthStage::MinecraftToken,
            Self::Entitlement { .. } => AuthStage::Entitlement,
            Self::Profile { .. } => AuthStage::Profile,
        }
    }
}

// Deliberately no Debug: bounded response bytes may still contain credentials.
#[allow(dead_code, reason = "Response parsing belongs to Task 2 MsaClient")]
pub(crate) struct AuthResponse {
    status: u16,
    body: Zeroizing<Vec<u8>>,
    retry_after: Option<Duration>,
}

#[allow(dead_code, reason = "Response parsing belongs to Task 2 MsaClient")]
impl AuthResponse {
    pub(crate) fn status(&self) -> u16 {
        self.status
    }
    pub(crate) fn body(&self) -> &[u8] {
        &self.body
    }
    pub(crate) fn retry_after(&self) -> Option<Duration> {
        self.retry_after
    }
}

/// Exported only through `accounts::testing` for isolated downstream fixtures.
#[derive(Clone)]
pub struct AuthHttp {
    client: Client,
    endpoints: [(AuthStage, Url); 7],
}

impl AuthHttp {
    #[allow(
        dead_code,
        reason = "Production construction is wired into Task 2 MsaClient"
    )]
    pub(crate) fn production() -> Result<Self, AuthError> {
        if msa_client_id().trim().is_empty() {
            return Err(AuthError::InvalidResponse {
                stage: AuthStage::DeviceCode,
            });
        }
        let endpoints =
            ENDPOINTS.map(|(stage, url)| (stage, Url::parse(url).expect("fixed auth URL")));
        Ok(Self {
            client: Self::client()?,
            endpoints,
        })
    }

    fn client() -> Result<Client, AuthError> {
        Client::builder()
            .redirect(Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(15))
            .no_proxy()
            .referer(false)
            .connection_verbose(false)
            .build()
            .map_err(|_| AuthError::Network {
                stage: AuthStage::DeviceCode,
            })
    }

    /// Test endpoints must be literal loopback IPs; no DNS, userinfo or URL secrets.
    #[cfg(any(test, feature = "test-support"))]
    pub fn for_test(endpoints: [(AuthStage, String); 7]) -> Result<Self, AuthError> {
        let mut parsed = Vec::with_capacity(7);
        for (stage, endpoint) in endpoints {
            let invalid = || AuthError::InvalidResponse { stage };
            if !ENDPOINTS.iter().any(|(s, _)| *s == stage)
                || parsed.iter().any(|(s, _)| *s == stage)
            {
                return Err(invalid());
            }
            let url = Url::parse(&endpoint).map_err(|_| invalid())?;
            let loopback = url
                .host_str()
                .and_then(|host| {
                    host.trim_matches(['[', ']'])
                        .parse::<std::net::IpAddr>()
                        .ok()
                })
                .is_some_and(|ip| ip.is_loopback());
            if !loopback
                || url.scheme() != "http"
                || !url.username().is_empty()
                || url.password().is_some()
                || url.query().is_some()
                || url.fragment().is_some()
            {
                return Err(invalid());
            }
            parsed.push((stage, url));
        }
        let endpoints = parsed.try_into().map_err(|_| AuthError::InvalidResponse {
            stage: AuthStage::DeviceCode,
        })?;
        Ok(Self {
            client: Self::client()?,
            endpoints,
        })
    }

    pub(super) fn build_request(&self, request: AuthRequest) -> Result<Request, AuthError> {
        let stage = request.stage();
        let url = self
            .endpoints
            .iter()
            .find(|(s, _)| *s == stage)
            .map(|(_, url)| url.clone())
            .expect("complete auth stage set");
        let builder = match request {
            AuthRequest::DeviceCode => self.client.post(url).form(&[
                ("client_id", msa_client_id()), ("scope", MSA_SCOPE),
            ]),
            AuthRequest::MicrosoftToken(MicrosoftGrant::DeviceCode(code)) => self.client.post(url).form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("client_id", msa_client_id()), ("device_code", code.expose()),
            ]),
            AuthRequest::MicrosoftToken(MicrosoftGrant::Refresh(token)) => self.client.post(url).form(&[
                ("grant_type", "refresh_token"), ("client_id", msa_client_id()),
                ("refresh_token", token.expose()), ("scope", MSA_SCOPE),
            ]),
            AuthRequest::XboxLive { access_token } => self.client.post(url).json(&serde_json::json!({
                "Properties": {"AuthMethod": "RPS", "SiteName": "user.auth.xboxlive.com", "RpsTicket": format!("d={}", access_token.expose())},
                "RelyingParty": "http://auth.xboxlive.com", "TokenType": "JWT",
            })),
            AuthRequest::Xsts { xbox_token } => self.client.post(url).json(&serde_json::json!({
                "Properties": {"SandboxId": "RETAIL", "UserTokens": [xbox_token.expose()]},
                "RelyingParty": "rp://api.minecraftservices.com/", "TokenType": "JWT",
            })),
            AuthRequest::MinecraftToken { user_hash, xsts_token } => self.client.post(url).json(&serde_json::json!({
                "identityToken": format!("XBL3.0 x={};{}", user_hash.expose(), xsts_token.expose()),
            })),
            AuthRequest::Entitlement { access_token } | AuthRequest::Profile { access_token } => {
                let mut bearer = header::HeaderValue::from_str(&format!("Bearer {}", access_token.expose()))
                    .map_err(|_| AuthError::InvalidResponse { stage })?;
                bearer.set_sensitive(true);
                self.client.get(url).header(header::AUTHORIZATION, bearer)
            }
        };
        builder
            .build()
            .map_err(|_| AuthError::InvalidResponse { stage })
    }

    #[allow(dead_code, reason = "Requests are sent by Task 2 MsaClient")]
    pub(crate) fn send(
        &self,
        request: AuthRequest,
        mut cancel: CancelToken,
    ) -> BoxFuture<'static, Result<AuthResponse, AuthError>> {
        let http = self.clone();
        Box::pin(async move {
            // Biased cancellation is polled before constructing or sending a request,
            // and remains active until the final response byte has been read.
            tokio::select! {
                biased;
                _ = cancelled(&mut cancel) => Err(AuthError::Cancelled),
                result = async {
                    let stage = request.stage();
                    let network = |_| AuthError::Network { stage };
                    let mut response = http.client.execute(http.build_request(request)?).await.map_err(network)?;
                    let status = response.status().as_u16();
                    // Keep only the numeric protocol hint, never arbitrary header text.
                    let retry_after = response.headers().get(header::RETRY_AFTER)
                        .and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok())
                        .map(Duration::from_secs);
                    if response.content_length().is_some_and(|size| size > MAX_BODY as u64) {
                        return Err(AuthError::InvalidResponse { stage });
                    }
                    let mut body = Zeroizing::new(Vec::new());
                    while let Some(chunk) = response.chunk().await.map_err(network)? {
                        if chunk.len() > MAX_BODY - body.len() {
                            return Err(AuthError::InvalidResponse { stage });
                        }
                        body.extend_from_slice(&chunk);
                    }
                    Ok(AuthResponse { status, body, retry_after })
                } => result,
            }
        })
    }
}

pub(crate) async fn cancelled(cancel: &mut CancelToken) {
    loop {
        if *cancel.borrow_and_update() || cancel.changed().await.is_err() {
            return;
        }
    }
}
