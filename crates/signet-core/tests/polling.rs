//! Which accounts get polled, and when.
//!
//! The sync itself needs a server and is covered elsewhere. What matters here is the selection:
//! `poll_minutes` was a stored, validated, editable setting that nothing ever read, so mail only
//! arrived when somebody pressed Send/Receive. These pin down the rule that replaced that.
//!
//! Every account points at an unreachable host, so each attempt fails — which is the point. The
//! failures say precisely which accounts were picked up.

use signet_core::App;
use signet_store::{AccountSettings, NewAccount, Security};

struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "signet-poll-{tag}-{}-{:?}",
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

fn add(app: &App, email: &str) -> i64 {
    app.add_account(
        NewAccount {
            display_name: "Dev".into(),
            email: email.into(),
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
            poll_minutes: 60,
            signature: String::new(),
            protocol: Default::default(),
        },
        "secret",
    )
    .expect("add account")
}

/// Change one account's settings, leaving the rest as they are.
fn settings(app: &App, id: i64, edit: impl FnOnce(&mut AccountSettings)) {
    let mut s = app.account_settings(id).unwrap();
    edit(&mut s);
    app.update_account_settings(id, s).unwrap();
}

fn attempted(outcomes: &[(i64, signet_core::Result<signet_sync::ingest::SyncReport>)]) -> Vec<i64> {
    let mut ids: Vec<i64> = outcomes.iter().map(|(id, _)| *id).collect();
    ids.sort();
    ids
}

#[tokio::test]
async fn an_account_that_has_never_synced_is_due_immediately() {
    // Otherwise a freshly added account sits idle until its first interval elapses, which looks
    // exactly like the account being broken.
    let dir = TempDir::new("first");
    let app = App::open(&dir.0).unwrap();
    let id = add(&app, "dev@example.com");

    assert_eq!(attempted(&app.sync_due().await), vec![id]);
}

#[tokio::test]
async fn an_account_synced_recently_is_left_alone() {
    let dir = TempDir::new("recent");
    let app = App::open(&dir.0).unwrap();
    let id = add(&app, "dev@example.com");

    // Pretend it just ran. With a 60 minute interval it must not be picked up again.
    app.store().touch_account_sync(id).unwrap();
    assert!(
        app.sync_due().await.is_empty(),
        "an account synced seconds ago was polled again"
    );
}

#[tokio::test]
async fn the_interval_is_per_account() {
    // A shared mailbox worth watching closely and an archive account checked hourly should not be
    // forced onto the same schedule.
    let dir = TempDir::new("peraccount");
    let app = App::open(&dir.0).unwrap();
    let eager = add(&app, "sales@example.com");
    let lazy = add(&app, "archive@example.com");

    app.store().touch_account_sync(eager).unwrap();
    app.store().touch_account_sync(lazy).unwrap();

    // One minute is the shortest the settings screen allows, and enough to be due already.
    settings(&app, eager, |s| s.poll_minutes = 1);
    settings(&app, lazy, |s| s.poll_minutes = 1440);

    let ids = attempted(&app.sync_due().await);
    assert!(
        !ids.contains(&lazy),
        "the daily account was polled a moment after its last sync"
    );
}

#[tokio::test]
async fn a_disabled_account_is_never_polled() {
    // Disabling an account is how you stop it doing anything without losing the mail already
    // downloaded. Polling it anyway would produce a sync error every minute for an account the
    // user has deliberately switched off.
    let dir = TempDir::new("disabled");
    let app = App::open(&dir.0).unwrap();
    let id = add(&app, "old@example.com");
    settings(&app, id, |s| s.is_enabled = false);

    assert!(
        app.sync_due().await.is_empty(),
        "a disabled account was polled"
    );
}

#[tokio::test]
async fn a_nonsense_interval_cannot_produce_a_tight_loop() {
    // The settings screen clamps this, but the database is a file on disk and the clamp has to hold
    // for a value that arrives some other way -- an import, or a hand edit.
    let dir = TempDir::new("clamp");
    let app = App::open(&dir.0).unwrap();
    let id = add(&app, "dev@example.com");

    settings(&app, id, |s| s.poll_minutes = 0);
    assert!(
        app.account_settings(id).unwrap().poll_minutes >= 1,
        "a zero interval survived being saved"
    );

    app.store().touch_account_sync(id).unwrap();
    assert!(
        app.sync_due().await.is_empty(),
        "an account was polled again within a second of its last sync"
    );
}

#[tokio::test]
async fn a_failing_account_does_not_stop_the_others_being_polled() {
    // Every host here is unreachable, so all of them fail. What must not happen is the first
    // failure abandoning the pass and leaving later accounts unchecked.
    let dir = TempDir::new("independent");
    let app = App::open(&dir.0).unwrap();
    let a = add(&app, "one@example.com");
    let b = add(&app, "two@example.com");

    let outcomes = app.sync_due().await;
    assert_eq!(attempted(&outcomes), {
        let mut want = vec![a, b];
        want.sort();
        want
    });
    assert!(
        outcomes.iter().all(|(_, r)| r.is_err()),
        "an unreachable server reported success"
    );
}
