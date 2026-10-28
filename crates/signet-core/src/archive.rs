//! Exporting and importing the data file.
//!
//! Outlook has one `.pst` you can copy to another machine; we have a SQLite database and a tree of
//! blobs, so this module packs both into a single `.signet` file and unpacks it again.
//!
//! Three decisions shape everything here.
//!
//! **The archive is a plain zip laid out exactly like the data directory.** Not a bespoke
//! container: a backup you cannot open without the program that wrote it is a bad backup. Anyone
//! can unzip one in Explorer and pull an `.eml` straight out, and restoring by hand is a matter of
//! copying the contents into place.
//!
//! **Import merges and never destroys.** It adds accounts it does not have and messages it has not
//! seen, and leaves everything else alone. Replacing would be far less code, but "import the wrong
//! file" would then be unrecoverable — and the common reasons to import (a new laptop, a restored
//! backup, a colleague's archive) are all served by merging.
//!
//! **Passwords are not in the archive.** They live in Windows Credential Manager, and writing them
//! into a file that gets emailed around would undo the reason for keeping them there. Imported
//! accounts arrive disabled, and say so, until a password is entered.

use std::collections::HashSet;
use std::fs::File;
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};

use signet_store::{Account, NewAccount, PortableMessage, Signature, Store};
use serde::{Deserialize, Serialize};

use crate::{App, Error, Event, Result};

/// Bumped only for a change an older build could not read correctly.
pub const FORMAT: u32 = 1;

/// What the archive says about itself, so an import can refuse a file it does not understand and
/// describe one it does before touching anything.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub format: u32,
    pub app_version: String,
    /// The database `user_version` at export time, for diagnosing a file from a future build.
    pub schema_version: i64,
    pub exported_utc: i64,
    pub accounts: Vec<ArchivedAccount>,
    pub messages: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchivedAccount {
    pub email: String,
    pub display_name: String,
    pub messages: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExportSummary {
    pub path: String,
    pub accounts: i64,
    pub messages: i64,
    pub files: i64,
    pub bytes: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ImportSummary {
    pub accounts_added: i64,
    pub accounts_matched: i64,
    pub messages_imported: i64,
    pub messages_skipped: i64,
    pub signatures: i64,
    pub sender_rules: i64,
    /// Accounts that arrived without a password and cannot sync until one is entered.
    pub needs_password: Vec<String>,
}

impl App {
    /// Read an archive's manifest without importing it, so the user can be told what is in a file
    /// before agreeing to merge it.
    pub fn inspect_archive(&self, path: String) -> Result<Manifest> {
        let file = File::open(&path)?;
        let mut zip = zip::ZipArchive::new(file).map_err(zip_err)?;
        read_manifest(&mut zip)
    }

    /// Write the whole data file to a single `.signet` archive.
    pub fn export_data_file(&self, path: String) -> Result<ExportSummary> {
        let dest = PathBuf::from(&path);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let work = self.store().data_dir().join(".export");
        let _ = std::fs::remove_dir_all(&work);
        std::fs::create_dir_all(&work)?;
        let snapshot = work.join("mail.sqlite");

        // Done first and separately: the snapshot has to be consistent, and it is the only part of
        // the archive that cannot simply be copied.
        self.store().snapshot_to(&snapshot)?;

        let result = self.write_archive(&dest, &snapshot);
        let _ = std::fs::remove_dir_all(&work);
        result
    }

    fn write_archive(&self, dest: &Path, snapshot: &Path) -> Result<ExportSummary> {
        let accounts = self.store().list_accounts()?;
        let mut manifest = Manifest {
            format: FORMAT,
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            schema_version: self.store().schema_version()?,
            exported_utc: signet_store::now_utc(),
            accounts: Vec::new(),
            messages: 0,
        };

        let mut zip = zip::ZipWriter::new(File::create(dest)?);
        // Deflate for everything: attachments are usually already compressed, but the database and
        // the raw messages are text and compress well, and a mixed policy is not worth the code.
        let opts: zip::write::FileOptions<()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);

        let mut files = 0i64;
        let mut total_messages = 0i64;

        for a in &accounts {
            let count = self.store().portable_messages(a.id)?.len() as i64;
            total_messages += count;
            manifest.accounts.push(ArchivedAccount {
                email: a.email.clone(),
                display_name: a.display_name.clone(),
                messages: count,
            });
        }
        manifest.messages = total_messages;

        zip.start_file("manifest.json", opts).map_err(zip_err)?;
        zip.write_all(serde_json::to_string_pretty(&manifest)?.as_bytes())?;
        files += 1;

        zip.start_file("mail.sqlite", opts).map_err(zip_err)?;
        copy_into(&mut zip, snapshot)?;
        files += 1;

        for a in &accounts {
            let dir = self.store().blob_dir(a.id);
            if !dir.exists() {
                continue;
            }
            for entry in walk(&dir)? {
                let rel = entry
                    .strip_prefix(self.store().data_dir())
                    .unwrap_or(&entry)
                    .to_string_lossy()
                    .replace('\\', "/");
                zip.start_file(rel, opts).map_err(zip_err)?;
                copy_into(&mut zip, &entry)?;
                files += 1;
            }
        }

        zip.finish().map_err(zip_err)?;

        Ok(ExportSummary {
            path: dest.to_string_lossy().into_owned(),
            accounts: accounts.len() as i64,
            messages: total_messages,
            files,
            bytes: std::fs::metadata(dest).map(|m| m.len()).unwrap_or(0),
        })
    }

    /// Merge an archive into this data file. Nothing is removed or overwritten.
    pub fn import_data_file(&self, path: String) -> Result<ImportSummary> {
        let work = self.store().data_dir().join(".import");
        let _ = std::fs::remove_dir_all(&work);
        std::fs::create_dir_all(&work)?;

        let result = self.import_from(&path, &work);
        let _ = std::fs::remove_dir_all(&work);
        result
    }

    fn import_from(&self, path: &str, work: &Path) -> Result<ImportSummary> {
        let file = File::open(path)?;
        let mut zip = zip::ZipArchive::new(file).map_err(zip_err)?;

        let manifest = read_manifest(&mut zip)?;
        if manifest.format > FORMAT {
            return Err(Error::Invalid(format!(
                "this archive was written by Signet {} and needs a newer version to read \
                 (archive format {}, this build understands {FORMAT})",
                manifest.app_version, manifest.format
            )));
        }

        extract(&mut zip, work)?;
        if !work.join("mail.sqlite").exists() {
            return Err(Error::Invalid(
                "this file is not an Signet archive: it contains no database".into(),
            ));
        }

        // Opening the extracted directory as an ordinary store means migrations run on it, so an
        // archive from an older build is brought up to the current schema before anything is read
        // out of it — the same path a real upgrade takes.
        let src = Store::open(work)?;
        let mut summary = ImportSummary::default();

        for src_account in src.list_accounts()? {
            let (dest_id, added) = self.match_or_create_account(&src_account)?;
            if added {
                summary.accounts_added += 1;
                summary.needs_password.push(src_account.email.clone());
            } else {
                summary.accounts_matched += 1;
            }
            self.import_account(&src, &src_account, dest_id, &mut summary)?;
        }

        self.import_signatures(&src, &mut summary)?;
        self.import_sender_rules(&src, &mut summary)?;

        self.emit_public(Event::MessagesChanged { account_id: 0 });
        Ok(summary)
    }

    /// Find the local account this archived one corresponds to, creating it if there is none.
    ///
    /// Matching is on the email address: it is the one thing about an account that is the same on
    /// every machine, whereas ids, display names and even server host names may not be.
    fn match_or_create_account(&self, src: &Account) -> Result<(i64, bool)> {
        let existing = self
            .store()
            .list_accounts()?
            .into_iter()
            .find(|a| a.email.eq_ignore_ascii_case(&src.email));
        if let Some(a) = existing {
            return Ok((a.id, false));
        }

        let id = self.store().add_account(
            &NewAccount {
                display_name: src.display_name.clone(),
                email: src.email.clone(),
                pop_host: src.pop_host.clone(),
                pop_port: src.pop_port,
                pop_security: src.pop_security,
                pop_user: src.pop_user.clone(),
                smtp_host: src.smtp_host.clone(),
                smtp_port: src.smtp_port,
                smtp_security: src.smtp_security,
                smtp_user: src.smtp_user.clone(),
                leave_on_server: src.leave_on_server,
                delete_after_days: src.delete_after_days,
                poll_minutes: src.poll_minutes,
                signature: src.signature.clone(),
                protocol: src.protocol,
            },
            // Empty: there is no password in the archive, and inventing a handle would point the
            // keystore at nothing. The account is disabled below until one is entered.
            "",
        )?;

        let mut settings = self.account_settings(id)?;
        settings.is_enabled = false;
        self.store().update_account_settings(id, &settings)?;
        Ok((id, true))
    }

    fn import_account(
        &self,
        src: &Store,
        src_account: &Account,
        dest_id: i64,
        summary: &mut ImportSummary,
    ) -> Result<()> {
        let known = self.store().known_rfc822_ids(dest_id)?;
        // Mail with no Message-ID cannot be matched the reliable way, so fall back to something
        // that is stable for the same message: who sent it, when, and about what.
        let mut weak: HashSet<String> = HashSet::new();
        for m in self.store().portable_messages(dest_id)? {
            if m.new.rfc822_message_id.is_none() {
                weak.insert(weak_key(&m));
            }
        }

        let src_blobs = src.blob_dir(src_account.id);
        let dest_blobs = self.store().blob_dir(dest_id);

        for m in src.portable_messages(src_account.id)? {
            let duplicate = match &m.new.rfc822_message_id {
                Some(id) => known.contains(id),
                None => weak.contains(&weak_key(&m)),
            };
            if duplicate {
                summary.messages_skipped += 1;
                continue;
            }
            if m.new.rfc822_message_id.is_none() {
                weak.insert(weak_key(&m));
            }

            let folder_id = self.folder_for(dest_id, &m.folder_kind, &m.folder_name)?;
            let mut new = m.new.clone();
            new.account_id = dest_id;
            new.folder_id = folder_id;

            new.raw_path = match copy_blob(&src_blobs, &dest_blobs, &m.raw_rel)? {
                Some(p) => p.to_string_lossy().into_owned(),
                None => String::new(),
            };
            for (att, rel) in new.attachments.iter_mut().zip(&m.attachment_rels) {
                att.blob_path = match copy_blob(&src_blobs, &dest_blobs, rel)? {
                    Some(p) => p.to_string_lossy().into_owned(),
                    None => String::new(),
                };
            }

            let id = self.store().insert_message(&new)?;
            if m.is_read {
                self.store().set_read(&[id], true)?;
            }
            if m.is_flagged {
                self.store().set_flagged(&[id], true)?;
            }
            if m.is_answered {
                self.store().set_answered(&[id], true)?;
            }
            if m.spam_score > 0.0 || !m.filed_by.is_empty() {
                self.store().set_spam_score(id, m.spam_score, &m.filed_by)?;
            }
            if !m.spam_reasons.is_empty() {
                self.store().set_spam_reasons(id, &m.spam_reasons)?;
            }

            summary.messages_imported += 1;
        }
        Ok(())
    }

    fn folder_for(&self, account_id: i64, kind: &str, name: &str) -> Result<i64> {
        if let Some(id) = self
            .store()
            .folder_by_kind_and_name(account_id, kind, name)?
        {
            return Ok(id);
        }
        Ok(self.store().create_folder(account_id, kind, name)?)
    }

    /// Signatures are shared by every account, so they are imported once, by name.
    fn import_signatures(&self, src: &Store, summary: &mut ImportSummary) -> Result<()> {
        let mine: HashSet<String> = self
            .store()
            .list_signatures()?
            .into_iter()
            .map(|s| s.name.to_lowercase())
            .collect();

        for s in src.list_signatures()? {
            if mine.contains(&s.name.to_lowercase()) {
                continue;
            }
            self.store().save_signature(&Signature {
                id: 0,
                name: s.name,
                body: s.body,
                // Never imported as the default: which signature is yours by default is a local
                // preference, and silently changing what gets attached to outgoing mail is not
                // something an import should do.
                is_default: false,
                sort_order: s.sort_order,
            })?;
            summary.signatures += 1;
        }
        Ok(())
    }

    fn import_sender_rules(&self, src: &Store, summary: &mut ImportSummary) -> Result<()> {
        let mine: HashSet<String> = self
            .store()
            .list_sender_rules()?
            .into_iter()
            .map(|r| r.pattern)
            .collect();

        for r in src.list_sender_rules()? {
            // An existing local rule wins: it is a decision this user made on this machine, and an
            // archive should not be able to quietly unblock a sender they blocked.
            if mine.contains(&r.pattern) {
                continue;
            }
            self.store().set_sender_rule(&r.pattern, &r.kind)?;
            summary.sender_rules += 1;
        }
        Ok(())
    }
}

/// A stand-in identity for a message with no `Message-ID`.
fn weak_key(m: &PortableMessage) -> String {
    format!(
        "{}|{}|{}",
        m.new.from_addr.to_lowercase(),
        m.new.date_utc,
        m.new.subject
    )
}

/// Copy one blob across, returning where it landed, or `None` if the archive did not contain it.
fn copy_blob(src_dir: &Path, dest_dir: &Path, rel: &str) -> Result<Option<PathBuf>> {
    if rel.is_empty() {
        return Ok(None);
    }
    let from = src_dir.join(rel);
    if !from.exists() {
        return Ok(None);
    }
    let to = unique_dest(&dest_dir.join(rel));
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(&from, &to)?;
    Ok(Some(to))
}

/// A free path near `wanted`.
///
/// Blob names come from server-chosen ids, which are only unique within one server. Two machines
/// can therefore hold different messages under the same file name, and overwriting would destroy a
/// message that is not being imported at all.
fn unique_dest(wanted: &Path) -> PathBuf {
    if !wanted.exists() {
        return wanted.to_path_buf();
    }
    let stem = wanted
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = wanted
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    let dir = wanted.parent().unwrap_or(Path::new("."));

    for n in 1.. {
        let candidate = dir.join(format!("{stem}-{n}{ext}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    unreachable!()
}

fn read_manifest<R: Read + Seek>(zip: &mut zip::ZipArchive<R>) -> Result<Manifest> {
    let mut entry = zip.by_name("manifest.json").map_err(|_| {
        Error::Invalid("this file is not an Signet archive: it has no manifest".into())
    })?;
    let mut text = String::new();
    entry.read_to_string(&mut text)?;
    serde_json::from_str(&text)
        .map_err(|e| Error::Invalid(format!("the archive's manifest is unreadable: {e}")))
}

/// Unpack the archive under `dest`.
///
/// Entry names are taken from `enclosed_name`, which rejects absolute paths and anything climbing
/// out with `..`. A zip is an untrusted input even when it came from us, and the alternative is
/// letting a crafted archive write wherever it likes.
fn extract<R: Read + Seek>(zip: &mut zip::ZipArchive<R>, dest: &Path) -> Result<()> {
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).map_err(zip_err)?;
        let Some(rel) = entry.enclosed_name() else {
            return Err(Error::Invalid(format!(
                "the archive contains an unsafe path ({}) and was not imported",
                entry.name()
            )));
        };
        let out = dest.join(rel);
        if entry.is_dir() {
            std::fs::create_dir_all(&out)?;
            continue;
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut f = File::create(&out)?;
        std::io::copy(&mut entry, &mut f)?;
    }
    Ok(())
}

fn copy_into<W: Write + Seek>(zip: &mut zip::ZipWriter<W>, from: &Path) -> Result<()> {
    let mut f = File::open(from)?;
    std::io::copy(&mut f, zip)?;
    Ok(())
}

/// Every file under `dir`, recursively.
fn walk(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(path);
            }
        }
    }
    out.sort();
    Ok(out)
}

fn zip_err(e: zip::result::ZipError) -> Error {
    Error::Invalid(format!("archive error: {e}"))
}
