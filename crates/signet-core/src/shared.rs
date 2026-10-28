//! The shared team-state file.
//!
//! POP3 has no shared state (PLAN.md §2), so a sales team working one mailbox has no way to tell
//! each other who is handling what. This module carries that state out-of-band: a JSON file on a
//! path every member can reach — a UNC share, a mapped drive, a synced folder — that each client
//! merges with its own view.
//!
//! **Keyed on the RFC822 `Message-ID`, not our row id.** Two people who each downloaded the same
//! message over POP3 hold different local ids for it but the same `Message-ID`, so that is the
//! only join key that means anything across machines. A message with no `Message-ID` cannot take
//! part; the UI says so rather than pretending.
//!
//! Conflict resolution is last-write-wins per message, decided on `updated_utc`. That is adequate
//! here and converges regardless of who writes first, which matters because there is no lock
//! worth trusting across a file share. Two people assigning the same message within the same
//! second is the one case it gets arbitrarily — and the loser sees the winner's choice appear,
//! which is the right failure for this.

use std::path::{Path, PathBuf};

use signet_store::{SharedEntry, Store};
use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// Where the shared file lives. Empty means team sharing is off.
pub const SETTING_SHARED_PATH: &str = "team.shared_path";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SharedFile {
    version: u32,
    #[serde(default)]
    members: Vec<signet_store::TeamMember>,
    #[serde(default)]
    messages: Vec<SharedEntry>,
}

impl Default for SharedFile {
    fn default() -> Self {
        Self {
            version: 1,
            members: Vec::new(),
            messages: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct SyncSummary {
    pub pulled: usize,
    pub pushed: usize,
    pub members_added: usize,
    /// Set when sharing is configured but the file could not be reached, so the UI can say the
    /// team view may be stale instead of silently showing one machine's opinion.
    pub unreachable: Option<String>,
}

/// Read, merge, write. Safe to call as often as you like.
pub fn sync(store: &Store, me_email: &str) -> Result<SyncSummary> {
    let Some(path) = configured_path(store)? else {
        return Ok(SyncSummary::default());
    };

    let mut summary = SyncSummary::default();

    let remote = match read_file(&path) {
        Ok(f) => f,
        Err(e) => {
            // A share that is down must not fail the whole app, and must not look like success.
            tracing::warn!(path = %path.display(), error = %e, "shared team file unreachable");
            summary.unreachable = Some(e.to_string());
            return Ok(summary);
        }
    };

    // --- pull -------------------------------------------------------------
    let local_team = store.list_team()?;
    for member in &remote.members {
        if !local_team
            .iter()
            .any(|m| m.email.eq_ignore_ascii_case(&member.email))
        {
            let mut incoming = member.clone();
            // "Who is me" is a property of this machine, never of the shared file.
            incoming.is_me = false;
            store.upsert_team_member(&incoming)?;
            summary.members_added += 1;
        }
    }

    for entry in &remote.messages {
        if store.merge_shared(entry)? {
            summary.pulled += 1;
        }
    }

    // Apply everything not yet written onto a local message — not just what arrived in this
    // round. Team state routinely reaches a machine before the message it refers to does, and
    // applying only fresh merges would lose it permanently.
    for entry in store.unapplied_shared()? {
        if apply_to_local(store, &entry)? {
            store.mark_shared_applied(&entry.message_key, entry.updated_utc)?;
        }
    }

    // --- push -------------------------------------------------------------
    let mine = store.all_shared()?;
    let merged = merge_for_write(&remote.messages, &mine);
    summary.pushed = mine.len();

    let mut members = remote.members.clone();
    for m in store.list_team()?.into_iter().filter(|m| m.is_active) {
        if !members
            .iter()
            .any(|x| x.email.eq_ignore_ascii_case(&m.email))
        {
            members.push(signet_store::TeamMember { is_me: false, ..m });
        }
    }

    let out = SharedFile {
        version: 1,
        members,
        messages: merged,
    };
    write_atomically(&path, &out)?;
    store.clear_dirty()?;

    tracing::info!(
        pulled = summary.pulled,
        pushed = summary.pushed,
        by = me_email,
        "team state synced"
    );
    Ok(summary)
}

/// Combine what the file already had with what this machine knows, newest wins per message.
fn merge_for_write(remote: &[SharedEntry], local: &[SharedEntry]) -> Vec<SharedEntry> {
    let mut by_key: std::collections::HashMap<&str, &SharedEntry> =
        remote.iter().map(|e| (e.message_key.as_str(), e)).collect();

    for entry in local {
        by_key
            .entry(entry.message_key.as_str())
            .and_modify(|existing| {
                if entry.updated_utc > existing.updated_utc {
                    *existing = entry;
                }
            })
            .or_insert(entry);
    }

    let mut out: Vec<SharedEntry> = by_key.into_values().cloned().collect();
    out.sort_by(|a, b| a.message_key.cmp(&b.message_key));
    out
}

/// Push a merged entry into this machine's own message rows.
///
/// Returns whether it was applied. `false` means the message is not on this machine yet, so the
/// entry stays unapplied and is retried on a later sync.
fn apply_to_local(store: &Store, entry: &SharedEntry) -> Result<bool> {
    let Some(message_id) = store.message_id_for_key(&entry.message_key)? else {
        // The message has not been downloaded here yet. The row stays in `shared_state` and is
        // adopted when it arrives, which is why the table is keyed on Message-ID rather than
        // joined to `messages`.
        return Ok(false);
    };

    if let Some(p) = entry.priority {
        store.set_priority(&[message_id], signet_store::Priority::from_i64(p))?;
    }
    if let Some(status) = &entry.work_status {
        store.set_work_status(&[message_id], status)?;
    }

    match &entry.assignee_email {
        Some(email) if !email.is_empty() => {
            let assignee = store.team_member_by_email(email)?.map(|m| m.id);
            store.set_assignee(&[message_id], assignee)?;
        }
        _ => store.set_assignee(&[message_id], None)?,
    }

    match entry.reminder_due_utc {
        Some(due) => {
            store.set_reminder(
                message_id,
                due,
                entry.reminder_note.as_deref().unwrap_or_default(),
            )?;
        }
        None => store.clear_reminder(message_id)?,
    }

    Ok(true)
}

pub fn configured_path(store: &Store) -> Result<Option<PathBuf>> {
    Ok(store
        .setting(SETTING_SHARED_PATH)?
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from))
}

fn read_file(path: &Path) -> std::io::Result<SharedFile> {
    if !path.exists() {
        return Ok(SharedFile::default());
    }
    let text = std::fs::read_to_string(path)?;
    if text.trim().is_empty() {
        return Ok(SharedFile::default());
    }
    serde_json::from_str(&text).map_err(|e| {
        // A corrupt shared file must not take the app down, but it must be loud.
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("shared team file is not valid JSON: {e}"),
        )
    })
}

/// Write via a temporary file and rename.
///
/// Someone else may be reading the file at any moment; a partial write would hand them truncated
/// JSON, and [`read_file`] would then treat the whole team's state as corrupt.
fn write_atomically(path: &Path, file: &SharedFile) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(Error::Io)?;
    }
    let json = serde_json::to_string_pretty(file)
        .map_err(|e| Error::Invalid(format!("could not encode team state: {e}")))?;

    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, json).map_err(Error::Io)?;
    // Rename over the target. On Windows this fails if the destination is open for writing, so
    // fall back to a direct write rather than losing the update entirely.
    if std::fs::rename(&tmp, path).is_err() {
        let json = serde_json::to_string_pretty(file)
            .map_err(|e| Error::Invalid(format!("could not encode team state: {e}")))?;
        std::fs::write(path, json).map_err(Error::Io)?;
        let _ = std::fs::remove_file(&tmp);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(key: &str, assignee: &str, updated: i64) -> SharedEntry {
        SharedEntry {
            message_key: key.into(),
            assignee_email: Some(assignee.into()),
            priority: Some(1),
            work_status: Some("new".into()),
            reminder_due_utc: None,
            reminder_note: None,
            updated_utc: updated,
            updated_by: "someone@example.com".into(),
        }
    }

    #[test]
    fn newer_local_changes_overwrite_the_file() {
        let remote = vec![entry("<a@x>", "old@example.com", 100)];
        let local = vec![entry("<a@x>", "new@example.com", 200)];
        let merged = merge_for_write(&remote, &local);
        assert_eq!(merged.len(), 1);
        assert_eq!(
            merged[0].assignee_email.as_deref(),
            Some("new@example.com")
        );
    }

    #[test]
    fn older_local_changes_do_not_clobber_a_teammate() {
        // The whole point of resolving on the timestamp rather than on who wrote last.
        let remote = vec![entry("<a@x>", "theirs@example.com", 500)];
        let local = vec![entry("<a@x>", "mine@example.com", 100)];
        let merged = merge_for_write(&remote, &local);
        assert_eq!(
            merged[0].assignee_email.as_deref(),
            Some("theirs@example.com")
        );
    }

    #[test]
    fn entries_only_one_side_knows_about_are_kept() {
        let remote = vec![entry("<a@x>", "a@example.com", 100)];
        let local = vec![entry("<b@x>", "b@example.com", 100)];
        let merged = merge_for_write(&remote, &local);
        assert_eq!(merged.len(), 2);
    }

    #[test]
    fn merging_is_order_independent() {
        // Three machines writing in any order must converge on the same answer.
        let a = entry("<k@x>", "a@h.com", 100);
        let b = entry("<k@x>", "b@h.com", 300);
        let c = entry("<k@x>", "c@h.com", 200);

        use std::slice::from_ref;
        let one = merge_for_write(from_ref(&a), &merge_for_write(from_ref(&b), from_ref(&c)));
        let two = merge_for_write(from_ref(&c), &merge_for_write(from_ref(&a), from_ref(&b)));
        assert_eq!(one[0].assignee_email, two[0].assignee_email);
        assert_eq!(one[0].assignee_email.as_deref(), Some("b@h.com"));
    }

    #[test]
    fn a_missing_file_reads_as_empty_rather_than_failing() {
        let path = std::env::temp_dir().join("signet-no-such-team-file.json");
        let _ = std::fs::remove_file(&path);
        let f = read_file(&path).expect("missing file should be empty, not an error");
        assert!(f.messages.is_empty());
    }

    #[test]
    fn a_corrupt_file_is_an_error_not_silent_data_loss() {
        let path = std::env::temp_dir().join(format!("signet-corrupt-{}.json", std::process::id()));
        std::fs::write(&path, b"{not json at all").unwrap();
        assert!(read_file(&path).is_err());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_written_file_reads_back_identically() {
        let path = std::env::temp_dir().join(format!("signet-rt-{}.json", std::process::id()));
        let file = SharedFile {
            version: 1,
            members: Vec::new(),
            messages: vec![entry("<a@x>", "a@example.com", 42)],
        };
        write_atomically(&path, &file).unwrap();
        let back = read_file(&path).unwrap();
        assert_eq!(back.messages.len(), 1);
        assert_eq!(back.messages[0].updated_utc, 42);
        let _ = std::fs::remove_file(&path);
    }
}
