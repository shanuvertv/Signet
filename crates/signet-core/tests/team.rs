//! Team triage, and the shared-state file that carries it between machines.
//!
//! The interesting tests here simulate **two separate installations** pointed at one shared file,
//! because that is the whole point: POP3 gives a sales team no shared state, and everything below
//! exists to work around that.

use signet_core::App;
use signet_store::{
    FolderKind, MessageQuery, NewAccount, NewMessage, Priority, Recipient, Security, TeamMember,
};

struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "signet-team-{tag}-{}-{:?}",
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

fn open(dir: &std::path::Path) -> (App, i64) {
    let app = App::open(dir).expect("open");
    let id = app
        .add_account(
            NewAccount {
                display_name: "Sales".into(),
                email: "sales@example.com".into(),
                pop_host: "mail.invalid".into(),
                pop_port: 995,
                pop_security: Security::Tls,
                pop_user: "sales".into(),
                smtp_host: String::new(),
                smtp_port: 587,
                smtp_security: Security::Starttls,
                smtp_user: String::new(),
                leave_on_server: true,
                delete_after_days: None,
                poll_minutes: 5,
                signature: String::new(),
                protocol: Default::default(),
            },
            "pw",
        )
        .expect("account");
    (app, id)
}

/// Seed a message with a given Message-ID, which is the key team state travels under.
fn seed(app: &App, account_id: i64, message_id: &str, subject: &str) -> i64 {
    let folder = app
        .store()
        .folder_id(account_id, FolderKind::Inbox)
        .unwrap();
    app.store()
        .insert_message(&NewMessage {
            account_id,
            folder_id: folder,
            rfc822_message_id: Some(message_id.to_string()),
            thread_id: subject.into(),
            from_name: "Customer".into(),
            from_addr: "buyer@customer.test".into(),
            subject: subject.into(),
            date_utc: 1_700_000_000,
            received_utc: 1_700_000_000,
            body_text: "enquiry".into(),
            recipients: vec![Recipient {
                kind: "to".into(),
                name: String::new(),
                addr: "sales@example.com".into(),
            }],
            ..Default::default()
        })
        .unwrap()
}

fn member(name: &str, email: &str, is_me: bool) -> TeamMember {
    TeamMember {
        id: 0,
        name: name.into(),
        email: email.into(),
        color: String::new(),
        initials: String::new(),
        is_active: true,
        is_me,
        sort_order: 0,
    }
}

// ---------------------------------------------------------------- basics

#[test]
fn a_new_member_gets_a_colour_and_initials() {
    let dir = TempDir::new("member");
    let (app, _) = open(&dir.0);

    app.save_team_member(member("Max Rahman", "max@example.com", true))
        .unwrap();
    let team = app.list_team().unwrap();
    assert_eq!(team.len(), 1);
    assert_eq!(team[0].initials, "MR");
    assert!(team[0].color.starts_with('#'), "no colour assigned");
    assert!(team[0].is_me);
}

#[test]
fn only_one_person_can_be_me() {
    let dir = TempDir::new("me");
    let (app, _) = open(&dir.0);

    app.save_team_member(member("A", "a@example.com", true))
        .unwrap();
    let b = app
        .save_team_member(member("B", "b@example.com", true))
        .unwrap();

    let team = app.list_team().unwrap();
    assert_eq!(team.iter().filter(|m| m.is_me).count(), 1);
    assert!(team.iter().find(|m| m.id == b).unwrap().is_me);
}

#[test]
fn assigning_marks_the_message_as_being_worked_on() {
    let dir = TempDir::new("assign");
    let (app, acc) = open(&dir.0);
    let msg = seed(&app, acc, "<one@customer.test>", "Enquiry");
    let who = app
        .save_team_member(member("Sarah", "sarah@example.com", false))
        .unwrap();

    app.assign(vec![msg], Some(who)).unwrap();

    let t = app.triage(msg).unwrap();
    assert_eq!(t.assignee.as_ref().unwrap().name, "Sarah");
    // Picking something up is what moves it out of the unclaimed queue.
    assert_eq!(t.work_status, "in_progress");
    assert!(t.shareable);
}

#[test]
fn a_message_without_a_message_id_is_marked_unshareable() {
    let dir = TempDir::new("nokey");
    let (app, acc) = open(&dir.0);
    let folder = app.store().folder_id(acc, FolderKind::Inbox).unwrap();
    let msg = app
        .store()
        .insert_message(&NewMessage {
            account_id: acc,
            folder_id: folder,
            rfc822_message_id: None,
            thread_id: "x".into(),
            date_utc: 1,
            received_utc: 1,
            ..Default::default()
        })
        .unwrap();

    // The UI needs to say so rather than silently dropping the assignment on the floor.
    assert!(!app.triage(msg).unwrap().shareable);
}

#[test]
fn the_list_carries_priority_and_assignee_for_colour_coding() {
    let dir = TempDir::new("list");
    let (app, acc) = open(&dir.0);
    let msg = seed(&app, acc, "<x@customer.test>", "Urgent order");
    let who = app
        .save_team_member(member("Bob Vance", "bob@example.com", false))
        .unwrap();

    app.set_priority(vec![msg], Priority::High).unwrap();
    app.assign(vec![msg], Some(who)).unwrap();

    // Denormalised onto the summary so the list can colour every row without a query per row.
    let list = app.list_messages(MessageQuery::default()).unwrap();
    let row = list.iter().find(|m| m.id == msg).unwrap();
    assert_eq!(row.priority, 2);
    assert_eq!(row.assignee_initials.as_deref(), Some("BV"));
    assert!(row.assignee_color.as_deref().unwrap().starts_with('#'));
    assert_eq!(row.work_status, "in_progress");
}

#[test]
fn filters_narrow_to_mine_unassigned_and_high_priority() {
    let dir = TempDir::new("filters");
    let (app, acc) = open(&dir.0);
    let mine = seed(&app, acc, "<a@c.test>", "Mine");
    let theirs = seed(&app, acc, "<b@c.test>", "Theirs");
    let nobody = seed(&app, acc, "<c@c.test>", "Nobody");

    let me = app
        .save_team_member(member("Me", "me@example.com", true))
        .unwrap();
    let them = app
        .save_team_member(member("Them", "them@example.com", false))
        .unwrap();
    app.assign(vec![mine], Some(me)).unwrap();
    app.assign(vec![theirs], Some(them)).unwrap();
    app.set_priority(vec![theirs], Priority::High).unwrap();

    let q = |f: &dyn Fn(&mut MessageQuery)| {
        let mut query = MessageQuery::default();
        f(&mut query);
        app.list_messages(query).unwrap()
    };

    let assigned_to_me = q(&|q: &mut MessageQuery| q.assignee_id = Some(me));
    assert_eq!(assigned_to_me.len(), 1);
    assert_eq!(assigned_to_me[0].id, mine);

    let unassigned = q(&|q: &mut MessageQuery| q.unassigned_only = true);
    assert_eq!(unassigned.len(), 1);
    assert_eq!(unassigned[0].id, nobody);

    let high = q(&|q: &mut MessageQuery| q.high_priority_only = true);
    assert_eq!(high.len(), 1);
    assert_eq!(high[0].id, theirs);
}

// ---------------------------------------------------------------- reminders

#[test]
fn a_reminder_appears_in_the_followup_list_and_can_be_completed() {
    let dir = TempDir::new("reminder");
    let (app, acc) = open(&dir.0);
    let msg = seed(&app, acc, "<r@customer.test>", "Chase the quote");

    let due = signet_store::now_utc() + 3600;
    app.set_reminder(msg, due, "call them".into()).unwrap();

    let open_now = app.open_reminders().unwrap();
    assert_eq!(open_now.len(), 1);
    assert_eq!(open_now[0].note, "call them");
    // Denormalised so the follow-up list needs no join per row.
    assert_eq!(open_now[0].subject, "Chase the quote");

    app.complete_reminder(msg).unwrap();
    assert!(app.open_reminders().unwrap().is_empty());
}

#[test]
fn setting_a_second_reminder_replaces_the_first() {
    let dir = TempDir::new("reminder2");
    let (app, acc) = open(&dir.0);
    let msg = seed(&app, acc, "<r2@customer.test>", "Chase");

    app.set_reminder(msg, signet_store::now_utc() + 60, String::new())
        .unwrap();
    app.set_reminder(msg, signet_store::now_utc() + 7200, "later".into())
        .unwrap();

    let open_now = app.open_reminders().unwrap();
    assert_eq!(open_now.len(), 1, "two open reminders on one message");
    assert_eq!(open_now[0].note, "later");
}

#[test]
fn a_due_reminder_is_announced_exactly_once() {
    let dir = TempDir::new("announce");
    let (app, acc) = open(&dir.0);
    let msg = seed(&app, acc, "<d@customer.test>", "Overdue");

    app.set_reminder(msg, signet_store::now_utc() - 60, String::new())
        .unwrap();

    assert_eq!(app.take_due_reminders().unwrap().len(), 1);
    // Otherwise it would pop up again every minute forever.
    assert!(app.take_due_reminders().unwrap().is_empty());
}

#[test]
fn a_future_reminder_is_not_announced_yet() {
    let dir = TempDir::new("future");
    let (app, acc) = open(&dir.0);
    let msg = seed(&app, acc, "<f@customer.test>", "Later");
    app.set_reminder(msg, signet_store::now_utc() + 86_400, String::new())
        .unwrap();
    assert!(app.take_due_reminders().unwrap().is_empty());
}

// ---------------------------------------------------------------- sharing

/// Two installations, one shared file — the scenario the whole design exists for.
#[test]
fn an_assignment_made_on_one_machine_reaches_another() {
    let shared_dir = TempDir::new("share-file");
    let shared = shared_dir.0.join("team-state.json");

    let dir_a = TempDir::new("machine-a");
    let dir_b = TempDir::new("machine-b");
    let (alice, acc_a) = open(&dir_a.0);
    let (bob, acc_b) = open(&dir_b.0);

    for app in [&alice, &bob] {
        app.set_shared_path(shared.to_string_lossy().to_string())
            .unwrap();
    }

    // Bob has some other mail first, so his local ids genuinely differ from Alice's — which is
    // the situation the Message-ID key exists for.
    seed(&bob, acc_b, "<unrelated@customer.test>", "Something else");

    // The same message, downloaded separately by each of them. Different local ids, same
    // Message-ID.
    let key = "<order-4471@customer.test>";
    let id_a = seed(&alice, acc_a, key, "PO 4471");
    let id_b = seed(&bob, acc_b, key, "PO 4471");
    assert_ne!(
        id_a, id_b,
        "fixture should give the two machines different local ids"
    );

    // Alice sets up the team and claims the message.
    let sarah = alice
        .save_team_member(member("Sarah Khan", "sarah@example.com", true))
        .unwrap();
    alice.set_priority(vec![id_a], Priority::High).unwrap();
    alice.assign(vec![id_a], Some(sarah)).unwrap();
    alice
        .set_reminder(id_a, 1_800_000_000, "chase the PO".into())
        .unwrap();
    let out = alice.sync_team().unwrap();
    assert!(out.unreachable.is_none(), "share unreachable: {out:?}");

    // Bob syncs and sees all of it, against his own copy of the message.
    let got = bob.sync_team().unwrap();
    assert!(got.unreachable.is_none());
    assert!(got.pulled >= 1, "nothing came across: {got:?}");
    assert!(got.members_added >= 1, "the team list did not travel");

    let t = bob.triage(id_b).unwrap();
    assert_eq!(
        t.assignee.as_ref().map(|m| m.email.as_str()),
        Some("sarah@example.com"),
        "assignment did not reach the other machine"
    );
    assert_eq!(t.priority, Priority::High);
    assert_eq!(t.reminder.as_ref().unwrap().note, "chase the PO");

    // Sarah is Alice, not Bob: "me" is a property of the machine, never of the shared file.
    assert!(
        !bob.list_team().unwrap().iter().any(|m| m.is_me),
        "the shared file overwrote who is sitting at this computer"
    );
}

#[test]
fn the_newer_change_wins_when_two_people_edit_the_same_message() {
    let shared_dir = TempDir::new("share-conflict");
    let shared = shared_dir.0.join("team-state.json");

    let dir_a = TempDir::new("conflict-a");
    let dir_b = TempDir::new("conflict-b");
    let (alice, acc_a) = open(&dir_a.0);
    let (bob, acc_b) = open(&dir_b.0);
    for app in [&alice, &bob] {
        app.set_shared_path(shared.to_string_lossy().to_string())
            .unwrap();
    }

    let key = "<contested@customer.test>";
    let id_a = seed(&alice, acc_a, key, "Contested");
    let id_b = seed(&bob, acc_b, key, "Contested");

    let a_member = alice
        .save_team_member(member("Alice", "alice@example.com", true))
        .unwrap();
    alice.assign(vec![id_a], Some(a_member)).unwrap();
    alice.sync_team().unwrap();
    bob.sync_team().unwrap();

    // Bob reassigns later, so his change is the newer one.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let b_member = bob
        .save_team_member(member("Bob", "bob@example.com", true))
        .unwrap();
    bob.assign(vec![id_b], Some(b_member)).unwrap();
    bob.sync_team().unwrap();

    alice.sync_team().unwrap();
    assert_eq!(
        alice.triage(id_a).unwrap().assignee.map(|m| m.email),
        Some("bob@example.com".to_string()),
        "the older assignment clobbered the newer one"
    );
}

#[test]
fn team_state_arriving_before_the_message_is_adopted_when_it_downloads() {
    // The common case in practice: one salesperson syncs mail and triages it before a colleague
    // has even downloaded that message.
    let shared_dir = TempDir::new("share-early");
    let shared = shared_dir.0.join("team-state.json");

    let dir_a = TempDir::new("early-a");
    let dir_b = TempDir::new("early-b");
    let (alice, acc_a) = open(&dir_a.0);
    let (bob, acc_b) = open(&dir_b.0);
    for app in [&alice, &bob] {
        app.set_shared_path(shared.to_string_lossy().to_string())
            .unwrap();
    }

    let key = "<early@customer.test>";
    let id_a = seed(&alice, acc_a, key, "Early");
    let sarah = alice
        .save_team_member(member("Sarah", "sarah@example.com", true))
        .unwrap();
    alice.assign(vec![id_a], Some(sarah)).unwrap();
    alice.sync_team().unwrap();

    // Bob syncs the team state while he does not yet have the message at all.
    bob.sync_team().unwrap();

    // Now the message arrives on his machine, and a later sync adopts the waiting state.
    let id_b = seed(&bob, acc_b, key, "Early");
    bob.sync_team().unwrap();

    assert_eq!(
        bob.triage(id_b).unwrap().assignee.map(|m| m.email),
        Some("sarah@example.com".to_string()),
        "state that arrived before the message was lost"
    );
}

#[test]
fn an_unreachable_share_is_reported_rather_than_silently_ignored() {
    let dir = TempDir::new("unreachable");
    let (app, _) = open(&dir.0);
    // A path under a file, which cannot be a directory — a stand-in for a share that is down.
    let bogus = dir.0.join("not-a-dir.txt");
    std::fs::write(&bogus, b"x").unwrap();
    app.set_shared_path(bogus.join("team.json").to_string_lossy().to_string())
        .unwrap();

    let out = app.sync_team();
    // An explicit "unreachable" or an outright error are both fine. Silently reporting success is
    // not: the team view would then look authoritative when it is one machine's opinion.
    if let Ok(s) = out {
        assert!(
            s.unreachable.is_some(),
            "a broken share looked like success"
        );
    }
}

#[test]
fn sharing_is_off_by_default_and_does_nothing() {
    let dir = TempDir::new("off");
    let (app, acc) = open(&dir.0);
    let msg = seed(&app, acc, "<solo@customer.test>", "Solo");
    let me = app
        .save_team_member(member("Solo", "solo@example.com", true))
        .unwrap();
    app.assign(vec![msg], Some(me)).unwrap();

    let out = app.sync_team().unwrap();
    assert_eq!(out.pulled, 0);
    assert_eq!(out.pushed, 0);
    assert!(
        out.unreachable.is_none(),
        "no share configured is not an error"
    );
    // The assignment still works locally.
    assert!(app.triage(msg).unwrap().assignee.is_some());
}
