//! Settings, rules, junk training and drafts, on top of [`crate::App`].
//!
//! Split from `lib.rs` to keep it navigable; these are ordinary `App` methods.

use signet_store::{AccountSettings, FolderKind, StoredDraft};
use signet_sync::junk;
use signet_sync::outbox::{Draft, DraftAttachment};
use signet_sync::rules::Rule;

use crate::{App, Error, Event, Result};

/// Why a message ended up where it did, for the reading pane.
#[derive(Debug, Clone, serde::Serialize)]
pub struct FilingInfo {
    pub spam_score: f64,
    pub filed_by: String,
    pub is_junk_folder: bool,
    /// Why the filter scored it as it did, in plain words.
    pub reasons: String,
    /// `allow`, `block`, or empty when the sender has no explicit rule.
    pub sender_rule: String,
}

impl App {
    // ---------------------------------------------------------------- settings

    pub fn account_settings(&self, id: i64) -> Result<AccountSettings> {
        let a = self.store().get_account(id)?;
        Ok(AccountSettings {
            display_name: a.display_name,
            signature: a.signature,
            poll_minutes: a.poll_minutes,
            leave_on_server: a.leave_on_server,
            delete_after_days: a.delete_after_days,
            junk_filter_enabled: a.junk_filter_enabled,
            junk_threshold: a.junk_threshold,
            is_enabled: a.is_enabled,
        })
    }

    pub fn update_account_settings(&self, id: i64, mut settings: AccountSettings) -> Result<()> {
        // A threshold outside this range makes the filter either useless or dangerous, and the
        // UI slider should never be the only thing preventing that.
        settings.junk_threshold = settings.junk_threshold.clamp(0.5, 0.99);
        settings.poll_minutes = settings.poll_minutes.clamp(1, 1440);
        if let Some(days) = settings.delete_after_days {
            settings.delete_after_days = (days > 0).then_some(days);
        }

        self.store().update_account_settings(id, &settings)?;
        self.emit_public(Event::AccountsChanged);
        Ok(())
    }

    // ---------------------------------------------------------------- rules

    pub fn list_rules(&self, account_id: Option<i64>) -> Result<Vec<Rule>> {
        Ok(self
            .store()
            .list_rules(account_id)?
            .into_iter()
            .filter_map(|(id, _, json)| {
                serde_json::from_str::<Rule>(&json)
                    .map(|mut r| {
                        r.id = id;
                        r
                    })
                    .map_err(|e| tracing::warn!(rule_id = id, error = %e, "unparseable rule"))
                    .ok()
            })
            .collect())
    }

    pub fn save_rule(&self, mut rule: Rule) -> Result<i64> {
        if rule.conditions.is_empty() {
            return Err(Error::Invalid(
                "a rule needs at least one condition, or it would match every message".into(),
            ));
        }
        if rule.actions.is_empty() {
            return Err(Error::Invalid("a rule needs at least one action".into()));
        }
        if rule.name.trim().is_empty() {
            rule.name = "Untitled rule".into();
        }

        let id = rule.id;
        let json = serde_json::to_string(&rule)
            .map_err(|e| Error::Invalid(format!("could not encode rule: {e}")))?;

        let saved = if id > 0 {
            self.store()
                .update_rule(id, &rule.name, rule.priority, rule.enabled, &json)?;
            id
        } else {
            self.store().insert_rule(
                rule.account_id,
                &rule.name,
                rule.priority,
                rule.enabled,
                &json,
            )?
        };
        Ok(saved)
    }

    pub fn delete_rule(&self, id: i64) -> Result<()> {
        Ok(self.store().delete_rule(id)?)
    }

    /// Apply the rules to messages already in the mailbox.
    ///
    /// Rules normally run at ingest, so a rule created today would otherwise do nothing about the
    /// thousand messages already sitting in the Inbox.
    pub fn apply_rules_now(&self, account_id: i64) -> Result<usize> {
        let rules = self.list_rules(Some(account_id))?;
        if rules.is_empty() {
            return Ok(0);
        }

        let inbox = self.store().folder_id(account_id, FolderKind::Inbox)?;
        let messages = self.store().list_messages(&signet_store::MessageQuery {
            account_id: Some(account_id),
            folder_id: Some(inbox),
            limit: 10_000,
            ..Default::default()
        })?;

        let mut moved = 0;
        for summary in messages {
            let detail = self.store().get_message(summary.id)?;
            let candidate = signet_sync::rules::Candidate {
                from: format!("{} {}", detail.from_name, detail.from_addr),
                to: recipients_of(&detail, "to"),
                cc: recipients_of(&detail, "cc"),
                subject: detail.subject.clone(),
                body: detail.body_text.chars().take(8000).collect(),
            };
            let outcome = signet_sync::rules::evaluate(&rules, &candidate);

            if outcome.mark_read {
                self.store().set_read(&[summary.id], true)?;
            }
            if outcome.flag {
                self.store().set_flagged(&[summary.id], true)?;
            }
            if let Some(kind) = outcome.move_to {
                let target = self.store().folder_id(account_id, kind)?;
                if target != detail.folder_id {
                    self.store().move_messages(&[summary.id], target)?;
                    self.store().set_spam_score(
                        summary.id,
                        0.0,
                        &format!("rule: {}", outcome.matched.join(", ")),
                    )?;
                    moved += 1;
                }
            }
        }

        self.emit_public(Event::MessagesChanged { account_id });
        Ok(moved)
    }

    // ---------------------------------------------------------------- junk

    /// Mark messages as junk: train the filter, then file them.
    pub fn mark_junk(&self, ids: Vec<i64>, is_junk: bool) -> Result<()> {
        for id in &ids {
            let tokens = self.junk_tokens(*id)?;

            // Only train when the classification actually changes. Training twice on the same
            // message would count it twice and skew the corpus.
            let previously_junk = self.is_in_junk(*id)?;
            if previously_junk != is_junk {
                if previously_junk {
                    self.store().untrain(&tokens, true)?;
                } else if self.was_trained_as_ham(*id)? {
                    self.store().untrain(&tokens, false)?;
                }
                self.store().train(&tokens, is_junk)?;
            }

            let detail = self.store().get_message(*id)?;
            let target = self.store().folder_id(
                detail.account_id,
                if is_junk {
                    FolderKind::Junk
                } else {
                    FolderKind::Inbox
                },
            )?;
            self.store().move_messages(&[*id], target)?;
            self.store().set_spam_score(
                *id,
                if is_junk { 1.0 } else { 0.0 },
                if is_junk {
                    "marked junk"
                } else {
                    "marked not junk"
                },
            )?;
            if !is_junk {
                self.store().set_read(&[*id], false)?;
            }
        }

        self.emit_public(Event::MessagesChanged { account_id: 0 });
        Ok(())
    }

    /// Why a message was filed where it was.
    pub fn filing_info(&self, id: i64) -> Result<FilingInfo> {
        let detail = self.store().get_message(id)?;
        let junk_folder = self
            .store()
            .folder_id(detail.account_id, FolderKind::Junk)?;
        Ok(FilingInfo {
            spam_score: self.store().spam_score(id).unwrap_or(0.0),
            filed_by: self.store().filed_by(id).unwrap_or_default(),
            is_junk_folder: detail.folder_id == junk_folder,
            reasons: self.store().spam_reasons(id).unwrap_or_default(),
            sender_rule: self
                .store()
                .sender_rule_for(&detail.from_addr)
                .ok()
                .flatten()
                .unwrap_or_default(),
        })
    }

    // ---------------------------------------------------------------- contacts

    /// The address book, filtered by name, address or company.
    pub fn list_contacts(&self, search: String) -> Result<Vec<signet_store::Contact>> {
        Ok(self.store().list_contacts(&search, 2000)?)
    }

    pub fn save_contact(&self, contact: signet_store::Contact) -> Result<()> {
        if contact.id == 0 {
            return Err(Error::Invalid("that contact no longer exists".into()));
        }
        self.store().update_contact(&contact)?;
        Ok(())
    }

    pub fn delete_contact(&self, id: i64) -> Result<()> {
        self.store().delete_contact(id)?;
        Ok(())
    }

    /// Write the address book to `path`, choosing the format from the extension.
    ///
    /// The filter is passed through rather than always exporting everything: "send me who we know
    /// at Gulf Lighting" is a question the search box already answers, and the export should agree
    /// with what is on screen.
    pub fn export_contacts(&self, path: String, search: String) -> Result<i64> {
        let contacts = self.store().list_contacts(&search, 100_000)?;
        let as_vcard = std::path::Path::new(&path)
            .extension()
            .map(|e| e.eq_ignore_ascii_case("vcf"))
            .unwrap_or(false);

        let body = if as_vcard {
            crate::contacts_export::to_vcard(&contacts)
        } else {
            crate::contacts_export::to_csv(&contacts)
        };

        // A BOM so Excel opens the file as UTF-8. Without it, every accented name in the list is
        // mangled on the machine it is most likely to be opened on.
        let mut bytes = Vec::new();
        if !as_vcard {
            bytes.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
        }
        bytes.extend_from_slice(body.as_bytes());
        std::fs::write(&path, bytes)?;

        Ok(contacts.len() as i64)
    }

    /// How much the junk filter has learned.
    pub fn junk_corpus(&self) -> Result<(i64, i64, bool)> {
        let (spam, ham) = self.store().corpus_size()?;
        let trained = junk::CorpusSize {
            spam_messages: spam,
            ham_messages: ham,
        }
        .is_trained();
        Ok((spam, ham, trained))
    }

    fn junk_tokens(&self, id: i64) -> Result<Vec<String>> {
        let (from_name, from_addr, subject, body, remote, recipients) =
            self.store().junk_features(id)?;
        let account_addr = self
            .store()
            .get_message(id)
            .and_then(|m| self.store().get_account(m.account_id))
            .map(|a| a.email)
            .unwrap_or_default();

        Ok(junk::tokenize(&junk::Features {
            from_name,
            from_addr,
            subject,
            body,
            has_remote_content: remote,
            recipients,
            account_addr,
        })
        .into_iter()
        .collect())
    }

    fn is_in_junk(&self, id: i64) -> Result<bool> {
        let detail = self.store().get_message(id)?;
        let junk_folder = self
            .store()
            .folder_id(detail.account_id, FolderKind::Junk)?;
        Ok(detail.folder_id == junk_folder)
    }

    /// Whether this message was previously trained as wanted, so a re-classification can undo it.
    fn was_trained_as_ham(&self, id: i64) -> Result<bool> {
        Ok(self.store().filed_by(id).unwrap_or_default() == "marked not junk")
    }

    // ---------------------------------------------------------------- sender rules

    pub fn list_sender_rules(&self) -> Result<Vec<signet_store::SenderRule>> {
        Ok(self.store().list_sender_rules()?)
    }

    /// Always deliver, or always file, mail from this address or domain.
    pub fn set_sender_rule(&self, pattern: String, kind: String) -> Result<()> {
        if !matches!(kind.as_str(), "allow" | "block") {
            return Err(Error::Invalid(format!("unknown rule kind {kind:?}")));
        }
        let pattern = pattern.trim().to_lowercase();
        if pattern.is_empty() || !pattern.contains('.') {
            return Err(Error::Invalid(
                "give an email address or a domain, e.g. buyer@customer.test or customer.test"
                    .into(),
            ));
        }
        self.store().set_sender_rule(&pattern, &kind)?;
        self.emit_public(Event::MessagesChanged { account_id: 0 });
        Ok(())
    }

    pub fn remove_sender_rule(&self, pattern: String) -> Result<()> {
        self.store().remove_sender_rule(&pattern)?;
        self.emit_public(Event::MessagesChanged { account_id: 0 });
        Ok(())
    }

    /// Allow or block the sender of a message, without having to retype the address.
    pub fn rule_for_message(
        &self,
        message_id: i64,
        kind: String,
        whole_domain: bool,
    ) -> Result<String> {
        let detail = self.store().get_message(message_id)?;
        let addr = detail.from_addr.trim().to_lowercase();
        if addr.is_empty() {
            return Err(Error::Invalid("this message has no sender address".into()));
        }
        let pattern = if whole_domain {
            addr.rsplit('@').next().unwrap_or(&addr).to_string()
        } else {
            addr
        };
        self.set_sender_rule(pattern.clone(), kind)?;
        Ok(pattern)
    }

    // ---------------------------------------------------------------- drafts

    pub fn save_draft(&self, draft: Draft) -> Result<i64> {
        let folder = self
            .store()
            .folder_id(draft.account_id, FolderKind::Drafts)?;
        let id = self.store().save_draft(
            &StoredDraft {
                message_id: draft.draft_message_id.unwrap_or(0),
                account_id: draft.account_id,
                to: draft.to.clone(),
                cc: draft.cc.clone(),
                bcc: draft.bcc.clone(),
                subject: draft.subject.clone(),
                body_text: draft.body_text.clone(),
                // Sanitized like any mail body: it is shown in the reading pane and loaded back
                // into the composer, and part of it is somebody else's quoted message.
                body_html: draft
                    .body_html
                    .as_deref()
                    .map(|html| signet_mime::sanitize(html, true).html),
                in_reply_to: draft.in_reply_to.clone(),
                references: draft.references.clone(),
                attachments: draft
                    .attachments
                    .iter()
                    .map(|a| (a.filename.clone(), a.path.clone()))
                    .collect(),
            },
            folder,
        )?;

        self.emit_public(Event::MessagesChanged {
            account_id: draft.account_id,
        });
        Ok(id)
    }

    pub fn load_draft(&self, message_id: i64) -> Result<Draft> {
        let stored = self.store().load_draft(message_id)?;
        Ok(Draft {
            account_id: stored.account_id,
            to: stored.to,
            cc: stored.cc,
            bcc: stored.bcc,
            subject: stored.subject,
            body_text: stored.body_text,
            body_html: stored.body_html,
            in_reply_to: stored.in_reply_to,
            references: stored.references,
            attachments: stored
                .attachments
                .into_iter()
                .map(|(filename, path)| DraftAttachment { filename, path })
                .collect(),
            signature: None,
            draft_message_id: Some(stored.message_id),
        })
    }

    pub fn mark_folder_read(&self, folder_id: i64) -> Result<usize> {
        let n = self.store().mark_folder_read(folder_id)?;
        self.emit_public(Event::MessagesChanged { account_id: 0 });
        Ok(n)
    }
}

fn recipients_of(detail: &signet_store::MessageDetail, kind: &str) -> Vec<String> {
    detail
        .recipients
        .iter()
        .filter(|r| r.kind == kind)
        .map(|r| r.addr.clone())
        .collect()
}
