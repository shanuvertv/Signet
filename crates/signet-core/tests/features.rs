//! Settings, signatures, rules, junk training, drafts and outgoing attachments, exercised through
//! the same facade the UI calls.
//!
//! These need no network: they are about what the app decides, not what a server says.

use signet_core::App;
use signet_core::outbox::{Draft, DraftAttachment};
use signet_core::rules::{Action, Condition, Field, Op, Rule};
use signet_store::{FolderKind, MessageQuery, NewAccount, NewMessage, Recipient, Security};

struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "signet-feat-{tag}-{}-{:?}",
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
                signature: String::new(),
                protocol: Default::default(),
            },
            "password",
        )
        .expect("add account");
    (dir, app, id)
}

/// Put a message in the Inbox without needing a server.
fn seed(app: &App, account_id: i64, from: &str, subject: &str, body: &str) -> i64 {
    let folder = app
        .store()
        .folder_id(account_id, FolderKind::Inbox)
        .unwrap();
    app.store()
        .insert_message(&NewMessage {
            account_id,
            folder_id: folder,
            thread_id: subject.into(),
            from_name: String::new(),
            from_addr: from.into(),
            subject: subject.into(),
            date_utc: 1_700_000_000,
            received_utc: 1_700_000_000,
            body_text: body.into(),
            preview_text: body.chars().take(80).collect(),
            recipients: vec![Recipient {
                kind: "to".into(),
                name: String::new(),
                addr: "dev@example.com".into(),
            }],
            ..Default::default()
        })
        .unwrap()
}

// ---------------------------------------------------------------- settings

#[test]
fn settings_round_trip_including_the_signature() {
    let (_d, app, id) = app_with_account("settings");

    let mut s = app.account_settings(id).unwrap();
    s.signature = "Max\nExample Co\n+971 4 123 4567".into();
    s.display_name = "Max (Sales)".into();
    s.poll_minutes = 15;
    s.delete_after_days = Some(30);
    app.update_account_settings(id, s).unwrap();

    let back = app.account_settings(id).unwrap();
    assert_eq!(back.signature, "Max\nExample Co\n+971 4 123 4567");
    assert_eq!(back.display_name, "Max (Sales)");
    assert_eq!(back.poll_minutes, 15);
    assert_eq!(back.delete_after_days, Some(30));
}

#[test]
fn settings_clamp_values_that_would_break_the_app() {
    let (_d, app, id) = app_with_account("clamp");

    let mut s = app.account_settings(id).unwrap();
    s.junk_threshold = 0.01; // would file nearly everything as junk
    s.poll_minutes = 0; // would poll in a tight loop
    s.delete_after_days = Some(0); // "delete after zero days" is not what anyone means
    app.update_account_settings(id, s).unwrap();

    let back = app.account_settings(id).unwrap();
    assert!(
        back.junk_threshold >= 0.5,
        "threshold was {}",
        back.junk_threshold
    );
    assert!(back.poll_minutes >= 1);
    assert_eq!(back.delete_after_days, None);
}

#[test]
fn the_signature_reaches_a_sent_message() {
    let (_d, app, id) = app_with_account("signature");
    let mut s = app.account_settings(id).unwrap();
    s.signature = "Max — Example Co".into();
    app.update_account_settings(id, s).unwrap();

    let account = app.store().get_account(id).unwrap();
    let raw = signet_core::outbox::build_message(
        &Draft {
            account_id: id,
            to: vec!["someone@example.com".into()],
            subject: "Hello".into(),
            body_text: "Body".into(),
            ..Default::default()
        },
        &account.display_name,
        &account.email,
        &account.signature,
    )
    .unwrap();

    let parsed = signet_mime::parse(&raw).unwrap();
    assert!(parsed.body_text.contains("Max — Example Co"));
    assert!(parsed.body_text.contains("-- "), "separator missing");
}

// ---------------------------------------------------------------- attachments

#[test]
fn outgoing_attachments_are_encoded_and_come_back_out_of_the_parser() {
    let dir = TempDir::new("attach");
    let file = dir.0.join("quote.csv");
    std::fs::write(&file, b"part,qty\nPL-100,25\n").unwrap();

    let raw = signet_core::outbox::build_message(
        &Draft {
            account_id: 1,
            to: vec!["buyer@example.com".into()],
            subject: "Quote".into(),
            body_text: "Figures attached.".into(),
            attachments: vec![DraftAttachment {
                filename: "quote.csv".into(),
                path: file.to_string_lossy().to_string(),
            }],
            ..Default::default()
        },
        "Dev",
        "dev@example.com",
        "",
    )
    .unwrap();

    let parsed = signet_mime::parse(&raw).expect("our own output must parse");
    assert_eq!(parsed.attachments.len(), 1);
    assert_eq!(parsed.attachments[0].filename, "quote.csv");
    assert_eq!(parsed.attachments[0].mime_type, "text/csv");
    assert_eq!(parsed.attachments[0].data, b"part,qty\nPL-100,25\n");
    assert!(parsed.body_text.contains("Figures attached."));
}

#[test]
fn an_attachment_filename_from_a_hostile_path_is_sanitized() {
    let dir = TempDir::new("attach-evil");
    let file = dir.0.join("payload.txt");
    std::fs::write(&file, b"x").unwrap();

    let raw = signet_core::outbox::build_message(
        &Draft {
            account_id: 1,
            to: vec!["a@b.test".into()],
            attachments: vec![DraftAttachment {
                filename: "../../../etc/passwd".into(),
                path: file.to_string_lossy().to_string(),
            }],
            ..Default::default()
        },
        "Dev",
        "dev@example.com",
        "",
    )
    .unwrap();

    let parsed = signet_mime::parse(&raw).unwrap();
    assert_eq!(parsed.attachments[0].filename, "passwd");
}

#[test]
fn a_missing_attachment_file_is_a_clear_error_not_a_silent_send() {
    let err = signet_core::outbox::build_message(
        &Draft {
            account_id: 1,
            to: vec!["a@b.test".into()],
            attachments: vec![DraftAttachment {
                filename: "gone.pdf".into(),
                path: "Z:/definitely/not/here.pdf".into(),
            }],
            ..Default::default()
        },
        "Dev",
        "dev@example.com",
        "",
    )
    .unwrap_err();
    assert!(err.to_string().contains("could not read"), "got {err}");
}

// ---------------------------------------------------------------- rules

#[test]
fn a_rule_needs_conditions_and_actions() {
    let (_d, app, id) = app_with_account("rule-validate");

    let bare = Rule {
        id: 0,
        account_id: id,
        name: "Bad".into(),
        priority: 0,
        enabled: true,
        match_all: true,
        conditions: vec![],
        actions: vec![Action::Delete],
    };
    // A rule with no conditions matches everything; refusing it is the point.
    assert!(app.save_rule(bare.clone()).is_err());

    let no_action = Rule {
        conditions: vec![Condition {
            field: Field::From,
            op: Op::Contains,
            value: "x".into(),
        }],
        actions: vec![],
        ..bare
    };
    assert!(app.save_rule(no_action).is_err());
}

#[test]
fn rules_persist_and_apply_to_existing_mail() {
    let (_d, app, id) = app_with_account("rule-apply");
    seed(&app, id, "newsletter@vendor.test", "Weekly digest", "news");
    seed(&app, id, "buyer@customer.test", "PO 4471", "order");

    app.save_rule(Rule {
        id: 0,
        account_id: id,
        name: "Newsletters".into(),
        priority: 0,
        enabled: true,
        match_all: true,
        conditions: vec![Condition {
            field: Field::From,
            op: Op::Contains,
            value: "vendor.test".into(),
        }],
        actions: vec![Action::MoveTo {
            folder: FolderKind::Archive,
        }],
    })
    .unwrap();

    assert_eq!(app.list_rules(Some(id)).unwrap().len(), 1);

    // Rules normally run at ingest, so existing mail needs an explicit sweep.
    let moved = app.apply_rules_now(id).unwrap();
    assert_eq!(moved, 1);

    let archive = app.store().folder_id(id, FolderKind::Archive).unwrap();
    let archived = app
        .list_messages(MessageQuery {
            folder_id: Some(archive),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(archived.len(), 1);
    assert_eq!(archived[0].subject, "Weekly digest");

    // The order must not have been touched.
    let inbox = app.store().folder_id(id, FolderKind::Inbox).unwrap();
    let remaining = app
        .list_messages(MessageQuery {
            folder_id: Some(inbox),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].subject, "PO 4471");
}

#[test]
fn a_disabled_rule_does_nothing() {
    let (_d, app, id) = app_with_account("rule-disabled");
    seed(&app, id, "newsletter@vendor.test", "Digest", "news");

    app.save_rule(Rule {
        id: 0,
        account_id: id,
        name: "Off".into(),
        priority: 0,
        enabled: false,
        match_all: true,
        conditions: vec![Condition {
            field: Field::From,
            op: Op::Contains,
            value: "vendor.test".into(),
        }],
        actions: vec![Action::MoveTo {
            folder: FolderKind::Archive,
        }],
    })
    .unwrap();

    assert_eq!(app.apply_rules_now(id).unwrap(), 0);
}

// ---------------------------------------------------------------- junk

#[test]
fn marking_junk_files_the_message_and_trains_the_filter() {
    let (_d, app, id) = app_with_account("junk-mark");
    let msg = seed(
        &app,
        id,
        "spam@bad.test",
        "FREE MONEY NOW",
        "click here now",
    );

    let (spam_before, ham_before, _) = app.junk_corpus().unwrap();
    app.mark_junk(vec![msg], true).unwrap();

    let junk_folder = app.store().folder_id(id, FolderKind::Junk).unwrap();
    assert_eq!(app.get_message(msg).unwrap().folder_id, junk_folder);

    let (spam_after, ham_after, _) = app.junk_corpus().unwrap();
    assert_eq!(spam_after, spam_before + 1, "filter did not learn");
    assert_eq!(ham_after, ham_before);
}

#[test]
fn marking_not_junk_moves_it_back_and_retrains() {
    let (_d, app, id) = app_with_account("junk-rescue");
    let msg = seed(
        &app,
        id,
        "buyer@customer.test",
        "URGENT ORDER",
        "please ship",
    );

    app.mark_junk(vec![msg], true).unwrap();
    let (spam_mid, _, _) = app.junk_corpus().unwrap();

    app.mark_junk(vec![msg], false).unwrap();

    let inbox = app.store().folder_id(id, FolderKind::Inbox).unwrap();
    assert_eq!(app.get_message(msg).unwrap().folder_id, inbox);
    // Unread again, so a wrongly-filed message is not silently missed.
    assert!(!app.get_message(msg).unwrap().is_read);

    let (spam_after, ham_after, _) = app.junk_corpus().unwrap();
    assert_eq!(spam_after, spam_mid - 1, "the junk vote was not withdrawn");
    assert_eq!(ham_after, 1);
}

#[test]
fn marking_junk_twice_does_not_train_twice() {
    // Double-counting would let one message skew the corpus arbitrarily.
    let (_d, app, id) = app_with_account("junk-idempotent");
    let msg = seed(&app, id, "spam@bad.test", "WIN BIG", "lottery");

    app.mark_junk(vec![msg], true).unwrap();
    let (spam_once, _, _) = app.junk_corpus().unwrap();
    app.mark_junk(vec![msg], true).unwrap();
    let (spam_twice, _, _) = app.junk_corpus().unwrap();

    assert_eq!(spam_once, spam_twice);
}

#[test]
fn filing_info_explains_where_a_message_went() {
    let (_d, app, id) = app_with_account("junk-explain");
    let msg = seed(&app, id, "spam@bad.test", "WIN BIG", "lottery");
    app.mark_junk(vec![msg], true).unwrap();

    let info = app.filing_info(msg).unwrap();
    assert!(info.is_junk_folder);
    assert_eq!(info.filed_by, "marked junk");
}

// ---------------------------------------------------------------- drafts

#[test]
fn a_draft_round_trips_with_recipients_and_attachments() {
    let (dir, app, id) = app_with_account("draft");
    let file = dir.0.join("spec.pdf");
    std::fs::write(&file, b"%PDF-1.4").unwrap();

    let draft_id = app
        .save_draft(Draft {
            account_id: id,
            to: vec!["a@example.com".into()],
            cc: vec!["b@example.com".into()],
            bcc: vec!["c@example.com".into()],
            subject: "Half-written".into(),
            body_text: "Will finish later".into(),
            attachments: vec![DraftAttachment {
                filename: "spec.pdf".into(),
                path: file.to_string_lossy().to_string(),
            }],
            ..Default::default()
        })
        .unwrap();

    let back = app.load_draft(draft_id).unwrap();
    assert_eq!(back.to, vec!["a@example.com".to_string()]);
    assert_eq!(back.cc, vec!["b@example.com".to_string()]);
    assert_eq!(back.bcc, vec!["c@example.com".to_string()]);
    assert_eq!(back.subject, "Half-written");
    assert_eq!(back.body_text, "Will finish later");
    assert_eq!(back.attachments.len(), 1);
    assert_eq!(back.draft_message_id, Some(draft_id));

    // It lives in Drafts, not the Inbox.
    let drafts = app.store().folder_id(id, FolderKind::Drafts).unwrap();
    assert_eq!(app.get_message(draft_id).unwrap().folder_id, drafts);
}

#[test]
fn saving_a_draft_twice_updates_rather_than_duplicating() {
    let (_d, app, id) = app_with_account("draft-update");

    let first = app
        .save_draft(Draft {
            account_id: id,
            subject: "Version one".into(),
            to: vec!["a@example.com".into()],
            ..Default::default()
        })
        .unwrap();

    let second = app
        .save_draft(Draft {
            account_id: id,
            subject: "Version two".into(),
            to: vec!["a@example.com".into(), "b@example.com".into()],
            draft_message_id: Some(first),
            ..Default::default()
        })
        .unwrap();

    assert_eq!(first, second, "a second save created a new draft");

    let drafts_folder = app.store().folder_id(id, FolderKind::Drafts).unwrap();
    let all = app
        .list_messages(MessageQuery {
            folder_id: Some(drafts_folder),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].subject, "Version two");

    let back = app.load_draft(first).unwrap();
    assert_eq!(back.to.len(), 2, "recipients were not replaced");
}

// ---------------------------------------------------------------- folders

#[test]
fn mark_folder_read_clears_the_unread_count() {
    let (_d, app, id) = app_with_account("mark-read");
    seed(&app, id, "a@x.test", "One", "x");
    seed(&app, id, "b@x.test", "Two", "y");

    let inbox = app.store().folder_id(id, FolderKind::Inbox).unwrap();
    assert_eq!(app.mark_folder_read(inbox).unwrap(), 2);
    // Idempotent: nothing left unread to change.
    assert_eq!(app.mark_folder_read(inbox).unwrap(), 0);

    let folders = app.list_folders(Some(id)).unwrap();
    let inbox_folder = folders
        .iter()
        .find(|f| f.kind == FolderKind::Inbox)
        .unwrap();
    assert_eq!(inbox_folder.unread, 0);
    assert_eq!(inbox_folder.total, 2);
}

#[test]
fn a_queued_send_can_be_retried_is_visible_and_is_never_sent_twice() {
    let (dir, app, account_id) = app_with_account("outbox");
    let draft = Draft {
        account_id,
        to: vec!["customer@example.com".into()],
        subject: "Quote".into(),
        body_text: "Price attached.".into(),
        ..Default::default()
    };
    let id = signet_core::outbox::queue(app.store(), &draft, "Dev", "dev@example.com", "").unwrap();

    // Stored with its draft, so the timer can retry it instead of parking it as unbuildable.
    let item = app.store().outbox_item(id).unwrap().unwrap();
    assert!(!item.draft_json.is_empty(), "no draft stored with the queued send");
    // And shown in the Outbox although it was never scheduled.
    assert!(app.scheduled_messages().unwrap().iter().any(|i| i.id == id));

    // Only one caller may take a row.
    assert!(app.store().claim_outbox(id).unwrap());
    assert!(!app.store().claim_outbox(id).unwrap(), "claimed twice: would send twice");

    // A run that dies mid-send leaves it for the user, never for an automatic resend.
    drop(app);
    let app = App::open(&dir.0).unwrap();
    let item = app.store().outbox_item(id).unwrap().unwrap();
    assert_eq!(item.state, "failed");
    assert!(item.last_error.unwrap().contains("Sent Items"));
    assert!(!app.store().claim_outbox(id).unwrap());
}

#[test]
fn a_saved_reply_reopens_with_its_quote_formatting_and_threading() {
    let (_dir, app, account_id) = app_with_account("richdraft");
    let html = r#"<div><b>Yes</b>, confirmed.</div><div class="signet-quote"><p>Original</p></div><script>x()</script>"#;
    let id = app
        .save_draft(Draft {
            account_id,
            to: vec!["a@example.com".into()],
            subject: "Re: Order".into(),
            body_text: "Yes, confirmed.".into(),
            body_html: Some(html.into()),
            in_reply_to: Some("<p@x>".into()),
            references: vec!["<root@x>".into(), "<p@x>".into()],
            ..Default::default()
        })
        .unwrap();

    let back = app.load_draft(id).unwrap();
    let body = back.body_html.expect("the rich body was dropped");
    assert!(body.contains("<b>Yes</b>"), "{body}");
    assert!(body.contains(r#"class="signet-quote""#), "the quote marker was lost: {body}");
    assert!(!body.contains("<script"), "a draft body is not sanitized: {body}");
    assert_eq!(back.references, vec!["<root@x>", "<p@x>"]);
}

#[test]
fn adding_an_existing_mailbox_again_is_refused_without_touching_its_password() {
    let (_dir, app, id) = app_with_account("dupadd");
    let account = app.list_accounts().unwrap().into_iter().find(|a| a.id == id).unwrap();

    let again = NewAccount {
        display_name: "Dev".into(),
        email: "DEV@example.com ".into(),
        pop_host: account.pop_host.clone(),
        pop_port: 995,
        pop_security: Security::Tls,
        pop_user: account.pop_user.clone(),
        smtp_host: "mail.invalid".into(),
        smtp_port: 587,
        smtp_security: Security::Starttls,
        smtp_user: "dev".into(),
        leave_on_server: true,
        delete_after_days: None,
        poll_minutes: 5,
        signature: String::new(),
        protocol: Default::default(),
    };
    let err = app.add_account(again, "a typo").unwrap_err().to_string();
    assert!(err.contains("already set up"), "{err}");

    // The working account's password is exactly what it was.
    let kept = signet_auth::load(&account.credential_handle).expect("the password was deleted");
    assert_eq!(kept.expose(), "password");
}
