//! Team triage: members, assignment, priority, reminders and the shared-state table.

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::{Error, Result, Store, now_utc};

/// 0 low, 1 normal, 2 high. Stored as an integer so the list can sort on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Priority {
    Low,
    Normal,
    High,
}

impl Priority {
    pub fn as_i64(self) -> i64 {
        match self {
            Priority::Low => 0,
            Priority::Normal => 1,
            Priority::High => 2,
        }
    }

    pub fn from_i64(v: i64) -> Self {
        match v {
            0 => Priority::Low,
            2 => Priority::High,
            _ => Priority::Normal,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamMember {
    #[serde(default)]
    pub id: i64,
    pub name: String,
    pub email: String,
    pub color: String,
    pub initials: String,
    pub is_active: bool,
    pub is_me: bool,
    #[serde(default)]
    pub sort_order: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reminder {
    pub id: i64,
    pub message_id: i64,
    pub due_utc: i64,
    pub note: String,
    pub done_utc: Option<i64>,
    /// Denormalised for the follow-up list, which would otherwise need a join per row.
    #[serde(default)]
    pub subject: String,
    #[serde(default)]
    pub from_addr: String,
}

/// One message's team state, as exchanged through the shared file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SharedEntry {
    pub message_key: String,
    pub assignee_email: Option<String>,
    pub priority: Option<i64>,
    pub work_status: Option<String>,
    pub reminder_due_utc: Option<i64>,
    pub reminder_note: Option<String>,
    pub updated_utc: i64,
    pub updated_by: String,
}

impl Store {
    // ---------------------------------------------------------------- settings

    pub fn setting(&self, key: &str) -> Result<Option<String>> {
        self.with_conn(|conn| {
            Ok(conn
                .query_row(
                    "SELECT value FROM settings WHERE key = ?1",
                    params![key],
                    |r| r.get(0),
                )
                .optional()?)
        })
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO settings (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, value],
            )?;
            Ok(())
        })
    }

    // ---------------------------------------------------------------- team

    pub fn list_team(&self) -> Result<Vec<TeamMember>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, name, email, color, initials, is_active, is_me, sort_order
                 FROM team_members ORDER BY sort_order, name",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok(TeamMember {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    email: r.get(2)?,
                    color: r.get(3)?,
                    initials: r.get(4)?,
                    is_active: r.get::<_, i64>(5)? != 0,
                    is_me: r.get::<_, i64>(6)? != 0,
                    sort_order: r.get(7)?,
                })
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    /// Insert or update by email, which is the identity that travels in the shared file.
    pub fn upsert_team_member(&self, m: &TeamMember) -> Result<i64> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO team_members (name, email, color, initials, is_active, is_me, sort_order)
                 VALUES (?1,?2,?3,?4,?5,?6,?7)
                 ON CONFLICT(email) DO UPDATE SET
                     name = excluded.name, color = excluded.color, initials = excluded.initials,
                     is_active = excluded.is_active, sort_order = excluded.sort_order",
                params![
                    m.name,
                    m.email.trim().to_lowercase(),
                    m.color,
                    m.initials,
                    m.is_active as i64,
                    m.is_me as i64,
                    m.sort_order
                ],
            )?;
            Ok(conn.query_row(
                "SELECT id FROM team_members WHERE email = ?1",
                params![m.email.trim().to_lowercase()],
                |r| r.get(0),
            )?)
        })
    }

    pub fn delete_team_member(&self, id: i64) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute("DELETE FROM team_members WHERE id = ?1", params![id])?;
            Ok(())
        })
    }

    /// Exactly one member can be "me"; setting one clears the others.
    pub fn set_me(&self, id: i64) -> Result<()> {
        self.with_tx(|tx| {
            tx.execute("UPDATE team_members SET is_me = 0", [])?;
            tx.execute(
                "UPDATE team_members SET is_me = 1 WHERE id = ?1",
                params![id],
            )?;
            Ok(())
        })
    }

    pub fn me(&self) -> Result<Option<TeamMember>> {
        Ok(self.list_team()?.into_iter().find(|m| m.is_me))
    }

    pub fn team_member_by_email(&self, email: &str) -> Result<Option<TeamMember>> {
        let email = email.trim().to_lowercase();
        Ok(self
            .list_team()?
            .into_iter()
            .find(|m| m.email.eq_ignore_ascii_case(&email)))
    }

    // ---------------------------------------------------------------- triage

    pub fn set_priority(&self, ids: &[i64], priority: Priority) -> Result<()> {
        self.with_tx(|tx| {
            let mut stmt = tx.prepare("UPDATE messages SET priority = ?2 WHERE id = ?1")?;
            for id in ids {
                stmt.execute(params![id, priority.as_i64()])?;
            }
            Ok(())
        })
    }

    pub fn set_assignee(&self, ids: &[i64], assignee_id: Option<i64>) -> Result<()> {
        self.with_tx(|tx| {
            let mut stmt = tx.prepare("UPDATE messages SET assignee_id = ?2 WHERE id = ?1")?;
            for id in ids {
                stmt.execute(params![id, assignee_id])?;
            }
            Ok(())
        })
    }

    pub fn set_work_status(&self, ids: &[i64], status: &str) -> Result<()> {
        self.with_tx(|tx| {
            let mut stmt = tx.prepare("UPDATE messages SET work_status = ?2 WHERE id = ?1")?;
            for id in ids {
                stmt.execute(params![id, status])?;
            }
            Ok(())
        })
    }

    /// The `Message-ID` used as the shared key, or `None` when the message has none.
    pub fn message_key(&self, message_id: i64) -> Result<Option<String>> {
        self.with_conn(|conn| {
            Ok(conn
                .query_row(
                    "SELECT rfc822_message_id FROM messages WHERE id = ?1",
                    params![message_id],
                    |r| r.get::<_, Option<String>>(0),
                )
                .optional()?
                .flatten()
                .filter(|k| !k.trim().is_empty()))
        })
    }

    pub fn message_id_for_key(&self, key: &str) -> Result<Option<i64>> {
        self.with_conn(|conn| {
            Ok(conn
                .query_row(
                    "SELECT id FROM messages WHERE rfc822_message_id = ?1 LIMIT 1",
                    params![key],
                    |r| r.get(0),
                )
                .optional()?)
        })
    }

    // ---------------------------------------------------------------- reminders

    /// Create or replace the open reminder for a message.
    pub fn set_reminder(&self, message_id: i64, due_utc: i64, note: &str) -> Result<i64> {
        self.with_tx(|tx| {
            tx.execute(
                "DELETE FROM reminders WHERE message_id = ?1 AND done_utc IS NULL",
                params![message_id],
            )?;
            tx.execute(
                "INSERT INTO reminders (message_id, due_utc, note, created_utc)
                 VALUES (?1,?2,?3,?4)",
                params![message_id, due_utc, note, now_utc()],
            )?;
            Ok(tx.last_insert_rowid())
        })
    }

    pub fn complete_reminder(&self, message_id: i64) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE reminders SET done_utc = ?2 WHERE message_id = ?1 AND done_utc IS NULL",
                params![message_id, now_utc()],
            )?;
            Ok(())
        })
    }

    pub fn clear_reminder(&self, message_id: i64) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "DELETE FROM reminders WHERE message_id = ?1 AND done_utc IS NULL",
                params![message_id],
            )?;
            Ok(())
        })
    }

    pub fn reminder_for(&self, message_id: i64) -> Result<Option<Reminder>> {
        self.with_conn(|conn| {
            Ok(conn
                .query_row(
                    "SELECT id, message_id, due_utc, note, done_utc FROM reminders
                     WHERE message_id = ?1 AND done_utc IS NULL",
                    params![message_id],
                    |r| {
                        Ok(Reminder {
                            id: r.get(0)?,
                            message_id: r.get(1)?,
                            due_utc: r.get(2)?,
                            note: r.get(3)?,
                            done_utc: r.get(4)?,
                            subject: String::new(),
                            from_addr: String::new(),
                        })
                    },
                )
                .optional()?)
        })
    }

    /// Open reminders, soonest first.
    pub fn open_reminders(&self) -> Result<Vec<Reminder>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT r.id, r.message_id, r.due_utc, r.note, r.done_utc, m.subject, m.from_addr
                 FROM reminders r JOIN messages m ON m.id = r.message_id
                 WHERE r.done_utc IS NULL ORDER BY r.due_utc",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok(Reminder {
                    id: r.get(0)?,
                    message_id: r.get(1)?,
                    due_utc: r.get(2)?,
                    note: r.get(3)?,
                    done_utc: r.get(4)?,
                    subject: r.get(5)?,
                    from_addr: r.get(6)?,
                })
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    /// Reminders that are due and have not been announced yet.
    pub fn reminders_to_announce(&self, now: i64) -> Result<Vec<Reminder>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT r.id, r.message_id, r.due_utc, r.note, r.done_utc, m.subject, m.from_addr
                 FROM reminders r JOIN messages m ON m.id = r.message_id
                 WHERE r.done_utc IS NULL AND r.notified_utc IS NULL AND r.due_utc <= ?1
                 ORDER BY r.due_utc",
            )?;
            let rows = stmt.query_map(params![now], |r| {
                Ok(Reminder {
                    id: r.get(0)?,
                    message_id: r.get(1)?,
                    due_utc: r.get(2)?,
                    note: r.get(3)?,
                    done_utc: r.get(4)?,
                    subject: r.get(5)?,
                    from_addr: r.get(6)?,
                })
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    pub fn mark_reminder_announced(&self, ids: &[i64]) -> Result<()> {
        self.with_tx(|tx| {
            let mut stmt = tx.prepare("UPDATE reminders SET notified_utc = ?2 WHERE id = ?1")?;
            for id in ids {
                stmt.execute(params![id, now_utc()])?;
            }
            Ok(())
        })
    }

    // ---------------------------------------------------------------- shared state

    /// Record a local change for publication to the team.
    pub fn stage_shared(&self, entry: &SharedEntry) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO shared_state (message_key, assignee_email, priority, work_status,
                     reminder_due_utc, reminder_note, updated_utc, updated_by, dirty)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,1)
                 ON CONFLICT(message_key) DO UPDATE SET
                     assignee_email = excluded.assignee_email,
                     priority = excluded.priority,
                     work_status = excluded.work_status,
                     reminder_due_utc = excluded.reminder_due_utc,
                     reminder_note = excluded.reminder_note,
                     updated_utc = excluded.updated_utc,
                     updated_by = excluded.updated_by,
                     dirty = 1",
                params![
                    entry.message_key,
                    entry.assignee_email,
                    entry.priority,
                    entry.work_status,
                    entry.reminder_due_utc,
                    entry.reminder_note,
                    entry.updated_utc,
                    entry.updated_by,
                ],
            )?;
            Ok(())
        })
    }

    /// Apply an entry that arrived from the shared file.
    ///
    /// Older entries are ignored, which is what makes the merge converge no matter what order the
    /// team's machines write in.
    pub fn merge_shared(&self, entry: &SharedEntry) -> Result<bool> {
        self.with_conn(|conn| {
            let existing: Option<i64> = conn
                .query_row(
                    "SELECT updated_utc FROM shared_state WHERE message_key = ?1",
                    params![entry.message_key],
                    |r| r.get(0),
                )
                .optional()?;

            if existing.is_some_and(|prev| prev >= entry.updated_utc) {
                return Ok(false);
            }

            conn.execute(
                "INSERT INTO shared_state (message_key, assignee_email, priority, work_status,
                     reminder_due_utc, reminder_note, updated_utc, updated_by, dirty)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,0)
                 ON CONFLICT(message_key) DO UPDATE SET
                     assignee_email = excluded.assignee_email,
                     priority = excluded.priority,
                     work_status = excluded.work_status,
                     reminder_due_utc = excluded.reminder_due_utc,
                     reminder_note = excluded.reminder_note,
                     updated_utc = excluded.updated_utc,
                     updated_by = excluded.updated_by,
                     dirty = 0",
                params![
                    entry.message_key,
                    entry.assignee_email,
                    entry.priority,
                    entry.work_status,
                    entry.reminder_due_utc,
                    entry.reminder_note,
                    entry.updated_utc,
                    entry.updated_by,
                ],
            )?;
            Ok(true)
        })
    }

    pub fn all_shared(&self) -> Result<Vec<SharedEntry>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT message_key, assignee_email, priority, work_status, reminder_due_utc,
                        reminder_note, updated_utc, updated_by FROM shared_state",
            )?;
            let rows = stmt.query_map([], row_to_shared)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    pub fn shared_for(&self, message_key: &str) -> Result<Option<SharedEntry>> {
        self.with_conn(|conn| {
            Ok(conn
                .query_row(
                    "SELECT message_key, assignee_email, priority, work_status, reminder_due_utc,
                            reminder_note, updated_utc, updated_by
                     FROM shared_state WHERE message_key = ?1",
                    params![message_key],
                    row_to_shared,
                )
                .optional()?)
        })
    }

    /// Shared entries whose state has not yet been written onto the local message.
    ///
    /// Includes entries for messages this machine has never downloaded — those simply do nothing
    /// until the message arrives, which is exactly the case this exists to handle.
    pub fn unapplied_shared(&self) -> Result<Vec<SharedEntry>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT message_key, assignee_email, priority, work_status, reminder_due_utc,
                        reminder_note, updated_utc, updated_by
                 FROM shared_state
                 WHERE applied_utc IS NULL OR applied_utc < updated_utc",
            )?;
            let rows = stmt.query_map([], row_to_shared)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    pub fn mark_shared_applied(&self, message_key: &str, updated_utc: i64) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE shared_state SET applied_utc = ?2 WHERE message_key = ?1",
                params![message_key, updated_utc],
            )?;
            Ok(())
        })
    }

    pub fn clear_dirty(&self) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute("UPDATE shared_state SET dirty = 0", [])?;
            Ok(())
        })
    }

    /// Local triage fields for one message, for the UI.
    pub fn triage(&self, message_id: i64) -> Result<(Priority, Option<i64>, String)> {
        self.with_conn(|conn| {
            conn.query_row(
                "SELECT priority, assignee_id, work_status FROM messages WHERE id = ?1",
                params![message_id],
                |r| {
                    Ok((
                        Priority::from_i64(r.get(0)?),
                        r.get::<_, Option<i64>>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?
            .ok_or(Error::NotFound {
                kind: "message",
                id: message_id,
            })
        })
    }
}

fn row_to_shared(r: &rusqlite::Row<'_>) -> rusqlite::Result<SharedEntry> {
    Ok(SharedEntry {
        message_key: r.get(0)?,
        assignee_email: r.get(1)?,
        priority: r.get(2)?,
        work_status: r.get(3)?,
        reminder_due_utc: r.get(4)?,
        reminder_note: r.get(5)?,
        updated_utc: r.get(6)?,
        updated_by: r.get(7)?,
    })
}

/// A distinct colour per team member, cycled in a fixed order.
///
/// Fixed rather than random so the same person tends to get the same colour on every machine even
/// before the shared file has synced.
pub fn palette(index: usize) -> &'static str {
    const COLORS: [&str; 10] = [
        "#2563eb", "#dc2626", "#059669", "#d97706", "#7c3aed", "#0891b2", "#db2777", "#65a30d",
        "#ea580c", "#4f46e5",
    ];
    COLORS[index % COLORS.len()]
}

/// Initials for the chip: first letters of the first two words, else the first two characters.
pub fn initials_for(name: &str) -> String {
    let words: Vec<&str> = name.split_whitespace().filter(|w| !w.is_empty()).collect();
    let out: String = match words.len() {
        0 => return "?".to_string(),
        1 => words[0].chars().take(2).collect(),
        _ => words
            .iter()
            .take(2)
            .filter_map(|w| w.chars().next())
            .collect(),
    };
    out.to_uppercase()
}

/// Folder bookkeeping for server-side folders (IMAP).
///
/// POP3 has none of this: its single folder is invented by us at account creation. IMAP folders
/// belong to the server, so they have to be discovered, mapped to a local row, and tracked with
/// the UIDVALIDITY they were synced under.
impl Store {
    /// Find or create the local folder for a server path.
    ///
    /// `kind` is the caller's best guess from the folder's name or its SPECIAL-USE attribute.
    pub fn folder_for_remote(
        &self,
        account_id: i64,
        remote_path: &str,
        display_name: &str,
        kind: crate::FolderKind,
    ) -> Result<i64> {
        if let Some(id) = self.with_conn(|conn| {
            Ok(conn
                .query_row(
                    "SELECT id FROM folders WHERE account_id = ?1 AND remote_path = ?2",
                    params![account_id, remote_path],
                    |r| r.get::<_, i64>(0),
                )
                .optional()?)
        })? {
            return Ok(id);
        }

        // Adopt the folder we already created locally for this role rather than making a second
        // "Sent" that differs only by having a remote path.
        if kind != crate::FolderKind::Custom {
            if let Some(id) = self.with_conn(|conn| {
                Ok(conn
                    .query_row(
                        "SELECT id FROM folders
                         WHERE account_id = ?1 AND kind = ?2 AND remote_path IS NULL",
                        params![account_id, kind.as_str()],
                        |r| r.get::<_, i64>(0),
                    )
                    .optional()?)
            })? {
                self.with_conn(|conn| {
                    conn.execute(
                        "UPDATE folders SET remote_path = ?2 WHERE id = ?1",
                        params![id, remote_path],
                    )?;
                    Ok(())
                })?;
                return Ok(id);
            }
        }

        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO folders (account_id, kind, name, remote_path, sort_order)
                 VALUES (?1,?2,?3,?4,100)",
                params![account_id, kind.as_str(), display_name, remote_path],
            )?;
            Ok(conn.last_insert_rowid())
        })
    }

    pub fn remote_path_of(&self, folder_id: i64) -> Result<Option<String>> {
        self.with_conn(|conn| {
            Ok(conn
                .query_row(
                    "SELECT remote_path FROM folders WHERE id = ?1",
                    params![folder_id],
                    |r| r.get::<_, Option<String>>(0),
                )
                .optional()?
                .flatten())
        })
    }

    pub fn uid_validity(&self, folder_id: i64) -> Result<Option<i64>> {
        self.with_conn(|conn| {
            Ok(conn
                .query_row(
                    "SELECT uid_validity FROM folders WHERE id = ?1",
                    params![folder_id],
                    |r| r.get::<_, Option<i64>>(0),
                )
                .optional()?
                .flatten())
        })
    }

    /// Record the UIDVALIDITY a folder was synced under.
    ///
    /// When it changes, every UID we hold for that folder is meaningless. [`Store::reset_folder_uids`]
    /// is what the caller must then do.
    pub fn set_uid_validity(&self, folder_id: i64, value: i64) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE folders SET uid_validity = ?2 WHERE id = ?1",
                params![folder_id, value],
            )?;
            Ok(())
        })
    }

    /// Forget every remote id for a folder, after the server invalidated them.
    ///
    /// The messages themselves are kept — losing mail because a server bumped a counter would be
    /// far worse than re-downloading it. They are simply no longer matched to server UIDs.
    pub fn reset_folder_uids(&self, account_id: i64, remote_path: &str) -> Result<()> {
        self.with_tx(|tx| {
            tx.execute(
                "DELETE FROM remote_seen WHERE account_id = ?1 AND folder = ?2",
                params![account_id, remote_path],
            )?;
            tx.execute(
                "UPDATE messages SET remote_uid = NULL
                 WHERE account_id = ?1 AND remote_folder = ?2",
                params![account_id, remote_path],
            )?;
            Ok(())
        })
    }

    /// The local message a remote id was ingested as, if any.
    pub fn seen_message_id(
        &self,
        account_id: i64,
        folder: &str,
        remote_id: &str,
    ) -> Result<Option<i64>> {
        self.with_conn(|conn| {
            Ok(conn
                .query_row(
                    "SELECT message_id FROM remote_seen
                     WHERE account_id = ?1 AND folder = ?2 AND remote_id = ?3",
                    params![account_id, folder, remote_id],
                    |r| r.get::<_, Option<i64>>(0),
                )
                .optional()?
                .flatten())
        })
    }

    pub fn set_remote_ref(&self, message_id: i64, folder: &str, uid: i64) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE messages SET remote_folder = ?2, remote_uid = ?3 WHERE id = ?1",
                params![message_id, folder, uid],
            )?;
            Ok(())
        })
    }

    /// The server-side identity of a message, for pushing a flag change back.
    pub fn remote_ref(&self, message_id: i64) -> Result<Option<(String, i64)>> {
        self.with_conn(|conn| {
            Ok(conn
                .query_row(
                    "SELECT remote_folder, remote_uid FROM messages WHERE id = ?1",
                    params![message_id],
                    |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, Option<i64>>(1)?)),
                )
                .optional()?
                .and_then(|(f, u)| Some((f?, u?))))
        })
    }

    /// Messages in a folder whose flags changed locally and have not reached the server.
    pub fn dirty_flag_messages(
        &self,
        account_id: i64,
        remote_folder: &str,
    ) -> Result<Vec<(i64, i64)>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, remote_uid FROM messages
                 WHERE account_id = ?1 AND remote_folder = ?2
                   AND flags_dirty = 1 AND remote_uid IS NOT NULL",
            )?;
            let rows = stmt.query_map(params![account_id, remote_folder], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    pub fn clear_flags_dirty(&self, message_ids: &[i64]) -> Result<()> {
        if message_ids.is_empty() {
            return Ok(());
        }
        self.with_tx(|tx| {
            let mut stmt = tx.prepare("UPDATE messages SET flags_dirty = 0 WHERE id = ?1")?;
            for id in message_ids {
                stmt.execute(params![id])?;
            }
            Ok(())
        })
    }

    /// Set flags without marking the row for push.
    ///
    /// Used when applying what the server just told us: echoing it straight back would be a
    /// pointless round trip, and worse, would fight a colleague's concurrent change.
    pub fn set_flags_from_server(
        &self,
        message_id: i64,
        read: bool,
        flagged: bool,
        answered: bool,
    ) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE messages SET is_read = ?2, is_flagged = ?3, is_answered = ?4,
                     flags_dirty = 0
                 WHERE id = ?1 AND flags_dirty = 0",
                params![message_id, read as i64, flagged as i64, answered as i64],
            )?;
            Ok(())
        })
    }

    /// Local read/flagged state for a set of messages, so a sync can push what changed.
    pub fn flags_of(&self, message_id: i64) -> Result<(bool, bool, bool)> {
        self.with_conn(|conn| {
            Ok(conn.query_row(
                "SELECT is_read, is_flagged, is_answered FROM messages WHERE id = ?1",
                params![message_id],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)? != 0,
                        r.get::<_, i64>(1)? != 0,
                        r.get::<_, i64>(2)? != 0,
                    ))
                },
            )?)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initials_handle_the_shapes_names_actually_take() {
        assert_eq!(initials_for("Max Rahman"), "MR");
        assert_eq!(initials_for("sarah"), "SA");
        assert_eq!(initials_for("Jean-Luc Picard"), "JP");
        assert_eq!(initials_for(""), "?");
        assert_eq!(initials_for("   "), "?");
    }

    #[test]
    fn the_palette_is_stable_and_wraps() {
        assert_eq!(palette(0), palette(10));
        assert_ne!(palette(0), palette(1));
    }

    #[test]
    fn priority_round_trips_through_the_database_representation() {
        for p in [Priority::Low, Priority::Normal, Priority::High] {
            assert_eq!(Priority::from_i64(p.as_i64()), p);
        }
        // Anything unexpected reads as normal rather than panicking.
        assert_eq!(Priority::from_i64(99), Priority::Normal);
    }
}

/// A named signature.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Signature {
    #[serde(default)]
    pub id: i64,
    pub name: String,
    pub body: String,
    pub is_default: bool,
    #[serde(default)]
    pub sort_order: i64,
}

impl Store {
    pub fn list_signatures(&self) -> Result<Vec<Signature>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, name, body, is_default, sort_order
                 FROM signatures ORDER BY is_default DESC, sort_order, name",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok(Signature {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    body: r.get(2)?,
                    is_default: r.get::<_, i64>(3)? != 0,
                    sort_order: r.get(4)?,
                })
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    pub fn default_signature(&self) -> Result<Option<Signature>> {
        Ok(self.list_signatures()?.into_iter().find(|s| s.is_default))
    }

    /// Insert or update. `id == 0` inserts.
    pub fn save_signature(&self, s: &Signature) -> Result<i64> {
        let id = self.with_conn(|conn| {
            if s.id > 0 {
                conn.execute(
                    "UPDATE signatures SET name = ?2, body = ?3, sort_order = ?4 WHERE id = ?1",
                    params![s.id, s.name, s.body, s.sort_order],
                )?;
                Ok(s.id)
            } else {
                conn.execute(
                    "INSERT INTO signatures (name, body, is_default, sort_order, created_utc)
                     VALUES (?1,?2,0,?3,?4)",
                    params![s.name, s.body, s.sort_order, now_utc()],
                )?;
                Ok(conn.last_insert_rowid())
            }
        })?;

        if s.is_default {
            self.set_default_signature(id)?;
        }
        Ok(id)
    }

    /// Exactly one signature is the default; setting one clears the rest.
    pub fn set_default_signature(&self, id: i64) -> Result<()> {
        self.with_tx(|tx| {
            tx.execute("UPDATE signatures SET is_default = 0", [])?;
            tx.execute(
                "UPDATE signatures SET is_default = 1 WHERE id = ?1",
                params![id],
            )?;
            Ok(())
        })
    }

    pub fn delete_signature(&self, id: i64) -> Result<()> {
        self.with_tx(|tx| {
            let was_default: bool = tx
                .query_row(
                    "SELECT is_default FROM signatures WHERE id = ?1",
                    params![id],
                    |r| r.get::<_, i64>(0),
                )
                .optional()?
                .is_some_and(|v| v != 0);

            tx.execute("DELETE FROM signatures WHERE id = ?1", params![id])?;

            // Deleting the default must not leave the list with none, or compose would silently
            // stop inserting anything.
            if was_default {
                tx.execute(
                    "UPDATE signatures SET is_default = 1
                     WHERE id = (SELECT MIN(id) FROM signatures)",
                    [],
                )?;
            }
            Ok(())
        })
    }
}

/// An explicit decision about a sender, overriding the statistical filter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SenderRule {
    /// A full address, or a bare domain to cover everyone at it.
    pub pattern: String,
    /// `allow` or `block`.
    pub kind: String,
}

impl Store {
    pub fn list_sender_rules(&self) -> Result<Vec<SenderRule>> {
        self.with_conn(|conn| {
            let mut stmt =
                conn.prepare("SELECT pattern, kind FROM sender_rules ORDER BY kind, pattern")?;
            let rows = stmt.query_map([], |r| {
                Ok(SenderRule {
                    pattern: r.get(0)?,
                    kind: r.get(1)?,
                })
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    pub fn set_sender_rule(&self, pattern: &str, kind: &str) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO sender_rules (pattern, kind, created_utc) VALUES (?1,?2,?3)
                 ON CONFLICT(pattern) DO UPDATE SET kind = excluded.kind",
                params![pattern.trim().to_lowercase(), kind, now_utc()],
            )?;
            Ok(())
        })
    }

    pub fn remove_sender_rule(&self, pattern: &str) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "DELETE FROM sender_rules WHERE pattern = ?1",
                params![pattern.trim().to_lowercase()],
            )?;
            Ok(())
        })
    }

    /// The rule covering an address, if any.
    ///
    /// The full address wins over the domain, so one blocked colleague at an otherwise-allowed
    /// company behaves as you would expect.
    pub fn sender_rule_for(&self, addr: &str) -> Result<Option<String>> {
        let addr = addr.trim().to_lowercase();
        let domain = addr.rsplit('@').next().unwrap_or("").to_string();

        self.with_conn(|conn| {
            if let Some(kind) = conn
                .query_row(
                    "SELECT kind FROM sender_rules WHERE pattern = ?1",
                    params![addr],
                    |r| r.get::<_, String>(0),
                )
                .optional()?
            {
                return Ok(Some(kind));
            }
            if domain.is_empty() {
                return Ok(None);
            }
            Ok(conn
                .query_row(
                    "SELECT kind FROM sender_rules WHERE pattern = ?1",
                    params![domain],
                    |r| r.get::<_, String>(0),
                )
                .optional()?)
        })
    }

    /// Whether we have corresponded with this address before.
    ///
    /// Someone you have written to is almost never a spammer, and this is the single most
    /// effective guard against filing a real customer's mail. Seeing them repeatedly in incoming
    /// mail counts for less than having replied, but still counts.
    pub fn is_known_correspondent(&self, addr: &str) -> Result<bool> {
        let addr = addr.trim().to_lowercase();
        if addr.is_empty() {
            return Ok(false);
        }
        self.with_conn(|conn| {
            let written_to: i64 = conn.query_row(
                "SELECT COUNT(*) FROM recipients r
                 JOIN messages m ON m.id = r.message_id
                 JOIN folders f ON f.id = m.folder_id
                 WHERE f.kind = 'sent' AND LOWER(r.addr) = ?1",
                params![addr],
                |r| r.get(0),
            )?;
            if written_to > 0 {
                return Ok(true);
            }
            let seen: i64 = conn
                .query_row(
                    "SELECT times_seen FROM contacts WHERE LOWER(addr) = ?1",
                    params![addr],
                    |r| r.get(0),
                )
                .optional()?
                .unwrap_or(0);
            Ok(seen >= 3)
        })
    }

    pub fn set_spam_reasons(&self, message_id: i64, reasons: &str) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE messages SET spam_reasons = ?2 WHERE id = ?1",
                params![message_id, reasons],
            )?;
            Ok(())
        })
    }

    pub fn spam_reasons(&self, message_id: i64) -> Result<String> {
        self.with_conn(|conn| {
            Ok(conn
                .query_row(
                    "SELECT spam_reasons FROM messages WHERE id = ?1",
                    params![message_id],
                    |r| r.get(0),
                )
                .optional()?
                .unwrap_or_default())
        })
    }
}
