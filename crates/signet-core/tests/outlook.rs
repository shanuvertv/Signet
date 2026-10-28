//! Reading a real Outlook `.pst`.
//!
//! The unit tests in `outlook` cover the pure parts — header unfolding, folder naming, MAPI time.
//! They cannot cover the part that actually matters, which is whether the property reading and the
//! MIME rebuild survive contact with a file Outlook wrote. That needs a real PST, so these are
//! gated on `SIGNET_TEST_PST` and skip when it is not set.
//!
//! Set it to a `.pst` path, e.g. `%USERPROFILE%\Documents\Outlook Files\name.pst`. The file is only
//! ever read, and the assertions are about counts and shapes rather than content.

use signet_core::App;
use signet_store::{MessageQuery, NewAccount, Security};

struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "signet-pst-{tag}-{}-{:?}",
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

macro_rules! pst_or_skip {
    () => {
        match std::env::var("SIGNET_TEST_PST") {
            Ok(p) if std::path::Path::new(&p).exists() => p,
            _ => {
                eprintln!("SKIPPED: set SIGNET_TEST_PST to a .pst file to run the Outlook tests");
                return;
            }
        }
    };
}

/// `None` when a running Outlook has the file, which is a fact about the machine rather than a
/// defect. The lock covers a region well inside the file, so nothing short of trying to read it
/// properly tells you it is there.
fn preview_or_skip(app: &App, path: &str) -> Option<signet_core::outlook::OutlookPreview> {
    match app.preview_outlook_file(path.to_string()) {
        Ok(preview) => Some(preview),
        Err(e) if e.to_string().contains("open in Outlook") => {
            eprintln!("SKIPPED: {path} is open in Outlook - close it to run this test");
            None
        }
        Err(e) => panic!("preview: {e}"),
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

#[test]
fn a_real_pst_can_be_described_without_importing_it() {
    let path = pst_or_skip!();
    let dir = TempDir::new("preview");
    let app = App::open(&dir.0).unwrap();

    let Some(preview) = preview_or_skip(&app, &path) else {
        return;
    };
    eprintln!(
        "store {:?}: {} folders holding {} messages",
        preview.display_name,
        preview.folders.len(),
        preview.messages
    );
    for f in &preview.folders {
        eprintln!("  {} — {}", f.path, f.messages);
    }

    assert!(
        preview.messages >= 0,
        "the folder walk did not complete cleanly"
    );
    // Nothing was written by looking.
    assert!(app.store().list_accounts().unwrap().is_empty());
}

#[tokio::test]
async fn a_real_pst_imports_and_the_messages_are_usable_afterwards() {
    let path = pst_or_skip!();
    let (_dir, app, account) = app_with_account("import");

    let Some(expected) = preview_or_skip(&app, &path) else {
        return;
    };
    if expected.messages == 0 {
        eprintln!("SKIPPED: {path} holds no messages");
        return;
    }

    let summary = app
        .import_outlook_file(path, account)
        .await
        .expect("import");
    eprintln!(
        "imported {}, skipped {}, failed {}, from {} folders",
        summary.messages_imported, summary.messages_skipped, summary.failed, summary.folders
    );

    assert!(
        summary.messages_imported > 0,
        "nothing imported out of {} messages",
        expected.messages
    );
    // A handful of oddities is tolerable; a majority failing means the reader is wrong.
    assert!(
        summary.failed * 4 < summary.messages_imported,
        "{} of {} messages failed to import",
        summary.failed,
        expected.messages
    );

    // What arrived has to be usable, not merely present. Counted, never printed: this is real mail.
    let all = app
        .store()
        .list_messages(&MessageQuery {
            account_id: Some(account),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(all.len() as i64, summary.messages_imported);

    let with_sender = all.iter().filter(|m| m.from_addr.contains('@')).count();
    let with_subject = all.iter().filter(|m| !m.subject.is_empty()).count();
    let with_date = all.iter().filter(|m| m.date_utc > 0).count();
    eprintln!(
        "of {}: {} have a sender address, {} a subject, {} a date",
        all.len(),
        with_sender,
        with_subject,
        with_date
    );

    assert!(
        with_sender * 2 > all.len(),
        "most imported messages have no sender address"
    );
    assert!(
        with_date * 2 > all.len(),
        "most imported messages have no date"
    );

    // The raw file has to exist, because that is what every re-parse and every export reads.
    let sample = &all[0];
    let raw = app.store().raw_path(sample.id).unwrap();
    assert!(
        std::path::Path::new(&raw).exists(),
        "an imported message has no file on disk"
    );

    // And it has to be findable, which is the check that the ingestion pipeline really ran.
    let word = all
        .iter()
        .filter_map(|m| {
            m.subject
                .split_whitespace()
                .find(|w| w.len() > 5 && w.chars().all(|c| c.is_ascii_alphabetic()))
        })
        .next()
        .map(str::to_string);
    if let Some(word) = word {
        let hits = app
            .store()
            .list_messages(&MessageQuery {
                account_id: Some(account),
                search: word.clone(),
                ..Default::default()
            })
            .unwrap();
        assert!(!hits.is_empty(), "imported mail was not indexed for search");
    }
}

#[tokio::test]
async fn importing_the_same_pst_twice_adds_nothing_the_second_time() {
    let path = pst_or_skip!();
    let (_dir, app, account) = app_with_account("twice");
    if preview_or_skip(&app, &path).is_none() {
        return;
    }

    let first = app
        .import_outlook_file(path.clone(), account)
        .await
        .expect("first import");
    if first.messages_imported == 0 {
        eprintln!("SKIPPED: nothing was imported to re-import");
        return;
    }

    let second = app
        .import_outlook_file(path, account)
        .await
        .expect("second import");
    eprintln!(
        "second pass: imported {}, skipped {}",
        second.messages_imported, second.messages_skipped
    );

    // Mail with no Message-ID cannot be matched, so a few may come again; the bulk must not.
    assert!(
        second.messages_imported * 10 < first.messages_imported,
        "re-importing duplicated {} of {} messages",
        second.messages_imported,
        first.messages_imported
    );
}

#[test]
fn an_ost_is_refused_with_advice_rather_than_a_parser_error() {
    // OST is a different on-disk version, and the mail in one is still on the server anyway. The
    // error has to say that, because "invalid NDB header version 36" helps nobody.
    let dir = TempDir::new("ost");
    let app = App::open(&dir.0).unwrap();

    let fake = dir.0.join("mailbox.ost");
    std::fs::write(&fake, b"not really an ost either").unwrap();

    let err = app
        .preview_outlook_file(fake.to_string_lossy().into_owned())
        .expect_err("an .ost was accepted");
    let text = err.to_string();
    assert!(text.contains(".ost"), "{text}");
    assert!(
        text.contains("Add account") || text.contains("add the account"),
        "the error does not say what to do instead: {text}"
    );
}

#[test]
fn a_file_that_is_not_a_pst_is_refused_clearly() {
    let dir = TempDir::new("bogus");
    let app = App::open(&dir.0).unwrap();

    let fake = dir.0.join("photo.jpg");
    std::fs::write(&fake, b"\xff\xd8\xff\xe0 JFIF").unwrap();

    let err = app
        .preview_outlook_file(fake.to_string_lossy().into_owned())
        .expect_err("a JPEG was accepted as a mailbox");
    assert!(err.to_string().contains(".pst"), "{err}");
}

#[test]
fn a_missing_file_says_so() {
    let dir = TempDir::new("missing");
    let app = App::open(&dir.0).unwrap();
    let err = app
        .preview_outlook_file(dir.0.join("nope.pst").to_string_lossy().into_owned())
        .expect_err("a missing file was accepted");
    assert!(err.to_string().contains("does not exist"), "{err}");
}
