//! Signal-based scoring on top of the Bayes classifier.
//!
//! Words alone miss the things that actually distinguish a phishing attempt from a customer
//! enquiry: whether the sender authenticated, whether a link goes where it claims, whether there
//! is an executable attached, whether you have ever written to this person. Those are facts
//! `signet_mime::signals` extracts; this module decides what each is worth and — importantly — keeps
//! a record of why, so "why is this in Junk?" has an answer.
//!
//! **Precedence, highest first:**
//!
//! 1. An explicit block or allow rule. The user's stated decision beats every guess.
//! 2. A known correspondent. Someone you have written to is not a spammer, and filing a customer's
//!    reply is the most expensive mistake this filter can make.
//! 3. Hard signals — authentication failure, deceptive links, executable attachments.
//! 4. The Bayes/heuristic blend from [`crate::junk`].

use signet_mime::signals::{AuthVerdict, Signals, is_shortener};

/// One contribution to a verdict, kept so the decision can be explained.
#[derive(Debug, Clone, PartialEq)]
pub struct Reason {
    /// Positive pushes toward junk, negative toward the Inbox.
    pub weight: f64,
    pub text: String,
}

impl Reason {
    fn new(weight: f64, text: impl Into<String>) -> Self {
        Self {
            weight,
            text: text.into(),
        }
    }
}

/// What the user has said about this sender, and whether we know them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Reputation {
    Blocked,
    Allowed,
    /// We have written to them, or seen them repeatedly.
    Known,
    #[default]
    Unknown,
}

#[derive(Debug, Clone)]
pub struct Verdict {
    /// 0.0 certainly wanted, 1.0 certainly junk.
    pub score: f64,
    pub reasons: Vec<Reason>,
    /// True when the decision was the user's explicit rule rather than a guess, so the caller can
    /// skip training on it — a rule teaches the classifier nothing.
    pub decided_by_rule: bool,
    /// How many *hard* signals fired — see [`score`]. Two or more is the shape of a forgery, and
    /// is worth telling the user about even on a message that was delivered anyway.
    pub hard_signals: u8,
}

impl Verdict {
    /// Only what is wrong with the message, worst first.
    ///
    /// [`Self::explain`] answers "why is this in Junk?", so it includes the reasons that argued
    /// for delivery too. This answers "what should I be careful of?" on a message that was
    /// delivered, where a reassuring line would bury the point.
    pub fn warnings(&self) -> String {
        self.top(|r| r.weight >= 0.15, 3)
    }

    /// The reasons that actually mattered, most significant first, as one line.
    pub fn explain(&self) -> String {
        self.top(|r| r.weight.abs() >= 0.05, 4)
    }

    fn top(&self, keep: impl Fn(&Reason) -> bool, limit: usize) -> String {
        let mut significant: Vec<&Reason> = self.reasons.iter().filter(|r| keep(r)).collect();
        significant.sort_by(|a, b| {
            b.weight
                .abs()
                .partial_cmp(&a.weight.abs())
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        significant
            .iter()
            .take(limit)
            .map(|r| r.text.clone())
            .collect::<Vec<_>>()
            .join("; ")
    }
}

/// Running total plus the reasons behind it.
///
/// A small struct rather than a closure threading its own state, so each contribution reads as one
/// line and the total cannot move without a reason being recorded alongside it.
struct Tally {
    total: f64,
    reasons: Vec<Reason>,
}

impl Tally {
    fn add(&mut self, weight: f64, text: impl Into<String>) {
        self.total += weight;
        self.reasons.push(Reason::new(weight, text));
    }
}

/// Combine the statistical score with the structural signals.
///
/// `base` is the Bayes/heuristic blend from [`crate::junk`]; everything here adjusts it.
pub fn score(base: f64, signals: &Signals, reputation: Reputation) -> Verdict {
    // --- 1. explicit rules win outright ------------------------------------
    match reputation {
        Reputation::Blocked => {
            return Verdict {
                score: 1.0,
                reasons: vec![Reason::new(1.0, "sender is on your block list")],
                decided_by_rule: true,
                hard_signals: 0,
            };
        }
        Reputation::Allowed => {
            return Verdict {
                score: 0.0,
                reasons: vec![Reason::new(-1.0, "sender is on your allow list")],
                decided_by_rule: true,
                hard_signals: 0,
            };
        }
        _ => {}
    }

    let mut t = Tally {
        total: base,
        reasons: vec![Reason::new(base, format!("content score {base:.2}"))],
    };

    // --- 2. someone we know -------------------------------------------------
    if reputation == Reputation::Known {
        // Deliberately large. Filing a customer's reply costs far more than letting one spam
        // through, and this is the strongest evidence available that a sender is legitimate.
        t.add(-0.55, "you have corresponded with this sender before");
    }

    // --- 3. authentication --------------------------------------------------
    match (signals.dmarc, signals.dkim, signals.spf) {
        (AuthVerdict::Fail, _, _) => t.add(
            0.35,
            "DMARC failed — the sender's own domain says this is not from them",
        ),
        (_, AuthVerdict::Fail, AuthVerdict::Fail) => t.add(0.30, "both DKIM and SPF failed"),
        (_, AuthVerdict::Fail, _) => t.add(0.15, "DKIM signature failed"),
        (_, _, AuthVerdict::Fail) => t.add(
            0.12,
            "SPF failed — sent from a server the domain does not authorise",
        ),
        (AuthVerdict::Pass, AuthVerdict::Pass, _) => t.add(-0.18, "DMARC and DKIM both passed"),
        _ => {}
    }

    // --- 4. links -----------------------------------------------------------
    if let Some(example) = signals.deceptive_links.first() {
        // The single most reliable phishing tell there is.
        t.add(
            0.45,
            format!(
                "a link reads \"{}\" but goes to {}",
                truncate(&example.text, 40),
                example.host
            ),
        );
    }
    if signals.ip_literal_links > 0 {
        t.add(0.22, "links to a bare IP address rather than a hostname");
    }
    if signals.links.iter().any(|l| is_shortener(&l.host)) {
        t.add(0.10, "uses a URL shortener, which hides the destination");
    }
    if signals.distinct_link_hosts >= 10 {
        t.add(
            0.10,
            format!("links to {} different sites", signals.distinct_link_hosts),
        );
    }

    // --- 5. attachments -----------------------------------------------------
    if !signals.risky_attachments.is_empty() {
        t.add(
            0.35,
            format!(
                "attaches {}, which Windows can execute",
                signals.risky_attachments.join(", ")
            ),
        );
    }

    // --- 6. header anomalies -------------------------------------------------
    if signals.reply_to_mismatch {
        t.add(
            0.15,
            "replies would go to a different domain than the sender",
        );
    }
    if signals.missing_message_id {
        t.add(
            0.15,
            "no Message-ID, which nearly every real sender includes",
        );
    }
    if signals.missing_from {
        t.add(0.20, "no usable sender address");
    }

    // Bulk mail is not spam. A newsletter that identifies itself and offers an unsubscribe link is
    // behaving correctly, and treating that as suspicious would file every supplier's mailshot.
    if signals.bulk && signals.has_list_unsubscribe {
        t.add(
            -0.05,
            "identifies itself as a mailing list with an unsubscribe link",
        );
    }

    // --- 7. co-occurrence ----------------------------------------------------
    //
    // Each of these has an innocent explanation on its own. Forwarding breaks authentication, a
    // marketing tool rewrites links, a supplier really does send a zip. Two of them together do
    // not have an innocent explanation, and adding the weights alone leaves a textbook phish just
    // under the threshold on a fresh install where the word statistics are still empty.
    //
    // DKIM or SPF failing by itself is deliberately not counted: forwarding breaks both routinely
    // and counting them would file every message a customer forwards from their phone.
    let hard = [
        signals.dmarc == AuthVerdict::Fail,
        !signals.deceptive_links.is_empty(),
        !signals.risky_attachments.is_empty(),
        signals.ip_literal_links > 0,
    ]
    .into_iter()
    .filter(|f| *f)
    .count() as u8;

    if hard >= 2 {
        t.add(0.25, "several independent signs of forgery at once");
    }

    Verdict {
        score: t.total.clamp(0.0, 1.0),
        reasons: t.reasons,
        decided_by_rule: false,
        hard_signals: hard,
    }
}

fn truncate(s: &str, max: usize) -> String {
    let cleaned = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if cleaned.chars().count() <= max {
        return cleaned;
    }
    cleaned.chars().take(max).collect::<String>() + "…"
}

#[cfg(test)]
mod tests {
    use super::*;
    use signet_mime::signals::Link;

    fn link(href: &str, text: &str, host: &str) -> Link {
        Link {
            href: href.into(),
            text: text.into(),
            host: host.into(),
        }
    }

    #[test]
    fn a_block_rule_decides_outright() {
        let v = score(0.0, &Signals::default(), Reputation::Blocked);
        assert_eq!(v.score, 1.0);
        assert!(v.decided_by_rule);
        assert!(v.explain().contains("block list"));
    }

    #[test]
    fn an_allow_rule_beats_every_other_signal() {
        // Even a message that looks awful: the user has said they want it.
        let mut s = Signals {
            dmarc: AuthVerdict::Fail,
            risky_attachments: vec!["setup.exe".into()],
            ..Default::default()
        };
        s.deceptive_links
            .push(link("https://evil.test", "paypal.com", "evil.test"));

        let v = score(0.95, &s, Reputation::Allowed);
        assert_eq!(v.score, 0.0);
        assert!(v.decided_by_rule);
    }

    #[test]
    fn a_known_correspondent_is_pulled_well_clear_of_the_threshold() {
        // The expensive mistake this filter can make is filing a customer's reply.
        let v = score(0.80, &Signals::default(), Reputation::Known);
        assert!(v.score < 0.5, "known sender still scored {}", v.score);
        assert!(v.explain().contains("corresponded"));
    }

    #[test]
    fn dmarc_failure_is_a_strong_push() {
        let s = Signals {
            dmarc: AuthVerdict::Fail,
            ..Default::default()
        };
        let v = score(0.3, &s, Reputation::Unknown);
        assert!(v.score > 0.6, "scored {}", v.score);
        assert!(v.explain().to_lowercase().contains("dmarc"));
    }

    #[test]
    fn passing_authentication_pulls_toward_the_inbox() {
        let s = Signals {
            dmarc: AuthVerdict::Pass,
            dkim: AuthVerdict::Pass,
            spf: AuthVerdict::Pass,
            ..Default::default()
        };
        let plain = score(0.5, &Signals::default(), Reputation::Unknown).score;
        let authed = score(0.5, &s, Reputation::Unknown).score;
        assert!(authed < plain);
    }

    #[test]
    fn silence_on_authentication_changes_nothing() {
        // Many servers add no Authentication-Results at all. Treating that as failure would file
        // an enormous amount of ordinary mail.
        let v = score(0.4, &Signals::default(), Reputation::Unknown);
        assert!((v.score - 0.4).abs() < 0.001, "scored {}", v.score);
    }

    #[test]
    fn a_deceptive_link_is_the_heaviest_content_signal() {
        let mut s = Signals::default();
        s.deceptive_links.push(link(
            "https://evil.test/login",
            "https://paypal.com/login",
            "evil.test",
        ));
        let v = score(0.2, &s, Reputation::Unknown);
        assert!(v.score > 0.6, "scored {}", v.score);
        let why = v.explain();
        assert!(why.contains("evil.test"), "{why}");
        assert!(why.contains("paypal.com"), "{why}");
    }

    #[test]
    fn an_executable_attachment_is_called_out_by_name() {
        let s = Signals {
            risky_attachments: vec!["invoice.exe".into()],
            ..Default::default()
        };
        let v = score(0.3, &s, Reputation::Unknown);
        assert!(v.score > 0.6);
        assert!(v.explain().contains("invoice.exe"));
    }

    #[test]
    fn a_well_behaved_newsletter_is_not_penalised_for_being_bulk() {
        // Suppliers send mailshots. A list that identifies itself and offers unsubscribe is
        // behaving correctly.
        let s = Signals {
            bulk: true,
            has_list_unsubscribe: true,
            dmarc: AuthVerdict::Pass,
            dkim: AuthVerdict::Pass,
            ..Default::default()
        };
        let v = score(0.45, &s, Reputation::Unknown);
        assert!(
            v.score < 0.45,
            "a compliant newsletter was pushed up to {}",
            v.score
        );
    }

    #[test]
    fn the_score_never_leaves_zero_to_one() {
        let mut s = Signals {
            dmarc: AuthVerdict::Fail,
            missing_message_id: true,
            missing_from: true,
            ip_literal_links: 2,
            risky_attachments: vec!["a.exe".into(), "b.scr".into()],
            reply_to_mismatch: true,
            ..Default::default()
        };
        s.deceptive_links
            .push(link("https://e.test", "bank.com", "e.test"));
        assert!(score(0.99, &s, Reputation::Unknown).score <= 1.0);
        assert!(score(0.0, &Signals::default(), Reputation::Known).score >= 0.0);
    }

    #[test]
    fn the_explanation_leads_with_what_mattered_most() {
        let mut s = Signals {
            reply_to_mismatch: true,
            ..Default::default()
        };
        s.deceptive_links
            .push(link("https://evil.test", "hsbc.com", "evil.test"));
        let why = score(0.1, &s, Reputation::Unknown).explain();
        assert!(
            why.starts_with("a link reads"),
            "the heaviest reason was not first: {why}"
        );
    }

    #[test]
    fn trivial_contributions_are_left_out_of_the_explanation() {
        let v = score(0.02, &Signals::default(), Reputation::Unknown);
        assert_eq!(v.explain(), "", "noise reached the explanation");
    }

    #[test]
    fn two_hard_signals_together_clear_the_default_threshold_untrained() {
        // The case that matters on day one: nothing has been trained yet, so the word statistics
        // contribute nothing and the structure has to carry the decision on its own.
        let mut s = Signals {
            dmarc: AuthVerdict::Fail,
            ..Default::default()
        };
        s.deceptive_links.push(link(
            "https://evil.test/login",
            "https://paypal.com",
            "evil.test",
        ));

        let v = score(0.0, &s, Reputation::Unknown);
        assert_eq!(v.hard_signals, 2);
        assert!(
            v.score >= 0.9,
            "a textbook phish scored {} against the 0.9 default",
            v.score
        );
    }

    #[test]
    fn one_hard_signal_alone_does_not_escalate() {
        // Forwarding breaks DMARC. On its own it must stay a nudge, not a verdict.
        let s = Signals {
            dmarc: AuthVerdict::Fail,
            ..Default::default()
        };
        let v = score(0.0, &s, Reputation::Unknown);
        assert_eq!(v.hard_signals, 1);
        assert!(!v.explain().contains("independent signs"));
        assert!(v.score < 0.9, "a single failed check filed the message");
    }

    #[test]
    fn broken_dkim_and_spf_are_not_counted_as_hard_signals() {
        // A customer forwarding from their phone breaks both, every time.
        let s = Signals {
            dkim: AuthVerdict::Fail,
            spf: AuthVerdict::Fail,
            ..Default::default()
        };
        let v = score(0.2, &s, Reputation::Unknown);
        assert_eq!(v.hard_signals, 0);
        assert!(v.score < 0.9, "a forwarded message scored {}", v.score);
    }

    #[test]
    fn knowing_the_sender_still_outweighs_the_escalation() {
        // The safety valve has to survive the change: a long-standing customer whose mail is
        // being forwarded and rewritten must not suddenly be filed.
        let mut s = Signals {
            dmarc: AuthVerdict::Fail,
            ..Default::default()
        };
        s.deceptive_links.push(link(
            "https://tracker.test/x",
            "www.customer.test",
            "tracker.test",
        ));

        let v = score(0.0, &s, Reputation::Known);
        assert!(
            v.score < 0.9,
            "a known correspondent was filed as junk at {}",
            v.score
        );
    }

    #[test]
    fn a_delivered_message_warns_about_what_is_wrong_and_nothing_else() {
        // On a message that was delivered anyway, "DMARC and DKIM both passed" would bury the
        // point. Warnings carry only the incriminating half.
        let mut s = Signals {
            dmarc: AuthVerdict::Fail,
            ..Default::default()
        };
        s.deceptive_links
            .push(link("https://evil.test", "hsbc.co.uk", "evil.test"));
        let v = score(0.0, &s, Reputation::Known);

        let w = v.warnings();
        assert!(w.contains("evil.test"), "{w}");
        assert!(
            !w.contains("corresponded"),
            "a reassuring reason reached the warning: {w}"
        );
    }

    #[test]
    fn a_clean_message_has_nothing_to_warn_about() {
        let s = Signals {
            dmarc: AuthVerdict::Pass,
            dkim: AuthVerdict::Pass,
            spf: AuthVerdict::Pass,
            ..Default::default()
        };
        let v = score(0.1, &s, Reputation::Unknown);
        assert_eq!(v.hard_signals, 0);
        assert_eq!(v.warnings(), "");
    }
}
