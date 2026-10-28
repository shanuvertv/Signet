//! Exporting and importing the data file.
//!
//! The test that matters is the round trip: export a mailbox, import it into an empty installation,
//! and check that what comes out is what went in — including the things that live outside the
//! database rows, like the raw message on disk and the search index.

use signet_core::App;
use signet_store::{
    FolderKind, MessageQuery, NewAccount, NewAttachment, NewMessage, Recipient, Security,
};

struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "signet-archive-{tag}-{}-{:?}",
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

fn app_with_account(tag: &str) -> (TempDir, App, i64) {
    let dir = TempDir::new(tag);
    let app = App::open(&dir.0).expect("open");
    let id = app
        .add_account(
            NewAccount {
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
            "secret",
        )
        .expect("add account");
    (dir, app, id)
}

/// Put a message in the Inbox, with a raw file and an attachment on disk beside it.
fn add_message(app: &App, account_id: i64, msgid: &str, subject: &str) -> i64 {
    let inbox = app
        .store()
        .folder_id(account_id, FolderKind::Inbox)
        .unwrap();
    let blobs = app.store().blob_dir(account_id);
    let raw_dir = blobs.join("raw");
    let att_dir = blobs.join("attachments");
    std::fs::create_dir_all(&raw_dir).unwrap();
    std::fs::create_dir_all(&att_dir).unwrap();

    let stem = msgid.replace(['<', '>', '@', '.'], "_");
    let raw = raw_dir.join(format!("{stem}.eml"));
    std::fs::write(
        &raw,
        format!("Subject: {subject}\r\n\r\nbody of {subject}\r\n"),
    )
    .unwrap();
    let att = att_dir.join(format!("{stem}-0-quote.pdf"));
    std::fs::write(&att, b"%PDF-1.4 pretend").unwrap();

    app.store()
        .insert_message(&NewMessage {
            account_id,
            folder_id: inbox,
            rfc822_message_id: Some(msgid.into()),
            in_reply_to: None,
            thread_id: stem.clone(),
            from_name: "Buyer".into(),
            from_addr: "buyer@customer.test".into(),
            subject: subject.into(),
            date_utc: 1_700_000_000,
            received_utc: 1_700_000_001,
            has_attachments: true,
            has_remote_content: false,
            size_bytes: 42,
            preview_text: subject.into(),
            body_text: format!("body of {subject}"),
            body_html: None,
            raw_path: raw.to_string_lossy().into_owned(),
            recipients: vec![Recipient {
                kind: "to".into(),
                name: "Dev".into(),
                addr: "dev@example.com".into(),
            }],
            attachments: vec![NewAttachment {
                filename: "quote.pdf".into(),
                mime_type: "application/pdf".into(),
                size_bytes: 16,
                content_id: None,
                blob_path: att.to_string_lossy().into_owned(),
                is_inline: false,
            }],
        })
        .unwrap()
}

fn inbox_of(app: &App, account_id: i64) -> Vec<signet_store::MessageSummary> {
    app.store()
        .list_messages(&MessageQuery {
            account_id: Some(account_id),
            folder_id: Some(
                app.store()
                    .folder_id(account_id, FolderKind::Inbox)
                    .unwrap(),
            ),
            ..Default::default()
        })
        .unwrap()
}

#[test]
fn a_mailbox_survives_the_round_trip_intact() {
    let (src_dir, src, src_account) = app_with_account("rt-src");
    let id = add_message(
        &src,
        src_account,
        "<one@customer.test>",
        "Quote for downlights",
    );
    src.store().set_read(&[id], true).unwrap();
    src.store().set_flagged(&[id], true).unwrap();

    let archive = src_dir.0.join("backup.signet");
    let out = src
        .export_data_file(archive.to_string_lossy().into_owned())
        .expect("export");
    assert_eq!(out.messages, 1);
    assert!(out.bytes > 0, "the archive is empty");
    assert!(archive.exists());

    // A fresh installation, with nothing in it at all.
    let dest_dir = TempDir::new("rt-dest");
    let dest = App::open(&dest_dir.0).unwrap();
    let summary = dest
        .import_data_file(archive.to_string_lossy().into_owned())
        .expect("import");

    assert_eq!(summary.accounts_added, 1);
    assert_eq!(summary.messages_imported, 1);
    assert_eq!(summary.messages_skipped, 0);

    let account = dest
        .store()
        .list_accounts()
        .unwrap()
        .into_iter()
        .find(|a| a.email == "dev@example.com")
        .expect("the account did not come across");

    let listed = inbox_of(&dest, account.id);
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].subject, "Quote for downlights");
    assert!(listed[0].is_read, "read state was lost");
    assert!(listed[0].is_flagged, "the flag was lost");

    // The parts that live outside the message row.
    let detail = dest.store().get_message(listed[0].id).unwrap();
    assert_eq!(detail.recipients.len(), 1);
    assert_eq!(detail.attachments.len(), 1);
    assert_eq!(detail.attachments[0].filename, "quote.pdf");

    let raw = dest.store().raw_path(listed[0].id).unwrap();
    assert!(
        std::path::Path::new(&raw).exists(),
        "the raw message did not come across: {raw}"
    );
    let (_, att) = dest
        .store()
        .attachment_path(detail.attachments[0].id)
        .unwrap();
    assert!(att.exists(), "the attachment file did not come across");
}

#[test]
fn imported_mail_is_searchable() {
    // Search runs off an index maintained in code, so an import that wrote rows directly would
    // produce mail that exists but cannot be found.
    let (src_dir, src, src_account) = app_with_account("fts-src");
    add_message(
        &src,
        src_account,
        "<two@customer.test>",
        "Bollard specification",
    );

    let archive = src_dir.0.join("backup.signet");
    src.export_data_file(archive.to_string_lossy().into_owned())
        .unwrap();

    let dest_dir = TempDir::new("fts-dest");
    let dest = App::open(&dest_dir.0).unwrap();
    dest.import_data_file(archive.to_string_lossy().into_owned())
        .unwrap();

    let account = dest.store().list_accounts().unwrap()[0].id;
    let hits = dest
        .store()
        .list_messages(&MessageQuery {
            account_id: Some(account),
            search: "bollard".into(),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(hits.len(), 1, "imported mail was not indexed for search");
}

#[test]
fn importing_the_same_archive_twice_adds_nothing_the_second_time() {
    // The obvious way to lose confidence in a backup is to have restoring it duplicate everything.
    let (src_dir, src, src_account) = app_with_account("twice-src");
    add_message(&src, src_account, "<a@customer.test>", "First");
    add_message(&src, src_account, "<b@customer.test>", "Second");

    let archive = src_dir.0.join("backup.signet");
    src.export_data_file(archive.to_string_lossy().into_owned())
        .unwrap();
    let path = archive.to_string_lossy().into_owned();

    let dest_dir = TempDir::new("twice-dest");
    let dest = App::open(&dest_dir.0).unwrap();

    let first = dest.import_data_file(path.clone()).unwrap();
    assert_eq!(first.messages_imported, 2);

    let second = dest.import_data_file(path).unwrap();
    assert_eq!(
        second.messages_imported, 0,
        "the second import duplicated mail"
    );
    assert_eq!(second.messages_skipped, 2);
    assert_eq!(second.accounts_added, 0);
    assert_eq!(second.accounts_matched, 1);

    let account = dest.store().list_accounts().unwrap()[0].id;
    assert_eq!(inbox_of(&dest, account).len(), 2);
}

#[test]
fn import_merges_rather_than_replacing() {
    // Importing the wrong file has to be survivable, so nothing already here may be removed.
    let (src_dir, src, src_account) = app_with_account("merge-src");
    add_message(
        &src,
        src_account,
        "<from-archive@customer.test>",
        "From the archive",
    );

    let archive = src_dir.0.join("backup.signet");
    src.export_data_file(archive.to_string_lossy().into_owned())
        .unwrap();

    let (_dest_dir, dest, dest_account) = app_with_account("merge-dest");
    add_message(
        &dest,
        dest_account,
        "<already-here@customer.test>",
        "Already here",
    );

    dest.import_data_file(archive.to_string_lossy().into_owned())
        .unwrap();

    let subjects: Vec<String> = inbox_of(&dest, dest_account)
        .into_iter()
        .map(|m| m.subject)
        .collect();
    assert!(
        subjects.iter().any(|s| s == "Already here"),
        "the import destroyed existing mail: {subjects:?}"
    );
    assert!(
        subjects.iter().any(|s| s == "From the archive"),
        "the archived mail did not arrive: {subjects:?}"
    );

    // Same email address, so the account is matched rather than duplicated.
    assert_eq!(dest.store().list_accounts().unwrap().len(), 1);
}

#[test]
fn an_imported_account_arrives_disabled_because_it_has_no_password() {
    // Passwords are deliberately not in the archive. An account that silently failed to sync would
    // be worse than one that plainly says it needs a password.
    let (src_dir, src, _) = app_with_account("pw-src");
    let archive = src_dir.0.join("backup.signet");
    src.export_data_file(archive.to_string_lossy().into_owned())
        .unwrap();

    let dest_dir = TempDir::new("pw-dest");
    let dest = App::open(&dest_dir.0).unwrap();
    let summary = dest
        .import_data_file(archive.to_string_lossy().into_owned())
        .unwrap();

    assert_eq!(
        summary.needs_password,
        vec!["dev@example.com".to_string()]
    );
    let account = &dest.store().list_accounts().unwrap()[0];
    assert!(
        !account.is_enabled,
        "an account with no password was left enabled"
    );
}

#[test]
fn the_archive_is_an_ordinary_zip_anyone_can_open() {
    // A backup you need this program to read is a worse backup.
    let (dir, app, account) = app_with_account("zip");
    add_message(&app, account, "<z@customer.test>", "Readable");

    let archive = dir.0.join("backup.signet");
    app.export_data_file(archive.to_string_lossy().into_owned())
        .unwrap();

    let bytes = std::fs::read(&archive).unwrap();
    assert_eq!(&bytes[..2], b"PK", "not a zip file");

    let names: Vec<String> = {
        let f = std::fs::File::open(&archive).unwrap();
        let mut z = zip::ZipArchive::new(f).unwrap();
        (0..z.len())
            .map(|i| z.by_index(i).unwrap().name().to_string())
            .collect()
    };
    assert!(names.contains(&"manifest.json".to_string()));
    assert!(names.contains(&"mail.sqlite".to_string()));
    assert!(
        names.iter().any(|n| n.ends_with(".eml")),
        "the raw messages are not in the archive: {names:?}"
    );
}

#[test]
fn a_file_that_is_not_an_archive_is_refused_clearly() {
    let dir = TempDir::new("bogus");
    let app = App::open(&dir.0).unwrap();

    let bogus = dir.0.join("holiday.jpg");
    std::fs::write(&bogus, b"not a zip at all").unwrap();

    let err = app
        .import_data_file(bogus.to_string_lossy().into_owned())
        .expect_err("a JPEG was accepted as a mailbox");
    let text = err.to_string();
    assert!(
        text.contains("archive"),
        "the error does not say what is wrong: {text}"
    );
}

#[test]
fn a_manifest_can_be_read_without_importing_anything() {
    let (dir, app, account) = app_with_account("inspect");
    add_message(&app, account, "<i@customer.test>", "Peek");

    let archive = dir.0.join("backup.signet");
    app.export_data_file(archive.to_string_lossy().into_owned())
        .unwrap();

    let dest_dir = TempDir::new("inspect-dest");
    let dest = App::open(&dest_dir.0).unwrap();
    let manifest = dest
        .inspect_archive(archive.to_string_lossy().into_owned())
        .unwrap();

    assert_eq!(manifest.messages, 1);
    assert_eq!(manifest.accounts.len(), 1);
    assert_eq!(manifest.accounts[0].email, "dev@example.com");
    assert!(manifest.schema_version > 0);

    // Nothing was imported by looking.
    assert!(dest.store().list_accounts().unwrap().is_empty());
}
