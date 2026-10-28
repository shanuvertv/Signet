//! SQLite store: schema, migrations, the FTS5 index and the attachment blob store.
//!
//! Every query is account-scoped (ADR 0001 / D4) — the unified inbox is a view across accounts,
//! not a special case bolted on later.
//!
//! `rusqlite`'s `bundled` feature compiles SQLite from source rather than linking the system
//! library, which is what makes the Android cross-build tractable.
//!
//! Calls here are synchronous. SQLite on a local file is measured in microseconds, and wrapping
//! every read in `spawn_blocking` would cost more than it saves; the writer is serialised behind a
//! mutex so the UI can never see a half-applied ingest.

pub mod contacts;
pub mod extras;
pub mod imap_ops;
pub mod models;
pub mod portable;
pub mod scheduled;
pub mod team;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OptionalExtension, Row, params};
use thiserror::Error;

pub use contacts::{Contact, HarvestedDetails};
pub use extras::{StoredDraft, TokenCounts};
pub use imap_ops::{ImapOp, MAX_OP_ATTEMPTS, PendingImapOp};
pub use models::Protocol;
pub use models::*;
pub use portable::PortableMessage;
pub use scheduled::OutboxItem;
pub use team::{
    Priority, Reminder, SenderRule, SharedEntry, Signature, TeamMember, initials_for, palette,
};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("no such {kind}: {id}")]
    NotFound { kind: &'static str, id: i64 },

    #[error("the store lock was poisoned by a panic in another thread")]
    Poisoned,
}

const MIGRATIONS: &[(i64, &str)] = &[
    (1, include_str!("../migrations/0001_initial.sql")),
    (2, include_str!("../migrations/0002_rules_junk_drafts.sql")),
    (3, include_str!("../migrations/0003_team.sql")),
    (4, include_str!("../migrations/0004_shared_applied.sql")),
    (5, include_str!("../migrations/0005_imap.sql")),
    (6, include_str!("../migrations/0006_flag_push.sql")),
    (7, include_str!("../migrations/0007_signatures.sql")),
    (8, include_str!("../migrations/0008_sender_rules.sql")),
    (9, include_str!("../migrations/0009_scheduled_send.sql")),
    (10, include_str!("../migrations/0010_contact_details.sql")),
    (11, include_str!("../migrations/0011_draft_rich.sql")),
    (12, include_str!("../migrations/0012_imap_ops.sql")),
];

/// Handle to the mail database and its blob directory.
#[derive(Debug, Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
    data_dir: PathBuf,
}

impl Store {
    /// Open (creating if needed) the database under `data_dir`.
    pub fn open(data_dir: impl AsRef<Path>) -> Result<Self> {
        let data_dir = data_dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&data_dir)?;
        let conn = Connection::open(data_dir.join("mail.sqlite"))?;
        Self::init(conn, data_dir)
    }

    /// An in-memory store, for tests.
    pub fn open_in_memory() -> Result<Self> {
        let dir = std::env::temp_dir().join(format!("signet-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        Self::init(Connection::open_in_memory()?, dir)
    }

    fn init(conn: Connection, data_dir: PathBuf) -> Result<Self> {
        // WAL keeps the UI reading while a sync writes. `foreign_keys` is off by default in
        // SQLite and our ON DELETE CASCADE rules are load-bearing.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;

        let store = Self {
            conn: Arc::new(Mutex::new(conn)),
            data_dir,
        };
        store.migrate()?;
        Ok(store)
    }

    pub(crate) fn with_conn<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let guard = self.conn.lock().map_err(|_| Error::Poisoned)?;
        f(&guard)
    }

    pub(crate) fn with_tx<T>(
        &self,
        f: impl FnOnce(&rusqlite::Transaction<'_>) -> Result<T>,
    ) -> Result<T> {
        let mut guard = self.conn.lock().map_err(|_| Error::Poisoned)?;
        let tx = guard.transaction()?;
        let out = f(&tx)?;
        tx.commit()?;
        Ok(out)
    }

    fn migrate(&self) -> Result<()> {
        self.with_conn(|conn| {
            let current: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
            for (version, sql) in MIGRATIONS {
                if *version > current {
                    tracing::info!(version, "applying migration");
                    // One transaction per migration, version bump included. A crash halfway used to
                    // leave some columns added and the version unchanged, and the next start then
                    // failed on "duplicate column" and the app would not open.
                    let tx = conn.unchecked_transaction()?;
                    tx.execute_batch(sql)?;
                    tx.pragma_update(None, "user_version", version)?;
                    tx.commit()?;
                }
            }
            Ok(())
        })
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }
    /// Where an account's raw messages and attachments live.
    pub fn blob_dir(&self, account_id: i64) -> PathBuf {
        self.data_dir.join("accounts").join(account_id.to_string())
    }

    // ---------------------------------------------------------------- accounts

    pub fn add_account(&self, a: &NewAccount, credential_handle: &str) -> Result<i64> {
        let now = now_utc();
        self.with_tx(|tx| {
            tx.execute(
                "INSERT INTO accounts (display_name, email, pop_host, pop_port, pop_security,
                     pop_user, smtp_host, smtp_port, smtp_security, smtp_user, credential_handle,
                     leave_on_server, delete_after_days, poll_minutes, signature, created_utc,
                     protocol)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)",
                params![
                    a.display_name,
                    a.email,
                    a.pop_host,
                    a.pop_port,
                    a.pop_security.as_str(),
                    a.pop_user,
                    a.smtp_host,
                    a.smtp_port,
                    a.smtp_security.as_str(),
                    a.smtp_user,
                    credential_handle,
                    a.leave_on_server as i64,
                    a.delete_after_days,
                    a.poll_minutes,
                    a.signature,
                    now,
                    a.protocol.as_str(),
                ],
            )?;
            let id = tx.last_insert_rowid();

            // POP3 has no folders, so we create the whole set ourselves (PLAN.md §2).
            for (order, (kind, name)) in FolderKind::defaults().into_iter().enumerate() {
                tx.execute(
                    "INSERT INTO folders (account_id, kind, name, sort_order)
                     VALUES (?1,?2,?3,?4)",
                    params![id, kind.as_str(), name, order as i64],
                )?;
            }
            Ok(id)
        })
    }

    pub fn list_accounts(&self) -> Result<Vec<Account>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(ACCOUNT_COLUMNS)?;
            let rows = stmt.query_map([], row_to_account)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    pub fn get_account(&self, id: i64) -> Result<Account> {
        self.with_conn(|conn| {
            conn.query_row(
                &format!("{ACCOUNT_COLUMNS} WHERE a.id = ?1"),
                params![id],
                row_to_account,
            )
            .optional()?
            .ok_or(Error::NotFound {
                kind: "account",
                id,
            })
        })
    }

    pub fn delete_account(&self, id: i64) -> Result<()> {
        self.with_tx(|tx| {
            // FTS rows are not reachable by cascade; drop them before the messages go.
            tx.execute(
                "DELETE FROM messages_fts WHERE rowid IN
                     (SELECT id FROM messages WHERE account_id = ?1)",
                params![id],
            )?;
            tx.execute("DELETE FROM accounts WHERE id = ?1", params![id])?;
            Ok(())
        })?;
        let dir = self.blob_dir(id);
        if dir.exists() {
            std::fs::remove_dir_all(dir)?;
        }
        Ok(())
    }

    pub fn touch_account_sync(&self, id: i64) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE accounts SET last_sync_utc = ?2 WHERE id = ?1",
                params![id, now_utc()],
            )?;
            Ok(())
        })
    }

    // ---------------------------------------------------------------- folders

    pub fn list_folders(&self, account_id: Option<i64>) -> Result<Vec<Folder>> {
        self.with_conn(|conn| {
            let sql = "SELECT f.id, f.account_id, f.kind, f.name, f.sort_order,
                              (SELECT COUNT(*) FROM messages m WHERE m.folder_id = f.id),
                              (SELECT COUNT(*) FROM messages m WHERE m.folder_id = f.id AND m.is_read = 0)
                       FROM folders f
                       WHERE (?1 IS NULL OR f.account_id = ?1)
                       ORDER BY f.account_id, f.sort_order";
            let mut stmt = conn.prepare(sql)?;
            let rows = stmt.query_map(params![account_id], |r| {
                Ok(Folder {
                    id: r.get(0)?,
                    account_id: r.get(1)?,
                    kind: FolderKind::parse(&r.get::<_, String>(2)?),
                    name: r.get(3)?,
                    sort_order: r.get(4)?,
                    total: r.get(5)?,
                    unread: r.get(6)?,
                })
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    pub fn folder_id(&self, account_id: i64, kind: FolderKind) -> Result<i64> {
        self.with_conn(|conn| {
            conn.query_row(
                "SELECT id FROM folders WHERE account_id = ?1 AND kind = ?2",
                params![account_id, kind.as_str()],
                |r| r.get(0),
            )
            .optional()?
            .ok_or(Error::NotFound {
                kind: "folder",
                id: account_id,
            })
        })
    }

    // ---------------------------------------------------------------- messages

    /// Insert a fully parsed message and index it for search.
    #[allow(clippy::too_many_arguments)]
    pub fn insert_message(&self, m: &NewMessage) -> Result<i64> {
        self.with_tx(|tx| {
            tx.execute(
                "INSERT INTO messages (account_id, folder_id, rfc822_message_id, in_reply_to,
                     thread_id, from_name, from_addr, subject, date_utc, received_utc,
                     has_attachments, has_remote_content, size_bytes, preview_text, body_text,
                     body_html, raw_path)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)",
                params![
                    m.account_id,
                    m.folder_id,
                    m.rfc822_message_id,
                    m.in_reply_to,
                    m.thread_id,
                    m.from_name,
                    m.from_addr,
                    m.subject,
                    m.date_utc,
                    m.received_utc,
                    m.has_attachments as i64,
                    m.has_remote_content as i64,
                    m.size_bytes,
                    m.preview_text,
                    m.body_text,
                    m.body_html,
                    m.raw_path,
                ],
            )?;
            let id = tx.last_insert_rowid();

            for r in &m.recipients {
                tx.execute(
                    "INSERT INTO recipients (message_id, kind, name, addr) VALUES (?1,?2,?3,?4)",
                    params![id, r.kind, r.name, r.addr],
                )?;
            }

            for att in &m.attachments {
                tx.execute(
                    "INSERT INTO attachments (message_id, filename, mime_type, size_bytes,
                         content_id, blob_path, is_inline)
                     VALUES (?1,?2,?3,?4,?5,?6,?7)",
                    params![
                        id,
                        att.filename,
                        att.mime_type,
                        att.size_bytes,
                        att.content_id,
                        att.blob_path,
                        att.is_inline as i64
                    ],
                )?;
            }

            let recipients_text = m
                .recipients
                .iter()
                .map(|r| format!("{} {}", r.name, r.addr))
                .collect::<Vec<_>>()
                .join(" ");
            tx.execute(
                "INSERT INTO messages_fts (rowid, subject, sender, recipients, body)
                 VALUES (?1,?2,?3,?4,?5)",
                params![
                    id,
                    m.subject,
                    format!("{} {}", m.from_name, m.from_addr),
                    recipients_text,
                    m.body_text,
                ],
            )?;

            // Harvest the sender for compose autocomplete.
            if !m.from_addr.is_empty() {
                tx.execute(
                    "INSERT INTO contacts (display_name, addr, last_seen_utc, times_seen)
                     VALUES (?1,?2,?3,1)
                     ON CONFLICT(addr) DO UPDATE SET
                         times_seen = times_seen + 1,
                         last_seen_utc = excluded.last_seen_utc,
                         display_name = CASE WHEN contacts.display_name = ''
                                             THEN excluded.display_name
                                             ELSE contacts.display_name END",
                    params![m.from_name, m.from_addr, m.received_utc],
                )?;
            }

            Ok(id)
        })
    }

    pub fn list_messages(&self, q: &MessageQuery) -> Result<Vec<MessageSummary>> {
        self.with_conn(|conn| {
            let mut sql = String::from(
                "SELECT m.id, m.account_id, m.folder_id, m.from_name, m.from_addr, m.subject,
                        m.date_utc, m.is_read, m.is_flagged, m.is_answered, m.has_attachments,
                        m.size_bytes, m.preview_text, m.priority, m.work_status,
                        t.name, t.color, t.initials,
                        (SELECT r.due_utc FROM reminders r
                          WHERE r.message_id = m.id AND r.done_utc IS NULL)
                 FROM messages m
                 LEFT JOIN team_members t ON t.id = m.assignee_id",
            );
            if !q.search.trim().is_empty() {
                sql.push_str(
                    " JOIN messages_fts f ON f.rowid = m.id AND messages_fts MATCH :search",
                );
            }
            sql.push_str(
                " JOIN folders fo ON fo.id = m.folder_id
                  WHERE (:account_id IS NULL OR m.account_id = :account_id)
                    AND (:folder_id IS NULL OR m.folder_id = :folder_id)
                    AND (:folder_kind IS NULL OR fo.kind = :folder_kind)
                    AND (:unread_only = 0 OR m.is_read = 0)
                    AND (:flagged_only = 0 OR m.is_flagged = 1)
                    AND (:assignee_id IS NULL OR m.assignee_id = :assignee_id)
                    AND (:unassigned_only = 0 OR m.assignee_id IS NULL)
                    AND (:high_priority_only = 0 OR m.priority = 2)
                    AND (:has_reminder_only = 0 OR EXISTS
                          (SELECT 1 FROM reminders r
                            WHERE r.message_id = m.id AND r.done_utc IS NULL))
                  ORDER BY m.date_utc DESC
                  LIMIT :limit OFFSET :offset",
            );

            let limit = if q.limit <= 0 { 200 } else { q.limit };
            let search = fts_query(&q.search);
            let folder_kind = q.folder_kind.map(|k| k.as_str());
            let unread_only = q.unread_only as i64;
            let flagged_only = q.flagged_only as i64;

            // Bound dynamically because `:search` only exists in the statement when the FTS join
            // does. Binding a name the statement does not contain is an error, not a no-op.
            let unassigned_only = q.unassigned_only as i64;
            let high_priority_only = q.high_priority_only as i64;
            let has_reminder_only = q.has_reminder_only as i64;

            let mut bound: Vec<(&str, &dyn rusqlite::ToSql)> = vec![
                (":assignee_id", &q.assignee_id),
                (":unassigned_only", &unassigned_only),
                (":high_priority_only", &high_priority_only),
                (":has_reminder_only", &has_reminder_only),
                (":account_id", &q.account_id),
                (":folder_id", &q.folder_id),
                (":folder_kind", &folder_kind),
                (":unread_only", &unread_only),
                (":flagged_only", &flagged_only),
                (":limit", &limit),
                (":offset", &q.offset),
            ];
            if !search.is_empty() {
                bound.push((":search", &search));
            }

            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(&bound[..], |r| {
                Ok(MessageSummary {
                    id: r.get(0)?,
                    account_id: r.get(1)?,
                    folder_id: r.get(2)?,
                    from_name: r.get(3)?,
                    from_addr: r.get(4)?,
                    subject: r.get(5)?,
                    date_utc: r.get(6)?,
                    is_read: r.get::<_, i64>(7)? != 0,
                    is_flagged: r.get::<_, i64>(8)? != 0,
                    is_answered: r.get::<_, i64>(9)? != 0,
                    has_attachments: r.get::<_, i64>(10)? != 0,
                    size_bytes: r.get(11)?,
                    preview_text: r.get(12)?,
                    priority: r.get(13)?,
                    work_status: r.get(14)?,
                    assignee_name: r.get(15)?,
                    assignee_color: r.get(16)?,
                    assignee_initials: r.get(17)?,
                    reminder_due_utc: r.get(18)?,
                })
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    pub fn get_message(&self, id: i64) -> Result<MessageDetail> {
        self.with_conn(|conn| {
            let mut detail = conn
                .query_row(
                    "SELECT id, account_id, folder_id, rfc822_message_id, from_name, from_addr,
                            subject, date_utc, is_read, is_flagged, is_answered, body_html,
                            body_text, has_remote_content, size_bytes
                     FROM messages WHERE id = ?1",
                    params![id],
                    |r| {
                        Ok(MessageDetail {
                            id: r.get(0)?,
                            account_id: r.get(1)?,
                            folder_id: r.get(2)?,
                            rfc822_message_id: r.get(3)?,
                            from_name: r.get(4)?,
                            from_addr: r.get(5)?,
                            subject: r.get(6)?,
                            date_utc: r.get(7)?,
                            is_read: r.get::<_, i64>(8)? != 0,
                            is_flagged: r.get::<_, i64>(9)? != 0,
                            is_answered: r.get::<_, i64>(10)? != 0,
                            body_html: r.get(11)?,
                            body_text: r.get(12)?,
                            has_remote_content: r.get::<_, i64>(13)? != 0,
                            recipients: Vec::new(),
                            attachments: Vec::new(),
                            size_bytes: r.get(14)?,
                        })
                    },
                )
                .optional()?
                .ok_or(Error::NotFound {
                    kind: "message",
                    id,
                })?;

            let mut stmt =
                conn.prepare("SELECT kind, name, addr FROM recipients WHERE message_id = ?1")?;
            detail.recipients = stmt
                .query_map(params![id], |r| {
                    Ok(Recipient {
                        kind: r.get(0)?,
                        name: r.get(1)?,
                        addr: r.get(2)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;

            let mut stmt = conn.prepare(
                "SELECT id, filename, mime_type, size_bytes, content_id, is_inline
                 FROM attachments WHERE message_id = ?1",
            )?;
            detail.attachments = stmt
                .query_map(params![id], |r| {
                    Ok(Attachment {
                        id: r.get(0)?,
                        filename: r.get(1)?,
                        mime_type: r.get(2)?,
                        size_bytes: r.get(3)?,
                        content_id: r.get(4)?,
                        is_inline: r.get::<_, i64>(5)? != 0,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;

            Ok(detail)
        })
    }

    /// On-disk path of a message's raw RFC822 bytes.
    pub fn raw_path(&self, message_id: i64) -> Result<String> {
        self.with_conn(|conn| {
            conn.query_row(
                "SELECT raw_path FROM messages WHERE id = ?1",
                params![message_id],
                |r| r.get(0),
            )
            .optional()?
            .ok_or(Error::NotFound {
                kind: "message",
                id: message_id,
            })
        })
    }

    /// The on-disk path of an attachment blob.
    pub fn attachment_path(&self, attachment_id: i64) -> Result<(String, PathBuf)> {
        self.with_conn(|conn| {
            conn.query_row(
                "SELECT filename, blob_path FROM attachments WHERE id = ?1",
                params![attachment_id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        PathBuf::from(r.get::<_, String>(1)?),
                    ))
                },
            )
            .optional()?
            .ok_or(Error::NotFound {
                kind: "attachment",
                id: attachment_id,
            })
        })
    }

    /// Filename, MIME type and size for one attachment, without reading the file itself.
    ///
    /// The size is the point: it is what decides whether something can be opened in the window or
    /// has to be saved, and answering that by reading the file would defeat the check.
    pub fn attachment_detail(&self, attachment_id: i64) -> Result<Attachment> {
        self.with_conn(|conn| {
            Ok(conn.query_row(
                "SELECT id, filename, mime_type, size_bytes, content_id, is_inline
                 FROM attachments WHERE id = ?1",
                params![attachment_id],
                |r| {
                    Ok(Attachment {
                        id: r.get(0)?,
                        filename: r.get(1)?,
                        mime_type: r.get(2)?,
                        size_bytes: r.get(3)?,
                        content_id: r.get(4)?,
                        is_inline: r.get::<_, i64>(5)? != 0,
                    })
                },
            )?)
        })
    }

    pub fn set_read(&self, ids: &[i64], read: bool) -> Result<()> {
        self.update_flag("is_read", ids, read)
    }

    pub fn set_flagged(&self, ids: &[i64], flagged: bool) -> Result<()> {
        self.update_flag("is_flagged", ids, flagged)
    }

    pub fn set_answered(&self, ids: &[i64], answered: bool) -> Result<()> {
        self.update_flag("is_answered", ids, answered)
    }

    fn update_flag(&self, column: &'static str, ids: &[i64], value: bool) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        self.with_tx(|tx| {
            // `column` is a compile-time constant from the three callers above, never user input.
            // `flags_dirty` marks the row for the next IMAP push; it is inert for POP3 accounts,
            // which have nowhere to push to.
            let sql = format!("UPDATE messages SET {column} = ?2, flags_dirty = 1 WHERE id = ?1");
            let mut stmt = tx.prepare(&sql)?;
            for id in ids {
                stmt.execute(params![id, value as i64])?;
            }
            Ok(())
        })
    }

    /// Move messages to another folder, and journal the same move for the IMAP server.
    ///
    /// The journal is written here rather than by each caller so the UI, rules and the junk filter
    /// all reach the server; see [`imap_ops`].
    pub fn move_messages(&self, ids: &[i64], folder_id: i64) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        self.with_tx(|tx| {
            for id in ids {
                imap_ops::journal_move(tx, *id, folder_id)?;
                tx.execute(
                    "UPDATE messages SET folder_id = ?2 WHERE id = ?1",
                    params![id, folder_id],
                )?;
            }
            Ok(())
        })
    }

    /// Permanently remove messages, their blobs and their search-index rows.
    pub fn purge_messages(&self, ids: &[i64]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let mut blobs = Vec::new();
        self.with_tx(|tx| {
            for id in ids {
                let raw: Option<String> = tx
                    .query_row(
                        "SELECT raw_path FROM messages WHERE id = ?1",
                        params![id],
                        |r| r.get(0),
                    )
                    .optional()?;
                if let Some(p) = raw.filter(|p| !p.is_empty()) {
                    blobs.push(PathBuf::from(p));
                }
                let mut stmt =
                    tx.prepare("SELECT blob_path FROM attachments WHERE message_id = ?1")?;
                for path in stmt.query_map(params![id], |r| r.get::<_, String>(0))? {
                    blobs.push(PathBuf::from(path?));
                }
                drop(stmt);

                // An IMAP message purged here must also go from the server, or the next sync
                // would bring it straight back.
                imap_ops::journal_delete(tx, *id)?;
                tx.execute("DELETE FROM messages_fts WHERE rowid = ?1", params![id])?;
                tx.execute("DELETE FROM messages WHERE id = ?1", params![id])?;
            }
            Ok(())
        })?;

        // Blobs go after the transaction commits: a failed delete here leaks a file, whereas
        // deleting first and then rolling back would lose mail.
        for path in blobs {
            if let Err(e) = std::fs::remove_file(&path) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(path = %path.display(), error = %e, "could not remove blob");
                }
            }
        }
        Ok(())
    }

    // ---------------------------------------------------------------- pop_seen

    /// Remote ids already ingested for a folder.
    ///
    /// POP3 passes `INBOX` and its UIDLs; IMAP passes the folder path and its UIDs, which are only
    /// unique within a folder.
    pub fn seen_ids(
        &self,
        account_id: i64,
        folder: &str,
    ) -> Result<std::collections::HashSet<String>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT remote_id FROM remote_seen WHERE account_id = ?1 AND folder = ?2",
            )?;
            let rows = stmt.query_map(params![account_id, folder], |r| r.get::<_, String>(0))?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })
    }

    /// Convenience for POP3, which has exactly one folder.
    pub fn seen_uidls(&self, account_id: i64) -> Result<std::collections::HashSet<String>> {
        self.seen_ids(account_id, "INBOX")
    }

    pub fn mark_seen(&self, account_id: i64, uidl: &str, message_id: Option<i64>) -> Result<()> {
        self.mark_seen_in(account_id, "INBOX", uidl, message_id, now_utc())
    }

    /// Record a remote id as ingested, in a named folder.
    pub fn mark_seen_in(
        &self,
        account_id: i64,
        folder: &str,
        remote_id: &str,
        message_id: Option<i64>,
        first_seen_utc: i64,
    ) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO remote_seen (account_id, folder, remote_id, first_seen_utc, message_id)
                 VALUES (?1,?2,?3,?4,?5)
                 ON CONFLICT(account_id, folder, remote_id)
                 DO UPDATE SET message_id = excluded.message_id",
                params![account_id, folder, remote_id, first_seen_utc, message_id],
            )?;
            Ok(())
        })
    }

    /// [`Store::mark_seen`] with an explicit first-seen time.
    ///
    /// `first_seen_utc` drives age-based retention, so an import that back-dates messages needs
    /// to set it rather than have everything look like it arrived today.
    pub fn mark_seen_at(
        &self,
        account_id: i64,
        uidl: &str,
        message_id: Option<i64>,
        first_seen_utc: i64,
    ) -> Result<()> {
        self.mark_seen_in(account_id, "INBOX", uidl, message_id, first_seen_utc)
    }

    /// UIDLs first seen before `cutoff_utc` and not yet deleted from the server.
    pub fn uidls_older_than(&self, account_id: i64, cutoff_utc: i64) -> Result<Vec<String>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT remote_id FROM remote_seen
                 WHERE account_id = ?1 AND folder = 'INBOX' AND first_seen_utc < ?2
                   AND deleted_from_server_utc IS NULL",
            )?;
            let rows =
                stmt.query_map(params![account_id, cutoff_utc], |r| r.get::<_, String>(0))?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    pub fn mark_deleted_from_server(&self, account_id: i64, uidls: &[String]) -> Result<()> {
        if uidls.is_empty() {
            return Ok(());
        }
        self.with_tx(|tx| {
            let mut stmt = tx.prepare(
                "UPDATE remote_seen SET deleted_from_server_utc = ?3
                 WHERE account_id = ?1 AND folder = 'INBOX' AND remote_id = ?2",
            )?;
            for uidl in uidls {
                stmt.execute(params![account_id, uidl, now_utc()])?;
            }
            Ok(())
        })
    }

    // ---------------------------------------------------------------- outbox

    pub fn queue_outgoing(
        &self,
        account_id: i64,
        to_addrs: &str,
        subject: &str,
        raw_path: &str,
    ) -> Result<i64> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO outbox (account_id, to_addrs, subject, raw_path, created_utc)
                 VALUES (?1,?2,?3,?4,?5)",
                params![account_id, to_addrs, subject, raw_path, now_utc()],
            )?;
            Ok(conn.last_insert_rowid())
        })
    }

    pub fn set_outbox_state(&self, id: i64, state: &str, error: Option<&str>) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE outbox SET state = ?2, last_error = ?3, attempts = attempts + 1
                 WHERE id = ?1",
                params![id, state, error],
            )?;
            Ok(())
        })
    }

    pub fn contacts(&self, prefix: &str, limit: i64) -> Result<Vec<(String, String)>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT display_name, addr FROM contacts
                 WHERE addr LIKE ?1 OR display_name LIKE ?1
                 ORDER BY times_seen DESC, last_seen_utc DESC LIMIT ?2",
            )?;
            let pattern = format!("%{prefix}%");
            let rows = stmt.query_map(params![pattern, limit], |r| Ok((r.get(0)?, r.get(1)?)))?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }
}

/// A parsed message ready to be stored.
#[derive(Debug, Clone, Default)]
pub struct NewMessage {
    pub account_id: i64,
    pub folder_id: i64,
    pub rfc822_message_id: Option<String>,
    pub in_reply_to: Option<String>,
    pub thread_id: String,
    pub from_name: String,
    pub from_addr: String,
    pub subject: String,
    pub date_utc: i64,
    pub received_utc: i64,
    pub has_attachments: bool,
    pub has_remote_content: bool,
    pub size_bytes: i64,
    pub preview_text: String,
    pub body_text: String,
    pub body_html: Option<String>,
    pub raw_path: String,
    pub recipients: Vec<Recipient>,
    pub attachments: Vec<NewAttachment>,
}

#[derive(Debug, Clone)]
pub struct NewAttachment {
    pub filename: String,
    pub mime_type: String,
    pub size_bytes: i64,
    pub content_id: Option<String>,
    pub blob_path: String,
    pub is_inline: bool,
}

const ACCOUNT_COLUMNS: &str = "SELECT a.id, a.display_name, a.email, a.pop_host, a.pop_port,
        a.pop_security, a.pop_user, a.smtp_host, a.smtp_port, a.smtp_security, a.smtp_user,
        a.credential_handle, a.leave_on_server, a.delete_after_days, a.poll_minutes, a.signature,
        a.is_enabled, a.last_sync_utc, a.junk_filter_enabled, a.junk_threshold, a.protocol
    FROM accounts a";

fn row_to_account(r: &Row<'_>) -> rusqlite::Result<Account> {
    Ok(Account {
        id: r.get(0)?,
        display_name: r.get(1)?,
        email: r.get(2)?,
        pop_host: r.get(3)?,
        pop_port: r.get(4)?,
        pop_security: Security::parse(&r.get::<_, String>(5)?),
        pop_user: r.get(6)?,
        smtp_host: r.get(7)?,
        smtp_port: r.get(8)?,
        smtp_security: Security::parse(&r.get::<_, String>(9)?),
        smtp_user: r.get(10)?,
        credential_handle: r.get(11)?,
        leave_on_server: r.get::<_, i64>(12)? != 0,
        delete_after_days: r.get(13)?,
        poll_minutes: r.get(14)?,
        signature: r.get(15)?,
        is_enabled: r.get::<_, i64>(16)? != 0,
        last_sync_utc: r.get(17)?,
        junk_filter_enabled: r.get::<_, i64>(18)? != 0,
        junk_threshold: r.get(19)?,
        protocol: Protocol::parse(&r.get::<_, String>(20)?),
    })
}

/// Turn user input into an FTS5 query that cannot be a syntax error.
///
/// FTS5 treats `"`, `*`, `:`, `^`, `-`, `(`, `)` and `AND`/`OR`/`NOT` as syntax, so a user typing
/// `re: budget` or an unbalanced quote would otherwise get an error rather than results. Each word
/// is quoted as a literal, and a trailing `*` gives prefix matching on the last word so search
/// feels live as you type.
pub fn fts_query(input: &str) -> String {
    let words: Vec<String> = input
        .split_whitespace()
        .map(|w| w.replace('"', ""))
        .filter(|w| !w.is_empty())
        .collect();
    if words.is_empty() {
        return String::new();
    }
    let last = words.len() - 1;
    words
        .iter()
        .enumerate()
        .map(|(i, w)| {
            if i == last {
                format!("\"{w}\"*")
            } else {
                format!("\"{w}\"")
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn now_utc() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account() -> NewAccount {
        NewAccount {
            display_name: "Dev".into(),
            email: "dev@example.com".into(),
            pop_host: "mail.example.com".into(),
            pop_port: 995,
            pop_security: Security::Tls,
            pop_user: "dev@example.com".into(),
            smtp_host: "mail.example.com".into(),
            smtp_port: 587,
            smtp_security: Security::Starttls,
            smtp_user: "dev@example.com".into(),
            leave_on_server: true,
            delete_after_days: None,
            poll_minutes: 5,
            signature: String::new(),
            protocol: Protocol::Pop3,
        }
    }

    fn message(store: &Store, account_id: i64, subject: &str, body: &str) -> i64 {
        let folder_id = store.folder_id(account_id, FolderKind::Inbox).unwrap();
        store
            .insert_message(&NewMessage {
                account_id,
                folder_id,
                thread_id: subject.to_string(),
                from_name: "Alice".into(),
                from_addr: "alice@example.com".into(),
                subject: subject.into(),
                date_utc: 1_700_000_000,
                received_utc: 1_700_000_000,
                body_text: body.into(),
                preview_text: body.chars().take(80).collect(),
                recipients: vec![Recipient {
                    kind: "to".into(),
                    name: "Dev".into(),
                    addr: "dev@example.com".into(),
                }],
                ..Default::default()
            })
            .unwrap()
    }

    #[test]
    fn migrations_create_every_default_folder() {
        let store = Store::open_in_memory().unwrap();
        let id = store.add_account(&account(), "handle").unwrap();
        let folders = store.list_folders(Some(id)).unwrap();
        assert_eq!(folders.len(), 6);
        assert_eq!(folders[0].kind, FolderKind::Inbox);
        // POP3 supplies none of these; we invent the whole set.
        assert!(folders.iter().any(|f| f.kind == FolderKind::Sent));
        assert!(folders.iter().any(|f| f.kind == FolderKind::Deleted));
    }

    #[test]
    fn unread_counts_track_flag_changes() {
        let store = Store::open_in_memory().unwrap();
        let acc = store.add_account(&account(), "h").unwrap();
        let a = message(&store, acc, "One", "hello");
        message(&store, acc, "Two", "world");

        let inbox = |s: &Store| {
            s.list_folders(Some(acc))
                .unwrap()
                .into_iter()
                .find(|f| f.kind == FolderKind::Inbox)
                .unwrap()
        };
        assert_eq!(inbox(&store).unread, 2);
        store.set_read(&[a], true).unwrap();
        assert_eq!(inbox(&store).unread, 1);
        assert_eq!(inbox(&store).total, 2);
    }

    #[test]
    fn listing_without_a_search_term_returns_everything() {
        // The default path the UI uses on every folder click: no FTS join, so `:search` must not
        // be bound at all.
        let store = Store::open_in_memory().unwrap();
        let acc = store.add_account(&account(), "h").unwrap();
        message(&store, acc, "One", "a");
        message(&store, acc, "Two", "b");

        let all = store.list_messages(&MessageQuery::default()).unwrap();
        assert_eq!(all.len(), 2);

        // Whitespace-only input is not a search either.
        let blank = store
            .list_messages(&MessageQuery {
                search: "   ".into(),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(blank.len(), 2);
    }

    #[test]
    fn filters_compose_with_and_without_search() {
        let store = Store::open_in_memory().unwrap();
        let acc = store.add_account(&account(), "h").unwrap();
        let a = message(&store, acc, "Budget report", "figures");
        message(&store, acc, "Budget lunch", "sandwiches");

        store.set_read(&[a], true).unwrap();
        store.set_flagged(&[a], true).unwrap();

        let unread = store
            .list_messages(&MessageQuery {
                unread_only: true,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(unread.len(), 1);
        assert_eq!(unread[0].subject, "Budget lunch");

        let flagged_and_searched = store
            .list_messages(&MessageQuery {
                flagged_only: true,
                search: "budget".into(),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(flagged_and_searched.len(), 1);
        assert_eq!(flagged_and_searched[0].id, a);
    }

    #[test]
    fn search_matches_subject_and_body() {
        let store = Store::open_in_memory().unwrap();
        let acc = store.add_account(&account(), "h").unwrap();
        message(&store, acc, "Quarterly budget", "figures attached");
        message(&store, acc, "Lunch", "sandwiches");

        let find = |s: &str| {
            store
                .list_messages(&MessageQuery {
                    search: s.into(),
                    ..Default::default()
                })
                .unwrap()
        };
        assert_eq!(find("budget").len(), 1);
        assert_eq!(find("figures").len(), 1);
        assert_eq!(find("sandwiches").len(), 1);
        assert_eq!(find("nothing").len(), 0);
        // Prefix matching on the last word, so the list narrows as the user types.
        assert_eq!(find("quart").len(), 1);
    }

    #[test]
    fn search_survives_fts5_syntax_in_user_input() {
        let store = Store::open_in_memory().unwrap();
        let acc = store.add_account(&account(), "h").unwrap();
        message(&store, acc, "Re: budget", "figures");

        // Each of these is FTS5 syntax and would be a query error if passed through raw.
        for input in ["re: budget", "budget OR", "\"unbalanced", "-budget", "a*(b"] {
            let out = store.list_messages(&MessageQuery {
                search: input.into(),
                ..Default::default()
            });
            assert!(out.is_ok(), "search {input:?} errored: {:?}", out.err());
        }
    }

    #[test]
    fn purging_removes_the_search_index_row_too() {
        let store = Store::open_in_memory().unwrap();
        let acc = store.add_account(&account(), "h").unwrap();
        let id = message(&store, acc, "Findable", "content");

        assert_eq!(
            store
                .list_messages(&MessageQuery {
                    search: "Findable".into(),
                    ..Default::default()
                })
                .unwrap()
                .len(),
            1
        );
        store.purge_messages(&[id]).unwrap();
        // A stale FTS row would resurrect deleted mail in search results.
        assert_eq!(
            store
                .list_messages(&MessageQuery {
                    search: "Findable".into(),
                    ..Default::default()
                })
                .unwrap()
                .len(),
            0
        );
    }

    #[test]
    fn uidl_bookkeeping_is_idempotent() {
        let store = Store::open_in_memory().unwrap();
        let acc = store.add_account(&account(), "h").unwrap();

        let msg = message(&store, acc, "Ingested", "x");

        store.mark_seen(acc, "UID-1", None).unwrap();
        store.mark_seen(acc, "UID-1", Some(msg)).unwrap();
        let seen = store.seen_uidls(acc).unwrap();
        assert_eq!(seen.len(), 1, "re-marking must not duplicate the row");
        assert!(seen.contains("UID-1"));

        // A UIDL must never point at a message that does not exist, or dedupe would claim mail
        // was ingested when it was not.
        assert!(store.mark_seen(acc, "UID-2", Some(999_999)).is_err());
    }

    #[test]
    fn moving_between_folders_updates_both_counts() {
        let store = Store::open_in_memory().unwrap();
        let acc = store.add_account(&account(), "h").unwrap();
        let id = message(&store, acc, "Movable", "x");
        let deleted = store.folder_id(acc, FolderKind::Deleted).unwrap();

        store.move_messages(&[id], deleted).unwrap();
        let folders = store.list_folders(Some(acc)).unwrap();
        let by = |k: FolderKind| folders.iter().find(|f| f.kind == k).unwrap();
        assert_eq!(by(FolderKind::Inbox).total, 0);
        assert_eq!(by(FolderKind::Deleted).total, 1);
    }

    #[test]
    fn deleting_an_account_takes_its_mail_and_index_with_it() {
        let store = Store::open_in_memory().unwrap();
        let acc = store.add_account(&account(), "h").unwrap();
        message(&store, acc, "Doomed", "x");

        store.delete_account(acc).unwrap();
        assert!(store.list_accounts().unwrap().is_empty());
        assert!(
            store
                .list_messages(&MessageQuery {
                    search: "Doomed".into(),
                    ..Default::default()
                })
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn contacts_are_harvested_and_ranked_by_frequency() {
        let store = Store::open_in_memory().unwrap();
        let acc = store.add_account(&account(), "h").unwrap();
        message(&store, acc, "One", "x");
        message(&store, acc, "Two", "y");

        let contacts = store.contacts("alice", 10).unwrap();
        assert_eq!(contacts.len(), 1, "same address must not duplicate");
        assert_eq!(contacts[0].1, "alice@example.com");
    }
}
