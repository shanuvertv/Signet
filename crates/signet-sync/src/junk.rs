//! Junk filtering: a Graham-style naive Bayes classifier over message tokens, with a heuristic
//! prior for the cold-start case.
//!
//! Two problems a mail filter has to solve, and they need different tools:
//!
//! 1. **Cold start.** A brand-new install has never seen a spam message, so Bayes has nothing to
//!    say. [`heuristic_score`] carries the filter until the corpus is big enough to trust.
//! 2. **Personalisation.** What counts as junk in a company mailbox is not what counts as junk
//!    generally. Bayes learns that from the user marking messages, and overtakes the heuristics
//!    once it has enough evidence.
//!
//! The costs here are asymmetric and the design reflects it: a spam message in the Inbox is an
//! annoyance, a real message silently filed in Junk is a lost order. So the default threshold is
//! high, the classifier is biased toward "not junk" when uncertain, and nothing is ever deleted —
//! only moved to a folder the user can look in.

use std::collections::{HashMap, HashSet};

/// Corpus counts for one token. Defined by the store, which is what persists them.
pub use signet_store::TokenCounts;

/// How many messages of each kind the corpus has been trained on.
#[derive(Debug, Clone, Copy, Default)]
pub struct CorpusSize {
    pub spam_messages: i64,
    pub ham_messages: i64,
}

impl CorpusSize {
    /// Below this, Bayes is not yet worth trusting on its own.
    ///
    /// Graham used far larger corpora, but he was classifying a public mail stream. For one
    /// mailbox, a couple of dozen examples of each already beats the heuristics.
    const MIN_TRAINED: i64 = 20;

    pub fn is_trained(&self) -> bool {
        self.spam_messages >= Self::MIN_TRAINED && self.ham_messages >= Self::MIN_TRAINED
    }

    /// 0.0 to 1.0 — how much weight the Bayes score should get versus the heuristics.
    fn confidence(&self) -> f64 {
        if self.is_trained() {
            return 1.0;
        }
        let least = self.spam_messages.min(self.ham_messages) as f64;
        (least / Self::MIN_TRAINED as f64).clamp(0.0, 1.0)
    }
}

/// What the filter looks at. Deliberately not the whole message: headers and the first part of the
/// body carry nearly all the signal, and reading a 500 MB attachment to classify it would be
/// absurd.
#[derive(Debug, Clone, Default)]
pub struct Features {
    pub from_name: String,
    pub from_addr: String,
    pub subject: String,
    pub body: String,
    pub has_remote_content: bool,
    /// Recipient addresses, so "not addressed to me" can count against a message.
    pub recipients: Vec<String>,
    /// The address of the mailbox that received it.
    pub account_addr: String,
}

/// Split text into classifier tokens.
///
/// Case-folded, punctuation-stripped, with single characters and pure numbers dropped — they carry
/// no signal and would bloat the corpus. Tokens are prefixed by where they came from, so
/// `subject:free` and `body:free` are learned separately: "free" in a subject line means something
/// quite different from "free" in the body of a colleague's mail.
pub fn tokenize(features: &Features) -> HashSet<String> {
    let mut out = HashSet::new();

    let mut add = |prefix: &str, text: &str| {
        for raw in text.split(|c: char| !c.is_alphanumeric() && c != '$' && c != '!' && c != '\'') {
            let token = raw.trim_matches('\'').to_lowercase();
            if token.len() < 2 || token.len() > 24 {
                continue;
            }
            if token.chars().all(|c| c.is_ascii_digit()) {
                continue;
            }
            out.insert(format!("{prefix}:{token}"));
        }
    };

    add("from", &features.from_name);
    add("from", &features.from_addr);
    add("subj", &features.subject);
    // Only the head of the body: enough for signal, bounded in cost.
    let body: String = features.body.chars().take(4000).collect();
    add("body", &body);

    // The sending domain on its own is one of the strongest single features there is.
    if let Some(domain) = features.from_addr.rsplit('@').next() {
        if !domain.is_empty() {
            out.insert(format!("domain:{}", domain.to_lowercase()));
        }
    }

    if features.has_remote_content {
        out.insert("meta:remote-images".to_string());
    }
    if !features.account_addr.is_empty()
        && !features
            .recipients
            .iter()
            .any(|r| r.eq_ignore_ascii_case(&features.account_addr))
    {
        // Bulk mail is frequently Bcc'd to everyone, so the recipient list does not contain you.
        out.insert("meta:not-addressed-to-me".to_string());
    }

    out
}

/// Rules of thumb, for before the corpus knows anything.
///
/// Returns 0.0 to 1.0. Each signal is weak on its own — the point is that several together are
/// not. None of these is strong enough to file a message by itself at the default threshold.
pub fn heuristic_score(features: &Features) -> f64 {
    let subject = features.subject.trim();
    let body: String = features.body.chars().take(4000).collect();
    let haystack = format!("{} {}", subject.to_lowercase(), body.to_lowercase());
    let mut score: f64 = 0.0;

    // Phrases that are overwhelmingly commercial-bulk rather than correspondence.
    const PHRASES: [(&str, f64); 14] = [
        ("viagra", 0.35),
        ("crypto", 0.10),
        ("bitcoin", 0.12),
        ("lottery", 0.30),
        ("you have won", 0.30),
        ("risk-free", 0.20),
        ("act now", 0.18),
        ("limited time offer", 0.18),
        ("click here now", 0.18),
        ("wire transfer", 0.15),
        ("nigerian prince", 0.40),
        ("100% free", 0.22),
        ("make money fast", 0.30),
        ("increase your sales", 0.15),
    ];
    for (phrase, weight) in PHRASES {
        if haystack.contains(phrase) {
            score += weight;
        }
    }

    // SHOUTING in the subject. Only counts for a subject long enough for it to be deliberate —
    // "FYI" and "RE: PO 4471" are not shouting.
    let letters: Vec<char> = subject.chars().filter(|c| c.is_alphabetic()).collect();
    if letters.len() >= 8 && letters.iter().all(|c| c.is_uppercase()) {
        score += 0.20;
    }

    if subject.matches('!').count() >= 3 {
        score += 0.15;
    }

    // Bulk senders lean on both together; ordinary correspondence rarely does.
    if features.has_remote_content && haystack.contains("unsubscribe") {
        score += 0.15;
    }

    // A display name pretending to be an address that is not the real sender.
    if features.from_name.contains('@')
        && !features
            .from_name
            .to_lowercase()
            .contains(&features.from_addr.to_lowercase())
    {
        score += 0.25;
    }

    // No sender at all.
    if features.from_addr.trim().is_empty() {
        score += 0.20;
    }

    score.clamp(0.0, 1.0)
}

/// Probability that a message is junk, given the corpus.
///
/// Graham's method: take the most *interesting* tokens — the ones furthest from neutral — and
/// combine them. Using every token lets a long message of neutral words drown out the few that
/// actually mean something.
pub fn bayes_score(
    tokens: &HashSet<String>,
    counts: &HashMap<String, TokenCounts>,
    corpus: CorpusSize,
) -> f64 {
    if corpus.spam_messages == 0 || corpus.ham_messages == 0 {
        return 0.5; // No evidence either way.
    }

    // Only tokens the corpus has actually seen carry information.
    let mut interesting: Vec<f64> = tokens
        .iter()
        .filter_map(|t| counts.get(t))
        .map(|c| token_probability(*c, corpus))
        .collect();

    if interesting.is_empty() {
        return 0.5;
    }

    // Keep the 15 furthest from neutral. Using every token lets a long run of unremarkable words
    // drown out the few that mean something.
    interesting.sort_by(|a, b| {
        (b - 0.5)
            .abs()
            .partial_cmp(&(a - 0.5).abs())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    interesting.truncate(15);

    // Combine: prod(p) / (prod(p) + prod(1-p)), in log space so a long run of small
    // probabilities cannot underflow to zero and silently make everything look like ham.
    let log_spam: f64 = interesting.iter().map(|p| p.ln()).sum();
    let log_ham: f64 = interesting.iter().map(|p| (1.0 - p).ln()).sum();
    let max = log_spam.max(log_ham);
    let spam = (log_spam - max).exp();
    let ham = (log_ham - max).exp();
    (spam / (spam + ham)).clamp(0.0, 1.0)
}

/// Probability that a message containing this token is junk.
fn token_probability(counts: TokenCounts, corpus: CorpusSize) -> f64 {
    // Doubling the ham count biases toward false negatives, which is the error we prefer: a spam
    // in the Inbox costs a click, a real message in Junk can cost an order.
    let ham_rate = (counts.ham as f64 * 2.0) / corpus.ham_messages.max(1) as f64;
    let spam_rate = counts.spam as f64 / corpus.spam_messages.max(1) as f64;

    let raw = spam_rate / (ham_rate + spam_rate).max(f64::EPSILON);

    // Clamped away from certainty: one token must never be able to decide a message on its own,
    // and a token seen only once or twice is not evidence of much.
    let evidence = counts.spam + counts.ham;
    let (lo, hi) = if evidence < 5 {
        (0.20, 0.80)
    } else {
        (0.01, 0.99)
    };
    raw.clamp(lo, hi)
}

/// The score the ingestion pipeline files on.
///
/// Blends heuristics and Bayes by how much the corpus has actually learned, so the filter is
/// useful on day one and personalised by day thirty without a switchover the user would notice.
pub fn combined_score(
    features: &Features,
    counts: &HashMap<String, TokenCounts>,
    corpus: CorpusSize,
) -> f64 {
    let heuristic = heuristic_score(features);
    let confidence = corpus.confidence();
    if confidence <= 0.0 {
        return heuristic;
    }
    let tokens = tokenize(features);
    let bayes = bayes_score(&tokens, counts, corpus);
    (bayes * confidence + heuristic * (1.0 - confidence)).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn features(subject: &str, body: &str, from: &str) -> Features {
        Features {
            from_name: String::new(),
            from_addr: from.to_string(),
            subject: subject.to_string(),
            body: body.to_string(),
            has_remote_content: false,
            recipients: vec!["dev@example.com".into()],
            account_addr: "dev@example.com".into(),
        }
    }

    fn corpus(spam: i64, ham: i64) -> CorpusSize {
        CorpusSize {
            spam_messages: spam,
            ham_messages: ham,
        }
    }

    #[test]
    fn tokens_are_namespaced_by_where_they_came_from() {
        let f = features("Free stuff", "free shipping", "a@b.test");
        let tokens = tokenize(&f);
        assert!(tokens.contains("subj:free"));
        assert!(tokens.contains("body:free"));
        // The same word in two places must be two features, not one.
        assert_ne!("subj:free", "body:free");
        assert!(tokens.contains("domain:b.test"));
    }

    #[test]
    fn tokenizer_drops_noise() {
        let f = features("a b 12345 hello", "", "x@y.test");
        let tokens = tokenize(&f);
        assert!(
            !tokens.iter().any(|t| t.ends_with(":a")),
            "single chars kept"
        );
        assert!(!tokens.contains("subj:12345"), "pure numbers kept");
        assert!(tokens.contains("subj:hello"));
    }

    #[test]
    fn ordinary_business_mail_scores_low_on_heuristics() {
        // The case that matters most: this must not be filed.
        let f = features(
            "Re: PO 4471 delivery date",
            "Hi, can you confirm the ship date for the pendant order? Thanks.",
            "buyer@customer.test",
        );
        assert!(
            heuristic_score(&f) < 0.3,
            "business mail scored {}",
            heuristic_score(&f)
        );
    }

    #[test]
    fn obvious_junk_scores_high_on_heuristics() {
        let f = features(
            "YOU HAVE WON THE LOTTERY!!!",
            "Click here now to claim your 100% free prize. Act now!",
            "",
        );
        assert!(
            heuristic_score(&f) > 0.6,
            "junk scored {}",
            heuristic_score(&f)
        );
    }

    #[test]
    fn short_uppercase_subjects_are_not_shouting() {
        // "FYI" and "RE: PO 4471" are normal; penalising them would file real mail.
        assert!(heuristic_score(&features("FYI", "see attached", "a@b.test")) < 0.1);
        assert!(heuristic_score(&features("RE: PO 4471", "ok", "a@b.test")) < 0.1);
    }

    #[test]
    fn a_display_name_impersonating_another_address_is_suspicious() {
        let mut f = features("Invoice", "please pay", "attacker@evil.test");
        f.from_name = "accounts@example.com".into();
        assert!(heuristic_score(&f) >= 0.25);
    }

    #[test]
    fn an_empty_corpus_is_neutral_not_confident() {
        let tokens = tokenize(&features("anything", "at all", "a@b.test"));
        assert_eq!(bayes_score(&tokens, &HashMap::new(), corpus(0, 0)), 0.5);
    }

    #[test]
    fn bayes_learns_to_separate_two_corpora() {
        let mut counts = HashMap::new();
        counts.insert("subj:invoice".to_string(), TokenCounts { spam: 0, ham: 40 });
        counts.insert("subj:winner".to_string(), TokenCounts { spam: 40, ham: 0 });
        let c = corpus(50, 50);

        let spammy = tokenize(&features("winner winner", "", "a@b.test"));
        let hammy = tokenize(&features("invoice attached", "", "a@b.test"));

        assert!(bayes_score(&spammy, &counts, c) > 0.7);
        assert!(bayes_score(&hammy, &counts, c) < 0.3);
    }

    #[test]
    fn a_single_rare_token_cannot_decide_a_message() {
        // Otherwise one unlucky word in a real message files it as junk.
        let mut counts = HashMap::new();
        counts.insert("subj:fluke".to_string(), TokenCounts { spam: 1, ham: 0 });
        let tokens = tokenize(&features("fluke", "", "a@b.test"));
        let score = bayes_score(&tokens, &counts, corpus(50, 50));
        assert!(score <= 0.8, "one rare token decided the message: {score}");
    }

    #[test]
    fn scoring_is_biased_toward_keeping_mail() {
        // Equal evidence both ways must not read as junk: the false-positive costs more.
        let mut counts = HashMap::new();
        counts.insert("subj:offer".to_string(), TokenCounts { spam: 20, ham: 20 });
        let tokens = tokenize(&features("offer", "", "a@b.test"));
        assert!(bayes_score(&tokens, &counts, corpus(50, 50)) < 0.5);
    }

    #[test]
    fn long_messages_do_not_underflow_to_a_false_verdict() {
        // Combining many small probabilities naively multiplies to zero; log space must not.
        let mut counts = HashMap::new();
        let mut body = String::new();
        for i in 0..200 {
            let word = format!("word{i}");
            body.push_str(&word);
            body.push(' ');
            counts.insert(format!("body:{word}"), TokenCounts { spam: 30, ham: 1 });
        }
        let tokens = tokenize(&features("subject", &body, "a@b.test"));
        let score = bayes_score(&tokens, &counts, corpus(50, 50));
        assert!(score.is_finite(), "score was not finite");
        assert!(score > 0.9, "overwhelming spam evidence scored {score}");
    }

    #[test]
    fn cold_start_uses_heuristics_and_trained_corpus_uses_bayes() {
        let junk = features("YOU HAVE WON THE LOTTERY!!!", "act now", "");
        let mut counts = HashMap::new();
        counts.insert("subj:won".to_string(), TokenCounts { spam: 0, ham: 30 });

        // Untrained: the heuristics carry it, and they say junk.
        let cold = combined_score(&junk, &counts, corpus(0, 0));
        assert!(cold > 0.6, "cold start ignored the heuristics: {cold}");

        // Trained the other way: Bayes should pull the verdict down.
        let warm = combined_score(&junk, &counts, corpus(60, 60));
        assert!(warm < cold, "training had no effect: {warm} vs {cold}");
    }

    #[test]
    fn confidence_ramps_rather_than_switching_over() {
        assert_eq!(corpus(0, 0).confidence(), 0.0);
        assert!(corpus(10, 10).confidence() > 0.0);
        assert!(corpus(10, 10).confidence() < 1.0);
        assert_eq!(corpus(30, 30).confidence(), 1.0);
        // One-sided training is not training.
        assert!(!corpus(100, 0).is_trained());
    }

    #[test]
    fn bulk_mail_markers_become_features() {
        let mut f = features("Newsletter", "unsubscribe here", "news@vendor.test");
        f.has_remote_content = true;
        f.recipients = vec!["someone-else@elsewhere.test".into()];
        let tokens = tokenize(&f);
        assert!(tokens.contains("meta:remote-images"));
        assert!(tokens.contains("meta:not-addressed-to-me"));
    }
}
