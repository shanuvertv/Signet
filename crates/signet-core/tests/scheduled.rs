//! Holding a message back until a chosen time.
//!
//! Sending itself needs a server, so what is checked here is everything around it: that a scheduled
//! message is not sent early, that it can be cancelled, that cancelling actually stops it, and that
//! the queue distinguishes a message waiting for Monday from one waiting out a network failure.

use signet_core::App;
use signet_core::outbox::Draft;
use signet_store::{NewAccount, Security};

struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "signet-sched-{tag}-{}-{:?}",
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
                // Deliberately unreachable: nothing here should get as far as a socket.
                smtp_host: "smtp.invalid".into(),
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

fn draft(account_id: i64, subject: &str) -> Draft {
    Draft {
        account_id,
        to: vec!["buyer@customer.test".into()],
        subject: subject.into(),
        body_text: "The quote is attached.".into(),
        ..Default::default()
    }
}

fn in_an_hour() -> i64 {
    signet_store::now_utc() + 3600
}

#[tokio::test]
async fn a_scheduled_message_waits_and_is_listed() {
    let (_d, app, account) = app_with_account("waits");
    let when = in_an_hour();

    app.schedule_send(draft(account, "Quote for downlights"), when)
        .await
        .expect("schedule");

    let waiting = app.scheduled_messages().unwrap();
    assert_eq!(waiting.len(), 1);
    assert_eq!(waiting[0].subject, "Quote for downlights");
    assert_eq!(waiting[0].scheduled_utc, when);
    assert_eq!(waiting[0].to_addrs, "buyer@customer.test");

    // The SMTP host is unreachable, so a flush that tried to send would fail loudly. Nothing is
    // due, so nothing should be attempted at all.
    let flush = app.flush_outbox().await.unwrap();
    assert_eq!(flush.sent, 0);
    assert_eq!(
        flush.failed, 0,
        "a message due in an hour was attempted now"
    );
    assert_eq!(app.scheduled_messages().unwrap().len(), 1);
}

#[tokio::test]
async fn a_time_in_the_past_is_refused() {
    // Silently sending immediately would be a reasonable guess and the wrong one: the user asked
    // for a specific time, and if it cannot be honoured they should know before they close the
    // window rather than after the message has gone.
    let (_d, app, account) = app_with_account("past");
    let err = app
        .schedule_send(draft(account, "Late"), signet_store::now_utc() - 60)
        .await
        .expect_err("a time in the past was accepted");
    assert!(err.to_string().contains("passed"), "{err}");
    assert!(app.scheduled_messages().unwrap().is_empty());
}

#[tokio::test]
async fn a_message_with_no_recipients_is_refused() {
    let (_d, app, account) = app_with_account("norecip");
    let mut d = draft(account, "Nobody");
    d.to.clear();

    let err = app
        .schedule_send(d, in_an_hour())
        .await
        .expect_err("a message with no recipients was scheduled");
    assert!(err.to_string().contains("recipient"), "{err}");
}

#[tokio::test]
async fn cancelling_removes_it_and_it_never_goes_out() {
    let (_d, app, account) = app_with_account("cancel");
    let id = app
        .schedule_send(draft(account, "Changed my mind"), in_an_hour())
        .await
        .unwrap();

    assert!(app.cancel_scheduled(id).unwrap(), "cancel reported nothing");
    assert!(app.scheduled_messages().unwrap().is_empty());

    // And it is not merely hidden from the list: a flush must not find it either.
    let flush = app.flush_outbox().await.unwrap();
    assert_eq!(flush.sent, 0);
    assert_eq!(flush.failed, 0, "a cancelled message was still attempted");
}

#[tokio::test]
async fn cancelling_something_that_is_not_there_says_so_rather_than_pretending() {
    let (_d, app, _) = app_with_account("nocancel");
    assert!(!app.cancel_scheduled(9999).unwrap());
}

#[tokio::test]
async fn a_failed_send_backs_off_instead_of_retrying_in_a_loop() {
    // The SMTP host does not resolve, so this exercises the failure path end to end. What matters
    // is that the next attempt is pushed into the future: a flush on a timer with no backoff would
    // hammer a dead server once a minute forever.
    let (_d, app, account) = app_with_account("backoff");
    let when = signet_store::now_utc() + 1;
    app.schedule_send(draft(account, "Doomed"), when)
        .await
        .unwrap();

    // Make it due without waiting.
    let id = app.scheduled_messages().unwrap()[0].id;
    app.store().send_scheduled_now(id).unwrap();

    let flush = app.flush_outbox().await.unwrap();
    assert_eq!(flush.sent, 0);
    assert_eq!(flush.failed, 1, "an unreachable server reported success");
    assert!(flush.last_error.is_some(), "the failure was not reported");

    // Still listed, so the user can see it went wrong and can cancel it.
    let waiting = app.scheduled_messages().unwrap();
    assert_eq!(waiting.len(), 1);

    // And a second flush straight away must not try again.
    let again = app.flush_outbox().await.unwrap();
    assert_eq!(
        again.failed, 0,
        "a failed message was retried immediately instead of backing off"
    );
}

#[tokio::test]
async fn scheduling_survives_reopening_the_app() {
    // The whole point of a scheduled message is that it outlives the compose window, which means
    // outliving the process. The draft is stored with the row precisely so it can be rebuilt.
    let dir = TempDir::new("restart");
    let when = in_an_hour();

    let account = {
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
                    smtp_host: "smtp.invalid".into(),
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
            .unwrap();
        app.schedule_send(draft(id, "Monday morning"), when)
            .await
            .unwrap();
        id
    };

    let reopened = App::open(&dir.0).expect("reopen");
    let waiting = reopened.scheduled_messages().unwrap();
    assert_eq!(waiting.len(), 1, "the scheduled message did not survive");
    assert_eq!(waiting[0].subject, "Monday morning");
    assert_eq!(waiting[0].account_id, account);
    assert!(
        !waiting[0].draft_json.is_empty(),
        "the draft was not stored, so it could never be sent"
    );
}
