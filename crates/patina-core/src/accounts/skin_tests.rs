//! Bounded decoration tests: only temporary files and loopback HTTP.
use super::service_tests::{Fixture, ID};
use super::skin::{SkinClient, SkinError, decode_head, texture_url};
use image::{DynamicImage, ImageFormat, Rgba, RgbaImage};
use std::{io::Cursor, time::Duration};
use tokio::sync::watch;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

fn png(width: u32, height: u32) -> Vec<u8> {
    let mut skin = RgbaImage::from_pixel(width, height, Rgba([0, 0, 0, 0]));
    if width >= 48 && height >= 16 {
        for y in 8..16 {
            for x in 8..16 {
                skin.put_pixel(x, y, Rgba([200, 40, 20, 255]));
            }
        }
        skin.put_pixel(40, 8, Rgba([0, 0, 220, 128]));
    }
    let mut out = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(skin)
        .write_to(&mut out, ImageFormat::Png)
        .unwrap();
    out.into_inner()
}

#[test]
fn skin_png_limits_and_head_crop() {
    for height in [32, 64] {
        let out = decode_head(&png(64, height)).unwrap();
        let head = image::load_from_memory(&out).unwrap().into_rgba8();
        assert_eq!(head.dimensions(), (64, 64));
        assert_eq!(head.get_pixel(8, 0).0, [200, 40, 20, 255]);
        let blended = head.get_pixel(0, 0).0;
        assert!((99..=100).contains(&blended[0]));
        assert!((119..=121).contains(&blended[2]));
        assert_eq!(head.get_pixel(7, 7).0, blended);
    }
    for (w, h) in [(63, 64), (64, 33), (128, 128), (1, 1)] {
        assert!(matches!(
            decode_head(&png(w, h)),
            Err(SkinError::InvalidDimensions)
        ));
    }
    assert!(matches!(
        decode_head(&vec![0; 256 * 1024 + 1]),
        Err(SkinError::TooLarge)
    ));
    assert!(decode_head(b"not a PNG").is_err());
    let mut corrupt = png(64, 64);
    corrupt.truncate(corrupt.len() / 2);
    assert!(decode_head(&corrupt).is_err());
}

#[test]
fn only_validated_hash_can_construct_texture_url() {
    assert_eq!(
        texture_url("abcdef").unwrap().as_str(),
        "https://textures.minecraft.net/texture/abcdef"
    );
    for bad in [
        "",
        "../abc",
        "abc?x",
        "abc#x",
        "https://evil/abc",
        "http://textures.minecraft.net/texture/abc",
        "textures.minecraft.net:443/abc",
        "ABCDEF",
    ] {
        assert!(texture_url(bad).is_none(), "{bad}");
    }
    assert!(texture_url(&"a".repeat(65)).is_none());
}

#[tokio::test]
async fn skin_requests_never_receive_auth() {
    let server = MockServer::start().await;
    let sink = MockServer::start().await;
    let client = SkinClient::for_test(&server.uri()).unwrap();
    Mock::given(method("GET"))
        .and(path("/texture/abcdef"))
        .and(|r: &wiremock::Request| {
            !r.headers.contains_key("authorization") && !r.headers.contains_key("cookie")
        })
        .respond_with(ResponseTemplate::new(200).set_body_bytes(png(64, 64)))
        .expect(1)
        .mount(&server)
        .await;
    let (_tx, cancel) = watch::channel(false);
    assert!(client.fetch("abcdef", cancel.clone()).await.is_some());
    server.reset().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(302).insert_header("Location", sink.uri()))
        .expect(1)
        .mount(&server)
        .await;
    assert!(client.fetch("abcdef", cancel).await.is_none());
    assert!(sink.received_requests().await.unwrap().is_empty());
    for r in server.received_requests().await.unwrap() {
        assert!(!r.headers.contains_key("authorization"));
        assert!(!r.headers.contains_key("cookie"));
    }
}

#[tokio::test]
async fn skin_download_limit_and_cancellation() {
    let server = MockServer::start().await;
    let client = SkinClient::for_test(&server.uri()).unwrap();
    let (tx, cancel) = watch::channel(false);
    for size in [256 * 1024, 256 * 1024 + 1] {
        server.reset().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![0; size]))
            .mount(&server)
            .await;
        assert_eq!(
            client.fetch("abc", cancel.clone()).await.map(|b| b.len()),
            (size == 256 * 1024).then_some(size)
        );
    }
    tx.send(true).unwrap();
    server.reset().await;
    assert!(client.fetch("abc", cancel).await.is_none());
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn stale_avatar_cannot_reappear_after_remove() {
    let mut f = Fixture::new().await;
    f.service
        .set_skin_endpoint_for_test(&f.server.uri())
        .unwrap();
    f.chain("Player_123").await;
    f.login().await;
    Mock::given(path("/texture/abcdef"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(png(64, 64))
                .set_delay(Duration::from_millis(200)),
        )
        .mount(&f.server)
        .await;
    let service = f.service.clone();
    let (_tx, cancel) = watch::channel(false);
    let fetch = tokio::spawn(async move { service.cache_head(ID, cancel).await });
    loop {
        if f.server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .any(|r| r.url.path().starts_with("/texture/"))
        {
            break;
        }
        tokio::task::yield_now().await;
    }
    f.service.remove(ID).await.unwrap();
    assert_eq!(fetch.await.unwrap().unwrap(), None);
    assert!(
        !f.data
            .cache()
            .join("account-heads")
            .join(format!("{ID}-abcdef.png"))
            .exists()
    );
}

async fn logged_in_fixture() -> Fixture {
    let mut f = Fixture::new().await;
    f.service
        .set_skin_endpoint_for_test(&f.server.uri())
        .unwrap();
    f.chain("Player_123").await;
    let events = f.login().await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, super::login::LoginEvent::Done { .. }))
    );
    f
}
async fn head_response(f: &Fixture, body: Vec<u8>) {
    Mock::given(method("GET"))
        .and(path("/texture/abcdef"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
        .mount(&f.server)
        .await;
}
async fn wait_for_skin(f: &Fixture) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if f.server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .any(|r| r.url.path().starts_with("/texture/"))
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn cached_head_is_reused_and_cleanup_failure_never_resurrects_it() {
    let f = logged_in_fixture().await;
    head_response(&f, png(64, 64)).await;
    let (_tx, token) = watch::channel(false);
    let p = f
        .service
        .cache_head(ID, token.clone())
        .await
        .unwrap()
        .unwrap();
    let bytes = f
        .service
        .read_cached_head(ID, token.clone())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(image::load_from_memory(&bytes).unwrap().width(), 64);
    let revision = f.service.snapshot().await.unwrap().revision();
    assert_eq!(
        f.service.cache_head(ID, token.clone()).await.unwrap(),
        Some(p.clone())
    );
    assert_eq!(
        f.server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.url.path().starts_with("/texture/"))
            .count(),
        1
    );
    assert_eq!(f.service.snapshot().await.unwrap().revision(), revision);
    f.keyring
        .set_error(Some(super::credentials::CredentialError::Unavailable));
    assert_eq!(
        f.service.remove(ID).await.unwrap(),
        super::RemovalOutcome::CleanupPending
    );
    assert!(!p.exists());
    assert!(f.service.snapshot().await.unwrap().accounts().is_empty());
    assert!(
        f.service
            .read_cached_head(ID, token.clone())
            .await
            .unwrap()
            .is_none()
    );
    assert!(matches!(
        f.service.cache_head(ID, token).await,
        Err(super::error::AuthError::Account(_))
    ));
}

#[tokio::test]
async fn bad_skin_and_missing_skin_do_not_change_authenticated_account() {
    let f = logged_in_fixture().await;
    head_response(&f, b"not PNG".to_vec()).await;
    let (_tx, token) = watch::channel(false);
    let before = f.service.snapshot().await.unwrap();
    assert!(
        f.service
            .cache_head(ID, token.clone())
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(f.service.snapshot().await.unwrap(), before);
    assert!(f.service.resolve_launch(ID, token.clone()).await.is_ok());
    super::testing::seed_verified(
        &f.service,
        "Player_123",
        uuid::Uuid::parse_str(ID).unwrap(),
        "NEW_SENTINEL",
        super::service_tests::NOW,
        super::service_tests::NOW + 3600,
    )
    .await
    .unwrap();
    assert!(f.service.cache_head(ID, token).await.unwrap().is_none());
}

#[tokio::test]
async fn relogin_and_hash_change_discard_delayed_skin() {
    for relogin in [false, true] {
        let f = logged_in_fixture().await;
        Mock::given(path("/texture/abcdef"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(png(64, 64))
                    .set_delay(Duration::from_secs(2)),
            )
            .mount(&f.server)
            .await;
        let service = f.service.clone();
        let (_tx, token) = watch::channel(false);
        let task = tokio::spawn(async move { service.cache_head(ID, token).await });
        wait_for_skin(&f).await;
        if relogin {
            // Same UUID and hash, but new credential reference/generation.
            f.login().await;
        } else {
            let mut store = super::AccountStore::load(&f.data).unwrap();
            store.accounts[0].skin_hash = Some("123abc".into());
            store.save().unwrap();
            f.service.reload().await.unwrap();
        }
        assert_eq!(task.await.unwrap().unwrap(), None);
        assert!(
            !f.data
                .cache()
                .join("account-heads")
                .join(format!("{ID}-abcdef.png"))
                .exists()
        );
    }
}

#[tokio::test]
async fn dropping_fetch_or_cancelling_response_cannot_write_a_head() {
    for drop_waiter in [false, true] {
        let f = logged_in_fixture().await;
        Mock::given(path("/texture/abcdef"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(png(64, 64))
                    .set_delay(Duration::from_millis(150)),
            )
            .mount(&f.server)
            .await;
        let service = f.service.clone();
        let (tx, token) = watch::channel(false);
        let task = tokio::spawn(async move { service.cache_head(ID, token).await });
        wait_for_skin(&f).await;
        if drop_waiter {
            task.abort();
            let _ = task.await;
        } else {
            tx.send(true).unwrap();
            assert!(matches!(
                task.await.unwrap(),
                Err(super::error::AuthError::Cancelled)
            ));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            !f.data
                .cache()
                .join("account-heads")
                .join(format!("{ID}-abcdef.png"))
                .exists()
        );
    }
}

// Build a valid ancillary chunk without adding a second PNG dependency.
fn chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut body = kind.to_vec();
    body.extend_from_slice(data);
    let mut crc = !0u32;
    for &byte in &body {
        crc ^= byte as u32;
        for _ in 0..8 {
            crc = (crc >> 1) ^ if crc & 1 != 0 { 0xedb88320 } else { 0 };
        }
    }
    let mut out = (data.len() as u32).to_be_bytes().to_vec();
    out.extend(body);
    out.extend((!crc).to_be_bytes());
    out
}
fn with_text(mut png: Vec<u8>, data: &[u8]) -> Vec<u8> {
    png.splice(33..33, chunk(b"tEXt", data));
    png
}
#[tokio::test]
async fn read_cached_head_normalizes_metadata_and_bounds_corrupt_files() {
    let f = logged_in_fixture().await;
    head_response(&f, png(64, 64)).await;
    let (_tx, token) = watch::channel(false);
    let path = f
        .service
        .cache_head(ID, token.clone())
        .await
        .unwrap()
        .unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let annotated = with_text(bytes, b"Comment\0UNTRUSTED_TEXT");
    std::fs::write(&path, annotated).unwrap();
    let read = f
        .service
        .read_cached_head(ID, token.clone())
        .await
        .unwrap()
        .unwrap();
    assert!(
        !read
            .windows(b"UNTRUSTED_TEXT".len())
            .any(|w| w == b"UNTRUSTED_TEXT")
    );
    for bad in [b"not png".to_vec(), vec![0; 256 * 1024 + 1], png(32, 32)] {
        std::fs::write(&path, bad).unwrap();
        assert!(
            f.service
                .read_cached_head(ID, token.clone())
                .await
                .unwrap()
                .is_none()
        );
    }
}

#[test]
fn png_header_and_decompression_bombs_are_bounded() {
    use std::io::Write;
    let mut header = png(64, 64);
    let mut ihdr = header[16..29].to_vec();
    ihdr[..4].copy_from_slice(&u32::MAX.to_be_bytes());
    header.splice(8..33, chunk(b"IHDR", &ihdr));
    assert_eq!(decode_head(&header), Err(SkinError::InvalidDimensions));
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
    encoder.write_all(&vec![0; 2 * 1024 * 1024]).unwrap();
    let compressed = encoder.finish().unwrap();
    let mut data = b"profile\0\0".to_vec();
    data.extend(compressed);
    let mut bomb = png(64, 64);
    bomb.splice(33..33, chunk(b"iCCP", &data));
    // png 0.18 deliberately ignores invalid/over-budget ICC chunks. Either
    // rejection or sanitized pixels is safe; no profile survives normalization.
    if let Ok(normalized) = decode_head(&bomb) {
        assert!(!normalized.windows(4).any(|w| w == b"iCCP"));
        assert!(normalized.len() < 32 * 1024);
    }
    // Malformed IDAT compressed bytes are never accepted as a face.
    let mut malformed = png(64, 64);
    let offset = malformed.windows(4).position(|w| w == b"IDAT").unwrap();
    malformed[offset + 4] ^= 0xff;
    assert!(decode_head(&malformed).is_err());
}

#[tokio::test]
async fn chunked_and_header_only_download_caps() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    for (size, header_only) in [
        (256 * 1024, false),
        (256 * 1024 + 1, false),
        (256 * 1024 + 1, true),
    ] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut b = [0];
                socket.read_exact(&mut b).await.unwrap();
                request.push(b[0]);
                assert!(request.len() < 4096);
            }
            let request = String::from_utf8(request).unwrap().to_lowercase();
            assert!(request.starts_with("get /texture/abc http/1.1"));
            assert!(!request.contains("authorization:") && !request.contains("cookie:"));
            if header_only {
                socket
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {size}\r\nConnection: close\r\n\r\n"
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
            } else {
                socket.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n").await.unwrap();
                for start in (0..size).step_by(8192) {
                    let count = (size - start).min(8192);
                    if socket
                        .write_all(format!("{count:x}\r\n").as_bytes())
                        .await
                        .is_err()
                    {
                        return;
                    }
                    if socket.write_all(&vec![0; count]).await.is_err() {
                        return;
                    }
                    if socket.write_all(b"\r\n").await.is_err() {
                        return;
                    }
                }
                let _ = socket.write_all(b"0\r\n\r\n").await;
            }
        });
        let (_tx, token) = watch::channel(false);
        assert_eq!(
            SkinClient::for_test(&base)
                .unwrap()
                .fetch("abc", token)
                .await
                .map(|b| b.len()),
            (size == 256 * 1024).then_some(size)
        );
        server.await.unwrap();
    }
}

#[tokio::test]
async fn stalled_skin_times_out_and_closed_cancel_sender_stops_requests() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(png(64, 64))
                .set_delay(Duration::from_secs(60)),
        )
        .mount(&server)
        .await;
    let client = SkinClient::for_test(&server.uri()).unwrap();
    let (tx, token) = watch::channel(false);
    let closed = token.clone();
    let task = tokio::spawn(async move { client.fetch("abc", token).await });
    while server.received_requests().await.unwrap().is_empty() {
        tokio::task::yield_now().await;
    }
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(16)).await;
    assert!(task.await.unwrap().is_none());
    drop(tx);
    assert!(
        SkinClient::for_test(&server.uri())
            .unwrap()
            .fetch("abc", closed)
            .await
            .is_none()
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[test]
fn test_endpoint_constructor_is_loopback_only() {
    for endpoint in [
        "https://textures.minecraft.net",
        "http://example.test",
        "http://127.0.0.1:80/path",
        "http://127.0.0.1:80/?q=x",
        "http://127.0.0.1:80/#x",
        "http://user@127.0.0.1:80/",
    ] {
        assert!(SkinClient::for_test(endpoint).is_none());
    }
}

#[tokio::test]
async fn failed_removal_keeps_head_and_cross_owner_change_advances_reader_revision() {
    let f = logged_in_fixture().await;
    head_response(&f, png(64, 64)).await;
    let (_tx, token) = watch::channel(false);
    let head = f
        .service
        .cache_head(ID, token.clone())
        .await
        .unwrap()
        .unwrap();
    f.service
        .set_metadata_fault(crate::platform::credentials::WriteStage::Rename);
    assert!(f.service.remove(ID).await.is_err());
    assert!(head.exists());
    assert!(
        f.service
            .read_cached_head(ID, token.clone())
            .await
            .unwrap()
            .is_some()
    );
    let revision = f.service.snapshot().await.unwrap().revision();
    f.restarted().remove(ID).await.unwrap();
    // This service has not received the other owner's watch. Its lock-held
    // read reloads/reconciles and publishes before the consumer can use bytes.
    assert!(
        f.service
            .read_cached_head(ID, token)
            .await
            .unwrap()
            .is_none()
    );
    assert!(f.service.snapshot().await.unwrap().revision() > revision);
}
