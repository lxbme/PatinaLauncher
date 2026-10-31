//! Request-level protocol fixtures use only synthetic credentials and loopback TCP.
use super::error::{AuthError, AuthStage, XboxRestriction};
use super::http::AuthHttp;
use super::login::LoginEvent;
use super::msa::{AuthClock, MsaClient, VerifiedLogin};
use super::secret::SecretString;
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;

const NOW: u64 = 1_800_000_000;
const STAGES: [AuthStage; 7] = [
    AuthStage::DeviceCode,
    AuthStage::MicrosoftToken,
    AuthStage::XboxLive,
    AuthStage::Xsts,
    AuthStage::MinecraftToken,
    AuthStage::Entitlement,
    AuthStage::Profile,
];
struct Clock;
impl AuthClock for Clock {
    fn unix_seconds(&self) -> u64 {
        NOW
    }
}

// A permanently runnable task prevents Tokio's paused clock from auto-advancing
// while actual socket IO is pending. Only explicit advance() moves protocol time.
struct TimeGuard(tokio::task::JoinHandle<()>);
impl TimeGuard {
    fn new() -> Self {
        Self(tokio::spawn(async {
            loop {
                tokio::task::yield_now().await;
            }
        }))
    }
}
impl Drop for TimeGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[derive(Clone)]
enum Reply {
    Json(u16, Value),
    Delayed(Value, Duration),
    Raw(Vec<u8>),
    Stall(bool),
    Disconnect,
}
fn ok(value: Value) -> Reply {
    Reply::Json(200, value)
}
fn device() -> Value {
    json!({"device_code":"DEVICE_SECRET", "user_code":"ABCD-EFGH", "verification_uri":"https://microsoft.com/link", "expires_in":120})
}
fn microsoft() -> Value {
    json!({"access_token":"MS_SECRET", "refresh_token":"REFRESH_SECRET", "expires_in":3600})
}
fn xbox(token: &str) -> Value {
    json!({"Token":token, "NotAfter":"2030-01-01T00:00:00Z", "DisplayClaims":{"xui":[{"uhs":"HASH_SECRET", "xid":"123456"}]}})
}
fn minecraft() -> Value {
    json!({"access_token":"MC_SECRET", "expires_in":3600, "token_type":"bEaReR"})
}
fn profile() -> Value {
    json!({"id":"0123456789abcdef0123456789abcdef", "name":"Player_123", "skins":[{"state":"ACTIVE", "url":"https://textures.minecraft.net/texture/abcdef0123"}]})
}
fn chain() -> Vec<(usize, Reply)> {
    vec![
        (0, ok(device())),
        (1, ok(microsoft())),
        (2, ok(xbox("XBL_SECRET"))),
        (3, ok(xbox("XSTS_SECRET"))),
        (4, ok(minecraft())),
        (5, ok(json!({"items":[{"name":"game_minecraft"}]}))),
        (6, ok(profile())),
    ]
}

#[derive(Clone, Debug)]
struct Captured {
    at: Instant,
    method: String,
    path: String,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}
struct Server {
    client: MsaClient,
    requests: Arc<Mutex<Vec<Captured>>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Server {
    async fn new(script: Vec<(usize, Reply)>) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let http = AuthHttp::for_test(std::array::from_fn(|i| (STAGES[i], format!("{base}/{i}"))))
            .unwrap();
        let client = MsaClient::new(http, Arc::new(Clock));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let task = tokio::spawn(async move {
            let mut script: VecDeque<_> = script.into();
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let at = Instant::now();
                let mut bytes = Vec::new();
                let header_end = loop {
                    let mut buffer = [0; 4096];
                    let n = stream.read(&mut buffer).await.unwrap();
                    if n == 0 {
                        return;
                    }
                    bytes.extend_from_slice(&buffer[..n]);
                    if let Some(pos) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        break pos + 4;
                    }
                };
                let head = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
                let mut lines = head.split("\r\n");
                let mut request_line = lines.next().unwrap().split_whitespace();
                let method = request_line.next().unwrap().to_owned();
                let path = request_line.next().unwrap().to_owned();
                let headers: BTreeMap<_, _> = lines
                    .filter_map(|l| l.split_once(':'))
                    .map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_owned()))
                    .collect();
                let size: usize = headers
                    .get("content-length")
                    .map_or(0, |v| v.parse().unwrap());
                while bytes.len() - header_end < size {
                    let mut buf = [0; 4096];
                    let n = stream.read(&mut buf).await.unwrap();
                    assert_ne!(n, 0);
                    bytes.extend_from_slice(&buf[..n]);
                }
                captured.lock().unwrap().push(Captured {
                    at,
                    method,
                    path: path.clone(),
                    headers,
                    body: bytes[header_end..].to_vec(),
                });
                let (stage, reply) = script.pop_front().expect("unexpected extra auth request");
                assert_eq!(path, format!("/{stage}"));
                match reply {
                    Reply::Json(status, body) => {
                        let body = body.to_string();
                        let response = format!(
                            "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = stream.write_all(response.as_bytes()).await;
                    }
                    Reply::Delayed(body, delay) => {
                        tokio::time::sleep(delay).await;
                        let body = body.to_string();
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = stream.write_all(response.as_bytes()).await;
                    }
                    Reply::Raw(bytes) => {
                        let _ = stream.write_all(&bytes).await;
                    }
                    Reply::Stall(body) => {
                        if body {
                            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1\r\nx\r\n").await;
                        }
                        // Hold this connection independently so timeout retry can connect.
                        tokio::spawn(async move {
                            let mut byte = [0];
                            let _ = stream.read(&mut byte).await;
                        });
                    }
                    Reply::Disconnect => {}
                }
            }
        });
        Self {
            client,
            requests,
            task,
        }
    }
    async fn wait_requests(&self, n: usize) {
        until(|| self.requests.lock().unwrap().len() >= n).await;
    }
    fn captured(&self) -> Vec<Captured> {
        self.requests.lock().unwrap().clone()
    }
}
async fn until(mut condition: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(
            std::time::Instant::now() < deadline,
            "fixture did not progress"
        );
        tokio::task::yield_now().await;
    }
}
async fn settle() {
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }
}
type Attempt = (
    tokio::task::JoinHandle<Result<VerifiedLogin, AuthError>>,
    mpsc::UnboundedReceiver<LoginEvent>,
    watch::Sender<bool>,
);
fn start(server: &Server) -> Attempt {
    let (tx, rx) = mpsc::unbounded_channel();
    let (cancel_tx, cancel) = watch::channel(false);
    (
        tokio::spawn(server.client.sign_in(tx, cancel)),
        rx,
        cancel_tx,
    )
}
async fn waiting(events: &mut mpsc::UnboundedReceiver<LoginEvent>) {
    assert!(matches!(events.recv().await, Some(LoginEvent::Code(_))));
    assert_eq!(events.recv().await, Some(LoginEvent::Waiting));
    settle().await;
}
async fn finish(
    task: tokio::task::JoinHandle<Result<VerifiedLogin, AuthError>>,
) -> Result<VerifiedLogin, AuthError> {
    until(|| task.is_finished()).await;
    task.await.unwrap()
}
async fn run(
    script: Vec<(usize, Reply)>,
) -> (
    Result<VerifiedLogin, AuthError>,
    Vec<Captured>,
    Vec<LoginEvent>,
) {
    let server = Server::new(script).await;
    let (task, mut events, _cancel) = start(&server);
    until(|| task.is_finished() || events.len() >= 2).await;
    if !task.is_finished() {
        settle().await;
        tokio::time::advance(Duration::from_secs(5)).await;
    }
    let result = finish(task).await;
    let mut emitted = Vec::new();
    while let Ok(event) = events.try_recv() {
        emitted.push(event);
    }
    (result, server.captured(), emitted)
}

#[tokio::test(start_paused = true)]
async fn full_chain_requires_java_entitlement_and_profile() {
    let _time = TimeGuard::new();
    let (result, requests, events) = run(chain()).await;
    let login = result.unwrap();
    assert_eq!(
        login.profile().uuid().to_string(),
        "01234567-89ab-cdef-0123-456789abcdef"
    );
    assert_eq!(login.profile().username(), "Player_123");
    assert_eq!(login.profile().xuid(), Some("123456"));
    assert_eq!(login.profile().skin_hash(), Some("abcdef0123"));
    assert_eq!(login.verified_at(), NOW);
    assert_eq!(login.session().expires_at(), NOW + 3600);
    assert_eq!(login.session().access_token().expose(), "MC_SECRET");
    assert_eq!(login.refresh_token().expose(), "REFRESH_SECRET");
    assert_eq!(events.len(), 2);
    assert!(matches!(events[0], LoginEvent::Code(_)));
    assert_eq!(events[1], LoginEvent::Waiting);
    for (i, request) in requests.iter().enumerate() {
        assert_eq!(request.path, format!("/{i}"));
        assert_eq!(request.method, if i < 5 { "POST" } else { "GET" });
        assert!(!request.headers.contains_key("cookie"));
        if i < 5 {
            assert!(!request.headers.contains_key("authorization"));
        } else {
            assert_eq!(request.headers["authorization"], "Bearer MC_SECRET");
            assert!(request.body.is_empty());
        }
    }
    let forms: Vec<BTreeMap<String, String>> = requests[..2]
        .iter()
        .map(|r| {
            reqwest::Url::parse(&format!(
                "http://127.0.0.1/?{}",
                String::from_utf8_lossy(&r.body)
            ))
            .unwrap()
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect()
        })
        .collect();
    assert_eq!(
        forms[0],
        BTreeMap::from([
            ("client_id".into(), super::msa_client_id().into()),
            ("scope".into(), "XboxLive.signin offline_access".into())
        ])
    );
    assert_eq!(
        forms[1],
        BTreeMap::from([
            ("client_id".into(), super::msa_client_id().into()),
            (
                "grant_type".into(),
                "urn:ietf:params:oauth:grant-type:device_code".into()
            ),
            ("device_code".into(), "DEVICE_SECRET".into())
        ])
    );
    assert!(!forms[1].contains_key("client_secret"));
    let expected = [
        json!({"Properties":{"AuthMethod":"RPS","SiteName":"user.auth.xboxlive.com","RpsTicket":"d=MS_SECRET"},"RelyingParty":"http://auth.xboxlive.com","TokenType":"JWT"}),
        json!({"Properties":{"SandboxId":"RETAIL","UserTokens":["XBL_SECRET"]},"RelyingParty":"rp://api.minecraftservices.com/","TokenType":"JWT"}),
        json!({"identityToken":"XBL3.0 x=HASH_SECRET;XSTS_SECRET"}),
    ];
    for (request, expected) in requests[2..5].iter().zip(expected) {
        assert_eq!(
            serde_json::from_slice::<Value>(&request.body).unwrap(),
            expected
        );
    }
    for (stage, reply, error) in [
        (5, ok(json!({"items":[]})), AuthError::NoEntitlement),
        (
            5,
            ok(json!({"items":[{"name":"unrelated"}]})),
            AuthError::NoEntitlement,
        ),
        (6, Reply::Json(404, json!({})), AuthError::ProfileMissing),
        (
            4,
            Reply::Json(403, json!({})),
            AuthError::Http {
                stage: AuthStage::MinecraftToken,
                status: 403,
            },
        ),
    ] {
        let mut script = chain();
        script[stage].1 = reply;
        script.truncate(stage + 1);
        let (actual, requests, _) = run(script).await;
        assert_eq!(actual.unwrap_err(), error);
        assert_eq!(requests.len(), stage + 1);
    }
    let mut script = chain();
    script[5].1 = ok(json!({"items":[{"name":"product_minecraft"}]}));
    assert!(run(script).await.0.is_ok());
}

#[tokio::test(start_paused = true)]
async fn device_pending_slow_down_and_expiry() {
    let _time = TimeGuard::new();
    let mut script = chain();
    script.splice(
        1..1,
        [
            (
                1,
                Reply::Json(400, json!({"error":"authorization_pending"})),
            ),
            (1, Reply::Json(400, json!({"error":"slow_down"}))),
            (1, Reply::Json(400, json!({"error":"slow_down"}))),
        ],
    );
    let server = Server::new(script).await;
    let start_at = Instant::now();
    let (task, mut events, _cancel) = start(&server);
    waiting(&mut events).await;
    for (i, delay) in [5, 5, 10, 15].into_iter().enumerate() {
        tokio::time::advance(Duration::from_secs(delay)).await;
        server.wait_requests(i + 2).await;
        settle().await;
    }
    assert!(finish(task).await.is_ok());
    let poll_starts: Vec<_> = server
        .captured()
        .iter()
        .filter(|r| r.path == "/1")
        .map(|r| r.at.duration_since(start_at).as_secs())
        .collect();
    assert_eq!(poll_starts, vec![5, 10, 20, 35]);
    assert!(events.try_recv().is_err());
    let mut code = device();
    code["expires_in"] = json!(12);
    let server = Server::new(vec![
        (0, ok(code)),
        (
            1,
            Reply::Json(400, json!({"error":"authorization_pending"})),
        ),
        (1, Reply::Json(400, json!({"error":"slow_down"}))),
    ])
    .await;
    let (task, mut events, _cancel) = start(&server);
    waiting(&mut events).await;
    for n in [2, 3] {
        tokio::time::advance(Duration::from_secs(5)).await;
        server.wait_requests(n).await;
        settle().await;
    }
    tokio::time::advance(Duration::from_secs(2)).await;
    assert_eq!(finish(task).await.unwrap_err(), AuthError::Expired);
    assert_eq!(server.captured().len(), 3);
}

#[tokio::test(start_paused = true)]
async fn cancel_at_every_auth_request() {
    let _time = TimeGuard::new();
    for stage in 0..7 {
        for body in [false, true] {
            for cancel_mode in 0..3 {
                let mut script = chain();
                script[stage].1 = Reply::Stall(body);
                script.truncate(stage + 1);
                let server = Server::new(script).await;
                let (task, mut events, cancel) = start(&server);
                if stage > 0 {
                    waiting(&mut events).await;
                    tokio::time::advance(Duration::from_secs(5)).await;
                }
                server.wait_requests(stage + 1).await;
                match cancel_mode {
                    0 => {
                        cancel.send(true).unwrap();
                    }
                    1 => drop(events),
                    _ => drop(cancel),
                }
                assert_eq!(finish(task).await.unwrap_err(), AuthError::Cancelled);
                settle().await;
                assert_eq!(server.captured().len(), stage + 1);
            }
        }
    }
    for drop_sender in [false, true] {
        let server = Server::new(chain()).await;
        let (tx, _events) = mpsc::unbounded_channel();
        let (cancel_tx, cancel) = watch::channel(false);
        if drop_sender {
            drop(cancel_tx);
        } else {
            cancel_tx.send(true).unwrap();
        }
        assert_eq!(
            server.client.sign_in(tx, cancel).await.unwrap_err(),
            AuthError::Cancelled
        );
        assert!(server.captured().is_empty());
    }
    for mode in 0..3 {
        let server = Server::new(chain()).await;
        let (task, mut events, cancel) = start(&server);
        waiting(&mut events).await;
        match mode {
            0 => {
                cancel.send(true).unwrap();
            }
            1 => drop(cancel),
            _ => drop(events),
        };
        assert_eq!(finish(task).await.unwrap_err(), AuthError::Cancelled);
        assert_eq!(server.captured().len(), 1);
    }
    let server = Server::new(vec![(0, Reply::Stall(true))]).await;
    let (task, _events, cancel) = start(&server);
    server.wait_requests(1).await;
    drop(cancel);
    assert_eq!(finish(task).await.unwrap_err(), AuthError::Cancelled);
    let server = Server::new(chain()).await;
    let (tx, events) = mpsc::unbounded_channel();
    drop(events);
    let (_cancel, cancel) = watch::channel(false);
    assert_eq!(
        server.client.sign_in(tx, cancel).await.unwrap_err(),
        AuthError::Cancelled
    );
    assert!(server.captured().is_empty());
}

#[tokio::test(start_paused = true)]
async fn xsts_restrictions_are_typed() {
    let _time = TimeGuard::new();
    for (code, restriction) in [
        (2148916233u64, XboxRestriction::AccountRequired),
        (2148916235, XboxRestriction::Region),
        (2148916236, XboxRestriction::AdultVerification),
        (2148916237, XboxRestriction::AdultVerification),
        (2148916238, XboxRestriction::Family),
    ] {
        let mut script = chain();
        script[3].1 = Reply::Json(
            401,
            json!({"XErr":code,"Message":"REMOTE_SECRET","Redirect":"https://evil.invalid/REMOTE_SECRET"}),
        );
        script.truncate(4);
        assert_eq!(
            run(script).await.0.unwrap_err(),
            AuthError::XboxRestriction(restriction)
        );
    }
    for body in [json!({"XErr":123}), json!({}), json!({"XErr":"2148916233"})] {
        let mut script = chain();
        script[3].1 = Reply::Json(401, body);
        script.truncate(4);
        assert_eq!(
            run(script).await.0.unwrap_err(),
            AuthError::Http {
                stage: AuthStage::Xsts,
                status: 401
            }
        );
    }
}

#[tokio::test(start_paused = true)]
async fn refresh_keeps_or_rotates_token() {
    let _time = TimeGuard::new();
    for rotated in [None, Some("ROTATED_SECRET")] {
        let mut body = microsoft();
        body.as_object_mut().unwrap().remove("refresh_token");
        if let Some(token) = rotated {
            body["refresh_token"] = json!(token);
        }
        let server = Server::new(vec![(1, ok(body)), (2, Reply::Json(503, json!({})))]).await;
        let (_tx, cancel) = watch::channel(false);
        let result = server
            .client
            .refresh_microsoft(SecretString::new("OLD_SECRET".into()), cancel.clone())
            .await
            .unwrap();
        assert_eq!(
            result.refresh_token().expose(),
            rotated.unwrap_or("OLD_SECRET")
        );
        assert_eq!(result.expires_at(), NOW + 3600);
        assert_eq!(server.captured().len(), 1);
        let request = &server.captured()[0];
        assert_eq!(request.method, "POST");
        let form: BTreeMap<_, _> = reqwest::Url::parse(&format!(
            "http://127.0.0.1/?{}",
            String::from_utf8_lossy(&request.body)
        ))
        .unwrap()
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
        assert_eq!(
            form,
            BTreeMap::from([
                ("client_id".into(), super::msa_client_id().into()),
                ("scope".into(), "XboxLive.signin offline_access".into()),
                ("grant_type".into(), "refresh_token".into()),
                ("refresh_token".into(), "OLD_SECRET".into())
            ])
        );
        assert_eq!(
            server
                .client
                .complete_minecraft(result, cancel)
                .await
                .unwrap_err(),
            AuthError::Http {
                stage: AuthStage::XboxLive,
                status: 503
            }
        );
    }
    for (body, error) in [
        (
            json!({"error":"invalid_grant","error_description":"REMOTE_SECRET"}),
            AuthError::ReauthenticationRequired,
        ),
        (
            json!({"error":"unknown","error_description":"REMOTE_SECRET"}),
            AuthError::InvalidResponse {
                stage: AuthStage::MicrosoftToken,
            },
        ),
    ] {
        let server = Server::new(vec![(1, Reply::Json(400, body))]).await;
        let (_tx, cancel) = watch::channel(false);
        let actual = server
            .client
            .refresh_microsoft(SecretString::new("OLD_SECRET".into()), cancel)
            .await
            .unwrap_err();
        assert_eq!(actual, error);
        assert!(!format!("{actual:?} {actual}").contains("REMOTE_SECRET"));
    }
}

#[tokio::test(start_paused = true)]
async fn polling_timeout_only_backoff_retry_after_and_in_flight_expiry() {
    let _time = TimeGuard::new();
    let mut script = chain();
    script.insert(1, (1, Reply::Stall(false)));
    let server = Server::new(script).await;
    let start_at = Instant::now();
    let (task, mut events, _cancel) = start(&server);
    waiting(&mut events).await;
    tokio::time::advance(Duration::from_secs(5)).await;
    server.wait_requests(2).await;
    settle().await;
    tokio::time::advance(Duration::from_secs(15)).await;
    settle().await;
    tokio::time::advance(Duration::from_secs(9)).await;
    settle().await;
    assert_eq!(server.captured().len(), 2);
    tokio::time::advance(Duration::from_secs(1)).await;
    assert!(finish(task).await.is_ok());
    assert_eq!(
        server.captured()[2].at.duration_since(start_at).as_secs(),
        30
    );
    for reply in [
        Reply::Disconnect,
        Reply::Json(503, json!({"error_description":"REMOTE_SECRET"})),
    ] {
        let mut script = chain();
        script[1].1 = reply;
        script.truncate(2);
        let (result, requests, _) = run(script).await;
        assert!(matches!(
            result,
            Err(AuthError::Network {
                stage: AuthStage::MicrosoftToken
            }) | Err(AuthError::Http {
                stage: AuthStage::MicrosoftToken,
                status: 503
            })
        ));
        assert_eq!(requests.len(), 2);
    }
    for (hint, delay) in [("2", 5), ("8", 8), ("nonsense", 5)] {
        let mut script = chain();
        script.insert(1,(1,Reply::Raw(format!("HTTP/1.1 429 Too Many\r\nRetry-After: {hint}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").into_bytes())));
        let server = Server::new(script).await;
        let start_at = Instant::now();
        let (task, mut events, _cancel) = start(&server);
        waiting(&mut events).await;
        tokio::time::advance(Duration::from_secs(5)).await;
        server.wait_requests(2).await;
        settle().await;
        tokio::time::advance(Duration::from_secs(delay)).await;
        assert!(finish(task).await.is_ok());
        assert_eq!(
            server.captured()[2].at.duration_since(start_at).as_secs(),
            5 + delay
        );
    }
    for reply in [Reply::Stall(false),Reply::Stall(true),Reply::Raw(b"HTTP/1.1 429 Too Many\r\nRetry-After: 18446744073709551615\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec())] { let mut code=device(); code["expires_in"]=json!(12); let server=Server::new(vec![(0,ok(code)),(1,reply)]).await; let (task,mut events,_cancel)=start(&server); waiting(&mut events).await; tokio::time::advance(Duration::from_secs(5)).await; server.wait_requests(2).await; settle().await; tokio::time::advance(Duration::from_secs(7)).await; assert_eq!(finish(task).await.unwrap_err(),AuthError::Expired); assert_eq!(server.captured().len(),2); }
}

#[tokio::test(start_paused = true)]
async fn protocol_rejects_malformed_missing_and_unsafe_fields() {
    let _time = TimeGuard::new();
    let mut cases = Vec::new();
    for stage in 0..7 {
        cases.push((
            stage,
            Reply::Raw(
                b"HTTP/1.1 200 OK\r\nContent-Length: 13\r\nConnection: close\r\n\r\nREMOTE_SECRET"
                    .to_vec(),
            ),
        ));
        cases.push((stage, ok(json!({}))));
    }
    for field in ["device_code", "user_code", "verification_uri", "expires_in"] {
        let mut body = device();
        body.as_object_mut().unwrap().remove(field);
        cases.push((0, ok(body)));
    }
    for field in ["device_code", "user_code"] {
        let mut body = device();
        body[field] = json!("");
        cases.push((0, ok(body)));
    }
    for uri in [
        "http://microsoft.com/link",
        "https://evil.invalid/link",
        "https://microsoft.com/link?code=REMOTE_SECRET",
        "https://microsoft.com/link#x",
        "https://user@microsoft.com/link",
        "https://microsoft.com:443/link",
        "https://www.microsoft.com/other",
    ] {
        let mut body = device();
        body["verification_uri"] = json!(uri);
        cases.push((0, ok(body)));
    }
    for (stage, field, base) in [
        (0, "expires_in", device()),
        (0, "interval", device()),
        (1, "expires_in", microsoft()),
        (4, "expires_in", minecraft()),
    ] {
        for value in [json!(0), json!(u64::MAX), json!(-1), json!(1.5)] {
            let mut body = base.clone();
            body[field] = value;
            cases.push((stage, ok(body)));
        }
    }
    for stage in [1, 4] {
        let mut body = if stage == 1 { microsoft() } else { minecraft() };
        body["access_token"] = json!("");
        cases.push((stage, ok(body)));
    }
    for value in [Value::Null, json!("")] {
        let mut body = microsoft();
        body["refresh_token"] = value;
        cases.push((1, ok(body)));
    }
    for stage in [2, 3] {
        for field in ["Token", "NotAfter"] {
            let mut body = xbox("SECRET");
            body[field] = json!("");
            cases.push((stage, ok(body)));
        }
        for date in ["invalid-REMOTE_SECRET", "2020-01-01T00:00:00Z"] {
            let mut body = xbox("SECRET");
            body["NotAfter"] = json!(date);
            cases.push((stage, ok(body)));
        }
        let mut body = xbox("SECRET");
        body["DisplayClaims"]["xui"][0]["uhs"] = json!("");
        cases.push((stage, ok(body)));
    }
    let mut body = minecraft();
    body["token_type"] = json!("MAC");
    cases.push((4, ok(body)));
    for id in [
        "",
        "not-a-uuid",
        "urn:uuid:01234567-89ab-cdef-0123-456789abcdef",
        "{01234567-89ab-cdef-0123-456789abcdef}",
    ] {
        let mut body = profile();
        body["id"] = json!(id);
        cases.push((6, ok(body)));
    }
    for name in ["", "ab", "abcdefghijklmnopq", "bad name", "玩家名", "abc-"] {
        let mut body = profile();
        body["name"] = json!(name);
        cases.push((6, ok(body)));
    }
    for (stage, reply) in cases {
        let mut script = chain();
        script[stage].1 = reply;
        script.truncate(stage + 1);
        let (result, requests, events) = run(script).await;
        let error = result.unwrap_err();
        assert_eq!(
            error,
            AuthError::InvalidResponse {
                stage: STAGES[stage]
            }
        );
        assert!(!format!("{error:?} {error}").contains("SECRET"));
        assert_eq!(requests.len(), stage + 1);
        if stage == 0 {
            assert!(events.is_empty());
        }
    }
    let mut script = chain();
    let mut body = xbox("XSTS_SECRET");
    body["DisplayClaims"]["xui"][0]["uhs"] = json!("OTHER");
    script[3].1 = ok(body);
    script.truncate(4);
    assert_eq!(
        run(script).await.0.unwrap_err(),
        AuthError::IdentityMismatch
    );
}

#[tokio::test(start_paused = true)]
async fn oauth_errors_short_expiry_and_optional_skin() {
    let _time = TimeGuard::new();
    for (code, error) in [
        ("access_denied", AuthError::Denied),
        ("authorization_declined", AuthError::Denied),
        ("expired_token", AuthError::Expired),
        (
            "unexpected",
            AuthError::InvalidResponse {
                stage: AuthStage::MicrosoftToken,
            },
        ),
    ] {
        let mut script = chain();
        script[1].1 = Reply::Json(
            400,
            json!({"error":code,"error_description":"REMOTE_SECRET"}),
        );
        script.truncate(2);
        assert_eq!(run(script).await.0.unwrap_err(), error);
    }
    let mut code = device();
    code["expires_in"] = json!(3);
    let server = Server::new(vec![(0, ok(code))]).await;
    let (task, mut events, _cancel) = start(&server);
    waiting(&mut events).await;
    tokio::time::advance(Duration::from_secs(3)).await;
    assert_eq!(finish(task).await.unwrap_err(), AuthError::Expired);
    assert_eq!(server.captured().len(), 1);
    for uri in [
        "https://microsoft.com/link",
        "https://www.microsoft.com/link",
        "https://www.microsoft.com/devicelogin",
    ] {
        let mut script = chain();
        let mut code = device();
        code["verification_uri"] = json!(uri);
        script[0].1 = ok(code);
        assert!(run(script).await.0.is_ok());
    }
    for skin in [
        json!(null),
        json!([]),
        json!([{"state":"ACTIVE","url":"https://evil.invalid/texture/abc"}]),
        json!([{"state":"ACTIVE","url":"https://textures.minecraft.net/texture/abc?secret=x"}]),
        json!([{"state":"ACTIVE","url":"https://textures.minecraft.net/texture/NOTHEX"}]),
    ] {
        let mut script = chain();
        let mut body = profile();
        body["skins"] = skin;
        script[6].1 = ok(body);
        let login = run(script).await.0.unwrap();
        assert_eq!(login.profile().skin_hash(), None);
        assert!(!format!("{login:?}").contains("SECRET"));
    }
}

#[tokio::test(start_paused = true)]
async fn every_stage_bounds_bodies_and_sanitizes_http_failures() {
    let _time = TimeGuard::new();
    for (stage, auth_stage) in STAGES.into_iter().enumerate() {
        for status in [301, 401, 403, 429, 500, 503] {
            // Poll 429 has its separate deadline/backoff tests.
            if stage == 1 && status == 429 {
                continue;
            }
            let mut script = chain();
            script[stage].1 = Reply::Json(
                status,
                json!({"error_description":"REMOTE_SECRET", "message":"REMOTE_SECRET"}),
            );
            script.truncate(stage + 1);
            let (result, requests, _) = run(script).await;
            assert_eq!(
                result.unwrap_err(),
                AuthError::Http {
                    stage: auth_stage,
                    status
                }
            );
            assert_eq!(requests.len(), stage + 1);
        }
        for chunked in [false, true] {
            let bytes = if chunked {
                let mut bytes = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n100001\r\n".to_vec();
                bytes.extend(vec![b'x'; 1024 * 1024 + 1]);
                bytes.extend_from_slice(b"\r\n0\r\n\r\n");
                bytes
            } else {
                b"HTTP/1.1 200 OK\r\nContent-Length: 1048577\r\nConnection: close\r\n\r\n".to_vec()
            };
            let mut script = chain();
            script[stage].1 = Reply::Raw(bytes);
            script.truncate(stage + 1);
            let (result, requests, _) = run(script).await;
            assert_eq!(
                result.unwrap_err(),
                AuthError::InvalidResponse { stage: auth_stage }
            );
            assert_eq!(requests.len(), stage + 1);
        }
    }
}

#[tokio::test(start_paused = true)]
async fn advertised_interval_and_approval_release_device_deadline() {
    let _time = TimeGuard::new();
    let mut script = chain();
    let mut code = device();
    code["interval"] = json!(2);
    code["expires_in"] = json!(3);
    script[0].1 = ok(code);
    // A valid response delayed beyond device expiry is still accepted after approval.
    script[2].1 = Reply::Delayed(xbox("XBL_SECRET"), Duration::from_secs(4));
    let server = Server::new(script).await;
    let start_at = Instant::now();
    let (task, mut events, _cancel) = start(&server);
    waiting(&mut events).await;
    tokio::time::advance(Duration::from_secs(1)).await;
    settle().await;
    assert_eq!(server.captured().len(), 1);
    tokio::time::advance(Duration::from_secs(1)).await;
    server.wait_requests(3).await;
    settle().await;
    tokio::time::advance(Duration::from_secs(4)).await;
    assert!(finish(task).await.is_ok());
    assert_eq!(
        server.captured()[1].at.duration_since(start_at).as_secs(),
        2
    );
    assert_eq!(
        server.captured()[3].at.duration_since(start_at).as_secs(),
        6
    );
    assert!(events.try_recv().is_err());
}

#[tokio::test(start_paused = true)]
async fn refresh_and_complete_respect_pre_cancellation_and_secret_debug() {
    let _time = TimeGuard::new();
    for closed in [false, true] {
        let server = Server::new(vec![]).await;
        let (sender, cancel) = watch::channel(false);
        if closed {
            drop(sender);
        } else {
            sender.send(true).unwrap();
        }
        assert_eq!(
            server
                .client
                .refresh_microsoft(SecretString::new("OLD_SECRET".into()), cancel)
                .await
                .unwrap_err(),
            AuthError::Cancelled
        );
        assert!(server.captured().is_empty());
    }
    let server = Server::new(vec![(1, ok(microsoft()))]).await;
    let (sender, cancel) = watch::channel(false);
    let tokens = server
        .client
        .refresh_microsoft(SecretString::new("OLD_SECRET".into()), cancel.clone())
        .await
        .unwrap();
    assert!(!format!("{tokens:?}").contains("SECRET"));
    sender.send(true).unwrap();
    assert_eq!(
        server
            .client
            .complete_minecraft(tokens, cancel)
            .await
            .unwrap_err(),
        AuthError::Cancelled
    );
    assert_eq!(server.captured().len(), 1);
}
