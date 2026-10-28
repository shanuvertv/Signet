//! Integration tests against a real POP3 server.
//!
//! These run against Mailpit, which is an implementation we did not write — the point being that
//! the unit tests only prove our codec agrees with itself. Set `SIGNET_TEST_POP3_HOST` to enable;
//! see `docs/testing.md`.
//!
//! Dovecot is the reference server (ADR 0001 / D6) and should be run against before each release,
//! but it is not something CI can spin up in seconds.

use std::time::Duration;

use signet_net::pop3::Pop3Config;
use signet_sync::pop3_transport::{Pop3Credentials, Pop3Transport};
use signet_sync::transport::{FolderId, MailTransport, RemoteId};

struct TestServer {
    host: String,
    port: u16,
    user: String,
    password: String,
}

/// Read the server details from the environment, or `None` if this run should skip.
fn server() -> Option<TestServer> {
    let host = std::env::var("SIGNET_TEST_POP3_HOST").ok()?;
    Some(TestServer {
        host,
        port: std::env::var("SIGNET_TEST_POP3_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(1110),
        user: std::env::var("SIGNET_TEST_POP3_USER").unwrap_or_else(|_| "signet".into()),
        password: std::env::var("SIGNET_TEST_POP3_PASSWORD").unwrap_or_else(|_| "spike".into()),
    })
}

macro_rules! server_or_skip {
    () => {
        match server() {
            Some(s) => s,
            None => {
                eprintln!("SKIPPED: set SIGNET_TEST_POP3_HOST to run the live POP3 tests");
                return;
            }
        }
    };
}

async fn connect(s: &TestServer) -> Pop3Transport {
    let config = Pop3Config::plaintext(&s.host)
        .with_port(s.port)
        .with_timeout(Duration::from_secs(15));
    let mut t = Pop3Transport::new(
        config,
        Pop3Credentials {
            user: s.user.clone(),
            password: s.password.clone(),
            allow_plaintext: true, // the local test server has no TLS
        },
    );
    t.connect().await.expect("connect to test POP3 server");
    t
}

/// Fetch every message and return (subject, full body) pairs.
async fn fetch_all(t: &mut Pop3Transport) -> Vec<(String, Vec<u8>)> {
    let inbox = FolderId::inbox();
    let ids: Vec<RemoteId> = t
        .list_messages(&inbox)
        .await
        .expect("list")
        .into_iter()
        .map(|m| m.id)
        .collect();

    let mut out = Vec::new();
    for id in ids {
        let mut raw = Vec::new();
        t.fetch_message(&inbox, &id, &mut raw).await.expect("retr");
        let text = String::from_utf8_lossy(&raw);
        let subject = text
            .lines()
            .find(|l| l.to_ascii_lowercase().starts_with("subject:"))
            .map(|l| l["subject:".len()..].trim().to_string())
            .unwrap_or_default();
        out.push((subject, raw));
    }
    out
}

#[tokio::test]
async fn retr_round_trips_dot_stuffed_lines() {
    let s = server_or_skip!();
    let mut t = connect(&s).await;
    let messages = fetch_all(&mut t).await;

    let (_, raw) = messages
        .iter()
        .find(|(subject, _)| subject.contains("start with a dot"))
        .expect("seeded dot-stuffed message is present; run tools/seed-mailpit.mjs");
    let body = String::from_utf8_lossy(raw);

    // Each of these was byte-stuffed by the sender and must arrive unstuffed exactly once.
    assert!(
        body.contains("\r\n.\r\n") || body.ends_with("\r\n."),
        "a lone '.' line did not survive: {body:?}"
    );
    assert!(
        body.contains("..and this one begins with two."),
        "'..and this one' lost a dot: {body:?}"
    );
    assert!(
        body.contains("\r\n. leading dot then text\r\n"),
        "'. leading dot' lost its dot: {body:?}"
    );
    assert!(
        body.contains("\r\ndone"),
        "content after the dotted lines was truncated: {body:?}"
    );

    Box::new(t).commit().await.expect("quit");
}

#[tokio::test]
async fn retr_streams_a_message_larger_than_the_read_buffer() {
    let s = server_or_skip!();
    let mut t = connect(&s).await;
    let inbox = FolderId::inbox();

    let messages = t.list_messages(&inbox).await.expect("list");
    let biggest = messages
        .iter()
        .max_by_key(|m| m.size)
        .expect("at least one message");
    assert!(
        biggest.size > 500_000,
        "expected the seeded ~528 KiB message; largest was {} bytes",
        biggest.size
    );

    let mut raw = Vec::new();
    let written = t
        .fetch_message(&inbox, &biggest.id, &mut raw)
        .await
        .expect("retr");

    assert_eq!(
        written,
        raw.len() as u64,
        "byte count disagrees with output"
    );
    // LIST size is the server's own accounting and may differ slightly in line endings, but not
    // by much. A large discrepancy means we truncated.
    let diff = (biggest.size as i64 - written as i64).abs();
    assert!(
        diff < 4096,
        "streamed {written} bytes but LIST reported {}",
        biggest.size
    );
    // The closing boundary must be present, and nothing but whitespace may follow it. Servers are
    // entitled to append a trailing CRLF (Mailpit does), so this is deliberately not `ends_with`.
    let tail = String::from_utf8_lossy(&raw[raw.len().saturating_sub(64)..]);
    let (_, after_terminator) = tail
        .rsplit_once("--BOUND--")
        .expect("multipart terminator missing; message was truncated");
    assert!(
        after_terminator.trim().is_empty(),
        "unexpected content after the multipart terminator: {after_terminator:?}"
    );

    Box::new(t).commit().await.expect("quit");
}

#[tokio::test]
async fn top_returns_headers_without_the_body() {
    let s = server_or_skip!();
    let mut t = connect(&s).await;
    let inbox = FolderId::inbox();

    let messages = t.list_messages(&inbox).await.expect("list");
    let biggest = messages
        .iter()
        .max_by_key(|m| m.size)
        .expect("at least one message");

    let headers = t
        .fetch_headers(&inbox, &biggest.id)
        .await
        .expect("top 0 lines");

    // The whole point of the filter-before-download pass: headers must be tiny even when the
    // message is not.
    assert!(
        headers.len() < 4096,
        "TOP returned {} bytes; it is fetching the body",
        headers.len()
    );
    let text = String::from_utf8_lossy(&headers);
    assert!(text.to_ascii_lowercase().contains("subject:"));
    assert!(!text.contains("--BOUND--"), "TOP leaked the body");

    Box::new(t).commit().await.expect("quit");
}

#[tokio::test]
async fn uidls_are_present_stable_and_unique() {
    let s = server_or_skip!();

    let first = {
        let mut t = connect(&s).await;
        let ids: Vec<_> = t
            .list_messages(&FolderId::inbox())
            .await
            .expect("list")
            .into_iter()
            .map(|m| m.id)
            .collect();
        Box::new(t).commit().await.expect("quit");
        ids
    };

    assert!(!first.is_empty(), "no messages; run tools/seed-mailpit.mjs");
    // A fallback id means the server gave us no UIDL, which would degrade dedupe.
    assert!(
        first.iter().all(|id| !id.0.starts_with("size:")),
        "server did not supply UIDL for every message"
    );

    let unique: std::collections::HashSet<_> = first.iter().map(|id| &id.0).collect();
    assert_eq!(unique.len(), first.len(), "UIDLs are not unique");

    // Across a separate session the ids must be identical — this is the property `pop_seen`
    // depends on, and the reason we key on UIDL rather than message number.
    let second = {
        let mut t = connect(&s).await;
        let ids: Vec<_> = t
            .list_messages(&FolderId::inbox())
            .await
            .expect("list")
            .into_iter()
            .map(|m| m.id)
            .collect();
        Box::new(t).commit().await.expect("quit");
        ids
    };
    assert_eq!(first, second, "UIDLs changed between sessions");
}

#[tokio::test]
async fn utf8_headers_survive_retrieval() {
    let s = server_or_skip!();
    let mut t = connect(&s).await;
    let messages = fetch_all(&mut t).await;

    let found = messages
        .iter()
        .any(|(subject, _)| subject.contains("=?utf-8?Q?Lumi"));
    assert!(found, "encoded-word subject was mangled in transit");

    let (_, raw) = messages
        .iter()
        .find(|(_, raw)| String::from_utf8_lossy(raw).contains("Accents:"))
        .expect("utf-8 message present");
    let body = String::from_utf8_lossy(raw);
    assert!(body.contains("éàüñ"), "8-bit body was corrupted: {body:?}");
    assert!(body.contains("日本語"), "multibyte body was corrupted");

    Box::new(t).commit().await.expect("quit");
}
