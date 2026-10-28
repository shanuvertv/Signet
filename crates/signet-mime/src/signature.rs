//! Pulling contact details out of an email signature.
//!
//! Everyone in this trade signs off with their direct line, their mobile and their job title, and
//! that information is retyped into a spreadsheet by hand in most offices. It is already arriving
//! in every message; this reads it.
//!
//! **Two rules keep the results trustworthy.**
//!
//! Details are only taken from a *detected signature block*, never from the message body. A quote
//! full of part numbers, an order confirmation with a reference like `+44 20 0000 1234 ext 5`, a
//! date written `2024 555 0100` — a body-wide search finds all of them and produces a contact list
//! nobody trusts. A signature block is a small, conventional place where a phone number means a
//! phone number.
//!
//! And details are only ever attributed to the *sender*. A forwarded enquiry carries the original
//! author's signature; filing that under the person who forwarded it would be worse than not
//! reading it at all. That rule is enforced by the caller, which only ever passes a message
//! alongside its own `From` address.

/// What a signature gave up. Every field is optional because most signatures have most of them
/// missing, and an empty string is not the same as "not found".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SignatureDetails {
    pub job_title: Option<String>,
    pub company: Option<String>,
    /// Landline or main switchboard.
    pub phone: Option<String>,
    pub mobile: Option<String>,
    pub website: Option<String>,
}

impl SignatureDetails {
    pub fn is_empty(&self) -> bool {
        self.job_title.is_none()
            && self.company.is_none()
            && self.phone.is_none()
            && self.mobile.is_none()
            && self.website.is_none()
    }
}

/// Words that mark the start of a quoted reply. Everything from here down belongs to somebody else.
const QUOTE_MARKERS: &[&str] = &[
    "-----original message-----",
    "________________________________",
    "from:",
    "sent from my",
    "get outlook for",
];

/// Openers for the legal boilerplate that follows many signatures. It is long, it is not contact
/// detail, and left in it swamps the block that is.
const DISCLAIMER_OPENERS: &[&str] = &[
    "this email and any",
    "this e-mail and any",
    "this message and any",
    "the information contained in this",
    "this email is confidential",
    "disclaimer:",
    "confidentiality notice",
    "please consider the environment",
    "p please consider",
];

/// Labels that say what a number is. Longest first, so `mobile` is not matched as `m`.
const MOBILE_LABELS: &[&str] = &["mobile", "cell", "mob", "cel", "m"];
const PHONE_LABELS: &[&str] = &[
    "telephone",
    "direct",
    "office",
    "landline",
    "phone",
    "tel",
    "ph",
    "t",
    "d",
];

/// Words that make a line look like a job title.
const TITLE_WORDS: &[&str] = &[
    "manager",
    "director",
    "engineer",
    "sales",
    "executive",
    "officer",
    "consultant",
    "coordinator",
    "supervisor",
    "specialist",
    "head of",
    "president",
    "founder",
    "partner",
    "analyst",
    "designer",
    "architect",
    "technician",
    "administrator",
    "assistant",
    "lead",
    "chief",
    "ceo",
    "cto",
    "cfo",
    "coo",
    "estimator",
    "purchaser",
    "buyer",
    "procurement",
];

/// Words that make a line look like a company rather than a person or a title.
const COMPANY_WORDS: &[&str] = &[
    "ltd",
    "limited",
    "llc",
    "l.l.c",
    "inc",
    "gmbh",
    "plc",
    "pte",
    "pty",
    "co.",
    "company",
    "group",
    "holdings",
    "trading",
    "industries",
    "electric",
    "lighting",
    "systems",
    "technologies",
    "solutions",
    "enterprises",
    "corporation",
    "corp",
    "s.a",
    "b.v",
    "fzc",
    "fze",
    "llp",
];

/// Read whatever contact detail the signature of `body` offers.
pub fn extract(body: &str) -> SignatureDetails {
    let block = match signature_block(body) {
        Some(b) => b,
        None => return SignatureDetails::default(),
    };

    let mut out = SignatureDetails::default();
    for line in &block {
        read_phones(line, &mut out);
        if out.website.is_none() {
            out.website = website_in(line);
        }
    }

    out.job_title = block.iter().find_map(|l| title_in(l));
    out.company = block.iter().find_map(|l| company_in(l));
    out
}

/// The lines that make up the signature, or `None` when there is nothing that looks like one.
///
/// `--` on its own line is the standard delimiter (RFC 3676 §4.3) and is believed outright when
/// present. Most mail does not use it, so the fallback is the tail of the message: signatures live
/// at the bottom, above any quoted reply, and are short.
fn signature_block(body: &str) -> Option<Vec<String>> {
    let text = body.replace("\r\n", "\n");

    // Anything below a quote marker was written by somebody else.
    let mut usable: Vec<&str> = Vec::new();
    for raw in text.lines() {
        let lower = raw.trim().to_lowercase();
        if raw.trim_start().starts_with('>') || QUOTE_MARKERS.iter().any(|m| lower.starts_with(m)) {
            break;
        }
        usable.push(raw);
    }

    // The standard delimiter, if it is there.
    if let Some(at) = usable.iter().rposition(|l| {
        let t = l.trim_end();
        t == "--" || t == "-- "
    }) {
        let lines = clean(&usable[at + 1..]);
        return (!lines.is_empty()).then_some(lines);
    }

    // Otherwise the tail. Twelve lines is generous for a signature and short enough that a
    // paragraph of prose rarely reaches back this far with anything phone-shaped in it.
    let tail_start = usable.len().saturating_sub(12);
    let lines = clean(&usable[tail_start..]);
    (!lines.is_empty()).then_some(lines)
}

/// Trim, drop blanks, and stop at legal boilerplate.
fn clean(lines: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    for line in lines {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let lower = trimmed.to_lowercase();
        if DISCLAIMER_OPENERS.iter().any(|d| lower.starts_with(d)) {
            break;
        }
        out.push(trimmed.to_string());
    }
    out
}

/// Read any phone numbers on one line, filing them by their label.
fn read_phones(line: &str, out: &mut SignatureDetails) {
    let Some(number) = phone_in(line) else {
        return;
    };

    // The label is whatever precedes the number, which is where "Mobile:" or "T." lives.
    let before = line
        .split(&number[..number.len().min(4)])
        .next()
        .unwrap_or("")
        .to_lowercase();
    let labelled = |set: &[&str]| {
        set.iter().any(|l| {
            before
                .rsplit(|c: char| !c.is_ascii_alphabetic())
                .find(|w| !w.is_empty())
                .map(|w| w == *l)
                .unwrap_or(false)
        })
    };

    if labelled(MOBILE_LABELS) {
        out.mobile.get_or_insert(number);
    } else if labelled(PHONE_LABELS) {
        out.phone.get_or_insert(number);
    } else {
        // Unlabelled. The first is far more often the main number than the mobile.
        out.phone.get_or_insert(number);
    }
}

/// A phone number on this line, normalised to how it was written minus decoration.
///
/// Deliberately conservative. Seven digits is the shortest real subscriber number; more than
/// fifteen is longer than E.164 allows and is almost always a reference or an order number.
fn phone_in(line: &str) -> Option<String> {
    let mut best: Option<String> = None;
    let bytes: Vec<char> = line.chars().collect();
    let mut i = 0;

    while i < bytes.len() {
        if !(bytes[i].is_ascii_digit() || bytes[i] == '+') {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len()
            && (bytes[i].is_ascii_digit()
                || matches!(bytes[i], '+' | ' ' | '-' | '(' | ')' | '.' | '/'))
        {
            i += 1;
        }
        let candidate: String = bytes[start..i].iter().collect();
        let digits = candidate.chars().filter(char::is_ascii_digit).count();

        if (7..=15).contains(&digits) {
            let tidy = candidate
                .trim()
                .trim_end_matches(['.', '-', '/'])
                .to_string();
            // Prefer the longest match on the line: "+971 4 123 4567" beats a bare "1234567".
            if best
                .as_ref()
                .map(|b: &String| b.len() < tidy.len())
                .unwrap_or(true)
            {
                best = Some(tidy);
            }
        }
    }
    best
}

fn website_in(line: &str) -> Option<String> {
    for word in line.split_whitespace() {
        let w =
            word.trim_matches(|c: char| !c.is_alphanumeric() && c != '/' && c != ':' && c != '.');
        let lower = w.to_lowercase();
        if lower.starts_with("http://") || lower.starts_with("https://") {
            return Some(w.to_string());
        }
        // A bare domain only counts with a `www.`, or "example.com" in prose would qualify.
        if lower.starts_with("www.") && lower.len() > 5 {
            return Some(w.to_string());
        }
    }
    None
}

fn title_in(line: &str) -> Option<String> {
    // A long line is a sentence, not a job title.
    if line.chars().count() > 60 || phone_in(line).is_some() {
        return None;
    }
    let lower = line.to_lowercase();
    TITLE_WORDS
        .iter()
        .any(|w| lower.contains(w))
        .then(|| line.to_string())
}

fn company_in(line: &str) -> Option<String> {
    if line.chars().count() > 70 || phone_in(line).is_some() {
        return None;
    }
    let lower = line.to_lowercase();
    // A line can look like both; a title word wins, because "Sales Manager" is not a company and
    // "Lighting Solutions LLC" does not usually contain one.
    if TITLE_WORDS.iter().any(|w| lower.contains(w)) {
        return None;
    }
    COMPANY_WORDS
        .iter()
        .any(|w| {
            lower
                .split(|c: char| !c.is_alphanumeric() && c != '.')
                .any(|token| token == *w)
        })
        .then(|| line.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_standard_delimited_signature_is_read() {
        let body = "Thanks for the quote, we will confirm on Monday.\n\
                    \n\
                    -- \n\
                    Amir Rahman\n\
                    Sales Manager\n\
                    Gulf Lighting Solutions LLC\n\
                    Tel: +971 4 123 4567\n\
                    Mobile: +971 50 987 6543\n\
                    www.gulflighting.ae\n";
        let d = extract(body);
        assert_eq!(d.job_title.as_deref(), Some("Sales Manager"));
        assert_eq!(d.company.as_deref(), Some("Gulf Lighting Solutions LLC"));
        assert_eq!(d.phone.as_deref(), Some("+971 4 123 4567"));
        assert_eq!(d.mobile.as_deref(), Some("+971 50 987 6543"));
        assert_eq!(d.website.as_deref(), Some("www.gulflighting.ae"));
    }

    #[test]
    fn a_signature_without_the_delimiter_is_still_read() {
        // Most real mail has no `--` line; Outlook does not add one.
        let body = "Please find the revised drawing attached.\n\
                    \n\
                    Best regards,\n\
                    Sara Khan\n\
                    Procurement Officer\n\
                    Emirates Trading Co.\n\
                    T. 04 555 0100\n";
        let d = extract(body);
        assert_eq!(d.job_title.as_deref(), Some("Procurement Officer"));
        assert_eq!(d.phone.as_deref(), Some("04 555 0100"));
    }

    #[test]
    fn numbers_in_the_body_are_not_mistaken_for_contact_details() {
        // The reason extraction is confined to the signature block at all. An order reference and
        // a price are both digit runs of a plausible length.
        let body = "Your order 3005512841 has shipped against invoice 9912340055.\n\
                    The total was 12,450.00 AED.\n\
                    \n\
                    -- \n\
                    Dispatch\n";
        let d = extract(body);
        assert_eq!(d.phone, None, "an order number became a phone number");
        assert_eq!(d.mobile, None);
    }

    #[test]
    fn a_quoted_reply_below_the_signature_is_ignored() {
        // Replying keeps the other person's signature in the message. Reading it would file their
        // details against the wrong contact.
        let body = "Confirmed, thank you.\n\
                    \n\
                    -- \n\
                    Amir Rahman\n\
                    Mobile: +971 50 111 2222\n\
                    \n\
                    From: Sara Khan\n\
                    Sent: Monday\n\
                    Mobile: +971 50 999 8888\n";
        let d = extract(body);
        assert_eq!(d.mobile.as_deref(), Some("+971 50 111 2222"));
    }

    #[test]
    fn a_legal_disclaimer_does_not_swallow_the_signature() {
        let body = "Regards,\n\
                    Amir Rahman\n\
                    Sales Manager\n\
                    Mobile: 050 111 2222\n\
                    \n\
                    This email and any attachments are confidential and may be privileged. If you\n\
                    have received this in error please call 800 000 0000 immediately.\n";
        let d = extract(body);
        assert_eq!(d.mobile.as_deref(), Some("050 111 2222"));
        assert_eq!(
            d.phone, None,
            "the disclaimer's number was taken as a contact detail"
        );
    }

    #[test]
    fn an_unlabelled_number_is_treated_as_the_main_line() {
        let body = "-- \nAmir Rahman\n+971 4 123 4567\n";
        let d = extract(body).clone();
        assert_eq!(d.phone.as_deref(), Some("+971 4 123 4567"));
        assert_eq!(d.mobile, None);
    }

    #[test]
    fn the_longest_number_on_a_line_wins() {
        // "+971 4 123 4567" contains shorter digit runs; the full number is the useful one.
        let body = "-- \nTel +971 4 123 4567\n";
        assert_eq!(extract(body).phone.as_deref(), Some("+971 4 123 4567"));
    }

    #[test]
    fn a_message_with_no_signature_gives_nothing() {
        let d = extract("Yes, that works. See you Tuesday.\n");
        assert!(d.is_empty());
    }

    #[test]
    fn a_title_line_is_not_also_taken_as_the_company() {
        let body = "-- \nAmir Rahman\nSales Manager\nGulf Lighting LLC\n";
        let d = extract(body);
        assert_eq!(d.job_title.as_deref(), Some("Sales Manager"));
        assert_eq!(d.company.as_deref(), Some("Gulf Lighting LLC"));
    }

    #[test]
    fn a_sentence_is_never_a_job_title() {
        let body = "-- \nPlease let me know if the sales manager needs anything further from us\n";
        assert_eq!(extract(body).job_title, None);
    }

    #[test]
    fn a_full_url_is_preferred_to_nothing() {
        let body = "-- \nAmir\nhttps://www.gulflighting.ae/products\n";
        assert_eq!(
            extract(body).website.as_deref(),
            Some("https://www.gulflighting.ae/products")
        );
    }

    #[test]
    fn a_domain_mentioned_in_prose_is_not_a_website() {
        // Only `www.` or a scheme counts, or every mention of a domain becomes a contact field.
        let body = "-- \nAmir\nWe also supply example.com fittings\n";
        assert_eq!(extract(body).website, None);
    }
}
