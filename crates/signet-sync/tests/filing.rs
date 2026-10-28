//! What the junk filter decides, exercised through the real ingestion path.
//!
//! The unit tests in `junk_signals` prove the scoring arithmetic. These prove the wiring around
//! it: that an explicit block actually files mail, that an explicit allow actually rescues it, and
//! that the reason shown to the user matches what happened.

use signet_store::{
    Account, AccountSettings, FolderKind, MessageDetail, NewAccount, NewMessage, Recipient,
    Security, Store,
};
use signet_sync::ingest::{StoreContext, store_message};

struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "signet-filing-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn account(store: &Store, junk_filter_enabled: bool) -> Account {
    let id = store
        .add_account(
            &NewAccount {
                display_name: "Dev".into(),
                email: "dev@example.com".into(),
                pop_host: "mail.test".into(),
                pop_port: 995,
                pop_security: Security::Tls,
                pop_user: "dev".into(),
                smtp_host: "mail.test".into(),
                smtp_port: 587,
                smtp_security: Security::Starttls,
                smtp_user: "dev".into(),
                leave_on_server: true,
                delete_after_days: None,
                poll_minutes: 5,
                signature: String::new(),
                protocol: Default::default(),
            },
            "handle",
        )
        .unwrap();

    let a = store.get_account(id).unwrap();
    store
        .update_account_settings(
            id,
            &AccountSettings {
                display_name: a.display_name.clone(),
                signature: a.signature.clone(),
                poll_minutes: a.poll_minutes,
                leave_on_server: a.leave_on_server,
                delete_after_days: a.delete_after_days,
                junk_filter_enabled,
                junk_threshold: a.junk_threshold,
                is_enabled: a.is_enabled,
            },
        )
        .unwrap();
    store.get_account(id).unwrap()
}

/// A message that every signal points at: authentication failed, the link lies about where it
/// goes, and there is an executable attached.
fn obvious_phish(from: &str) -> Vec<u8> {
    format!(
        "From: Accounts <{from}>\r\n\
         To: dev@example.com\r\n\
         Subject: Urgent: verify your account\r\n\
         Message-ID: <phish-1@sender.test>\r\n\
         Authentication-Results: mx.example.com; spf=fail; dkim=fail; dmarc=fail\r\n\
         Content-Type: text/html\r\n\
         \r\n\
         <a href=\"https://evil.test/login\">https://paypal.com/login</a>\r\n"
    )
    .into_bytes()
}

/// An entirely ordinary customer enquiry.
fn plain_enquiry(from: &str) -> Vec<u8> {
    format!(
        "From: Buyer <{from}>\r\n\
         To: dev@example.com\r\n\
         Subject: Quote for 40 downlights\r\n\
         Message-ID: <enquiry-1@customer.test>\r\n\
         Authentication-Results: mx.example.com; spf=pass; dkim=pass; dmarc=pass\r\n\
         \r\n\
         Could you price 40 of the 12W downlights please?\r\n"
    )
    .into_bytes()
}

/// Run one message through the real pipeline and return the store row it produced.
async fn ingest(
    store: &Store,
    acct: &Account,
    raw: Vec<u8>,
    stem: &str,
) -> (MessageDetail, String) {
    let ctx = StoreContext::new(store, acct).await.unwrap();
    let raw_path = ctx.raw_dir.join(format!("{stem}.eml"));
    std::fs::write(&raw_path, &raw).unwrap();
    let parsed = signet_mime::parse(&raw).expect("parse");
    let size = raw.len() as u64;

    let stored = store_message(
        store,
        acct,
        ctx.inbox_folder,
        &ctx,
        &parsed,
        &raw_path,
        stem,
        size,
        false,
        true,
    )
    .await
    .expect("store");

    (
        store.get_message(stored.message_id).unwrap(),
        store.filed_by(stored.message_id).unwrap(),
    )
}

#[tokio::test]
async fn a_blocked_sender_is_filed_even_when_the_junk_filter_is_switched_off() {
    // The filter being off means "stop guessing", not "ignore what I told you". A block button
    // that quietly does nothing is worse than no block button.
    let dir = TempDir::new("block-off");
    let store = Store::open(&dir.0).unwrap();
    let acct = account(&store, false);
    store.set_sender_rule("spammer.test", "block").unwrap();

    let (msg, filed_by) = ingest(&store, &acct, plain_enquiry("hi@spammer.test"), "b1").await;

    let junk = store.folder_id(acct.id, FolderKind::Junk).unwrap();
    assert_eq!(msg.folder_id, junk, "a blocked sender reached the Inbox");
    assert_eq!(
        filed_by, "your block list",
        "the user was told the filter guessed, when it did not"
    );
    assert!(
        store.spam_reasons(msg.id).unwrap().is_empty(),
        "a rule needs no statistical explanation"
    );
}

#[tokio::test]
async fn nothing_is_guessed_at_when_the_junk_filter_is_switched_off() {
    let dir = TempDir::new("off");
    let store = Store::open(&dir.0).unwrap();
    let acct = account(&store, false);

    let (msg, filed_by) = ingest(&store, &acct, obvious_phish("x@sender.test"), "o1").await;

    assert_eq!(
        msg.folder_id,
        ctx_inbox(&store, &acct),
        "the filter was off but a message was filed anyway: {filed_by}"
    );
}

fn ctx_inbox(store: &Store, acct: &Account) -> i64 {
    store.folder_id(acct.id, FolderKind::Inbox).unwrap()
}

#[tokio::test]
async fn an_allowed_sender_survives_every_bad_signal() {
    let dir = TempDir::new("allow");
    let store = Store::open(&dir.0).unwrap();
    let acct = account(&store, true);
    store.set_sender_rule("sender.test", "allow").unwrap();

    let (msg, _) = ingest(&store, &acct, obvious_phish("x@sender.test"), "a1").await;

    assert_eq!(
        msg.folder_id,
        ctx_inbox(&store, &acct),
        "an allowed sender was filed as junk"
    );
}

#[tokio::test]
async fn an_obvious_phish_is_filed_with_a_reason_the_user_can_read() {
    let dir = TempDir::new("phish");
    let store = Store::open(&dir.0).unwrap();
    let acct = account(&store, true);

    let (msg, filed_by) = ingest(&store, &acct, obvious_phish("x@sender.test"), "p1").await;

    let junk = store.folder_id(acct.id, FolderKind::Junk).unwrap();
    assert_eq!(msg.folder_id, junk, "an obvious phish reached the Inbox");

    let why = store.spam_reasons(msg.id).unwrap();
    assert!(
        why.contains("evil.test"),
        "the deceptive link was not explained: {why}"
    );
    assert!(
        filed_by.starts_with("junk filter:"),
        "filed_by was {filed_by:?}"
    );
}

#[tokio::test]
async fn an_ordinary_enquiry_is_left_alone() {
    // The expensive mistake. A customer's quote request must not land in Junk.
    let dir = TempDir::new("ham");
    let store = Store::open(&dir.0).unwrap();
    let acct = account(&store, true);

    let (msg, filed_by) = ingest(&store, &acct, plain_enquiry("buyer@customer.test"), "h1").await;

    assert_eq!(
        msg.folder_id,
        ctx_inbox(&store, &acct),
        "a customer enquiry was filed as junk: {filed_by}"
    );
}

/// Record that we have written to someone, which is what makes them a known correspondent.
fn we_have_written_to(store: &Store, acct: &Account, addr: &str) {
    let sent = store.folder_id(acct.id, FolderKind::Sent).unwrap();
    store
        .insert_message(&NewMessage {
            account_id: acct.id,
            folder_id: sent,
            rfc822_message_id: Some(format!("<sent-to-{addr}>")),
            in_reply_to: None,
            thread_id: format!("sent-to-{addr}"),
            from_name: "Dev".into(),
            from_addr: acct.email.clone(),
            subject: "Our quote".into(),
            date_utc: signet_store::now_utc(),
            received_utc: signet_store::now_utc(),
            has_attachments: false,
            has_remote_content: false,
            size_bytes: 10,
            preview_text: "attached".into(),
            body_text: "attached".into(),
            body_html: None,
            raw_path: String::new(),
            recipients: vec![Recipient {
                kind: "to".into(),
                name: String::new(),
                addr: addr.into(),
            }],
            attachments: vec![],
        })
        .unwrap();
}

#[tokio::test]
async fn a_known_supplier_showing_signs_of_forgery_is_delivered_but_flagged() {
    // Invoice fraud, and the reason the safety valve cannot simply be removed: the sender really
    // is the supplier we have been emailing, because their account was taken over. Filing it
    // would hide it; delivering it silently would let it work.
    let dir = TempDir::new("bec");
    let store = Store::open(&dir.0).unwrap();
    let acct = account(&store, true);
    we_have_written_to(&store, &acct, "accounts@supplier.test");

    let (msg, filed_by) = ingest(
        &store,
        &acct,
        obvious_phish("accounts@supplier.test"),
        "bec1",
    )
    .await;

    assert_eq!(
        msg.folder_id,
        ctx_inbox(&store, &acct),
        "a supplier we deal with was filed as junk: {filed_by}"
    );

    let warning = store.spam_reasons(msg.id).unwrap();
    assert!(
        warning.contains("evil.test"),
        "delivered with no warning at all: {warning:?}"
    );
    assert!(
        !warning.contains("corresponded"),
        "the warning reassured instead of warning: {warning}"
    );
}
