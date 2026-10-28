//! The journal of local changes owed to an IMAP server.
//!
//! [`Store::move_messages`] and [`Store::purge_messages`] write to it, so every caller — the UI,
//! rules, the junk filter — reaches the server without having to know about it. The sync replays
//! it before reading any folder, for the same reason flags are pushed first: reading first would
//! apply the server's old state straight back over what the user just did.

use rusqlite::{Transaction, params};

use crate::{Result, Store, now_utc};

/// One change waiting for the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImapOp {
    Move { from: String, uid: i64, to: String },
    Delete { from: String, uid: i64 },
    Append { to: String, raw_path: String },
}

#[derive(Debug, Clone)]
pub struct PendingImapOp {
    pub id: i64,
    pub op: ImapOp,
    pub attempts: i64,
}

/// After this many failed attempts an op is dropped and reported, so one message the server will
/// never accept cannot block the account forever.
pub const MAX_OP_ATTEMPTS: i64 = 10;

/// Journal a move of `message_id` into `folder_id` if both ends exist on the server, and detach the
/// message from its old UID. Runs inside the caller's transaction.
pub(crate) fn journal_move(tx: &Transaction<'_>, message_id: i64, folder_id: i64) -> Result<()> {
    let target: Option<String> = tx
        .query_row(
            "SELECT remote_path FROM folders WHERE id = ?1",
            params![folder_id],
            |r| r.get(0),
        )
        .ok()
        .flatten();

    // Moved again before the first move reached the server: redirect that move rather than
    // queueing a second one for a UID the message no longer has.
    let pending: Option<(i64, String, i64)> = tx
        .query_row(
            "SELECT id, from_path, uid FROM imap_ops WHERE message_id = ?1 AND kind = 'move'",
            params![message_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .ok();
    if let Some((op_id, from, uid)) = pending {
        match target.filter(|to| *to != from) {
            Some(to) => {
                tx.execute(
                    "UPDATE imap_ops SET to_path = ?2 WHERE id = ?1",
                    params![op_id, to],
                )?;
            }
            None => {
                // Back where it started, or somewhere local-only: nothing to do on the server, and
                // the message is still that UID in that mailbox.
                tx.execute("DELETE FROM imap_ops WHERE id = ?1", params![op_id])?;
                tx.execute(
                    "UPDATE messages SET remote_folder = ?2, remote_uid = ?3 WHERE id = ?1",
                    params![message_id, from, uid],
                )?;
            }
        }
        return Ok(());
    }

    let row: Option<(i64, Option<String>, Option<i64>)> = tx
        .query_row(
            "SELECT account_id, remote_folder, remote_uid FROM messages WHERE id = ?1",
            params![message_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .ok();
    let Some((account_id, Some(from), Some(uid))) = row else {
        // POP3, or not linked to a server UID: the move is local by nature.
        return Ok(());
    };
    let Some(to) = target.filter(|to| *to != from) else {
        // A local-only folder, or no real move: the server copy stays where it is.
        return Ok(());
    };

    tx.execute(
        "INSERT INTO imap_ops (account_id, kind, message_id, from_path, uid, to_path, created_utc)
         VALUES (?1, 'move', ?2, ?3, ?4, ?5, ?6)",
        params![account_id, message_id, from, uid, to, now_utc()],
    )?;
    // The UID belongs to the old mailbox. The server gives the moved copy a new one, which the
    // next sync adopts by Message-ID rather than downloading it a second time.
    tx.execute(
        "UPDATE messages SET remote_folder = NULL, remote_uid = NULL WHERE id = ?1",
        params![message_id],
    )?;
    Ok(())
}

/// Journal the server-side delete owed by purging `message_id`. Runs inside the caller's
/// transaction, before the row is removed.
pub(crate) fn journal_delete(tx: &Transaction<'_>, message_id: i64) -> Result<()> {
    // Moved to Deleted Items and purged before the move reached the server: delete it where it
    // still is, instead of moving it and then losing track of the copy.
    let converted = tx.execute(
        "UPDATE imap_ops SET kind = 'delete', to_path = NULL
         WHERE message_id = ?1 AND kind = 'move'",
        params![message_id],
    )?;
    if converted > 0 {
        return Ok(());
    }

    let row: Option<(i64, Option<String>, Option<i64>)> = tx
        .query_row(
            "SELECT account_id, remote_folder, remote_uid FROM messages WHERE id = ?1",
            params![message_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .ok();
    if let Some((account_id, Some(from), Some(uid))) = row {
        tx.execute(
            "INSERT INTO imap_ops (account_id, kind, message_id, from_path, uid, created_utc)
             VALUES (?1, 'delete', ?2, ?3, ?4, ?5)",
            params![account_id, message_id, from, uid, now_utc()],
        )?;
    }
    Ok(())
}

impl Store {
    /// Ask for a file to be uploaded into a server mailbox — a sent copy into Sent.
    pub fn queue_imap_append(&self, account_id: i64, to_path: &str, raw_path: &str) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO imap_ops (account_id, kind, to_path, raw_path, created_utc)
                 VALUES (?1, 'append', ?2, ?3, ?4)",
                params![account_id, to_path, raw_path, now_utc()],
            )?;
            Ok(())
        })
    }

    /// Everything owed to this account's server, oldest first.
    pub fn pending_imap_ops(&self, account_id: i64) -> Result<Vec<PendingImapOp>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, kind, from_path, uid, to_path, raw_path, attempts
                 FROM imap_ops WHERE account_id = ?1 ORDER BY id",
            )?;
            let rows = stmt.query_map(params![account_id], |r| {
                let kind: String = r.get(1)?;
                let from: Option<String> = r.get(2)?;
                let uid: Option<i64> = r.get(3)?;
                let to: Option<String> = r.get(4)?;
                let raw: Option<String> = r.get(5)?;
                let op = match kind.as_str() {
                    "move" => ImapOp::Move {
                        from: from.unwrap_or_default(),
                        uid: uid.unwrap_or(0),
                        to: to.unwrap_or_default(),
                    },
                    "delete" => ImapOp::Delete {
                        from: from.unwrap_or_default(),
                        uid: uid.unwrap_or(0),
                    },
                    _ => ImapOp::Append {
                        to: to.unwrap_or_default(),
                        raw_path: raw.unwrap_or_default(),
                    },
                };
                Ok(PendingImapOp {
                    id: r.get(0)?,
                    op,
                    attempts: r.get(6)?,
                })
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    pub fn finish_imap_op(&self, id: i64) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute("DELETE FROM imap_ops WHERE id = ?1", params![id])?;
            Ok(())
        })
    }

    /// Record a failed attempt. Returns true when the op has now been given up on and removed.
    pub fn fail_imap_op(&self, id: i64, error: &str) -> Result<bool> {
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE imap_ops SET attempts = attempts + 1, last_error = ?2 WHERE id = ?1",
                params![id, error],
            )?;
            let dropped = conn.execute(
                "DELETE FROM imap_ops WHERE id = ?1 AND attempts >= ?2",
                params![id, MAX_OP_ATTEMPTS],
            )?;
            Ok(dropped > 0)
        })
    }

    /// Whether a local folder holds messages not linked to any server UID — moved or sent copies
    /// waiting to be matched to the server's version. Decides whether new UIDs are worth a header
    /// fetch before downloading.
    pub fn has_unlinked_messages(&self, folder_id: i64) -> Result<bool> {
        self.with_conn(|conn| {
            Ok(conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM messages
                  WHERE folder_id = ?1 AND remote_uid IS NULL
                    AND rfc822_message_id IS NOT NULL AND rfc822_message_id != '')",
                params![folder_id],
                |r| r.get(0),
            )?)
        })
    }

    /// The unlinked local message in `folder_id` with this Message-ID, if there is one.
    pub fn unlinked_by_message_id(&self, folder_id: i64, message_id: &str) -> Result<Option<i64>> {
        let bare = message_id.trim().trim_matches(|c| c == '<' || c == '>');
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id FROM messages
                 WHERE folder_id = ?1 AND remote_uid IS NULL
                   AND (rfc822_message_id = ?2 OR rfc822_message_id = ?3)
                 LIMIT 1",
            )?;
            let mut rows = stmt.query(params![folder_id, bare, format!("<{bare}>")])?;
            Ok(match rows.next()? {
                Some(r) => Some(r.get(0)?),
                None => None,
            })
        })
    }

    /// Local messages linked to a UID in `remote_folder` that the server no longer lists.
    pub fn vanished_from_server(
        &self,
        account_id: i64,
        remote_folder: &str,
        present: &std::collections::HashSet<i64>,
    ) -> Result<Vec<i64>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, remote_uid FROM messages
                 WHERE account_id = ?1 AND remote_folder = ?2 AND remote_uid IS NOT NULL",
            )?;
            let rows = stmt.query_map(params![account_id, remote_folder], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
            })?;
            let mut gone = Vec::new();
            for row in rows {
                let (id, uid) = row?;
                if !present.contains(&uid) {
                    gone.push(id);
                }
            }
            Ok(gone)
        })
    }

    /// Remove local copies of messages the server no longer has, without telling the server to
    /// delete them — it already has.
    pub fn forget_vanished(&self, ids: &[i64]) -> Result<()> {
        self.with_tx(|tx| {
            for id in ids {
                tx.execute(
                    "UPDATE messages SET remote_folder = NULL, remote_uid = NULL WHERE id = ?1",
                    params![id],
                )?;
            }
            Ok(())
        })?;
        self.purge_messages(ids)
    }
}
