//! Triage: priority, assignment, work status and reminders — and publishing each change to the
//! team (see [`crate::shared`]).
//!
//! Every mutation here follows the same shape: change it locally, then stage the change for the
//! shared file. Staging is deliberately separate from writing, so working with the share offline
//! is normal rather than an error — the change queues and goes out on the next sync.

use signet_store::{Priority, Reminder, Signature, TeamMember, initials_for, palette};

use crate::{App, Error, Event, Result, shared};

/// Everything the UI shows about a message beyond the mail itself.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Triage {
    pub priority: Priority,
    pub assignee: Option<TeamMember>,
    pub work_status: String,
    pub reminder: Option<Reminder>,
    /// False when the message has no `Message-ID`, so team state cannot be shared for it.
    pub shareable: bool,
}

impl App {
    // ---------------------------------------------------------------- team

    pub fn list_team(&self) -> Result<Vec<TeamMember>> {
        Ok(self.store().list_team()?)
    }

    /// Add or update a team member. Colour and initials are filled in when left blank.
    pub fn save_team_member(&self, mut member: TeamMember) -> Result<i64> {
        if member.email.trim().is_empty() {
            return Err(Error::Invalid(
                "a team member needs an email address — it is how assignments travel between \
                 machines"
                    .into(),
            ));
        }
        if member.name.trim().is_empty() {
            member.name = member.email.split('@').next().unwrap_or("").to_string();
        }
        if member.initials.trim().is_empty() {
            member.initials = initials_for(&member.name);
        }
        if member.color.trim().is_empty() {
            member.color = palette(self.store().list_team()?.len()).to_string();
        }

        let id = self.store().upsert_team_member(&member)?;
        if member.is_me {
            self.store().set_me(id)?;
        }
        self.emit_public(Event::AccountsChanged);
        Ok(id)
    }

    pub fn delete_team_member(&self, id: i64) -> Result<()> {
        self.store().delete_team_member(id)?;
        self.emit_public(Event::MessagesChanged { account_id: 0 });
        Ok(())
    }

    pub fn set_me(&self, id: i64) -> Result<()> {
        self.store().set_me(id)?;
        self.emit_public(Event::AccountsChanged);
        Ok(())
    }

    fn me_email(&self) -> String {
        self.store()
            .me()
            .ok()
            .flatten()
            .map(|m| m.email)
            .unwrap_or_default()
    }

    // ---------------------------------------------------------------- triage

    pub fn triage(&self, message_id: i64) -> Result<Triage> {
        let (priority, assignee_id, work_status) = self.store().triage(message_id)?;
        let assignee = match assignee_id {
            Some(id) => self.store().list_team()?.into_iter().find(|m| m.id == id),
            None => None,
        };
        Ok(Triage {
            priority,
            assignee,
            work_status,
            reminder: self.store().reminder_for(message_id)?,
            shareable: self.store().message_key(message_id)?.is_some(),
        })
    }

    pub fn set_priority(&self, ids: Vec<i64>, priority: Priority) -> Result<()> {
        self.store().set_priority(&ids, priority)?;
        for id in &ids {
            self.publish(*id)?;
        }
        self.emit_public(Event::MessagesChanged { account_id: 0 });
        Ok(())
    }

    /// Assign to a team member, or to nobody with `None`.
    pub fn assign(&self, ids: Vec<i64>, member_id: Option<i64>) -> Result<()> {
        self.store().set_assignee(&ids, member_id)?;
        // Picking something up is what moves it out of the unclaimed queue.
        if member_id.is_some() {
            self.store().set_work_status(&ids, "in_progress")?;
        }
        for id in &ids {
            self.publish(*id)?;
        }
        self.emit_public(Event::MessagesChanged { account_id: 0 });
        Ok(())
    }

    pub fn set_work_status(&self, ids: Vec<i64>, status: String) -> Result<()> {
        const ALLOWED: [&str; 4] = ["new", "in_progress", "waiting", "done"];
        if !ALLOWED.contains(&status.as_str()) {
            return Err(Error::Invalid(format!("unknown status {status:?}")));
        }
        self.store().set_work_status(&ids, &status)?;
        for id in &ids {
            self.publish(*id)?;
        }
        self.emit_public(Event::MessagesChanged { account_id: 0 });
        Ok(())
    }

    // ---------------------------------------------------------------- reminders

    pub fn set_reminder(&self, message_id: i64, due_utc: i64, note: String) -> Result<()> {
        self.store().set_reminder(message_id, due_utc, &note)?;
        self.publish(message_id)?;
        self.emit_public(Event::MessagesChanged { account_id: 0 });
        Ok(())
    }

    pub fn complete_reminder(&self, message_id: i64) -> Result<()> {
        self.store().complete_reminder(message_id)?;
        self.publish(message_id)?;
        self.emit_public(Event::MessagesChanged { account_id: 0 });
        Ok(())
    }

    pub fn clear_reminder(&self, message_id: i64) -> Result<()> {
        self.store().clear_reminder(message_id)?;
        self.publish(message_id)?;
        self.emit_public(Event::MessagesChanged { account_id: 0 });
        Ok(())
    }

    pub fn open_reminders(&self) -> Result<Vec<Reminder>> {
        Ok(self.store().open_reminders()?)
    }

    /// Reminders that have come due and not yet been announced.
    ///
    /// Marks them announced as it returns them, so the caller can fire notifications without
    /// having to remember what it already showed.
    pub fn take_due_reminders(&self) -> Result<Vec<Reminder>> {
        let due = self.store().reminders_to_announce(signet_store::now_utc())?;
        if !due.is_empty() {
            let ids: Vec<i64> = due.iter().map(|r| r.id).collect();
            self.store().mark_reminder_announced(&ids)?;
        }
        Ok(due)
    }

    // ---------------------------------------------------------------- sharing

    /// Stage this message's team state for the shared file.
    fn publish(&self, message_id: i64) -> Result<()> {
        let Some(key) = self.store().message_key(message_id)? else {
            // No Message-ID, so there is nothing the team can agree on. `Triage::shareable` tells
            // the UI to say so rather than silently dropping the change.
            return Ok(());
        };

        let (priority, assignee_id, work_status) = self.store().triage(message_id)?;
        let assignee_email = match assignee_id {
            Some(id) => self
                .store()
                .list_team()?
                .into_iter()
                .find(|m| m.id == id)
                .map(|m| m.email),
            None => None,
        };
        let reminder = self.store().reminder_for(message_id)?;

        self.store().stage_shared(&signet_store::SharedEntry {
            message_key: key,
            assignee_email,
            priority: Some(priority.as_i64()),
            work_status: Some(work_status),
            reminder_due_utc: reminder.as_ref().map(|r| r.due_utc),
            reminder_note: reminder.map(|r| r.note),
            updated_utc: signet_store::now_utc(),
            updated_by: self.me_email(),
        })?;
        Ok(())
    }

    /// Exchange team state with the shared file.
    pub fn sync_team(&self) -> Result<shared::SyncSummary> {
        let summary = shared::sync(self.store(), &self.me_email())?;
        if summary.pulled > 0 || summary.members_added > 0 {
            self.emit_public(Event::MessagesChanged { account_id: 0 });
        }
        Ok(summary)
    }

    pub fn shared_path(&self) -> Result<String> {
        Ok(self
            .store()
            .setting(shared::SETTING_SHARED_PATH)?
            .unwrap_or_default())
    }

    pub fn set_shared_path(&self, path: String) -> Result<()> {
        self.store()
            .set_setting(shared::SETTING_SHARED_PATH, path.trim())?;
        Ok(())
    }

    // ---------------------------------------------------------------- signatures

    pub fn list_signatures(&self) -> Result<Vec<Signature>> {
        Ok(self.store().list_signatures()?)
    }

    pub fn save_signature(&self, mut signature: Signature) -> Result<i64> {
        if signature.name.trim().is_empty() {
            return Err(Error::Invalid("give the signature a name".into()));
        }
        if signature.body.trim().is_empty() {
            return Err(Error::Invalid("the signature is empty".into()));
        }
        signature.name = signature.name.trim().to_string();

        // The very first signature becomes the default, so composing works without anyone having
        // to go and set one.
        if self.store().list_signatures()?.is_empty() {
            signature.is_default = true;
        }
        Ok(self.store().save_signature(&signature)?)
    }

    pub fn delete_signature(&self, id: i64) -> Result<()> {
        Ok(self.store().delete_signature(id)?)
    }

    pub fn set_default_signature(&self, id: i64) -> Result<()> {
        Ok(self.store().set_default_signature(id)?)
    }

    // ---------------------------------------------------------------- settings kv

    pub fn get_setting(&self, key: String) -> Result<Option<String>> {
        Ok(self.store().setting(&key)?)
    }

    pub fn put_setting(&self, key: String, value: String) -> Result<()> {
        self.store().set_setting(&key, &value)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_member_without_an_email_is_refused() {
        // Email is the identity assignments travel under; without it sharing cannot work.
        let dir = std::env::temp_dir().join(format!("signet-team-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let app = App::open(&dir).unwrap();

        let err = app
            .save_team_member(TeamMember {
                id: 0,
                name: "No Email".into(),
                email: "  ".into(),
                color: String::new(),
                initials: String::new(),
                is_active: true,
                is_me: false,
                sort_order: 0,
            })
            .unwrap_err();
        assert!(matches!(err, Error::Invalid(_)));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
