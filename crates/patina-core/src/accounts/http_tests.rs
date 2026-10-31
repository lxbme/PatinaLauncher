//! Offline contract tests for the trusted transport (protocol parsing is Task 2).
use super::error::{AuthError, AuthStage};
use super::http::{AuthHttp, AuthRequest, MSA_SCOPE, MicrosoftGrant};
use super::login::{DeviceCode, LoginError, LoginEvent};
use super::secret::SecretString;
use super::{AccountError, DEFAULT_MSA_CLIENT_ID, msa_client_id};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::watch;
use tracing::instrument::WithSubscriber;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

const SENTINEL: &str = "SECRET_SENTINEL";
const STAGES: [AuthStage; 7] = [
    AuthStage::DeviceCode,
    AuthStage::MicrosoftToken,
    AuthStage::XboxLive,
    AuthStage::Xsts,
    AuthStage::MinecraftToken,
    AuthStage::Entitlement,
    AuthStage::Profile,
];

fn endpoints(base: &str) -> [(AuthStage, String); 7] {
    std::array::from_fn(|i| (STAGES[i], format!("{base}/{i}")))
}
fn secret() -> SecretString {
    SecretString::new(SENTINEL.to_owned())
}
fn requests() -> Vec<AuthRequest> {
    vec![
        AuthRequest::DeviceCode,
        AuthRequest::MicrosoftToken(MicrosoftGrant::DeviceCode(secret())),
        AuthRequest::MicrosoftToken(MicrosoftGrant::Refresh(secret())),
        AuthRequest::XboxLive {
            access_token: secret(),
        },
        AuthRequest::Xsts {
            xbox_token: secret(),
        },
        AuthRequest::MinecraftToken {
            user_hash: secret(),
            xsts_token: secret(),
        },
        AuthRequest::Entitlement {
            access_token: secret(),
        },
        AuthRequest::Profile {
            access_token: secret(),
        },
    ]
}

#[tokio::test]
async fn auth_https_allowlist_and_redirects() {
    assert_eq!(
        DEFAULT_MSA_CLIENT_ID,
        "e8d9cf1c-b85b-4413-876d-190d5e552218"
    );
    assert_eq!(MSA_SCOPE, "XboxLive.signin offline_access");
    let production = AuthHttp::production().unwrap();
    let urls = [
        "https://login.microsoftonline.com/consumers/oauth2/v2.0/devicecode",
        "https://login.microsoftonline.com/consumers/oauth2/v2.0/token",
        "https://login.microsoftonline.com/consumers/oauth2/v2.0/token",
        "https://user.auth.xboxlive.com/user/authenticate",
        "https://xsts.auth.xboxlive.com/xsts/authorize",
        "https://api.minecraftservices.com/authentication/login_with_xbox",
        "https://api.minecraftservices.com/entitlements/mcstore",
        "https://api.minecraftservices.com/minecraft/profile",
    ];
    for (request, url) in requests().into_iter().zip(urls) {
        let built = production.build_request(request).unwrap();
        assert_eq!(built.url().as_str(), url);
        assert!(!format!("{built:?}").contains(SENTINEL));
        if let Some(header) = built.headers().get("authorization") {
            assert!(header.is_sensitive());
        }
    }

    let origin = MockServer::start().await;
    let target = MockServer::start().await;
    let mirror = MockServer::start().await;
    let http = AuthHttp::for_test(endpoints(&origin.uri())).unwrap();
    let (_tx, cancel) = watch::channel(false);
    for status in [301, 302, 303, 307, 308] {
        for location in [target.uri(), mirror.uri()] {
            origin.reset().await;
            Mock::given(path("/6"))
                .respond_with(ResponseTemplate::new(status).insert_header("Location", location))
                .expect(1)
                .mount(&origin)
                .await;
            let response = http
                .send(
                    AuthRequest::Profile {
                        access_token: secret(),
                    },
                    cancel.clone(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), status);
        }
    }
    let redirect_target_requests = target.received_requests().await.unwrap().len();
    let mirror_requests = mirror.received_requests().await.unwrap().len();
    assert_eq!(redirect_target_requests, 0);
    assert_eq!(mirror_requests, 0);
}

#[test]
fn test_transport_rejects_untrusted_urls_and_duplicate_stages() {
    for url in [
        "https://login.microsoftonline.com/path",
        "http://localhost/path",
        "http://192.0.2.1/path",
        "file:///tmp/auth",
        "http://127.0.0.1/path?SECRET_SENTINEL",
        "http://SECRET_SENTINEL@127.0.0.1/path",
        "http://127.0.0.1/#SECRET_SENTINEL",
    ] {
        let mut urls = endpoints("http://127.0.0.1:9");
        urls[0].1 = url.into();
        let error = AuthHttp::for_test(urls).err().expect("unsafe URL accepted");
        assert!(!format!("{error:?} {error}").contains(SENTINEL));
    }
    for stage in [AuthStage::DeviceCode, AuthStage::Skin] {
        let mut urls = endpoints("http://127.0.0.1:9");
        urls[6].0 = stage;
        assert!(AuthHttp::for_test(urls).is_err());
    }
    assert!(AuthHttp::for_test(endpoints("http://[::1]:9")).is_ok());
}

#[tokio::test]
async fn auth_requests_use_only_the_stage_contract() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let http = AuthHttp::for_test(endpoints(&server.uri())).unwrap();
    let (_tx, cancel) = watch::channel(false);
    for request in requests() {
        http.send(request, cancel.clone()).await.unwrap();
    }
    let received = server.received_requests().await.unwrap();
    assert_eq!(received.len(), 8);
    let forms = [
        vec![
            ("client_id", msa_client_id()),
            ("scope", "XboxLive.signin offline_access"),
        ],
        vec![
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ("client_id", msa_client_id()),
            ("device_code", SENTINEL),
        ],
        vec![
            ("grant_type", "refresh_token"),
            ("client_id", msa_client_id()),
            ("refresh_token", SENTINEL),
            ("scope", "XboxLive.signin offline_access"),
        ],
    ];
    for (request, fields) in received[..3].iter().zip(forms) {
        let parsed = reqwest::Url::parse(&format!(
            "http://127.0.0.1/?{}",
            String::from_utf8_lossy(&request.body)
        ))
        .unwrap();
        let actual: std::collections::BTreeMap<_, _> = parsed
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        let expected = fields
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect();
        assert_eq!(actual, expected);
        assert_eq!(
            request.headers["content-type"],
            "application/x-www-form-urlencoded"
        );
    }
    let json = [
        serde_json::json!({"Properties":{"AuthMethod":"RPS", "SiteName":"user.auth.xboxlive.com", "RpsTicket":"d=SECRET_SENTINEL"}, "RelyingParty":"http://auth.xboxlive.com", "TokenType":"JWT"}),
        serde_json::json!({"Properties":{"SandboxId":"RETAIL", "UserTokens":[SENTINEL]}, "RelyingParty":"rp://api.minecraftservices.com/", "TokenType":"JWT"}),
        serde_json::json!({"identityToken":"XBL3.0 x=SECRET_SENTINEL;SECRET_SENTINEL"}),
    ];
    for (request, expected) in received[3..6].iter().zip(json) {
        assert_eq!(request.body_json::<serde_json::Value>().unwrap(), expected);
        assert_eq!(request.headers["content-type"], "application/json");
    }
    for (i, request) in received.iter().enumerate() {
        assert_eq!(request.method.as_str(), if i < 6 { "POST" } else { "GET" });
        assert_eq!(
            request.url.path(),
            ["/0", "/1", "/1", "/2", "/3", "/4", "/5", "/6"][i]
        );
        assert!(request.url.query().is_none());
        assert!(!request.headers.contains_key("cookie"));
        if i < 6 {
            assert!(!request.headers.contains_key("authorization"));
        } else {
            assert_eq!(request.headers["authorization"], "Bearer SECRET_SENTINEL");
            assert!(request.body.is_empty());
        }
    }
}

// Raw HTTP fixtures exercise missing Content-Length and in-flight body cancellation.
async fn raw_server(
    response: Vec<u8>,
    stall: bool,
) -> (
    String,
    tokio::sync::oneshot::Receiver<()>,
    tokio::task::JoinHandle<()>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (tx, rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buffer = [0u8; 4096];
        let _ = stream.read(&mut buffer).await.unwrap();
        let _ = stream.write_all(&response).await;
        let _ = tx.send(());
        if stall {
            std::future::pending::<()>().await;
        }
    });
    (url, rx, task)
}

#[tokio::test]
async fn auth_responses_are_bounded() {
    let server = MockServer::start().await;
    let http = AuthHttp::for_test(endpoints(&server.uri())).unwrap();
    let (_tx, cancel) = watch::channel(false);
    for size in [1024 * 1024, 1024 * 1024 + 1] {
        server.reset().await;
        Mock::given(path("/0"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![b'x'; size]))
            .mount(&server)
            .await;
        let oversized_result = http.send(AuthRequest::DeviceCode, cancel.clone()).await;
        if size == 1024 * 1024 {
            assert_eq!(oversized_result.unwrap().body().len(), size);
        } else {
            assert!(matches!(
                oversized_result,
                Err(AuthError::InvalidResponse {
                    stage: AuthStage::DeviceCode
                })
            ));
        }
    }
    let mut bytes = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
    for _ in 0..17 {
        bytes.extend_from_slice(b"10000\r\n");
        bytes.extend(vec![b'x'; 65536]);
        bytes.extend_from_slice(b"\r\n");
    }
    bytes.extend_from_slice(b"0\r\n\r\n");
    let (url, _sent, task) = raw_server(bytes, false).await;
    let http = AuthHttp::for_test(endpoints(&url)).unwrap();
    let oversized_result = http.send(AuthRequest::DeviceCode, cancel).await;
    assert!(matches!(
        oversized_result,
        Err(AuthError::InvalidResponse { .. })
    ));
    task.await.unwrap();
}

#[tokio::test]
async fn cancel_before_request_sends_nothing() {
    let server = MockServer::start().await;
    let http = AuthHttp::for_test(endpoints(&server.uri())).unwrap();
    for dropped in [false, true] {
        let (tx, cancel) = watch::channel(false);
        let _sender = if dropped {
            drop(tx);
            None
        } else {
            tx.send(true).unwrap();
            Some(tx)
        };
        for request in requests() {
            assert!(matches!(
                http.send(request, cancel.clone()).await,
                Err(AuthError::Cancelled)
            ));
        }
    }
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn cancel_during_headers_or_body_stops_request() {
    for bytes in [
        Vec::new(),
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1\r\nx\r\n".to_vec(),
    ] {
        let (url, sent, task) = raw_server(bytes, true).await;
        let http = AuthHttp::for_test(endpoints(&url)).unwrap();
        let (tx, cancel) = watch::channel(false);
        let request = tokio::spawn(http.send(AuthRequest::DeviceCode, cancel));
        sent.await.unwrap();
        // An unchanged false notification is not cancellation.
        tx.send(false).unwrap();
        tokio::task::yield_now().await;
        assert!(!request.is_finished());
        tx.send(true).unwrap();
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(2), request)
                .await
                .unwrap()
                .unwrap(),
            Err(AuthError::Cancelled)
        ));
        task.abort();
    }
}

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);
impl std::io::Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
    type Writer = Self;
    fn make_writer(&'a self) -> Self {
        self.clone()
    }
}

#[tokio::test]
async fn auth_secrets_never_format() {
    let capture = Capture::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .without_time()
        .with_writer(capture.clone())
        .finish();
    let output = async {
        tracing::debug!("auth secrecy capture active");
        let mut output = format!("{:?}", secret());
        let code = DeviceCode {
            user_code: SENTINEL.into(),
            verification_uri: format!("https://microsoft.com/link?{SENTINEL}"),
            expires_in: Duration::from_secs(30),
        };
        assert_eq!(code.user_code, SENTINEL); // Explicit UI access remains available.
        output.push_str(&format!("{:?}", LoginEvent::Code(code)));
        for error in [
            AccountError::Parse(SENTINEL.into()),
            AccountError::NotFound(SENTINEL.into()),
            AccountError::InvalidName(SENTINEL.into()),
            AccountError::Io {
                path: SENTINEL.into(),
                error: SENTINEL.into(),
            },
        ] {
            let error = AuthError::Account(error);
            output.push_str(&format!(
                "{error:?} {error} {:?}",
                LoginError::Auth(error.clone())
            ));
            assert!(std::error::Error::source(&error).is_none());
        }
        let server = MockServer::start().await;
        // Transport must not parse or echo even a malformed protocol body.
        Mock::given(path("/0"))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_string("{SECRET_SENTINEL")
                    .insert_header("set-cookie", SENTINEL)
                    .insert_header("retry-after", SENTINEL),
            )
            .mount(&server)
            .await;
        let http = AuthHttp::for_test(endpoints(&server.uri())).unwrap();
        let (_tx, cancel) = watch::channel(false);
        let response = http
            .send(AuthRequest::DeviceCode, cancel.clone())
            .await
            .unwrap();
        assert_eq!(response.status(), 400);
        assert_eq!(response.body(), b"{SECRET_SENTINEL");
        assert_eq!(response.retry_after(), None);
        for request in requests() {
            output.push_str(&format!("{request:?}"));
            // Exercise actual form/JSON/bearer traffic while TRACE is captured.
            http.send(request, cancel.clone()).await.unwrap();
        }
        // Invalid sensitive bearer values produce a sanitized build error, not a header error string.
        let error = http
            .send(
                AuthRequest::Profile {
                    access_token: SecretString::new("SECRET_SENTINEL\n".into()),
                },
                cancel,
            )
            .await
            .err()
            .unwrap();
        output.push_str(&format!("{error:?} {error}"));
        output
    }
    .with_subscriber(subscriber)
    .await;
    let trace = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    assert!(trace.contains("auth secrecy capture active"));
    let debug_and_error_and_trace_output = format!("{output}{trace}");
    assert!(!debug_and_error_and_trace_output.contains(SENTINEL));
}

#[test]
fn production_validates_build_time_client_id() {
    let result = AuthHttp::production();
    if msa_client_id().trim().is_empty() {
        assert!(matches!(
            result,
            Err(AuthError::InvalidResponse {
                stage: AuthStage::DeviceCode
            })
        ));
    } else {
        assert!(result.is_ok());
    }
}

#[tokio::test]
async fn auth_request_timeout_covers_headers_and_streamed_body() {
    let cases = [
        Vec::new(),
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1\r\nx\r\n".to_vec(),
    ];
    futures::future::join_all(cases.into_iter().map(|bytes| async move {
        let (url, _sent, task) = raw_server(bytes, true).await;
        let http = AuthHttp::for_test(endpoints(&url)).unwrap();
        let (_tx, cancel) = watch::channel(false);
        let start = tokio::time::Instant::now();
        let result = tokio::time::timeout(
            Duration::from_secs(20),
            http.send(AuthRequest::DeviceCode, cancel),
        )
        .await;
        task.abort();
        assert!(matches!(
            result,
            Ok(Err(AuthError::Timeout {
                stage: AuthStage::DeviceCode
            }))
        ));
        assert!(start.elapsed() >= Duration::from_secs(14));
    }))
    .await;
}

#[tokio::test]
async fn dropped_sender_cancels_in_flight_request() {
    let (url, sent, task) = raw_server(Vec::new(), true).await;
    let http = AuthHttp::for_test(endpoints(&url)).unwrap();
    let (tx, cancel) = watch::channel(false);
    let request = tokio::spawn(http.send(AuthRequest::DeviceCode, cancel));
    sent.await.unwrap();
    drop(tx);
    let result = tokio::time::timeout(Duration::from_secs(2), request).await;
    task.abort();
    assert!(matches!(result, Ok(Ok(Err(AuthError::Cancelled)))));
}

#[tokio::test]
async fn response_status_and_retry_hint_are_preserved_without_remote_text() {
    let server = MockServer::start().await;
    let http = AuthHttp::for_test(endpoints(&server.uri())).unwrap();
    let (_tx, cancel) = watch::channel(false);
    for status in [200, 400, 401, 403, 404, 429, 500, 503] {
        server.reset().await;
        Mock::given(path("/1"))
            .respond_with(
                ResponseTemplate::new(status)
                    .insert_header("retry-after", "17")
                    .set_body_string("SECRET_SENTINEL"),
            )
            .mount(&server)
            .await;
        let response = http
            .send(
                AuthRequest::MicrosoftToken(MicrosoftGrant::DeviceCode(secret())),
                cancel.clone(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), status);
        assert_eq!(response.retry_after(), Some(Duration::from_secs(17)));
        assert_eq!(response.body(), SENTINEL.as_bytes());
    }
}
