//! Store operations added after the initial read/write path: account settings, rules, junk-filter
//! training and drafts.
//!
//! Split out of `lib.rs` purely to keep that file navigable; these are ordinary `Store` methods.

use std::collections::HashMap;

use rusqlite::{OptionalExtension, params};

use crate::{AccountSettings, Error, Result, Store, now_utc};

/// Corpus counts for one classifier token.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenCounts {
    pub spam: i64,
    pub ham: i64,
}

/// A draft as stored: enough to reopen the compose window exactly as it was left.
#[derive(Debug, Clone, Default)]
pub struct StoredDraft {
    pub message_id: i64,
    pub account_id: i64,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub subject: String,
    pub body_text: String,
    /// What the composer showed, quote and formatting included. `None` for a plain-text draft.
    pub body_html: Option<String>,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    /// Files the user picked, still living where they picked them.
    pub attachments: Vec<(String, String)>,
}

impl Store {
    // ---------------------------------------------------------------- settings

    pub fn update_account_settings(&self, id: i64, s: &AccountSettings) -> Result<()> {
        self.with_conn(|conn| {
            let changed = conn.execute(
                "UPDATE accounts SET display_name = ?2, signature = ?3, poll_minutes = ?4,
                     leave_on_server = ?5, delete_after_days = ?6, junk_filter_enabled = ?7,
                     junk_threshold = ?8, is_enabled = ?9
                 WHERE id = ?1",
                params![
                    id,
                    s.display_name,
                    s.signature,
                    s.poll_minutes,
                    s.leave_on_server as i64,
                    s.delete_after_days,
                    s.junk_filter_enabled as i64,
                    s.junk_threshold,
                    s.is_enabled as i64,
                ],
            )?;
            if changed == 0 {
                return Err(Error::NotFound {
                    kind: "account",
                    id,
                });
            }
            Ok(())
        })
    }

    // ---------------------------------------------------------------- rules

    /// Rules for an account, or every rule when `account_id` is `None`.
    ///
    /// Returned as raw JSON so `signet-store` does not need to know the rule grammar — that lives in
    /// `signet-sync`, next to the engine that evaluates it.
    pub fn list_rules(&self, account_id: Option<i64>) -> Result<Vec<(i64, i64, String)>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, account_id, match_json FROM rules
                 WHERE (?1 IS NULL OR account_id = ?1)
                 ORDER BY account_id, priority, id",
            )?;
            let rows = stmt.query_map(params![account_id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    pub fn insert_rule(
        &self,
        account_id: i64,
        name: &str,
        priority: i64,
        enabled: bool,
        json: &str,
    ) -> Result<i64> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO rules (account_id, priority, enabled, name, match_json, action_json)
                 VALUES (?1,?2,?3,?4,?5,'')",
                params![account_id, priority, enabled as i64, name, json],
            )?;
            Ok(conn.last_insert_rowid())
        })
    }

    pub fn update_rule(
        &self,
        id: i64,
        name: &str,
        priority: i64,
        enabled: bool,
        json: &str,
    ) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE rules SET name = ?2, priority = ?3, enabled = ?4, match_json = ?5
                 WHERE id = ?1",
                params![id, name, priority, enabled as i64, json],
            )?;
            Ok(())
        })
    }

    pub fn delete_rule(&self, id: i64) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute("DELETE FROM rules WHERE id = ?1", params![id])?;
            Ok(())
        })
    }

    // ---------------------------------------------------------------- junk filter

    /// Counts for the given tokens. Only tokens the corpus has seen come back.
    pub fn token_counts(&self, tokens: &[String]) -> Result<HashMap<String, TokenCounts>> {
        if tokens.is_empty() {
            return Ok(HashMap::new());
        }
        self.with_conn(|conn| {
            let mut stmt =
                conn.prepare("SELECT spam_count, ham_count FROM spam_tokens WHERE token = ?1")?;
            let mut out = HashMap::with_capacity(tokens.len());
            for token in tokens {
                if let Some((spam, ham)) = stmt
                    .query_row(params![token], |r| Ok((r.get(0)?, r.get(1)?)))
                    .optional()?
                {
                    out.insert(token.clone(), TokenCounts { spam, ham });
                }
            }
            Ok(out)
        })
    }

    pub fn corpus_size(&self) -> Result<(i64, i64)> {
        self.with_conn(|conn| {
            Ok(conn.query_row(
                "SELECT spam_messages, ham_messages FROM spam_meta WHERE id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?)
        })
    }

    /// Train the classifier on one message.
    ///
    /// Idempotence is the caller's problem: training on the same message twice would count it
    /// twice, so `signet-core` only trains when a message actually changes classification.
    pub fn train(&self, tokens: &[String], is_spam: bool) -> Result<()> {
        self.with_tx(|tx| {
            let column = if is_spam { "spam_count" } else { "ham_count" };
            // `column` is one of two literals chosen here, never user input.
            let sql = format!(
                "INSERT INTO spam_tokens (token, {column}) VALUES (?1, 1)
                 ON CONFLICT(token) DO UPDATE SET {column} = {column} + 1"
            );
            let mut stmt = tx.prepare(&sql)?;
            for token in tokens {
                stmt.execute(params![token])?;
            }
            drop(stmt);

            let counter = if is_spam {
                "spam_messages"
            } else {
                "ham_messages"
            };
            tx.execute(
                &format!("UPDATE spam_meta SET {counter} = {counter} + 1 WHERE id = 1"),
                [],
            )?;
            Ok(())
        })
    }

    /// Undo one training example, for when the user changes their mind.
    pub fn untrain(&self, tokens: &[String], was_spam: bool) -> Result<()> {
        self.with_tx(|tx| {
            let column = if was_spam { "spam_count" } else { "ham_count" };
            let sql =
                format!("UPDATE spam_tokens SET {column} = MAX({column} - 1, 0) WHERE token = ?1");
            let mut stmt = tx.prepare(&sql)?;
            for token in tokens {
                stmt.execute(params![token])?;
            }
            drop(stmt);

            let counter = if was_spam {
                "spam_messages"
            } else {
                "ham_messages"
            };
            tx.execute(
                &format!("UPDATE spam_meta SET {counter} = MAX({counter} - 1, 0) WHERE id = 1"),
                [],
            )?;
            Ok(())
        })
    }

    pub fn set_spam_score(&self, message_id: i64, score: f64, filed_by: &str) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE messages SET spam_score = ?2, filed_by = ?3 WHERE id = ?1",
                params![message_id, score, filed_by],
            )?;
            Ok(())
        })
    }

    /// What filed this message, if anything did. Empty for ordinary Inbox delivery.
    pub fn filed_by(&self, message_id: i64) -> Result<String> {
        self.with_conn(|conn| {
            Ok(conn.query_row(
                "SELECT filed_by FROM messages WHERE id = ?1",
                params![message_id],
                |r| r.get(0),
            )?)
        })
    }

    pub fn spam_score(&self, message_id: i64) -> Result<f64> {
        self.with_conn(|conn| {
            Ok(conn.query_row(
                "SELECT spam_score FROM messages WHERE id = ?1",
                params![message_id],
                |r| r.get(0),
            )?)
        })
    }

    // ---------------------------------------------------------------- drafts

    /// Create or replace a draft, returning its message id.
    pub fn save_draft(&self, draft: &StoredDraft, folder_id: i64) -> Result<i64> {
        let now = now_utc();
        let existing = draft.message_id;

        self.with_tx(|tx| {
            let id = if existing > 0 {
                tx.execute(
                    "UPDATE messages SET subject = ?2, body_text = ?3, preview_text = ?4,
                         date_utc = ?5, received_utc = ?5, in_reply_to = ?6,
                         has_attachments = ?7, body_html = ?8, draft_references = ?9
                     WHERE id = ?1",
                    params![
                        existing,
                        draft.subject,
                        draft.body_text,
                        preview(&draft.body_text),
                        now,
                        draft.in_reply_to,
                        !draft.attachments.is_empty() as i64,
                        draft.body_html,
                        draft.references.join("\n"),
                    ],
                )?;
                tx.execute(
                    "DELETE FROM recipients WHERE message_id = ?1",
                    params![existing],
                )?;
                tx.execute(
                    "DELETE FROM draft_attachments WHERE message_id = ?1",
                    params![existing],
                )?;
                tx.execute(
                    "UPDATE messages_fts SET subject = ?2, body = ?3 WHERE rowid = ?1",
                    params![existing, draft.subject, draft.body_text],
                )?;
                existing
            } else {
                tx.execute(
                    "INSERT INTO messages (account_id, folder_id, thread_id, from_name, from_addr,
                         subject, date_utc, received_utc, is_read, has_attachments, preview_text,
                         body_text, in_reply_to, body_html, draft_references)
                     VALUES (?1,?2,'draft','','',?3,?4,?4,1,?5,?6,?7,?8,?9,?10)",
                    params![
                        draft.account_id,
                        folder_id,
                        draft.subject,
                        now,
                        !draft.attachments.is_empty() as i64,
                        preview(&draft.body_text),
                        draft.body_text,
                        draft.in_reply_to,
                        draft.body_html,
                        draft.references.join("\n"),
                    ],
                )?;
                let id = tx.last_insert_rowid();
                tx.execute(
                    "INSERT INTO messages_fts (rowid, subject, sender, recipients, body)
                     VALUES (?1,?2,'','',?3)",
                    params![id, draft.subject, draft.body_text],
                )?;
                id
            };

            for (kind, list) in [("to", &draft.to), ("cc", &draft.cc), ("bcc", &draft.bcc)] {
                for addr in list.iter().filter(|a| !a.trim().is_empty()) {
                    tx.execute(
                        "INSERT INTO recipients (message_id, kind, name, addr)
                         VALUES (?1,?2,'',?3)",
                        params![id, kind, addr.trim()],
                    )?;
                }
            }

            for (filename, path) in &draft.attachments {
                let size = std::fs::metadata(path).map(|m| m.len() as i64).unwrap_or(0);
                tx.execute(
                    "INSERT INTO draft_attachments (message_id, filename, source_path, size_bytes)
                     VALUES (?1,?2,?3,?4)",
                    params![id, filename, path, size],
                )?;
            }

            Ok(id)
        })
    }

    pub fn load_draft(&self, message_id: i64) -> Result<StoredDraft> {
        self.with_conn(|conn| {
            let (account_id, subject, body_text, in_reply_to, body_html, references) = conn
                .query_row(
                    "SELECT account_id, subject, body_text, in_reply_to, body_html,
                            draft_references
                     FROM messages WHERE id = ?1",
                    params![message_id],
                    |r| {
                        Ok((
                            r.get(0)?,
                            r.get(1)?,
                            r.get(2)?,
                            r.get(3)?,
                            r.get(4)?,
                            r.get::<_, String>(5)?,
                        ))
                    },
                )
                .optional()?
                .ok_or(Error::NotFound {
                    kind: "draft",
                    id: message_id,
                })?;

            let mut draft = StoredDraft {
                message_id,
                account_id,
                subject,
                body_text,
                body_html,
                in_reply_to,
                references: references.lines().map(str::to_string).collect(),
                ..Default::default()
            };

            let mut stmt =
                conn.prepare("SELECT kind, addr FROM recipients WHERE message_id = ?1")?;
            for row in stmt.query_map(params![message_id], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })? {
                let (kind, addr) = row?;
                match kind.as_str() {
                    "to" => draft.to.push(addr),
                    "cc" => draft.cc.push(addr),
                    "bcc" => draft.bcc.push(addr),
                    _ => {}
                }
            }

            let mut stmt = conn.prepare(
                "SELECT filename, source_path FROM draft_attachments WHERE message_id = ?1",
            )?;
            draft.attachments = stmt
                .query_map(params![message_id], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;

            Ok(draft)
        })
    }

    /// Mark every message in a folder read.
    pub fn mark_folder_read(&self, folder_id: i64) -> Result<usize> {
        self.with_conn(|conn| {
            Ok(conn.execute(
                "UPDATE messages SET is_read = 1 WHERE folder_id = ?1 AND is_read = 0",
                params![folder_id],
            )?)
        })
    }

    /// Everything the junk filter needs to score or train on a stored message.
    pub fn junk_features(
        &self,
        message_id: i64,
    ) -> Result<(String, String, String, String, bool, Vec<String>)> {
        self.with_conn(|conn| {
            let (from_name, from_addr, subject, body, remote) = conn
                .query_row(
                    "SELECT from_name, from_addr, subject, body_text, has_remote_content
                     FROM messages WHERE id = ?1",
                    params![message_id],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, String>(3)?,
                            r.get::<_, i64>(4)? != 0,
                        ))
                    },
                )
                .optional()?
                .ok_or(Error::NotFound {
                    kind: "message",
                    id: message_id,
                })?;

            let mut stmt = conn.prepare("SELECT addr FROM recipients WHERE message_id = ?1")?;
            let recipients = stmt
                .query_map(params![message_id], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;

            Ok((from_name, from_addr, subject, body, remote, recipients))
        })
    }
}

fn preview(body: &str) -> String {
    body.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(200)
        .collect()
}
