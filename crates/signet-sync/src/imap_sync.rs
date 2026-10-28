//! IMAP ingestion.
//!
//! Different in shape from POP3 in three ways that matter, and the same everywhere else:
//!
//! 1. **Folders come from the server.** They are discovered and mapped onto local rows rather than
//!    invented at account creation.
//! 2. **Flags are shared.** Read, flagged and answered live on the server, so they are pulled down
//!    on every sync and pushed back when changed locally. This is the whole reason a sales team
//!    sharing one mailbox should be on IMAP.
//! 3. **UIDVALIDITY can invalidate everything.** When the server bumps it, every UID we hold for
//!    that folder means nothing and the folder must be resynced.

use signet_store::{Account, FolderKind, Store};
use tokio::io::AsyncWriteExt;

use crate::imap_transport::{ImapCredentials, ImapTransport};
use crate::ingest::{StoreContext, SyncReport, store_message};
use crate::transport::{Flag, FolderId, MailTransport, RemoteId};
use crate::{Error, Result};

/// Poll every folder of an IMAP account.
pub async fn sync_account(
    store: &Store,
    account: &Account,
    password: &str,
    progress: &crate::progress::Progressing,
) -> Result<SyncReport> {
    use crate::progress::{Phase, Progress};

    let mut report = SyncReport::default();

    let config = crate::imap_config_for(account);
    let mut transport = ImapTransport::new(
        config,
        ImapCredentials {
            user: account.pop_user.clone(),
            password: password.to_string(),
            allow_plaintext: account.pop_security == signet_store::Security::Plain,
        },
    );
    progress.phase(account.id, Phase::Connecting);
    transport.connect().await?;

    let ctx = StoreContext::new(store, account).await?;

    // Local moves, deletes and sent copies go to the server before anything is read from it, so
    // this sync does not re-download what the user just filed or bring back what they deleted.
    replay_pending_ops(store, account, &mut transport, &mut report).await?;

    progress.phase(account.id, Phase::Listing);
    let folders = transport.list_folders().await?;
    tracing::info!(count = folders.len(), "imap folders discovered");

    for remote_folder in &folders {
        let kind = map_kind(remote_folder.kind);
        let local_folder = store
            .folder_for_remote(account.id, &remote_folder.id.0, &remote_folder.name, kind)
            .map_err(Error::Store)?;

        // Push local flag changes *before* reading the folder. `list_messages` captures flags as
        // they are at that moment, so pushing afterwards would leave this round applying the
        // pre-change state straight back over what the user just did.
        match push_dirty_flags(store, account, &mut transport, &remote_folder.id).await {
            Ok(n) if n > 0 => {
                tracing::info!(folder = %remote_folder.id.0, n, "pushed flag changes")
            }
            Err(e) => {
                tracing::warn!(folder = %remote_folder.id.0, error = %e, "could not push flags");
                report
                    .errors
                    .push(format!("{}: flag push failed: {e}", remote_folder.id.0));
            }
            _ => {}
        }

        let listing = match transport.list_messages(&remote_folder.id).await {
            Ok(l) => l,
            Err(e) => {
                // One unreadable folder — a permissions problem, usually — must not stop the rest
                // of the mailbox syncing.
                tracing::warn!(folder = %remote_folder.id.0, error = %e, "could not list folder");
                report.errors.push(format!("{}: {e}", remote_folder.id.0));
                continue;
            }
        };

        // UIDVALIDITY: if it changed, every id we hold for this folder is meaningless.
        if let Some(validity) = transport.uid_validity(&remote_folder.id.0) {
            let known = store.uid_validity(local_folder).map_err(Error::Store)?;
            match known {
                Some(previous) if previous != validity as i64 => {
                    tracing::warn!(
                        folder = %remote_folder.id.0,
                        previous,
                        current = validity,
                        "UIDVALIDITY changed; resyncing folder from scratch"
                    );
                    // The messages are kept — losing mail because a server bumped a counter would
                    // be far worse than downloading it again.
                    store
                        .reset_folder_uids(account.id, &remote_folder.id.0)
                        .map_err(Error::Store)?;
                    report.errors.push(format!(
                        "{}: UIDVALIDITY changed, resynced",
                        remote_folder.id.0
                    ));
                }
                _ => {}
            }
            store
                .set_uid_validity(local_folder, validity as i64)
                .map_err(Error::Store)?;
        }

        // Mail a colleague moved or deleted on the server goes here too. Only after a listing
        // that succeeded, so a failed read can never empty a folder.
        let present: std::collections::HashSet<i64> =
            listing.iter().map(|r| parse_uid(&r.id)).collect();
        let vanished = store
            .vanished_from_server(account.id, &remote_folder.id.0, &present)
            .map_err(Error::Store)?;
        if !vanished.is_empty() {
            tracing::info!(folder = %remote_folder.id.0, n = vanished.len(), "removed on the server");
            store.forget_vanished(&vanished).map_err(Error::Store)?;
            report.removed_on_server += vanished.len();
        }

        let seen = store
            .seen_ids(account.id, &remote_folder.id.0)
            .map_err(Error::Store)?;
        // Moved and sent copies are already here, waiting for their new UID. Matching them costs a
        // header fetch per new UID, so it is only done when there is something to match.
        let adopt = store
            .has_unlinked_messages(local_folder)
            .map_err(Error::Store)?;

        // Per folder rather than across the whole mailbox: the totals are only known one folder at
        // a time, and a bar that restarts at each folder name is more honest than one that pretends
        // to know how much of the account is left.
        let pending: Vec<_> = listing.iter().filter(|r| !seen.contains(&r.id.0)).collect();
        let to_fetch = pending.len();
        let bytes_total: u64 = pending.iter().map(|r| r.size).sum();
        let mut fetched = 0usize;
        let mut bytes_done: u64 = 0;
        if to_fetch > 0 {
            progress.send(
                Progress::counted(account.id, Phase::Downloading, 0, to_fetch)
                    .with_bytes(0, bytes_total)
                    .in_folder(remote_folder.name.clone()),
            );
        }

        for remote in &listing {
            if seen.contains(&remote.id.0) {
                report.already_seen += 1;
                // Flags are server state and can change after we downloaded a message, so they are
                // refreshed every sync rather than only on first ingest.
                apply_remote_flags(store, account, &remote_folder.id, remote)?;
                continue;
            }

            if adopt {
                match adopt_existing(store, account, local_folder, &mut transport, &remote_folder.id, remote)
                    .await
                {
                    Ok(true) => {
                        report.already_seen += 1;
                        continue;
                    }
                    Ok(false) => {}
                    // Not fatal: at worst the message is downloaded as it always was.
                    Err(e) => tracing::warn!(uid = %remote.id.0, error = %e, "could not match by Message-ID"),
                }
            }

            fetched += 1;
            progress.send(
                Progress::counted(account.id, Phase::Downloading, fetched, to_fetch)
                    .with_bytes(bytes_done, bytes_total)
                    .in_folder(remote_folder.name.clone()),
            );
            bytes_done += remote.size;

            match ingest_one(
                store,
                account,
                local_folder,
                &ctx,
                &mut transport,
                &remote_folder.id,
                &remote.id,
                remote.size,
            )
            .await
            {
                Ok(outcome) => {
                    report.new_messages += 1;
                    report.bytes_downloaded += outcome.bytes;
                    if outcome.junked {
                        report.junked += 1;
                    }
                    if outcome.filed_by_rule {
                        report.filed_by_rule += 1;
                    }
                    apply_remote_flags(store, account, &remote_folder.id, remote)?;
                }
                Err(e) => {
                    tracing::warn!(uid = %remote.id.0, error = %e, "could not ingest message");
                    report.errors.push(format!("{}: {e}", remote.id.0));
                }
            }
        }
    }

    Box::new(transport).commit().await?;
    store.touch_account_sync(account.id).map_err(Error::Store)?;
    Ok(report)
}

struct Outcome {
    bytes: u64,
    junked: bool,
    filed_by_rule: bool,
}

#[allow(clippy::too_many_arguments)]
async fn ingest_one(
    store: &Store,
    account: &Account,
    local_folder: i64,
    ctx: &StoreContext,
    transport: &mut ImapTransport,
    remote_folder: &FolderId,
    id: &RemoteId,
    reported_size: u64,
) -> Result<Outcome> {
    let stem = format!("imap-{}-{}", file_key(&remote_folder.0), id.0);
    let raw_path = ctx.raw_dir.join(format!("{stem}.eml"));

    let bytes = {
        let file = tokio::fs::File::create(&raw_path).await?;
        let mut writer = tokio::io::BufWriter::new(file);
        let n = transport
            .fetch_message(remote_folder, id, &mut writer)
            .await?;
        writer.flush().await?;
        n
    };

    let raw = tokio::fs::read(&raw_path).await?;
    let parsed = signet_mime::parse(&raw)
        .ok_or_else(|| Error::Parse(format!("message {} is not parseable", id.0)))?;

    // Junk filtering and rules apply to the Inbox only. Mail the user (or the server) already
    // filed elsewhere has been triaged; re-filing it would fight them.
    let apply_filters = local_folder == ctx.inbox_folder;

    let stored = store_message(
        store,
        account,
        local_folder,
        ctx,
        &parsed,
        &raw_path,
        &stem,
        reported_size.max(bytes),
        false,
        apply_filters,
    )
    .await?;

    store
        .set_remote_ref(stored.message_id, &remote_folder.0, parse_uid(id))
        .map_err(Error::Store)?;
    // Filed by a rule or the junk filter on the way in: the server copy must follow, or the next
    // look at this folder would find it still in the Inbox.
    if stored.junked || stored.filed_by_rule {
        let filed_to = store
            .get_message(stored.message_id)
            .map_err(Error::Store)?
            .folder_id;
        store
            .move_messages(&[stored.message_id], filed_to)
            .map_err(Error::Store)?;
    }
    store
        .mark_seen_in(
            account.id,
            &remote_folder.0,
            &id.0,
            Some(stored.message_id),
            signet_store::now_utc(),
        )
        .map_err(Error::Store)?;

    Ok(Outcome {
        bytes,
        junked: stored.junked,
        filed_by_rule: stored.filed_by_rule,
    })
}

/// Apply the journal of local changes to the server, oldest first.
///
/// An op that fails stays queued for the next sync and is given up on only after
/// [`signet_store::MAX_OP_ATTEMPTS`], with the reason in the report.
async fn replay_pending_ops(
    store: &Store,
    account: &Account,
    transport: &mut ImapTransport,
    report: &mut SyncReport,
) -> Result<()> {
    use signet_store::ImapOp;

    for pending in store.pending_imap_ops(account.id).map_err(Error::Store)? {
        let result = match &pending.op {
            ImapOp::Move { from, uid, to } => {
                transport
                    .move_to(&FolderId(from.clone()), &RemoteId(uid.to_string()), to)
                    .await
            }
            ImapOp::Delete { from, uid } => {
                transport
                    .delete(&FolderId(from.clone()), &RemoteId(uid.to_string()))
                    .await
            }
            ImapOp::Append { to, raw_path } => match tokio::fs::read(raw_path).await {
                Ok(raw) => transport.append(to, &raw, true).await,
                Err(e) => Err(e.into()),
            },
        };
        match result {
            Ok(()) => {
                store.finish_imap_op(pending.id).map_err(Error::Store)?;
                report.pushed_to_server += 1;
            }
            Err(e) => {
                let dropped = store
                    .fail_imap_op(pending.id, &e.to_string())
                    .map_err(Error::Store)?;
                tracing::warn!(op = ?pending.op, error = %e, dropped, "server change failed");
                report.errors.push(if dropped {
                    format!("gave up telling the server about a change: {e}")
                } else {
                    format!("could not update the server, will retry: {e}")
                });
            }
        }
    }
    Ok(())
}

/// Link a new server UID to the local copy it already is, instead of downloading it again.
///
/// Returns true when a local message was adopted.
async fn adopt_existing(
    store: &Store,
    account: &Account,
    local_folder: i64,
    transport: &mut ImapTransport,
    folder: &FolderId,
    remote: &crate::transport::RemoteMessage,
) -> Result<bool> {
    let headers = transport.fetch_headers(folder, &remote.id).await?;
    let Some(message_id) = signet_mime::parse(&headers).and_then(|p| p.message_id) else {
        return Ok(false);
    };
    let Some(local) = store
        .unlinked_by_message_id(local_folder, &message_id)
        .map_err(Error::Store)?
    else {
        return Ok(false);
    };
    store
        .set_remote_ref(local, &folder.0, parse_uid(&remote.id))
        .map_err(Error::Store)?;
    store
        .mark_seen_in(account.id, &folder.0, &remote.id.0, Some(local), signet_store::now_utc())
        .map_err(Error::Store)?;
    apply_remote_flags(store, account, folder, remote)?;
    Ok(true)
}

/// Send every locally-changed flag in this folder to the server.
///
/// Returns how many messages were pushed. Cleared only after the server has accepted them, so a
/// failed push is retried on the next sync rather than lost.
async fn push_dirty_flags(
    store: &Store,
    account: &Account,
    transport: &mut ImapTransport,
    folder: &FolderId,
) -> Result<usize> {
    let dirty = store
        .dirty_flag_messages(account.id, &folder.0)
        .map_err(Error::Store)?;
    if dirty.is_empty() {
        return Ok(0);
    }

    let mut pushed = Vec::new();
    for (message_id, uid) in dirty {
        let (read, flagged, answered) = store.flags_of(message_id).map_err(Error::Store)?;
        transport
            .push_flags(folder, &RemoteId(uid.to_string()), read, flagged, answered)
            .await?;
        pushed.push(message_id);
    }

    let n = pushed.len();
    store.clear_flags_dirty(&pushed).map_err(Error::Store)?;
    Ok(n)
}

/// Copy the server's flags onto our local row.
///
/// The server is authoritative for IMAP: if a colleague marked something read, it is read — but a
/// change of ours that has not been pushed yet wins, which is what `flags_dirty` guards.
fn apply_remote_flags(
    store: &Store,
    account: &Account,
    folder: &FolderId,
    remote: &crate::transport::RemoteMessage,
) -> Result<()> {
    let Some(message_id) = store
        .seen_message_id(account.id, &folder.0, &remote.id.0)
        .map_err(Error::Store)?
    else {
        return Ok(());
    };

    store
        .set_flags_from_server(
            message_id,
            remote.flags.contains(&Flag::Seen),
            remote.flags.contains(&Flag::Flagged),
            remote.flags.contains(&Flag::Answered),
        )
        .map_err(Error::Store)?;
    Ok(())
}

/// Push a local flag change back to the server.
///
/// Called when the user marks something read or flagged, so the rest of the team sees it.
pub async fn push_flags(
    store: &Store,
    account: &Account,
    password: &str,
    message_id: i64,
) -> Result<()> {
    let Some((folder, uid)) = store.remote_ref(message_id).map_err(Error::Store)? else {
        // Not an IMAP message, or not yet matched to a server UID.
        return Ok(());
    };
    let (read, flagged, answered) = store.flags_of(message_id).map_err(Error::Store)?;

    let mut transport = ImapTransport::new(
        crate::imap_config_for(account),
        ImapCredentials {
            user: account.pop_user.clone(),
            password: password.to_string(),
            allow_plaintext: account.pop_security == signet_store::Security::Plain,
        },
    );
    transport.connect().await?;
    transport
        .push_flags(
            &FolderId(folder),
            &RemoteId(uid.to_string()),
            read,
            flagged,
            answered,
        )
        .await?;
    Box::new(transport).commit().await
}

fn map_kind(kind: crate::transport::FolderKind) -> FolderKind {
    use crate::transport::FolderKind as K;
    match kind {
        K::Inbox => FolderKind::Inbox,
        K::Sent => FolderKind::Sent,
        K::Drafts => FolderKind::Drafts,
        K::Deleted => FolderKind::Deleted,
        K::Junk => FolderKind::Junk,
        K::Archive => FolderKind::Archive,
        K::Other => FolderKind::Custom,
    }
}

fn parse_uid(id: &RemoteId) -> i64 {
    id.0.parse().unwrap_or(0)
}

/// A folder's part of a raw-message filename: readable, and unique per folder.
///
/// The readable part is cut short, so two folders that differ only after it ("… Quotations 2025"
/// and "… Quotations 2026") used to share a name and overwrite each other's messages. The hash of
/// the whole path keeps them apart. Only new downloads use it; stored paths are kept as recorded.
fn file_key(path: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut h);
    format!("{}-{:08x}", sanitize(path), h.finish() as u32)
}

/// A folder path reduced to something safe in a filename.
fn sanitize(path: &str) -> String {
    path.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .take(40)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_paths_are_reduced_to_safe_filenames() {
        // Server-chosen paths contain dots, slashes and anything else the server likes.
        assert_eq!(sanitize("INBOX.Sent Items"), "INBOX_Sent_Items");
        assert!(!sanitize("../../etc/passwd").contains('/'));
        assert!(!sanitize("../../etc/passwd").contains('.'));
    }

    #[test]
    fn folders_that_differ_only_past_the_cut_get_different_files() {
        let a = file_key("Customers.ACME Lighting Ltd.Quotations and Orders 2025");
        let b = file_key("Customers.ACME Lighting Ltd.Quotations and Orders 2026");
        assert_ne!(a, b, "two folders would overwrite each other's messages");
    }

    #[test]
    fn a_non_numeric_uid_does_not_panic() {
        assert_eq!(parse_uid(&RemoteId("not-a-number".into())), 0);
        assert_eq!(parse_uid(&RemoteId("42".into())), 42);
    }
}
