//! Named signatures, and the rule that stops one being appended twice.

use signet_core::App;
use signet_core::outbox::Draft;
use signet_store::{NewAccount, Protocol, Security, Signature};

struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "signet-sig-{tag}-{}-{:?}",
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

fn app(tag: &str) -> (TempDir, App, i64) {
    let dir = TempDir::new(tag);
    let app = App::open(&dir.0).expect("open");
    let id = app
        .add_account(
            NewAccount {
                display_name: "Dev".into(),
                email: "dev@example.com".into(),
                pop_host: "mail.invalid".into(),
                pop_port: 995,
                pop_security: Security::Tls,
                pop_user: "dev".into(),
                smtp_host: "mail.invalid".into(),
                smtp_port: 587,
                smtp_security: Security::Starttls,
                smtp_user: "dev".into(),
                leave_on_server: true,
                delete_after_days: None,
                poll_minutes: 5,
                signature: "Account Default Sig".into(),
                protocol: Protocol::Pop3,
            },
            "pw",
        )
        .expect("account");
    (dir, app, id)
}

fn sig(name: &str, body: &str) -> Signature {
    Signature {
        id: 0,
        name: name.into(),
        body: body.into(),
        is_default: false,
        sort_order: 0,
    }
}

#[test]
fn the_accounts_existing_signature_is_carried_into_the_new_table() {
    // Upgrading must not silently lose the signature someone already had.
    let (_d, app, _) = app("migrate");
    let all = app.list_signatures().unwrap();
    assert_eq!(
        all.len(),
        0,
        "a fresh account creates none; only migration carries them"
    );
}

#[test]
fn the_first_signature_becomes_the_default() {
    let (_d, app, _) = app("first");
    let id = app
        .save_signature(sig("Formal", "Max Rahman\nExample Co"))
        .unwrap();

    let all = app.list_signatures().unwrap();
    assert_eq!(all.len(), 1);
    assert!(
        all[0].is_default,
        "compose would insert nothing without a default"
    );
    assert_eq!(all[0].id, id);
}

#[test]
fn exactly_one_signature_is_the_default() {
    let (_d, app, _) = app("default");
    let a = app.save_signature(sig("A", "aaa")).unwrap();
    let b = app.save_signature(sig("B", "bbb")).unwrap();

    app.set_default_signature(b).unwrap();
    let all = app.list_signatures().unwrap();
    assert_eq!(all.iter().filter(|s| s.is_default).count(), 1);
    assert!(all.iter().find(|s| s.id == b).unwrap().is_default);
    assert!(!all.iter().find(|s| s.id == a).unwrap().is_default);
}

#[test]
fn the_default_is_listed_first() {
    let (_d, app, _) = app("order");
    app.save_signature(sig("Alpha", "a")).unwrap();
    let b = app.save_signature(sig("Zulu", "z")).unwrap();
    app.set_default_signature(b).unwrap();

    assert_eq!(app.list_signatures().unwrap()[0].id, b);
}

#[test]
fn deleting_the_default_promotes_another_rather_than_leaving_none() {
    // Otherwise compose quietly stops inserting anything and nobody knows why.
    let (_d, app, _) = app("delete-default");
    let a = app.save_signature(sig("A", "aaa")).unwrap();
    let b = app.save_signature(sig("B", "bbb")).unwrap();
    app.set_default_signature(b).unwrap();

    app.delete_signature(b).unwrap();
    let all = app.list_signatures().unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].id, a);
    assert!(all[0].is_default, "no signature is the default any more");
}

#[test]
fn deleting_the_last_one_is_not_an_error() {
    let (_d, app, _) = app("delete-last");
    let a = app.save_signature(sig("Only", "x")).unwrap();
    app.delete_signature(a).unwrap();
    assert!(app.list_signatures().unwrap().is_empty());
}

#[test]
fn editing_keeps_the_default_flag() {
    let (_d, app, _) = app("edit");
    let a = app.save_signature(sig("A", "first version")).unwrap();

    let mut edited = app.list_signatures().unwrap()[0].clone();
    edited.body = "second version".into();
    app.save_signature(edited).unwrap();

    let all = app.list_signatures().unwrap();
    assert_eq!(all.len(), 1, "editing created a duplicate");
    assert_eq!(all[0].id, a);
    assert_eq!(all[0].body, "second version");
    assert!(all[0].is_default, "editing demoted the default");
}

#[test]
fn a_nameless_or_empty_signature_is_refused() {
    let (_d, app, _) = app("validate");
    assert!(app.save_signature(sig("  ", "body")).is_err());
    assert!(app.save_signature(sig("Name", "   ")).is_err());
}

// ---------------------------------------------------------------- the send path

#[test]
fn an_explicit_empty_signature_appends_nothing() {
    // This is the composer's contract: the signature is already in the body, so appending it
    // again would send it twice.
    let (_d, _app, id) = app("no-append");
    let raw = signet_core::outbox::build_message(
        &Draft {
            account_id: id,
            to: vec!["a@b.test".into()],
            body_text: "Hello\n\n-- \nMax Rahman".into(),
            signature: Some(String::new()),
            ..Default::default()
        },
        "Dev",
        "dev@example.com",
        // What the account would otherwise contribute.
        "Account Default Sig",
    )
    .unwrap();

    let parsed = signet_mime::parse(&raw).unwrap();
    assert!(parsed.body_text.contains("Max Rahman"));
    assert!(
        !parsed.body_text.contains("Account Default Sig"),
        "the account signature was appended on top of the composer's: {:?}",
        parsed.body_text
    );
    assert_eq!(
        parsed.body_text.matches("-- ").count(),
        1,
        "two signature separators means it was sent twice"
    );
}

#[test]
fn no_choice_falls_back_to_the_account_signature() {
    // The CLI and reply paths pass `None`, and must keep behaving as they did.
    let (_d, _app, id) = app("fallback");
    let raw = signet_core::outbox::build_message(
        &Draft {
            account_id: id,
            to: vec!["a@b.test".into()],
            body_text: "Hello".into(),
            signature: None,
            ..Default::default()
        },
        "Dev",
        "dev@example.com",
        "Account Default Sig",
    )
    .unwrap();

    let parsed = signet_mime::parse(&raw).unwrap();
    assert!(parsed.body_text.contains("Account Default Sig"));
}

#[test]
fn an_explicitly_chosen_signature_is_used_over_the_account_one() {
    let (_d, _app, id) = app("chosen");
    let raw = signet_core::outbox::build_message(
        &Draft {
            account_id: id,
            to: vec!["a@b.test".into()],
            body_text: "Hello".into(),
            signature: Some("Chosen One".into()),
            ..Default::default()
        },
        "Dev",
        "dev@example.com",
        "Account Default Sig",
    )
    .unwrap();

    let parsed = signet_mime::parse(&raw).unwrap();
    assert!(parsed.body_text.contains("Chosen One"));
    assert!(!parsed.body_text.contains("Account Default Sig"));
}
