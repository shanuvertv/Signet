//! End-to-end: add an account, sync it from a real POP3 server, and read the result back through
//! the same facade the UI uses.
//!
//! Gated on `SIGNET_TEST_POP3_HOST`, like the other live tests. See `docs/testing.md`.

use signet_core::{App, Security};
use signet_store::{FolderKind, MessageQuery, NewAccount};

fn enabled() -> Option<(String, u16)> {
    let host = std::env::var("SIGNET_TEST_POP3_HOST").ok()?;
    let port = std::env::var("SIGNET_TEST_POP3_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(1110);
    Some((host, port))
}

fn account(host: &str, port: u16) -> NewAccount {
    NewAccount {
        display_name: "End To End".into(),
        email: "e2e@example.com".into(),
        pop_host: host.into(),
        pop_port: port,
        // The local test server has no TLS. Anywhere else this would be refused.
        pop_security: Security::Plain,
        pop_user: std::env::var("SIGNET_TEST_POP3_USER").unwrap_or_else(|_| "signet".into()),
        smtp_host: String::new(),
        smtp_port: 587,
        smtp_security: Security::Starttls,
        smtp_user: String::new(),
        leave_on_server: true,
        delete_after_days: None,
        poll_minutes: 5,
        signature: String::new(),
        protocol: Default::default(),
    }
}

/// A temporary data directory that cleans itself up.
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("signet-e2e-{tag}-{}", std::process::id()));
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

#[tokio::test]
async fn sync_ingests_mail_and_the_facade_reads_it_back() {
    let Some((host, port)) = enabled() else {
        eprintln!("SKIPPED: set SIGNET_TEST_POP3_HOST to run the end-to-end test");
        return;
    };

    let dir = TempDir::new("sync");
    let app = App::open(&dir.0).expect("open store");

    let password = std::env::var("SIGNET_TEST_POP3_PASSWORD").unwrap_or_else(|_| "spike".into());
    let account_id = app
        .add_account(account(&host, port), &password)
        .expect("add account");

    // --- first sync: everything is new -----------------------------------
    let report = app.sync_account(account_id).await.expect("sync");
    assert!(
        report.new_messages >= 4,
        "expected the seeded fixtures; got {report:?}"
    );
    assert!(report.errors.is_empty(), "sync reported errors: {report:?}");
    assert!(
        report.bytes_downloaded > 500_000,
        "large fixture not fetched"
    );

    // --- the folders POP3 never gave us ----------------------------------
    let folders = app.list_folders(Some(account_id)).expect("folders");
    let inbox = folders
        .iter()
        .find(|f| f.kind == FolderKind::Inbox)
        .expect("inbox");
    let junk = folders
        .iter()
        .find(|f| f.kind == FolderKind::Junk)
        .expect("junk");
    // Ingested mail is split between the Inbox and whatever the junk filter took, so the Inbox
    // alone is no longer the whole count.
    assert_eq!(
        inbox.total + junk.total,
        report.new_messages as i64,
        "messages went somewhere unaccounted for: {report:?}"
    );
    assert_eq!(junk.total, report.junked as i64);
    assert_eq!(inbox.unread, inbox.total, "freshly synced mail is unread");

    // --- the message list ------------------------------------------------
    let messages = app
        .list_messages(MessageQuery {
            folder_id: Some(inbox.id),
            limit: 100,
            ..Default::default()
        })
        .expect("list");
    assert_eq!(messages.len() as i64, inbox.total);
    assert_eq!(
        report.junked, 0,
        "the ordinary fixtures should not be filed as junk: {report:?}"
    );
    assert!(
        messages.iter().any(|m| m.has_attachments),
        "the multipart fixture should have an attachment"
    );
    assert!(
        messages.iter().all(|m| !m.from_addr.is_empty()),
        "every fixture has a sender"
    );

    // Encoded-word headers must be decoded by the time they reach the UI.
    assert!(
        messages
            .iter()
            .any(|m| m.subject.contains("Lumière") && m.subject.contains("résumé")),
        "encoded-word subject was not decoded: {:?}",
        messages.iter().map(|m| &m.subject).collect::<Vec<_>>()
    );

    // --- dot-unstuffing survives the whole pipeline ----------------------
    let dotted = messages
        .iter()
        .find(|m| m.subject.contains("start with a dot"))
        .expect("dot-stuffed fixture");
    let detail = app.get_message(dotted.id).expect("get message");
    assert!(
        detail
            .body_text
            .contains(".. and this one begins with two.")
            || detail.body_text.contains("..and this one begins with two."),
        "dot-stuffed line was mangled: {:?}",
        detail.body_text
    );

    // --- search ----------------------------------------------------------
    let hits = app
        .list_messages(MessageQuery {
            search: "sandwiches".into(),
            ..Default::default()
        })
        .expect("search");
    assert!(
        hits.is_empty(),
        "search matched a phrase that is not present"
    );

    let hits = app
        .list_messages(MessageQuery {
            search: "unusual".into(),
            ..Default::default()
        })
        .expect("search");
    assert_eq!(hits.len(), 1, "body search should find the plain fixture");

    // --- read state ------------------------------------------------------
    app.set_read(vec![dotted.id], true).expect("mark read");
    let folders = app.list_folders(Some(account_id)).expect("folders");
    let inbox_after = folders
        .iter()
        .find(|f| f.kind == FolderKind::Inbox)
        .unwrap();
    assert_eq!(inbox_after.unread, inbox.unread - 1);

    // --- delete moves to Deleted Items, then purges ----------------------
    app.delete_messages(vec![dotted.id]).expect("first delete");
    let deleted_folder = folders
        .iter()
        .find(|f| f.kind == FolderKind::Deleted)
        .unwrap();
    let in_deleted = app
        .list_messages(MessageQuery {
            folder_id: Some(deleted_folder.id),
            ..Default::default()
        })
        .expect("list deleted");
    assert_eq!(in_deleted.len(), 1, "delete should move, not destroy");

    app.delete_messages(vec![dotted.id]).expect("second delete");
    assert!(
        app.get_message(dotted.id).is_err(),
        "second delete should purge"
    );

    // --- re-sync is idempotent -------------------------------------------
    // The whole reason `pop_seen` is keyed on UIDL. A second poll must not re-download anything,
    // and in particular must not resurrect the message we just purged.
    let second = app.sync_account(account_id).await.expect("second sync");
    assert_eq!(
        second.new_messages, 0,
        "re-polling duplicated mail: {second:?}"
    );
    assert!(second.already_seen >= 4);
}

#[tokio::test]
async fn a_wrong_password_fails_cleanly_and_stores_nothing() {
    let Some((host, port)) = enabled() else {
        eprintln!("SKIPPED: set SIGNET_TEST_POP3_HOST to run the end-to-end test");
        return;
    };

    let dir = TempDir::new("badpw");
    let app = App::open(&dir.0).expect("open store");

    let check = app
        .check_account(&account(&host, port), "definitely-not-the-password")
        .await;
    assert!(!check.ok, "a wrong password must not report success");
    assert!(
        !check.incoming.ok,
        "the incoming half is what should have failed"
    );
    assert!(
        !check.incoming.detail.is_empty(),
        "a failed check must say what went wrong"
    );

    // A failed connection test must not have created an account.
    assert!(app.list_accounts().unwrap().is_empty());
}
