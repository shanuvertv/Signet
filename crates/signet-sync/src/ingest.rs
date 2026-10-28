//! The ingestion pipeline: poll, dedupe, download, parse, store, apply retention.
//!
//! The whole thing is built around one invariant: **re-running a sync must never duplicate mail
//! and must never lose it.** `pop_seen` keyed on UIDL is what delivers that, and every step is
//! ordered so that a crash leaves the maildrop intact rather than the database.

use std::path::{Path, PathBuf};

use signet_store::{
    Account, FolderKind, NewAttachment, NewMessage, Recipient, Security as StoreSecurity, Store,
};
use tokio::io::AsyncWriteExt;

use crate::junk_signals::{Reputation, Verdict};
use crate::pop3_transport::{Pop3Credentials, Pop3Transport};
use crate::transport::{FolderId, MailTransport, RemoteId};
use crate::{Error, Result, junk, junk_signals, rules};
use signet_mime::signals::Signals;

/// Messages larger than this are stored with headers only rather than parsed in full.
///
/// `mail-parser` needs the whole message as one slice, so parsing peaks at roughly the message
/// size in memory. That is fine on a desktop and not fine on a phone, so there is a ceiling.
/// The raw message is still saved to disk in full either way.
pub const MAX_PARSE_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct SyncReport {
    pub new_messages: usize,
    pub already_seen: usize,
    pub deleted_from_server: usize,
    pub bytes_downloaded: u64,
    pub oversized: usize,
    /// Filed into Junk by the filter.
    pub junked: usize,
    /// Filed somewhere other than the Inbox by a rule.
    pub filed_by_rule: usize,
    /// IMAP: local copies removed because the message is gone from the server.
    pub removed_on_server: usize,
    /// IMAP: local moves, deletes and sent copies applied to the server this sync.
    pub pushed_to_server: usize,
    /// Per-message failures. One bad message must not abort the whole sync.
    pub errors: Vec<String>,
}

/// Poll a POP3 account and ingest whatever is new.
///
/// Dispatch by protocol happens in [`crate::sync_account`].
pub async fn sync_pop3(
    store: &Store,
    account: &Account,
    password: &str,
    progress: &crate::progress::Progressing,
) -> Result<SyncReport> {
    use crate::progress::{Phase, Progress};

    let mut report = SyncReport::default();

    let config = crate::pop3_config_for(account);
    let credentials = Pop3Credentials {
        user: account.pop_user.clone(),
        password: password.to_string(),
        allow_plaintext: account.pop_security == StoreSecurity::Plain,
    };

    progress.phase(account.id, Phase::Connecting);
    let mut transport = Pop3Transport::new(config, credentials);
    transport.connect().await?;

    let ctx = StoreContext::new(store, account).await?;
    let inbox_folder = ctx.inbox_folder;

    progress.phase(account.id, Phase::Listing);
    let remote_folder = FolderId::inbox();
    let listing = transport.list_messages(&remote_folder).await?;
    let seen = store.seen_uidls(account.id).map_err(Error::Store)?;

    let mut ingested_uidls = Vec::new();

    // Counted before the loop rather than reported as a fraction of everything on the server: on a
    // mailbox left on the server for the whole team, most of the listing is already downloaded, and
    // a bar that spends its life at 98% tells you nothing.
    let pending: Vec<_> = listing.iter().filter(|r| !seen.contains(&r.id.0)).collect();
    let to_fetch = pending.len();
    // The listing carries a size for every message, so the byte total is known before the first
    // fetch. That is what the bar is drawn from: one 40 MB drawing among forty short replies makes
    // a message count nearly meaningless.
    let bytes_total: u64 = pending.iter().map(|r| r.size).sum();
    let mut fetched = 0usize;
    let mut bytes_done: u64 = 0;

    for remote in &listing {
        if seen.contains(&remote.id.0) {
            report.already_seen += 1;
            continue;
        }

        fetched += 1;
        progress.send(
            Progress::counted(account.id, Phase::Downloading, fetched, to_fetch)
                .with_bytes(bytes_done, bytes_total),
        );
        bytes_done += remote.size;

        match ingest_one(
            store,
            account,
            inbox_folder,
            &mut transport,
            &remote_folder,
            &remote.id,
            remote.size,
            &ctx,
        )
        .await
        {
            Ok(outcome) => {
                report.new_messages += 1;
                report.bytes_downloaded += outcome.bytes;
                if outcome.oversized {
                    report.oversized += 1;
                }
                if outcome.junked {
                    report.junked += 1;
                }
                if outcome.filed_by_rule {
                    report.filed_by_rule += 1;
                }
                ingested_uidls.push(remote.id.0.clone());
            }
            Err(e) => {
                // Keep going: one unparseable message must not block the rest of the mailbox.
                tracing::warn!(uidl = %remote.id.0, error = %e, "could not ingest message");
                report.errors.push(format!("{}: {e}", remote.id.0));
            }
        }
    }

    // Retention. Only ever delete what we have already stored locally.
    let to_delete = retention_targets(store, account, &listing, &ingested_uidls)?;
    for uidl in &to_delete {
        if let Some(remote) = listing.iter().find(|m| &m.id.0 == uidl) {
            match transport.delete(&remote_folder, &remote.id).await {
                Ok(()) => report.deleted_from_server += 1,
                Err(e) => {
                    tracing::warn!(uidl, error = %e, "could not mark for deletion");
                    report.errors.push(format!("delete {uidl}: {e}"));
                }
            }
        }
    }

    // QUIT is what commits the DELEs. If it fails, nothing was deleted server-side, so the local
    // bookkeeping must not claim otherwise.
    match Box::new(transport).commit().await {
        Ok(()) => {
            store
                .mark_deleted_from_server(account.id, &to_delete)
                .map_err(Error::Store)?;
        }
        Err(e) => {
            tracing::warn!(error = %e, "QUIT failed; server-side deletions were rolled back");
            report.deleted_from_server = 0;
            report.errors.push(format!("quit: {e}"));
        }
    }

    store.touch_account_sync(account.id).map_err(Error::Store)?;
    Ok(report)
}

struct Outcome {
    bytes: u64,
    oversized: bool,
    junked: bool,
    filed_by_rule: bool,
}

/// Rules for an account, parsed from their stored JSON.
///
/// A rule that no longer parses is skipped with a warning rather than failing the sync: a bad row
/// left by an older version must not stop mail arriving.
fn load_rules(store: &Store, account_id: i64) -> Vec<rules::Rule> {
    match store.list_rules(Some(account_id)) {
        Ok(rows) => rows
            .into_iter()
            .filter_map(
                |(id, _, json)| match serde_json::from_str::<rules::Rule>(&json) {
                    Ok(mut rule) => {
                        rule.id = id;
                        Some(rule)
                    }
                    Err(e) => {
                        tracing::warn!(rule_id = id, error = %e, "skipping unparseable rule");
                        None
                    }
                },
            )
            .collect(),
        Err(e) => {
            tracing::warn!(error = %e, "could not load rules; ingesting without them");
            Vec::new()
        }
    }
}

/// Everything an ingest needs that is worth computing once per sync rather than per message.
#[derive(Debug)]
pub struct StoreContext {
    pub raw_dir: PathBuf,
    pub att_dir: PathBuf,
    /// The account's Inbox, used as the fallback when a rule or the junk filter names a folder
    /// that does not exist.
    pub inbox_folder: i64,
    pub rules: Vec<rules::Rule>,
    pub corpus: junk::CorpusSize,
}

impl StoreContext {
    pub async fn new(store: &Store, account: &Account) -> Result<Self> {
        let blob_dir = store.blob_dir(account.id);
        let raw_dir = blob_dir.join("raw");
        let att_dir = blob_dir.join("attachments");
        tokio::fs::create_dir_all(&raw_dir).await?;
        tokio::fs::create_dir_all(&att_dir).await?;

        Ok(Self {
            raw_dir,
            att_dir,
            inbox_folder: store
                .folder_id(account.id, FolderKind::Inbox)
                .map_err(Error::Store)?,
            rules: load_rules(store, account.id),
            corpus: store
                .corpus_size()
                .map(|(spam, ham)| junk::CorpusSize {
                    spam_messages: spam,
                    ham_messages: ham,
                })
                .unwrap_or_default(),
        })
    }
}

/// What storing a message decided.
#[derive(Debug)]
pub struct Stored {
    pub message_id: i64,
    pub junked: bool,
    pub filed_by_rule: bool,
}

/// Write a parsed message into the store, applying rules and the junk filter.
///
/// Shared by both protocols. POP3 and IMAP differ in how they *find* messages — one maildrop of
/// opaque UIDLs versus many folders of UIDs — but what happens to a message once it has been
/// downloaded and parsed is identical, and duplicating it would mean a rule that works over POP3
/// quietly not working over IMAP.
///
/// `apply_filters` is false for anything outside the Inbox: mail the user or the server already
/// filed has been triaged, and re-filing it would fight them.
#[allow(clippy::too_many_arguments)]
pub async fn store_message(
    store: &Store,
    account: &Account,
    folder_id: i64,
    ctx: &StoreContext,
    parsed: &signet_mime::ParsedMessage,
    raw_path: &Path,
    stem: &str,
    size_bytes: u64,
    oversized: bool,
    apply_filters: bool,
) -> Result<Stored> {
    let received_utc = signet_store::now_utc();
    let (body_html, has_remote_content) = match &parsed.body_html {
        Some(html) => {
            let clean = signet_mime::sanitize(html, false);
            (Some(clean.html), clean.had_remote_content)
        }
        None => (None, false),
    };

    let mut attachments = Vec::new();
    for (index, att) in parsed.attachments.iter().enumerate() {
        let path = ctx.att_dir.join(format!("{stem}-{index}-{}", att.filename));
        tokio::fs::write(&path, &att.data).await?;
        attachments.push(NewAttachment {
            filename: att.filename.clone(),
            mime_type: att.mime_type.clone(),
            size_bytes: att.data.len() as i64,
            content_id: att.content_id.clone(),
            blob_path: path.to_string_lossy().to_string(),
            is_inline: att.is_inline,
        });
    }

    let mut recipients = Vec::new();
    for (kind, list) in [("to", &parsed.to), ("cc", &parsed.cc), ("bcc", &parsed.bcc)] {
        for a in list {
            recipients.push(Recipient {
                kind: kind.to_string(),
                name: a.name.clone(),
                addr: a.addr.clone(),
            });
        }
    }

    let from = parsed.from.clone().unwrap_or(signet_mime::Addr {
        name: String::new(),
        addr: String::new(),
    });

    let body_text = if oversized {
        format!(
            "This message is {:.1} MB and was downloaded but not rendered inline. \
             The full message is saved at {}",
            size_bytes as f64 / (1024.0 * 1024.0),
            raw_path.display()
        )
    } else {
        parsed.body_text.clone()
    };

    // Rules first: an explicit user rule outranks a statistical guess, and `NeverJunk` has to be
    // known before the filter runs.
    let candidate = rules::Candidate {
        from: format!("{} {}", from.name, from.addr),
        to: parsed.to.iter().map(|a| a.addr.clone()).collect(),
        cc: parsed.cc.iter().map(|a| a.addr.clone()).collect(),
        subject: parsed.subject.clone(),
        body: parsed.body_text.chars().take(8000).collect(),
    };
    let ruled = if apply_filters {
        rules::evaluate(&ctx.rules, &candidate)
    } else {
        rules::Outcome::default()
    };

    let features = junk::Features {
        from_name: from.name.clone(),
        from_addr: from.addr.clone(),
        subject: parsed.subject.clone(),
        body: parsed.body_text.clone(),
        has_remote_content,
        recipients: parsed
            .to
            .iter()
            .chain(&parsed.cc)
            .map(|a| a.addr.clone())
            .collect(),
        account_addr: account.email.clone(),
    };

    let mut spam_score = 0.0;
    let mut junked = false;
    let mut verdict: Option<Verdict> = None;
    if apply_filters && !ruled.never_junk && ruled.move_to.is_none() {
        // What the user has said about this sender, and whether we know them, outrank the words.
        let reputation = match store.sender_rule_for(&from.addr).ok().flatten().as_deref() {
            Some("block") => Reputation::Blocked,
            Some("allow") => Reputation::Allowed,
            _ if store.is_known_correspondent(&from.addr).unwrap_or(false) => Reputation::Known,
            _ => Reputation::Unknown,
        };

        // A block is the user's own decision rather than a guess, so it stands even when the
        // statistical filter is switched off — otherwise "block this sender" would quietly do
        // nothing. Everything below it is a guess, and guesses need the filter on.
        let v = if reputation == Reputation::Blocked {
            Some(junk_signals::score(0.0, &Signals::default(), reputation))
        } else if account.junk_filter_enabled {
            let tokens: Vec<String> = junk::tokenize(&features).into_iter().collect();
            let counts = store.token_counts(&tokens).unwrap_or_default();
            let base = junk::combined_score(&features, &counts, ctx.corpus);
            let signals = signet_mime::signals::extract(parsed, &parsed.raw_headers);
            Some(junk_signals::score(base, &signals, reputation))
        } else {
            None
        };

        if let Some(v) = v {
            spam_score = v.score;
            junked = spam_score >= account.junk_threshold;
            verdict = Some(v);
        }
    }

    let (target_folder, filed_by) = if junked {
        (
            store
                .folder_id(account.id, FolderKind::Junk)
                .unwrap_or(folder_id),
            match verdict.as_ref() {
                // A block rule explains itself, and the reader offers to undo it, so naming the
                // filter here would be misleading — nothing was guessed.
                Some(v) if v.decided_by_rule => "your block list".to_string(),
                Some(v) if !v.explain().is_empty() => format!("junk filter: {}", v.explain()),
                _ => format!("junk filter (score {spam_score:.2})"),
            },
        )
    } else if let Some(kind) = ruled.move_to {
        (
            store.folder_id(account.id, kind).unwrap_or(folder_id),
            format!("rule: {}", ruled.matched.join(", ")),
        )
    } else {
        (folder_id, String::new())
    };
    let filed_by_rule = ruled.move_to.is_some() && !junked;

    let sender_addr = from.addr.clone();
    let new = NewMessage {
        account_id: account.id,
        folder_id: target_folder,
        rfc822_message_id: parsed.message_id.clone(),
        in_reply_to: parsed.in_reply_to.clone(),
        thread_id: parsed.thread_id(),
        from_name: from.name,
        from_addr: from.addr.clone(),
        subject: parsed.subject.clone(),
        // A message with no usable Date must not sort to 1970.
        date_utc: parsed.date_utc.unwrap_or(received_utc),
        received_utc,
        has_attachments: !attachments.is_empty(),
        has_remote_content,
        size_bytes: size_bytes as i64,
        preview_text: if oversized {
            body_text.clone()
        } else {
            parsed.preview()
        },
        body_text,
        body_html,
        raw_path: raw_path.to_string_lossy().to_string(),
        recipients,
        attachments,
    };

    let message_id = store.insert_message(&new).map_err(Error::Store)?;

    // The sender's own X-Priority / Importance header, when they set one. Users can override it,
    // and a rule or the junk filter never touches it.
    if let Some(p) = parsed.priority {
        let _ = store.set_priority(&[message_id], signet_store::Priority::from_i64(p));
    }

    if spam_score > 0.0 || !filed_by.is_empty() {
        let _ = store.set_spam_score(message_id, spam_score, &filed_by);
    }
    // Record why, so "this should not be in Junk" has an answer beyond a number.
    if let Some(v) = verdict.as_ref().filter(|v| !v.decided_by_rule) {
        // In Junk the question is "why is this here?", and the reasons that argued for delivery
        // are part of that answer. In the Inbox the question is "what should I watch for?", and
        // only a message showing the shape of a forgery has an answer worth interrupting for.
        // After the escalation in `junk_signals`, a delivered message with two hard signals is
        // one held back solely because we know the sender — which is exactly the invoice-fraud
        // case, where the sender is a real supplier whose account has been taken over.
        let why = if junked {
            v.explain()
        } else if v.hard_signals >= 2 {
            v.warnings()
        } else {
            String::new()
        };
        if !why.is_empty() {
            let _ = store.set_spam_reasons(message_id, &why);
        }
    }
    // Read the sender's signature into the address book. Only the sender's, and only from a
    // detected signature block: a forwarded enquiry carries the original author's sign-off, and
    // filing that against whoever forwarded it would be worse than not reading it at all.
    let details = signet_mime::signature::extract(&parsed.body_text);
    if !details.is_empty() {
        let _ = store.enrich_contact(
            &sender_addr,
            &signet_store::HarvestedDetails {
                job_title: details.job_title,
                company: details.company,
                phone: details.phone,
                mobile: details.mobile,
                website: details.website,
            },
        );
    }

    if ruled.mark_read {
        let _ = store.set_read(&[message_id], true);
    }
    if ruled.flag {
        let _ = store.set_flagged(&[message_id], true);
    }

    Ok(Stored {
        message_id,
        junked,
        filed_by_rule,
    })
}

#[allow(clippy::too_many_arguments)]
async fn ingest_one(
    store: &Store,
    account: &Account,
    folder_id: i64,
    transport: &mut Pop3Transport,
    remote_folder: &FolderId,
    id: &RemoteId,
    reported_size: u64,
    ctx: &StoreContext,
) -> Result<Outcome> {
    let stem = uidl_stem(&id.0);
    let raw_path = ctx.raw_dir.join(format!("{stem}.eml"));

    // Stream straight to disk. Nothing bigger than one line is held in memory here, which is what
    // makes a 200 MB attachment survivable.
    let bytes = {
        let file = tokio::fs::File::create(&raw_path).await?;
        let mut writer = tokio::io::BufWriter::new(file);
        let n = transport
            .fetch_message(remote_folder, id, &mut writer)
            .await?;
        writer.flush().await?;
        n
    };

    let oversized = bytes > MAX_PARSE_BYTES;
    let parsed = if oversized {
        // Headers only, so the message still appears in the list with a real sender and subject.
        let headers = transport.fetch_headers(remote_folder, id).await?;
        signet_mime::parse(&headers)
    } else {
        let raw = tokio::fs::read(&raw_path).await?;
        signet_mime::parse(&raw)
    }
    .ok_or_else(|| Error::Parse(format!("message {} is not parseable", id.0)))?;

    let stored = store_message(
        store,
        account,
        folder_id,
        ctx,
        &parsed,
        &raw_path,
        &stem,
        reported_size.max(bytes),
        oversized,
        true,
    )
    .await?;
    let message_id = stored.message_id;

    // Marked seen only after the message is durably stored. The other order would lose mail on a
    // crash; this order at worst re-downloads it.
    store
        .mark_seen(account.id, &id.0, Some(message_id))
        .map_err(Error::Store)?;

    Ok(Outcome {
        bytes,
        oversized,
        junked: stored.junked,
        filed_by_rule: stored.filed_by_rule,
    })
}

/// Which UIDLs should be deleted from the server this session.
fn retention_targets(
    store: &Store,
    account: &Account,
    listing: &[crate::transport::RemoteMessage],
    ingested: &[String],
) -> Result<Vec<String>> {
    if !account.leave_on_server {
        // Delete-on-download: only what we just stored successfully.
        return Ok(ingested.to_vec());
    }

    let Some(days) = account.delete_after_days else {
        return Ok(Vec::new()); // Keep everything.
    };

    let cutoff = signet_store::now_utc() - days * 86_400;
    let old = store
        .uidls_older_than(account.id, cutoff)
        .map_err(Error::Store)?;

    // Only delete what is actually still on the server.
    let present: std::collections::HashSet<&str> =
        listing.iter().map(|m| m.id.0.as_str()).collect();
    Ok(old
        .into_iter()
        .filter(|u| present.contains(u.as_str()))
        .collect())
}

/// A filesystem-safe stem derived from a UIDL.
///
/// UIDLs are server-chosen and may contain anything printable, including path separators. The
/// hash keeps the name bounded and safe while staying stable, so a re-download overwrites rather
/// than accumulating.
fn uidl_stem(uidl: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325; // FNV-1a
    for b in uidl.as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    let readable: String = uidl
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(16)
        .collect();
    format!("{readable}{hash:016x}")
}

/// Where an account's blobs live, for callers outside this module.
pub fn blob_dirs(store: &Store, account_id: i64) -> (PathBuf, PathBuf) {
    let base = store.blob_dir(account_id);
    (base.join("raw"), base.join("attachments"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uidl_stems_are_safe_stable_and_distinct() {
        // Server-chosen UIDLs have no character restrictions worth relying on.
        let nasty = uidl_stem("../../etc/passwd");
        assert!(
            !nasty.contains('/'),
            "stem could escape the directory: {nasty}"
        );
        assert!(!nasty.contains('.'));

        assert_eq!(uidl_stem("ABC123"), uidl_stem("ABC123"), "must be stable");
        assert_ne!(uidl_stem("ABC123"), uidl_stem("ABC124"));
        // Different UIDLs that share a readable prefix must still differ.
        assert_ne!(uidl_stem("same-prefix-a"), uidl_stem("same-prefix-b"));
    }

    #[test]
    fn retention_keeps_everything_when_configured_to() {
        let store = Store::open_in_memory().unwrap();
        let account = test_account(&store, true, None);
        let targets = retention_targets(&store, &account, &[], &["UID-1".into()]).unwrap();
        assert!(targets.is_empty(), "leave_on_server must delete nothing");
    }

    #[test]
    fn delete_on_download_targets_exactly_what_was_stored() {
        let store = Store::open_in_memory().unwrap();
        let account = test_account(&store, false, None);
        let ingested = vec!["UID-1".to_string(), "UID-2".to_string()];
        let targets = retention_targets(&store, &account, &[], &ingested).unwrap();
        assert_eq!(targets, ingested);
    }

    #[test]
    fn age_based_retention_only_targets_messages_still_on_the_server() {
        let store = Store::open_in_memory().unwrap();
        let account = test_account(&store, true, Some(30));

        // Both first seen 90 days ago, well past the 30-day cutoff.
        let long_ago = signet_store::now_utc() - 90 * 86_400;
        store
            .mark_seen_at(account.id, "OLD-PRESENT", None, long_ago)
            .unwrap();
        store
            .mark_seen_at(account.id, "OLD-GONE", None, long_ago)
            .unwrap();

        let listing = vec![crate::transport::RemoteMessage {
            id: RemoteId("OLD-PRESENT".into()),
            size: 100,
            flags: Vec::new(),
        }];
        let targets = retention_targets(&store, &account, &listing, &[]).unwrap();
        assert_eq!(targets, vec!["OLD-PRESENT".to_string()]);
    }

    #[test]
    fn age_based_retention_spares_recent_mail() {
        let store = Store::open_in_memory().unwrap();
        let account = test_account(&store, true, Some(30));
        store.mark_seen(account.id, "RECENT", None).unwrap();

        let listing = vec![crate::transport::RemoteMessage {
            id: RemoteId("RECENT".into()),
            size: 100,
            flags: Vec::new(),
        }];
        assert!(
            retention_targets(&store, &account, &listing, &[])
                .unwrap()
                .is_empty()
        );
    }

    fn test_account(store: &Store, leave: bool, days: Option<i64>) -> Account {
        let id = store
            .add_account(
                &signet_store::NewAccount {
                    display_name: "T".into(),
                    email: "t@example.com".into(),
                    pop_host: "mail.test".into(),
                    pop_port: 995,
                    pop_security: StoreSecurity::Tls,
                    pop_user: "t".into(),
                    smtp_host: "mail.test".into(),
                    smtp_port: 587,
                    smtp_security: StoreSecurity::Starttls,
                    smtp_user: "t".into(),
                    leave_on_server: leave,
                    delete_after_days: days,
                    poll_minutes: 5,
                    signature: String::new(),
                    protocol: Default::default(),
                },
                "handle",
            )
            .unwrap();
        store.get_account(id).unwrap()
    }
}
