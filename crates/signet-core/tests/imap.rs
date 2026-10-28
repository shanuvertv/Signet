//! IMAP, against a real server.
//!
//! The tests that matter here simulate **two installations sharing one mailbox**, because that is
//! the whole argument for IMAP over POP3 for a sales team: what one person does is visible to
//! everyone, with no shared file to maintain.
//!
//! Gated on `SIGNET_TEST_IMAP_HOST`. GreenMail is the server in CI; see `docs/testing.md`.

use signet_core::App;
use signet_store::{FolderKind, MessageQuery, NewAccount, Protocol, Security};

struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "signet-imap-{tag}-{}-{:?}",
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

struct Server {
    host: String,
    port: u16,
    user: String,
    password: String,
}

fn server() -> Option<Server> {
    let host = std::env::var("SIGNET_TEST_IMAP_HOST").ok()?;
    Some(Server {
        host,
        port: std::env::var("SIGNET_TEST_IMAP_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(3143),
        user: std::env::var("SIGNET_TEST_IMAP_USER").unwrap_or_else(|_| "signet".into()),
        password: std::env::var("SIGNET_TEST_IMAP_PASSWORD").unwrap_or_else(|_| "spike".into()),
    })
}

macro_rules! server_or_skip {
    () => {
        match server() {
            Some(s) => s,
            None => {
                eprintln!("SKIPPED: set SIGNET_TEST_IMAP_HOST to run the live IMAP tests");
                return;
            }
        }
    };
}

fn account(s: &Server) -> NewAccount {
    NewAccount {
        display_name: "Sales team".into(),
        email: "signet@example.com".into(),
        pop_host: s.host.clone(),
        pop_port: s.port,
        // The local test server presents a self-signed certificate, which the platform verifier
        // rightly refuses. Plaintext is the lab-only opt-in; nothing else is relaxed.
        pop_security: Security::Plain,
        pop_user: s.user.clone(),
        smtp_host: String::new(),
        smtp_port: 587,
        smtp_security: Security::Starttls,
        smtp_user: String::new(),
        leave_on_server: true,
        delete_after_days: None,
        poll_minutes: 5,
        signature: String::new(),
        protocol: Protocol::Imap,
    }
}

async fn machine(tag: &str, s: &Server) -> (TempDir, App, i64) {
    let dir = TempDir::new(tag);
    let app = App::open(&dir.0).expect("open");
    let id = app
        .add_account(account(s), &s.password)
        .expect("add imap account");
    app.sync_account(id).await.expect("initial sync");
    (dir, app, id)
}

fn inbox_of(app: &App, account_id: i64) -> signet_store::Folder {
    app.list_folders(Some(account_id))
        .unwrap()
        .into_iter()
        .find(|f| f.kind == FolderKind::Inbox)
        .expect("inbox")
}

#[tokio::test]
async fn a_sync_discovers_folders_and_ingests_mail() {
    let s = server_or_skip!();
    let (_d, app, id) = machine("basic", &s).await;

    let folders = app.list_folders(Some(id)).unwrap();
    // Folders come from the server for IMAP, not from our own defaults.
    assert!(
        folders.iter().any(|f| f.kind == FolderKind::Inbox),
        "no INBOX discovered"
    );

    let inbox = inbox_of(&app, id);
    assert!(inbox.total > 0, "nothing ingested; seed the server first");

    let messages = app
        .list_messages(MessageQuery {
            folder_id: Some(inbox.id),
            limit: 100,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(messages.len() as i64, inbox.total);
    assert!(messages.iter().all(|m| !m.from_addr.is_empty()));
}

#[tokio::test]
async fn re_syncing_does_not_duplicate_mail() {
    let s = server_or_skip!();
    let (_d, app, id) = machine("idempotent", &s).await;

    let before = inbox_of(&app, id).total;
    let report = app.sync_account(id).await.expect("second sync");

    assert_eq!(
        report.new_messages, 0,
        "re-sync duplicated mail: {report:?}"
    );
    assert!(report.already_seen > 0);
    assert_eq!(inbox_of(&app, id).total, before);
}

#[tokio::test]
async fn encoded_headers_and_attachments_survive_imap() {
    let s = server_or_skip!();
    let (_d, app, id) = machine("content", &s).await;

    let inbox = inbox_of(&app, id);
    let messages = app
        .list_messages(MessageQuery {
            folder_id: Some(inbox.id),
            limit: 100,
            ..Default::default()
        })
        .unwrap();

    assert!(
        messages
            .iter()
            .any(|m| m.subject.contains("Lumière") && m.subject.contains("résumé")),
        "encoded-word subject was not decoded: {:?}",
        messages.iter().map(|m| &m.subject).collect::<Vec<_>>()
    );
    assert!(
        messages.iter().any(|m| m.has_attachments),
        "the multipart fixture lost its attachment"
    );

    // Byte-stuffing is a POP3 concern, but the same fixture must arrive intact over IMAP.
    let dotted = messages
        .iter()
        .find(|m| m.subject.contains("start with a dot"))
        .expect("dot fixture");
    let detail = app.get_message(dotted.id).unwrap();
    assert!(
        detail.body_text.contains("..and this one begins with two."),
        "body was mangled: {:?}",
        detail.body_text
    );
}

/// The headline difference from POP3.
#[tokio::test]
async fn read_state_is_shared_between_two_machines() {
    let s = server_or_skip!();

    let (_da, alice, alice_acc) = machine("share-a", &s).await;
    let (_db, bob, bob_acc) = machine("share-b", &s).await;

    // Pick the same message on each machine by its Message-ID, since local row ids differ.
    let pick = |app: &App, acc: i64| -> (i64, String) {
        let inbox = inbox_of(app, acc);
        let list = app
            .list_messages(MessageQuery {
                folder_id: Some(inbox.id),
                limit: 100,
                ..Default::default()
            })
            .unwrap();
        let first = list
            .iter()
            .find(|m| m.subject.contains("Phase 0 smoke test"))
            .expect("smoke-test fixture");
        let detail = app.get_message(first.id).unwrap();
        (first.id, detail.rfc822_message_id.unwrap_or_default())
    };

    let (alice_msg, key_a) = pick(&alice, alice_acc);
    let (bob_msg, key_b) = pick(&bob, bob_acc);
    assert_eq!(key_a, key_b, "the two machines picked different messages");

    // Start from a known state, agreed by both.
    alice.set_read(vec![alice_msg], false).unwrap();
    alice.sync_account(alice_acc).await.unwrap();
    bob.sync_account(bob_acc).await.unwrap();
    assert!(!bob.get_message(bob_msg).unwrap().is_read);

    // Alice reads it and syncs, which pushes \Seen to the server.
    alice.set_read(vec![alice_msg], true).unwrap();
    alice.sync_account(alice_acc).await.unwrap();

    // Bob syncs and sees it — with no shared file anywhere in the picture.
    bob.sync_account(bob_acc).await.unwrap();
    assert!(
        bob.get_message(bob_msg).unwrap().is_read,
        "read state did not reach the other machine; this is the whole point of IMAP"
    );
}

#[tokio::test]
async fn flagging_is_shared_between_two_machines() {
    let s = server_or_skip!();

    let (_da, alice, alice_acc) = machine("flag-a", &s).await;
    let (_db, bob, bob_acc) = machine("flag-b", &s).await;

    let find = |app: &App, acc: i64| -> i64 {
        let inbox = inbox_of(app, acc);
        app.list_messages(MessageQuery {
            folder_id: Some(inbox.id),
            limit: 100,
            ..Default::default()
        })
        .unwrap()
        .iter()
        .find(|m| m.subject.contains("Lines that start with a dot"))
        .expect("dot fixture")
        .id
    };

    let alice_msg = find(&alice, alice_acc);
    let bob_msg = find(&bob, bob_acc);

    alice.set_flagged(vec![alice_msg], true).unwrap();
    alice.sync_account(alice_acc).await.unwrap();
    bob.sync_account(bob_acc).await.unwrap();

    assert!(
        bob.get_message(bob_msg).unwrap().is_flagged,
        "flag did not reach the other machine"
    );

    // And unflagging travels too, which a naive "only add flags" push would get wrong.
    alice.set_flagged(vec![alice_msg], false).unwrap();
    alice.sync_account(alice_acc).await.unwrap();
    bob.sync_account(bob_acc).await.unwrap();
    assert!(!bob.get_message(bob_msg).unwrap().is_flagged);
}

#[tokio::test]
async fn a_local_change_is_not_overwritten_by_the_state_it_replaced() {
    // Push happens before pull within one sync. Without that ordering, marking something read and
    // syncing would pull the server's stale "unread" back over the change that was just made.
    let s = server_or_skip!();
    let (_d, app, id) = machine("ordering", &s).await;

    let inbox = inbox_of(&app, id);
    let msg = app
        .list_messages(MessageQuery {
            folder_id: Some(inbox.id),
            limit: 10,
            ..Default::default()
        })
        .unwrap()[0]
        .id;

    app.set_read(vec![msg], true).unwrap();
    app.sync_account(id).await.unwrap();

    assert!(
        app.get_message(msg).unwrap().is_read,
        "the sync undid the change the user had just made"
    );
}

#[tokio::test]
async fn a_wrong_password_fails_cleanly() {
    let s = server_or_skip!();
    let dir = TempDir::new("badpw");
    let app = App::open(&dir.0).expect("open");

    let check = app
        .check_account(&account(&s), "definitely-not-the-password")
        .await;
    assert!(
        !check.incoming.ok,
        "a wrong password must not report success"
    );
    assert!(app.list_accounts().unwrap().is_empty());
}

#[tokio::test]
async fn plaintext_is_refused_unless_explicitly_allowed() {
    // The lab opt-in is `Security::Plain`; anything else must not send a password in the clear.
    let s = server_or_skip!();
    let dir = TempDir::new("noplain");
    let app = App::open(&dir.0).expect("open");

    let mut strict = account(&s);
    strict.pop_security = Security::Starttls;

    // GreenMail's plaintext port does not offer STARTTLS, so this must fail rather than quietly
    // falling back to an unencrypted session.
    assert!(
        !app.check_account(&strict, &s.password).await.incoming.ok,
        "a STARTTLS account connected to a plaintext port anyway"
    );
}

// ------------------------------------------------------------------ changes that reach the server
//
// Each test puts its own uniquely-subjected message on the server, so moving or deleting it cannot
// disturb the fixtures the other tests read.

/// Put a fresh message in the server's INBOX and return its subject.
async fn plant(s: &Server, app: &App, account_id: i64, tag: &str) -> String {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let subject = format!("two-way {tag} {nonce}");
    let raw = format!(
        "From: Customer <buyer@customer.test>\r\nTo: signet@example.com\r\n\
         Subject: {subject}\r\nMessage-ID: <{tag}-{nonce}@customer.test>\r\n\
         Date: Tue, 29 Sep 2026 10:00:00 +0000\r\n\r\nPlease quote 40 downlights.\r\n"
    );
    let account = app
        .list_accounts()
        .unwrap()
        .into_iter()
        .find(|a| a.id == account_id)
        .unwrap();
    let mut client = signet_net::imap::ImapClient::connect(
        &signet_sync::imap_config_for(&account),
        &s.user,
        &s.password,
        true,
    )
    .await
    .expect("connect to plant");
    client.append("INBOX", raw.as_bytes(), &[]).await.expect("append");
    client.logout().await.unwrap();
    subject
}

/// How many copies of `subject` a machine holds in the folder of `kind`.
fn copies(app: &App, account_id: i64, kind: FolderKind, subject: &str) -> usize {
    let folder = app
        .list_folders(Some(account_id))
        .unwrap()
        .into_iter()
        .find(|f| f.kind == kind)
        .unwrap_or_else(|| panic!("no {kind:?} folder"));
    app.list_messages(MessageQuery {
        folder_id: Some(folder.id),
        limit: 1000,
        ..Default::default()
    })
    .unwrap()
    .iter()
    .filter(|m| m.subject == subject)
    .count()
}

fn find(app: &App, account_id: i64, subject: &str) -> i64 {
    app.list_messages(MessageQuery {
        search: String::new(),
        limit: 5000,
        ..Default::default()
    })
    .unwrap()
    .into_iter()
    .find(|m| m.account_id == account_id && m.subject == subject)
    .unwrap_or_else(|| panic!("{subject} not found locally"))
    .id
}

#[tokio::test]
async fn filing_a_message_moves_it_for_everyone_without_duplicating_it() {
    let s = server_or_skip!();
    let (_da, alice, a) = machine("move-a", &s).await;
    let (_db, bob, b) = machine("move-b", &s).await;
    let subject = plant(&s, &alice, a, "move").await;
    alice.sync_account(a).await.unwrap();
    bob.sync_account(b).await.unwrap();
    assert_eq!(copies(&bob, b, FolderKind::Inbox, &subject), 1);

    let archive = alice
        .list_folders(Some(a))
        .unwrap()
        .into_iter()
        .find(|f| f.kind == FolderKind::Archive)
        .expect("Archive on the server");
    alice.move_messages(vec![find(&alice, a, &subject)], archive.id).unwrap();
    let report = alice.sync_account(a).await.unwrap();
    assert!(report.pushed_to_server >= 1, "the move never reached the server: {report:?}");

    // Alice's own copy was matched to the server's moved one, not downloaded a second time.
    assert_eq!(copies(&alice, a, FolderKind::Archive, &subject), 1, "duplicated on the mover");
    assert_eq!(copies(&alice, a, FolderKind::Inbox, &subject), 0);

    // And Bob sees it filed, with no shared file involved.
    bob.sync_account(b).await.unwrap();
    assert_eq!(copies(&bob, b, FolderKind::Inbox, &subject), 0, "still in Bob's Inbox");
    assert_eq!(copies(&bob, b, FolderKind::Archive, &subject), 1, "not in Bob's Archive");
}

#[tokio::test]
async fn deleting_and_then_purging_reaches_the_server() {
    let s = server_or_skip!();
    let (_da, alice, a) = machine("del-a", &s).await;
    let (_db, bob, b) = machine("del-b", &s).await;
    let subject = plant(&s, &alice, a, "delete").await;
    alice.sync_account(a).await.unwrap();
    bob.sync_account(b).await.unwrap();

    // First delete: to Deleted Items, on the server too.
    alice.delete_messages(vec![find(&alice, a, &subject)]).unwrap();
    alice.sync_account(a).await.unwrap();
    bob.sync_account(b).await.unwrap();
    assert_eq!(copies(&bob, b, FolderKind::Inbox, &subject), 0);
    assert_eq!(copies(&bob, b, FolderKind::Deleted, &subject), 1);

    // Second delete: gone for good, everywhere, and not resurrected by the next sync.
    alice.delete_messages(vec![find(&alice, a, &subject)]).unwrap();
    alice.sync_account(a).await.unwrap();
    assert_eq!(copies(&alice, a, FolderKind::Deleted, &subject), 0, "came back after purge");
    bob.sync_account(b).await.unwrap();
    assert_eq!(copies(&bob, b, FolderKind::Deleted, &subject), 0, "Bob still has it");
}

#[tokio::test]
async fn a_message_sent_from_one_machine_appears_in_sent_on_the_other() {
    let s = server_or_skip!();
    // Alice sends through the test server's own SMTP port.
    let dir = TempDir::new("sent-a");
    let alice = App::open(&dir.0).unwrap();
    let a = alice
        .add_account(
            NewAccount {
                smtp_host: s.host.clone(),
                smtp_port: 3025,
                smtp_security: Security::Plain,
                smtp_user: s.user.clone(),
                ..account(&s)
            },
            &s.password,
        )
        .unwrap();
    alice.sync_account(a).await.unwrap();
    let (_db, bob, b) = machine("sent-b", &s).await;
    let account = alice.list_accounts().unwrap().into_iter().find(|x| x.id == a).unwrap();
    let raw_subject = format!("two-way sent {}", std::process::id());
    let draft = signet_core::outbox::Draft {
        account_id: a,
        to: vec!["signet@example.com".into()],
        subject: raw_subject.clone(),
        body_text: "Quote attached.".into(),
        ..Default::default()
    };
    let id = signet_core::outbox::queue(alice.store(), &draft, "Sales team", &account.email, "").unwrap();
    signet_core::outbox::send_now(alice.store(), id, &draft, &s.password).await.expect("smtp send");

    assert_eq!(copies(&alice, a, FolderKind::Sent, &raw_subject), 1);
    alice.sync_account(a).await.unwrap();
    assert_eq!(copies(&alice, a, FolderKind::Sent, &raw_subject), 1, "the uploaded copy was duplicated");

    bob.sync_account(b).await.unwrap();
    assert_eq!(copies(&bob, b, FolderKind::Sent, &raw_subject), 1, "Bob cannot see what Alice sent");
}

#[tokio::test]
async fn moving_twice_before_a_sync_lands_once_in_the_final_folder() {
    let s = server_or_skip!();
    let (_da, alice, a) = machine("twice-a", &s).await;
    let subject = plant(&s, &alice, a, "twice").await;
    alice.sync_account(a).await.unwrap();

    let folder = |kind| {
        alice
            .list_folders(Some(a))
            .unwrap()
            .into_iter()
            .find(|f| f.kind == kind)
            .unwrap()
            .id
    };
    let id = find(&alice, a, &subject);
    alice.move_messages(vec![id], folder(FolderKind::Archive)).unwrap();
    alice.move_messages(vec![id], folder(FolderKind::Deleted)).unwrap();
    alice.sync_account(a).await.unwrap();

    assert_eq!(copies(&alice, a, FolderKind::Deleted, &subject), 1);
    assert_eq!(copies(&alice, a, FolderKind::Archive, &subject), 0, "left a copy on the way");
    let (_db, bob, b) = machine("twice-b", &s).await;
    assert_eq!(copies(&bob, b, FolderKind::Deleted, &subject), 1);
    assert_eq!(copies(&bob, b, FolderKind::Archive, &subject), 0);
}

/// The reported failure: one message to several people, named the way Outlook writes them, must
/// reach every one of them. Checked by logging into each recipient's own mailbox on the server.
#[tokio::test]
async fn one_message_to_several_named_recipients_reaches_every_one() {
    let s = server_or_skip!();
    let dir = TempDir::new("multi");
    let app = App::open(&dir.0).unwrap();
    let a = app
        .add_account(
            NewAccount {
                smtp_host: s.host.clone(),
                smtp_port: 3025,
                smtp_security: Security::Plain,
                smtp_user: s.user.clone(),
                ..account(&s)
            },
            &s.password,
        )
        .unwrap();

    let nonce = std::process::id();
    let subject = format!("several recipients {nonce}");
    let to = [format!("Janice Carroll <janice{nonce}@alliance.example>")];
    let cc = [
        format!("\"Doe, John\" <john{nonce}@alliance.example>"),
        format!("maria{nonce}@alliance.example"),
    ];
    let sent = app
        .send(signet_core::outbox::Draft {
            account_id: a,
            to: to.to_vec(),
            cc: cc.to_vec(),
            subject: subject.clone(),
            body_text: "Please see attached.".into(),
            ..Default::default()
        })
        .await
        .expect("send");
    assert!(sent, "the server did not take it");

    for who in ["janice", "john", "maria"] {
        let addr = format!("{who}{nonce}@alliance.example");
        let account = app.list_accounts().unwrap().into_iter().find(|x| x.id == a).unwrap();
        // The test server makes a mailbox for each new recipient, with the address as password.
        let mut client = signet_net::imap::ImapClient::connect(
            &signet_sync::imap_config_for(&account),
            &addr,
            &addr,
            true,
        )
        .await
        .unwrap_or_else(|e| panic!("{addr} has no mailbox, so nothing was delivered: {e}"));
        let listed = client.list_messages("INBOX").await.unwrap();
        assert_eq!(listed.len(), 1, "{addr} got {} messages", listed.len());
        let headers = client.fetch_headers("INBOX", listed[0].uid).await.unwrap();
        let headers = String::from_utf8_lossy(&headers);
        assert!(headers.contains(&subject), "{addr} got something else: {headers}");
        client.logout().await.unwrap();
    }
}

#[tokio::test]
async fn a_mistyped_recipient_is_refused_before_anything_is_sent() {
    let s = server_or_skip!();
    let (_d, app, a) = machine("typo", &s).await;
    let err = app
        .send(signet_core::outbox::Draft {
            account_id: a,
            to: vec!["ok@alliance.example".into(), "janice at alliance".into()],
            subject: "never sent".into(),
            ..Default::default()
        })
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("janice at alliance"), "{err}");
    assert!(app.scheduled_messages().unwrap().is_empty(), "it was queued anyway");
}
