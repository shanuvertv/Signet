//! Reading messages out in a form that can be written back somewhere else.
//!
//! Export and import exist so a mailbox can move between machines, which makes the round trip the
//! whole point: whatever this module reads has to be enough for [`Store::insert_message`] and the
//! flag setters to reproduce the message, because those are the only writers that keep the search
//! index and the contact list in step. Copying columns straight out and straight back would be
//! shorter and would silently leave both behind.
//!
//! Folders travel by kind and name rather than by id, and blob paths travel relative to the
//! account directory. Ids are local to one database, and an absolute path stops being true the
//! moment the archive reaches a machine with a different user name on it.

use std::collections::HashSet;
use std::path::Path;

use rusqlite::{OptionalExtension, params};

use crate::{NewAttachment, NewMessage, Recipient, Result, Store};

/// One message, detached from the database that produced it.
#[derive(Debug, Clone)]
pub struct PortableMessage {
    /// Everything `insert_message` needs. `account_id` and `folder_id` are still the *source* ids
    /// and must be remapped before the insert.
    pub new: NewMessage,
    /// How to find the right folder in the destination.
    pub folder_kind: String,
    pub folder_name: String,
    pub is_read: bool,
    pub is_flagged: bool,
    pub is_answered: bool,
    pub spam_score: f64,
    pub filed_by: String,
    pub spam_reasons: String,
    /// The raw message file, relative to the account's blob directory.
    pub raw_rel: String,
    /// Attachment blobs, relative, in the same order as `new.attachments`.
    pub attachment_rels: Vec<String>,
}

impl Store {
    /// Every message belonging to an account, ready to be inserted elsewhere.
    pub fn portable_messages(&self, account_id: i64) -> Result<Vec<PortableMessage>> {
        let blob_dir = self.blob_dir(account_id);

        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT m.id, m.folder_id, m.rfc822_message_id, m.in_reply_to, m.thread_id,
                        m.from_name, m.from_addr, m.subject, m.date_utc, m.received_utc,
                        m.has_attachments, m.has_remote_content, m.size_bytes, m.preview_text,
                        m.body_text, m.body_html, m.raw_path, m.is_read, m.is_flagged,
                        m.is_answered, m.spam_score, m.filed_by, m.spam_reasons,
                        f.kind, f.name
                 FROM messages m
                 JOIN folders f ON f.id = m.folder_id
                 WHERE m.account_id = ?1
                 ORDER BY m.id",
            )?;

            let rows = stmt.query_map(params![account_id], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    PortableMessage {
                        new: NewMessage {
                            account_id,
                            folder_id: r.get(1)?,
                            rfc822_message_id: r.get(2)?,
                            in_reply_to: r.get(3)?,
                            thread_id: r.get(4)?,
                            from_name: r.get(5)?,
                            from_addr: r.get(6)?,
                            subject: r.get(7)?,
                            date_utc: r.get(8)?,
                            received_utc: r.get(9)?,
                            has_attachments: r.get::<_, i64>(10)? != 0,
                            has_remote_content: r.get::<_, i64>(11)? != 0,
                            size_bytes: r.get(12)?,
                            preview_text: r.get(13)?,
                            body_text: r.get(14)?,
                            body_html: r.get(15)?,
                            raw_path: r.get(16)?,
                            recipients: Vec::new(),
                            attachments: Vec::new(),
                        },
                        folder_kind: r.get(23)?,
                        folder_name: r.get(24)?,
                        is_read: r.get::<_, i64>(17)? != 0,
                        is_flagged: r.get::<_, i64>(18)? != 0,
                        is_answered: r.get::<_, i64>(19)? != 0,
                        spam_score: r.get(20)?,
                        filed_by: r.get(21)?,
                        spam_reasons: r.get(22)?,
                        raw_rel: String::new(),
                        attachment_rels: Vec::new(),
                    },
                ))
            })?;

            let mut out = Vec::new();
            for row in rows {
                let (id, mut m) = row?;

                let mut rstmt =
                    conn.prepare("SELECT kind, name, addr FROM recipients WHERE message_id = ?1")?;
                m.new.recipients = rstmt
                    .query_map(params![id], |r| {
                        Ok(Recipient {
                            kind: r.get(0)?,
                            name: r.get(1)?,
                            addr: r.get(2)?,
                        })
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;

                let mut astmt = conn.prepare(
                    "SELECT filename, mime_type, size_bytes, content_id, blob_path, is_inline
                     FROM attachments WHERE message_id = ?1 ORDER BY id",
                )?;
                m.new.attachments = astmt
                    .query_map(params![id], |r| {
                        Ok(NewAttachment {
                            filename: r.get(0)?,
                            mime_type: r.get(1)?,
                            size_bytes: r.get(2)?,
                            content_id: r.get(3)?,
                            blob_path: r.get(4)?,
                            is_inline: r.get::<_, i64>(5)? != 0,
                        })
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;

                m.raw_rel = relative_to(&m.new.raw_path, &blob_dir);
                m.attachment_rels = m
                    .new
                    .attachments
                    .iter()
                    .map(|a| relative_to(&a.blob_path, &blob_dir))
                    .collect();

                out.push(m);
            }
            Ok(out)
        })
    }

    /// The `Message-ID`s already held for an account, for skipping what is already there.
    ///
    /// Gathered once per import rather than queried per message: an archive can hold tens of
    /// thousands of messages and this check runs for every one of them.
    pub fn known_rfc822_ids(&self, account_id: i64) -> Result<HashSet<String>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT rfc822_message_id FROM messages
                 WHERE account_id = ?1 AND rfc822_message_id IS NOT NULL",
            )?;
            let rows = stmt.query_map(params![account_id], |r| r.get::<_, String>(0))?;
            let mut set = HashSet::new();
            for r in rows {
                set.insert(r?);
            }
            Ok(set)
        })
    }

    /// A folder of this kind and name, or `None`.
    pub fn folder_by_kind_and_name(
        &self,
        account_id: i64,
        kind: &str,
        name: &str,
    ) -> Result<Option<i64>> {
        self.with_conn(|conn| {
            Ok(conn
                .query_row(
                    "SELECT id FROM folders WHERE account_id = ?1 AND kind = ?2 AND name = ?3",
                    params![account_id, kind, name],
                    |r| r.get(0),
                )
                .optional()?)
        })
    }

    /// Create a folder the archive had and this database does not.
    pub fn create_folder(&self, account_id: i64, kind: &str, name: &str) -> Result<i64> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO folders (account_id, kind, name) VALUES (?1,?2,?3)",
                params![account_id, kind, name],
            )?;
            Ok(conn.last_insert_rowid())
        })
    }

    /// The schema version the database is currently at.
    pub fn schema_version(&self) -> Result<i64> {
        self.with_conn(|conn| Ok(conn.pragma_query_value(None, "user_version", |r| r.get(0))?))
    }

    /// Write a consistent snapshot of the database to `dest`.
    ///
    /// `VACUUM INTO` rather than copying the file: the live database is in WAL mode, so the bytes
    /// on disk are only half the story and copying `mail.sqlite` alone would produce an archive
    /// missing whatever is still in the log. This runs inside SQLite's own locking, so it is safe
    /// while the app is syncing.
    pub fn snapshot_to(&self, dest: &Path) -> Result<()> {
        if dest.exists() {
            std::fs::remove_file(dest)?;
        }
        self.with_conn(|conn| {
            conn.execute("VACUUM INTO ?1", params![dest.to_string_lossy()])?;
            Ok(())
        })
    }
}

/// Turn an absolute blob path back into one relative to its account directory.
fn relative_to(path: &str, blob_dir: &Path) -> String {
    let p = Path::new(path);
    if let Ok(rel) = p.strip_prefix(blob_dir) {
        return rel.to_string_lossy().replace('\\', "/");
    }

    // Stripping the prefix only works while we are still on the machine that wrote the row. Reading
    // an archive means reading paths that name somebody else's drive, and there the layout is the
    // only thing left to go on: every blob lives at `<blob dir>/raw/<file>` or
    // `<blob dir>/attachments/<file>`, so the last two components are the answer. Falling back to
    // the file name alone would drop the subdirectory and lose the file on import.
    let tail: Vec<String> = p
        .components()
        .rev()
        .take(2)
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    match tail.len() {
        2 => format!("{}/{}", tail[1], tail[0]),
        1 => tail[0].clone(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::relative_to;
    use std::path::Path;

    #[test]
    fn a_path_under_the_account_directory_is_stripped() {
        let dir = Path::new("C:/data/accounts/2");
        assert_eq!(
            relative_to("C:/data/accounts/2/raw/UID1.eml", dir),
            "raw/UID1.eml"
        );
    }

    #[test]
    fn a_path_from_another_machine_keeps_its_subdirectory() {
        // The case that matters: reading an archive written by someone else, whose rows name a
        // drive and a user that do not exist here. Dropping "attachments/" would lose the file.
        let dir = Path::new("C:/mine/accounts/2");
        assert_eq!(
            relative_to(
                "D:/Users/Someone/AppData/x/accounts/7/attachments/a-0-q.pdf",
                dir
            ),
            "attachments/a-0-q.pdf"
        );
        assert_eq!(
            relative_to("/home/other/.local/share/x/accounts/1/raw/UID9.eml", dir),
            "raw/UID9.eml"
        );
    }

    #[test]
    fn an_empty_path_stays_empty() {
        assert_eq!(relative_to("", Path::new("C:/data/accounts/2")), "");
    }
}
