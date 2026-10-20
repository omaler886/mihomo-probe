use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, Response, StatusCode, Uri};
use axum::Router;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use super::*;

const NAME: &str = "a.example";
const TIMEOUT: Duration = Duration::from_secs(2);
/// Deadline for tests whose mock answers immediately. It bounds a hung or
/// regressed client instead of being the verdict under test: the loopback
/// exchange completes in about a millisecond, so this leaves ~5000x headroom.
///
/// The oversized-declared-body case used to pass 100ms here, which made its
/// verdict a race. A loaded `cargo test --workspace` run can delay the response
/// headers past a tight deadline, and the expected size rejection then turns
/// into "DoH request timed out" -- a different string, so the exact assertion
/// failed for a reason that had nothing to do with the check it covers.
const HANG_GUARD: Duration = Duration::from_secs(5);

#[derive(Clone)]
enum Reply {
    Valid,
    BadId,
    BadName,
    BadType,
    BadClass,
    Malformed,
    Oversized,
    StreamedOversized,
    WrongMime,
    MissingMime,
    DuplicateMime,
    Status(StatusCode),
    Redirect(StatusCode, String),
    Negative,
    SlowHeaders,
}

/// Own a loopback endpoint and abort its server when the test drops it.
struct MockServer {
    view: ViewConfig,
    hits: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}

impl Drop for MockServer {
    /// Stop this mock task; takes no arguments and returns no value.
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Decode the single DNS query parameter from `uri`, asserting GET wire conventions.
fn decode_query(uri: &Uri, headers: &HeaderMap) -> Vec<u8> {
    assert_eq!(headers[ACCEPT], "application/dns-message");
    let url = reqwest::Url::parse(&format!("http://127.0.0.1{uri}")).unwrap();
    let tokens: Vec<_> = url.query_pairs().filter(|(key, _)| key == "dns").collect();
    assert_eq!(tokens.len(), 1);
    assert!(!tokens[0].1.contains('='));
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(tokens[0].1.as_bytes())
        .unwrap()
}

/// Find the end of the locally generated, uncompressed question in `query`.
fn question_end(query: &[u8]) -> usize {
    let mut end = 12;
    while query[end] != 0 {
        end += 1 + usize::from(query[end]);
    }
    end + 5
}

/// Echo `query`'s ID/question with duplicate addresses and a distinct third address.
fn answer_query(query: &[u8]) -> Vec<u8> {
    let end = question_end(query);
    let qtype = u16::from_be_bytes([query[end - 4], query[end - 3]]);
    let mut packet = query[..end].to_vec();
    packet[2..4].copy_from_slice(&0x8180u16.to_be_bytes());
    packet[6..8].copy_from_slice(&3u16.to_be_bytes());
    packet[8..12].fill(0);
    for (last_byte, ttl) in [(1, 300u32), (1, 120), (2, 60)] {
        let mut rdata = if qtype == TYPE_A {
            vec![192, 0, 2, 0]
        } else {
            let ip: std::net::Ipv6Addr = "2001:db8::".parse().unwrap();
            ip.octets().to_vec()
        };
        *rdata.last_mut().unwrap() = last_byte;
        packet.extend_from_slice(&[0xc0, 12]);
        packet.extend_from_slice(&qtype.to_be_bytes());
        packet.extend_from_slice(&1u16.to_be_bytes());
        packet.extend_from_slice(&ttl.to_be_bytes());
        packet.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
        packet.extend_from_slice(&rdata);
    }
    packet
}

/// Echo `query` as NXDOMAIN with an enclosing SOA and a 60-second negative TTL.
fn negative_answer(query: &[u8]) -> Vec<u8> {
    let mut packet = query[..question_end(query)].to_vec();
    packet[2..4].copy_from_slice(&0x8183u16.to_be_bytes());
    packet[6..12].copy_from_slice(&[0, 0, 0, 1, 0, 0]);
    packet.extend(crate::wire::encode_name("example").unwrap());
    packet.extend_from_slice(&[0, 6, 0, 1]);
    packet.extend_from_slice(&120u32.to_be_bytes());
    packet.extend_from_slice(&22u16.to_be_bytes());
    packet.extend_from_slice(&[0, 0]);
    for value in [1u32, 3600, 600, 86400, 60] {
        packet.extend_from_slice(&value.to_be_bytes());
    }
    packet
}

/// Apply `mode`'s DNS corruption to a valid answer, returning the resulting body.
fn reply_packet(mode: &Reply, query: &[u8]) -> Vec<u8> {
    let mut packet = answer_query(query);
    let end = question_end(query);
    match mode {
        Reply::BadId => packet[0] ^= 0xff,
        Reply::BadName => packet[13] = b'b',
        Reply::BadType => packet[end - 3] = TYPE_AAAA as u8,
        Reply::BadClass => packet[end - 1] = 3,
        Reply::Malformed => packet.truncate(packet.len() - 1),
        Reply::Oversized | Reply::StreamedOversized => {
            packet.resize(MAX_DNS_MESSAGE_SIZE + 1, 0);
        }
        Reply::Negative => return negative_answer(query),
        _ => {}
    }
    packet
}

/// Construct the HTTP response for `mode`, with optionally unknown-length streaming.
fn http_reply(mode: Reply, query: &[u8]) -> Response<Body> {
    let packet = reply_packet(&mode, query);
    let body = if matches!(mode, Reply::StreamedOversized) {
        Body::from_stream(Body::from(packet).into_data_stream())
    } else {
        Body::from(packet)
    };
    let mut response = Response::new(body);
    if !matches!(mode, Reply::MissingMime) {
        response.headers_mut().insert(
            CONTENT_TYPE,
            HeaderValue::from_static("application/dns-message"),
        );
    }
    match mode {
        Reply::WrongMime => {
            response
                .headers_mut()
                .insert(CONTENT_TYPE, HeaderValue::from_static("text/html"));
        }
        Reply::DuplicateMime => {
            response.headers_mut().append(
                CONTENT_TYPE,
                HeaderValue::from_static("application/dns-message"),
            );
        }
        Reply::Status(status) => *response.status_mut() = status,
        Reply::Redirect(status, location) => {
            *response.status_mut() = status;
            response
                .headers_mut()
                .insert("location", location.parse().unwrap());
        }
        _ => {}
    }
    response
}

/// Start a loopback-only DoH server that echoes queries using the requested `mode`.
async fn start_mock(mode: Reply) -> MockServer {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();
    let app = Router::new().fallback(move |uri: Uri, headers: HeaderMap| {
        let mode = mode.clone();
        let counter = counter.clone();
        async move {
            counter.fetch_add(1, Ordering::SeqCst);
            let query = decode_query(&uri, &headers);
            if matches!(mode, Reply::SlowHeaders) {
                tokio::time::sleep(Duration::from_millis(400)).await;
            }
            http_reply(mode, &query)
        }
    });
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    MockServer {
        view: ViewConfig {
            resolver: format!("http://{address}/dns-query?key=test-only-marker"),
            ecs: Some("192.0.2.255/24".to_string()),
            ecs_prefix: 25,
        },
        hits,
        task,
    }
}

/// Read one bounded mock HTTP request from `stream`, stopping after its headers.
async fn read_request(stream: &mut tokio::net::TcpStream) {
    let mut request = Vec::new();
    let mut buffer = [0; 1024];
    while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
        let length = stream.read(&mut buffer).await.unwrap();
        assert!(length > 0);
        request.extend_from_slice(&buffer[..length]);
        assert!(request.len() <= 16_384);
    }
}

/// Serve literal HTTP `headers`/`body`, optionally delaying body delivery.
async fn start_raw_mock(headers: &'static str, body: &'static [u8], delay: Duration) -> MockServer {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();
    let task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        counter.fetch_add(1, Ordering::SeqCst);
        read_request(&mut stream).await;
        stream.write_all(headers.as_bytes()).await.unwrap();
        tokio::time::sleep(delay).await;
        stream.write_all(body).await.expect("write local mock body");
    });
    MockServer {
        view: ViewConfig {
            resolver: format!("http://{address}/dns-query?key=test-only-marker"),
            ecs: None,
            ecs_prefix: DEFAULT_ECS_PREFIX,
        },
        hits,
        task,
    }
}

/// Successful local A/AAAA requests match dynamic IDs and expose complete TTL metadata.
#[tokio::test]
async fn query_answer_and_legacy_query_succeed_against_local_doh() {
    let server = start_mock(Reply::Valid).await;
    let resolver = DohResolver::new();
    let answer = resolver
        .query_answer(NAME, TYPE_A, &server.view, TIMEOUT)
        .await
        .unwrap();
    assert_eq!(answer.ips, ["192.0.2.1", "192.0.2.2"]);
    assert!(answer.cname_chain.is_empty());
    assert_eq!(answer.ttl_secs, 60);
    assert!(!answer.negative);
    let ips = resolver
        .query(NAME, TYPE_AAAA, &server.view, TIMEOUT)
        .await
        .unwrap();
    assert_eq!(ips, ["2001:db8::1", "2001:db8::2"]);
    assert_eq!(server.hits.load(Ordering::SeqCst), 2);
}

/// A validated local NXDOMAIN carries SOA TTL and maps to an empty legacy IP list.
#[tokio::test]
async fn negative_query_answer_and_wrapper_preserve_their_contracts() {
    let server = start_mock(Reply::Negative).await;
    let resolver = DohResolver::new();
    let answer = resolver
        .query_answer(NAME, TYPE_A, &server.view, TIMEOUT)
        .await
        .unwrap();
    assert!(answer.negative);
    assert!(answer.ips.is_empty());
    assert_eq!(answer.ttl_secs, 60);
    assert!(resolver
        .query(NAME, TYPE_A, &server.view, TIMEOUT)
        .await
        .unwrap()
        .is_empty());
}

/// The transport rejects wrong response IDs and each mismatched question field.
#[tokio::test]
async fn mismatched_local_responses_are_errors() {
    let resolver = DohResolver::new();
    for (mode, expected) in [
        (Reply::BadId, "DNS response ID mismatch"),
        (Reply::BadName, "DNS response question mismatch"),
        (Reply::BadType, "DNS response question mismatch"),
        (Reply::BadClass, "DNS response question mismatch"),
    ] {
        let server = start_mock(mode).await;
        let error = resolver
            .query_answer(NAME, TYPE_A, &server.view, TIMEOUT)
            .await
            .unwrap_err();
        assert_eq!(error, expected);
    }
}

/// A truncated answer body fails even when earlier address records were valid.
#[tokio::test]
async fn malformed_body_cannot_return_partial_ips() {
    let server = start_mock(Reply::Malformed).await;
    let error = DohResolver::new()
        .query(NAME, TYPE_A, &server.view, TIMEOUT)
        .await
        .unwrap_err();
    assert_eq!(error, "truncated DNS packet");
}

/// Both declared and unknown-length bodies stop at the DNS message byte limit.
#[tokio::test]
async fn oversized_fixed_and_chunked_bodies_are_rejected() {
    let resolver = DohResolver::new();
    for mode in [Reply::Oversized, Reply::StreamedOversized] {
        let server = start_mock(mode).await;
        let error = resolver
            .query_answer(NAME, TYPE_A, &server.view, TIMEOUT)
            .await
            .unwrap_err();
        assert_eq!(error, "DoH response exceeds 65535 bytes");
    }
}

/// Oversized Content-Length is rejected before a slow or absent body is awaited.
#[tokio::test]
async fn oversized_declared_body_is_rejected_before_reading_it() {
    // The mock declares 65536 bytes and never sends one, so no client can reach
    // this verdict by reading: only the declared length can produce it. A client
    // that waited for the body would see the connection close with the body
    // undelivered and report a transport error instead, so the exact text below
    // still proves the decision came from the headers.
    //
    // HANG_GUARD is what keeps that proof from depending on machine load: it is
    // a bound on a regressed client, far outside the ~1ms the exchange needs, so
    // a slow scheduler cannot substitute its own "DoH request timed out" verdict
    // for the size verdict this test is about.
    let server = start_raw_mock(
        "HTTP/1.1 200 OK\r\nContent-Type: application/dns-message\r\nContent-Length: 65536\r\n\r\n",
        b"",
        Duration::from_secs(1),
    )
    .await;
    let error = DohResolver::new()
        .query_answer(NAME, TYPE_A, &server.view, HANG_GUARD)
        .await
        .unwrap_err();
    assert_eq!(error, "DoH response exceeds 65535 bytes");
}

/// Missing, duplicate, and non-DNS media types cannot be interpreted as DNS.
#[tokio::test]
async fn wrong_mime_types_are_rejected() {
    let resolver = DohResolver::new();
    for mode in [Reply::WrongMime, Reply::MissingMime, Reply::DuplicateMime] {
        let server = start_mock(mode).await;
        let error = resolver
            .query_answer(NAME, TYPE_A, &server.view, TIMEOUT)
            .await
            .unwrap_err();
        assert_eq!(error, "Invalid DoH response media type");
    }
}

/// Non-200 statuses remain errors even when their body contains valid DNS.
#[tokio::test]
async fn wrong_http_statuses_are_rejected_without_body_or_url_leakage() {
    let resolver = DohResolver::new();
    for status in [
        StatusCode::CREATED,
        StatusCode::NO_CONTENT,
        StatusCode::BAD_REQUEST,
        StatusCode::SERVICE_UNAVAILABLE,
    ] {
        let server = start_mock(Reply::Status(status)).await;
        let error = resolver
            .query_answer(NAME, TYPE_A, &server.view, TIMEOUT)
            .await
            .unwrap_err();
        assert_eq!(error, format!("HTTP {}", status.as_u16()));
        assert!(!error.contains("test-only-marker"));
    }
}

/// Every common redirect code fails locally without contacting its valid target.
#[tokio::test]
async fn redirects_are_never_followed() {
    let destination = start_mock(Reply::Valid).await;
    let resolver = DohResolver::new();
    for code in [301, 302, 303, 307, 308] {
        let status = StatusCode::from_u16(code).unwrap();
        let source = start_mock(Reply::Redirect(status, destination.view.resolver.clone())).await;
        let error = resolver
            .query_answer(NAME, TYPE_A, &source.view, TIMEOUT)
            .await
            .unwrap_err();
        assert_eq!(error, format!("HTTP {code}"));
        assert_eq!(source.hits.load(Ordering::SeqCst), 1);
    }
    assert_eq!(destination.hits.load(Ordering::SeqCst), 0);
}

/// Supplied deadlines cover waiting for both response headers and response body bytes.
#[tokio::test]
async fn caller_timeout_applies_to_headers_and_body() {
    let slow_headers = start_mock(Reply::SlowHeaders).await;
    let slow_body = start_raw_mock(
        "HTTP/1.1 200 OK\r\nContent-Type: application/dns-message\r\nContent-Length: 100\r\n\r\n",
        b"",
        Duration::from_millis(400),
    )
    .await;
    let resolver = DohResolver::new();
    for view in [&slow_headers.view, &slow_body.view] {
        let error = resolver
            .query_answer(NAME, TYPE_A, view, Duration::from_millis(75))
            .await
            .unwrap_err();
        assert_eq!(error, "DoH request timed out");
        assert!(!error.contains("test-only-marker"));
    }
}

/// A prematurely closed HTTP body produces a sanitized error, not partial DNS.
#[tokio::test]
async fn failed_body_reads_are_sanitized() {
    let server = start_raw_mock(
        "HTTP/1.1 200 OK\r\nContent-Type: application/dns-message\r\nContent-Length: 100\r\n\r\n",
        b"not-dns",
        Duration::ZERO,
    )
    .await;
    let error = DohResolver::new()
        .query_answer(NAME, TYPE_A, &server.view, TIMEOUT)
        .await
        .unwrap_err();
    assert_eq!(error, "DoH transport failed");
}

/// View resolution deduplicates real local answers, skips empty endpoints, and isolates errors.
#[tokio::test]
async fn resolve_views_dedupes_answers_keeps_order_and_isolates_failures() {
    let good = start_mock(Reply::Valid).await;
    let bad = start_mock(Reply::BadId).await;
    let views = [
        ("cn".to_string(), good.view.clone()),
        ("overseas".to_string(), bad.view.clone()),
        (
            "disabled".to_string(),
            ViewConfig {
                resolver: String::new(),
                ecs: None,
                ecs_prefix: 24,
            },
        ),
    ];
    let actual = DohResolver::new()
        .resolve_views(NAME, &views, TIMEOUT)
        .await;
    assert_eq!(
        actual,
        [
            (
                "cn".to_string(),
                vec![
                    "192.0.2.1".into(),
                    "192.0.2.2".into(),
                    "2001:db8::1".into(),
                    "2001:db8::2".into()
                ]
            ),
            ("overseas".to_string(), vec![]),
        ]
    );
    assert_eq!(good.hits.load(Ordering::SeqCst), 2);
    assert_eq!(bad.hits.load(Ordering::SeqCst), 2);
}

/// Structured URL construction preserves endpoint parameters and replaces all old DNS pairs.
#[test]
fn query_parameters_are_encoded_without_duplicate_dns_values() {
    let url = query_url(
        "https://resolver.example/dns-query?key=test-only-marker&dns=old&dns=older&value=%2B%2F",
        "new-token",
    )
    .unwrap();
    let pairs: Vec<_> = url.query_pairs().collect();
    assert_eq!(pairs.iter().filter(|(key, _)| key == "dns").count(), 1);
    assert!(pairs
        .iter()
        .any(|(key, value)| key == "dns" && value == "new-token"));
    assert!(pairs
        .iter()
        .any(|(key, value)| key == "key" && value == "test-only-marker"));
    assert!(pairs
        .iter()
        .any(|(key, value)| key == "value" && value == "+/"));
}

/// Invalid schemes, userinfo, fragments, and malformed endpoints fail before any request.
#[tokio::test]
async fn invalid_endpoints_fail_locally_without_echoing_user_input() {
    let resolver = DohResolver::new();
    for endpoint in [
        "ftp://127.0.0.1/test-only-marker",
        "https://user:test-only-marker@127.0.0.1/",
        "http://127.0.0.1/#test-only-marker",
        "test-only-marker",
    ] {
        let view = ViewConfig {
            resolver: endpoint.to_string(),
            ecs: None,
            ecs_prefix: 24,
        };
        let error = resolver
            .query_answer(NAME, TYPE_A, &view, TIMEOUT)
            .await
            .unwrap_err();
        assert_eq!(error, "Invalid DoH resolver URL");
    }
    let view = ViewConfig {
        resolver: "http://127.0.0.1/".into(),
        ecs: None,
        ecs_prefix: 24,
    };
    let error = resolver
        .query_answer("test-only-marker/bad", TYPE_A, &view, TIMEOUT)
        .await
        .unwrap_err();
    assert!(!error.contains("test-only-marker"));
}
