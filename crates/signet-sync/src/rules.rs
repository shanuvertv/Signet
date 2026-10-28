//! Client-side rules, applied at ingest.
//!
//! POP3 has no server-side rules, so this is the only place filing can happen. Rules run in
//! priority order against each newly downloaded message, before it reaches the Inbox.
//!
//! Deliberately conservative: `Delete` moves to Deleted Items rather than destroying anything, and
//! a rule that throws does not stop the message being stored. A filing mistake should be
//! recoverable by looking in another folder, never by re-downloading mail we no longer have.

use signet_store::FolderKind;
use serde::{Deserialize, Serialize};

/// Which part of the message a condition looks at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Field {
    From,
    To,
    Cc,
    Subject,
    Body,
    /// Sender, recipients, subject and body together.
    Any,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    Contains,
    NotContains,
    Equals,
    StartsWith,
    EndsWith,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Condition {
    pub field: Field,
    pub op: Op,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Action {
    /// File into one of the standard folders.
    MoveTo {
        folder: FolderKind,
    },
    MarkRead,
    Flag,
    /// Move to Deleted Items. Never a hard delete — see the module docs.
    Delete,
    /// Skip the junk filter for this message. The escape hatch for a sender whose mail keeps
    /// scoring badly but is genuinely wanted.
    NeverJunk,
    /// Stop evaluating further rules.
    Stop,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    #[serde(default)]
    pub id: i64,
    pub account_id: i64,
    pub name: String,
    pub priority: i64,
    pub enabled: bool,
    /// All conditions must match, versus any of them.
    #[serde(default = "default_true")]
    pub match_all: bool,
    pub conditions: Vec<Condition>,
    pub actions: Vec<Action>,
}

fn default_true() -> bool {
    true
}

/// The parts of a message a rule can see.
#[derive(Debug, Clone, Default)]
pub struct Candidate {
    pub from: String,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub subject: String,
    pub body: String,
}

/// What the rules decided.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Outcome {
    pub move_to: Option<FolderKind>,
    pub mark_read: bool,
    pub flag: bool,
    pub never_junk: bool,
    /// Names of the rules that matched, for the "filed by" banner.
    pub matched: Vec<String>,
}

impl Outcome {
    pub fn did_anything(&self) -> bool {
        self.move_to.is_some() || self.mark_read || self.flag || self.never_junk
    }
}

/// Run the rules for one account against one message.
///
/// Matching is case-insensitive throughout: nobody writing a rule means "only if they capitalised
/// it the same way", and a rule that silently stops matching because a sender changed their
/// signature is worse than useless.
pub fn evaluate(rules: &[Rule], candidate: &Candidate) -> Outcome {
    let mut outcome = Outcome::default();

    let mut ordered: Vec<&Rule> = rules.iter().filter(|r| r.enabled).collect();
    ordered.sort_by_key(|r| r.priority);

    for rule in ordered {
        if !matches(rule, candidate) {
            continue;
        }
        outcome.matched.push(rule.name.clone());

        let mut stop = false;
        for action in &rule.actions {
            match action {
                // Last matching rule wins the destination, unless one of them stops processing.
                Action::MoveTo { folder } => outcome.move_to = Some(*folder),
                Action::Delete => outcome.move_to = Some(FolderKind::Deleted),
                Action::MarkRead => outcome.mark_read = true,
                Action::Flag => outcome.flag = true,
                Action::NeverJunk => outcome.never_junk = true,
                Action::Stop => stop = true,
            }
        }
        if stop {
            break;
        }
    }

    outcome
}

fn matches(rule: &Rule, candidate: &Candidate) -> bool {
    // A rule with no conditions would match everything, which is never what someone meant to
    // build and would silently file the whole mailbox.
    if rule.conditions.is_empty() {
        return false;
    }

    let mut results = rule
        .conditions
        .iter()
        .map(|c| matches_condition(c, candidate));
    if rule.match_all {
        results.all(|r| r)
    } else {
        results.any(|r| r)
    }
}

fn matches_condition(condition: &Condition, candidate: &Candidate) -> bool {
    let haystack = match condition.field {
        Field::From => candidate.from.clone(),
        Field::To => candidate.to.join(" "),
        Field::Cc => candidate.cc.join(" "),
        Field::Subject => candidate.subject.clone(),
        Field::Body => candidate.body.clone(),
        Field::Any => format!(
            "{} {} {} {} {}",
            candidate.from,
            candidate.to.join(" "),
            candidate.cc.join(" "),
            candidate.subject,
            candidate.body
        ),
    }
    .to_lowercase();

    let needle = condition.value.trim().to_lowercase();
    if needle.is_empty() {
        return false;
    }

    match condition.op {
        Op::Contains => haystack.contains(&needle),
        Op::NotContains => !haystack.contains(&needle),
        Op::Equals => haystack.trim() == needle,
        Op::StartsWith => haystack.trim_start().starts_with(&needle),
        Op::EndsWith => haystack.trim_end().ends_with(&needle),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate() -> Candidate {
        Candidate {
            from: "Alice Smith <alice@customer.test>".into(),
            to: vec!["dev@example.com".into()],
            cc: vec!["sales@example.com".into()],
            subject: "Re: PO 4471 delivery".into(),
            body: "Please confirm the ship date.".into(),
        }
    }

    fn rule(name: &str, priority: i64, conditions: Vec<Condition>, actions: Vec<Action>) -> Rule {
        Rule {
            id: 0,
            account_id: 1,
            name: name.into(),
            priority,
            enabled: true,
            match_all: true,
            conditions,
            actions,
        }
    }

    fn contains(field: Field, value: &str) -> Condition {
        Condition {
            field,
            op: Op::Contains,
            value: value.into(),
        }
    }

    #[test]
    fn a_matching_rule_files_the_message() {
        let rules = vec![rule(
            "Customer mail",
            0,
            vec![contains(Field::From, "customer.test")],
            vec![Action::MoveTo {
                folder: FolderKind::Archive,
            }],
        )];
        let out = evaluate(&rules, &candidate());
        assert_eq!(out.move_to, Some(FolderKind::Archive));
        assert_eq!(out.matched, vec!["Customer mail".to_string()]);
    }

    #[test]
    fn matching_is_case_insensitive() {
        let rules = vec![rule(
            "Shouty",
            0,
            vec![contains(Field::Subject, "DELIVERY")],
            vec![Action::Flag],
        )];
        assert!(evaluate(&rules, &candidate()).flag);
    }

    #[test]
    fn match_all_requires_every_condition() {
        let mut r = rule(
            "Both",
            0,
            vec![
                contains(Field::From, "customer.test"),
                contains(Field::Subject, "invoice"),
            ],
            vec![Action::Flag],
        );
        assert!(
            !evaluate(&[r.clone()], &candidate()).flag,
            "match_all was ignored"
        );

        r.match_all = false;
        assert!(evaluate(&[r], &candidate()).flag, "match_any was ignored");
    }

    #[test]
    fn a_rule_with_no_conditions_matches_nothing() {
        // Otherwise a half-built rule silently files the entire mailbox.
        let rules = vec![rule("Empty", 0, vec![], vec![Action::Delete])];
        assert_eq!(evaluate(&rules, &candidate()).move_to, None);
    }

    #[test]
    fn an_empty_value_matches_nothing() {
        let rules = vec![rule(
            "Blank",
            0,
            vec![contains(Field::Subject, "   ")],
            vec![Action::Delete],
        )];
        assert_eq!(evaluate(&rules, &candidate()).move_to, None);
    }

    #[test]
    fn disabled_rules_are_skipped() {
        let mut r = rule(
            "Off",
            0,
            vec![contains(Field::From, "customer.test")],
            vec![Action::Delete],
        );
        r.enabled = false;
        assert_eq!(evaluate(&[r], &candidate()).move_to, None);
    }

    #[test]
    fn rules_run_in_priority_order_and_stop_halts_the_rest() {
        let rules = vec![
            rule(
                "Later",
                10,
                vec![contains(Field::Any, "po 4471")],
                vec![Action::MoveTo {
                    folder: FolderKind::Junk,
                }],
            ),
            rule(
                "Earlier",
                1,
                vec![contains(Field::From, "customer.test")],
                vec![
                    Action::MoveTo {
                        folder: FolderKind::Archive,
                    },
                    Action::Stop,
                ],
            ),
        ];
        let out = evaluate(&rules, &candidate());
        assert_eq!(out.move_to, Some(FolderKind::Archive), "priority ignored");
        assert_eq!(out.matched, vec!["Earlier".to_string()], "Stop was ignored");
    }

    #[test]
    fn delete_files_to_deleted_items_rather_than_destroying() {
        let rules = vec![rule(
            "Bin it",
            0,
            vec![contains(Field::From, "customer.test")],
            vec![Action::Delete],
        )];
        assert_eq!(
            evaluate(&rules, &candidate()).move_to,
            Some(FolderKind::Deleted)
        );
    }

    #[test]
    fn not_contains_inverts_correctly() {
        let rules = vec![rule(
            "External only",
            0,
            vec![Condition {
                field: Field::From,
                op: Op::NotContains,
                value: "example.com".into(),
            }],
            vec![Action::Flag],
        )];
        assert!(evaluate(&rules, &candidate()).flag);
    }

    #[test]
    fn never_junk_is_recorded_for_the_filter_to_honour() {
        let rules = vec![rule(
            "Trusted sender",
            0,
            vec![contains(Field::From, "customer.test")],
            vec![Action::NeverJunk],
        )];
        let out = evaluate(&rules, &candidate());
        assert!(out.never_junk);
        assert!(out.did_anything());
    }

    #[test]
    fn rules_round_trip_through_json() {
        // They are stored as JSON, so a schema change that breaks this loses every user rule.
        let r = rule(
            "Round trip",
            5,
            vec![contains(Field::Subject, "invoice")],
            vec![
                Action::MoveTo {
                    folder: FolderKind::Archive,
                },
                Action::MarkRead,
            ],
        );
        let json = serde_json::to_string(&r).unwrap();
        let back: Rule = serde_json::from_str(&json).unwrap();
        assert_eq!(r, back);
    }
}
