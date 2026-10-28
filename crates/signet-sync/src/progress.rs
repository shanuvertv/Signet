//! Telling the caller how a sync is getting on.
//!
//! A sync is the one operation in this application that routinely takes long enough for somebody to
//! wonder whether it has hung. A spinner cannot answer that question — it looks identical whether
//! the client is downloading its fortieth message or waiting out a TCP timeout against a server
//! that is never going to answer. Counts, sizes and a named phase can.
//!
//! The reporting is a callback rather than a channel so that a caller wanting none of it pays
//! nothing at all, and so the sync code has no opinion about where the numbers go: the desktop app
//! forwards them to every window, the CLI prints them, and the tests ignore them.

use std::sync::Arc;

/// Where a sync has got to.
///
/// Phases exist because the interesting delays are not all in the download. Connecting is where a
/// wrong port or a dead host burns twenty seconds, and saying so is the difference between "this is
/// broken" and "this is working through a slow handshake".
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Phase {
    /// Opening the connection and signing in.
    Connecting,
    /// Asking what is on the server.
    Listing,
    /// Downloading messages.
    Downloading,
    /// Sending queued mail.
    Sending,
    /// Retention, flag pushes, and closing the session cleanly.
    Finishing,
}

impl Phase {
    /// Wording for a status line, in the user's terms rather than the protocol's.
    pub fn label(self) -> &'static str {
        match self {
            Phase::Connecting => "Connecting",
            Phase::Listing => "Checking for new mail",
            Phase::Downloading => "Downloading",
            Phase::Sending => "Sending",
            Phase::Finishing => "Finishing up",
        }
    }
}

/// One progress report.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Progress {
    pub account_id: i64,
    pub phase: Phase,
    /// How many items are finished. Zero for phases that have no count.
    pub done: usize,
    /// How many there are in total, or zero when that is not yet known.
    ///
    /// Zero is deliberately "unknown" rather than "none": a bar cannot be drawn against a total
    /// nobody has counted yet, and pretending otherwise produces the bar that jumps to full and
    /// then sits there.
    pub total: usize,
    /// Which mailbox, for IMAP. Empty over POP3, which has only one.
    pub folder: String,
    /// Bytes fetched so far.
    pub bytes_done: u64,
    /// Bytes expected in total, or zero when not known.
    ///
    /// Both listings carry a size per message, so this is known before the first fetch — which
    /// matters, because bytes are what actually take the time. A message count puts a 40 MB
    /// drawing and a two-line reply on equal footing, and produces the bar that reaches 46 of 47
    /// and then appears to hang.
    pub bytes_total: u64,
}

impl Progress {
    pub fn new(account_id: i64, phase: Phase) -> Self {
        Self {
            account_id,
            phase,
            done: 0,
            total: 0,
            folder: String::new(),
            bytes_done: 0,
            bytes_total: 0,
        }
    }

    pub fn counted(account_id: i64, phase: Phase, done: usize, total: usize) -> Self {
        Self {
            account_id,
            phase,
            done,
            total,
            folder: String::new(),
            bytes_done: 0,
            bytes_total: 0,
        }
    }

    /// Attach the byte figures, which is what the bar is drawn from when they are known.
    pub fn with_bytes(mut self, done: u64, total: u64) -> Self {
        self.bytes_done = done;
        self.bytes_total = total;
        self
    }

    pub fn in_folder(mut self, folder: impl Into<String>) -> Self {
        self.folder = folder.into();
        self
    }

    /// 0.0 to 1.0, or `None` when nothing is known well enough to draw a bar.
    ///
    /// Bytes win over the message count wherever both are known: they are proportional to the time
    /// the transfer actually takes, and the count is not.
    pub fn fraction(&self) -> Option<f64> {
        if self.bytes_total > 0 {
            return Some((self.bytes_done as f64 / self.bytes_total as f64).clamp(0.0, 1.0));
        }
        (self.total > 0).then(|| (self.done as f64 / self.total as f64).clamp(0.0, 1.0))
    }
}

/// Somewhere for progress to go.
///
/// `Send + Sync` because a sync runs on the async runtime and may be polled from any worker thread.
pub type Reporter = Arc<dyn Fn(Progress) + Send + Sync>;

/// A reporter that may or may not be there, with the "is anybody listening" check in one place
/// rather than at every call site.
#[derive(Clone, Default)]
pub struct Progressing(Option<Reporter>);

impl Progressing {
    /// Report nothing. What every caller that does not care uses, and what the tests use.
    pub fn silent() -> Self {
        Self(None)
    }

    pub fn to(reporter: Reporter) -> Self {
        Self(Some(reporter))
    }

    pub fn send(&self, progress: Progress) {
        if let Some(f) = &self.0 {
            f(progress);
        }
    }

    /// Convenience for the common shape: a phase with no counts.
    pub fn phase(&self, account_id: i64, phase: Phase) {
        self.send(Progress::new(account_id, phase));
    }
}

impl std::fmt::Debug for Progressing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The closure has no useful Debug, and a sync report containing `Progressing { .. }` is
        // noise. Say only whether anything is listening.
        f.debug_struct("Progressing")
            .field("reporting", &self.0.is_some())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn an_unknown_total_has_no_fraction() {
        // A bar drawn against a total nobody has counted yet is a bar that lies.
        let p = Progress::new(1, Phase::Connecting);
        assert_eq!(p.fraction(), None);
    }

    #[test]
    fn a_fraction_is_bounded_even_if_the_counts_disagree() {
        // Servers add mail while a sync is running, so `done` can outrun the total that was taken
        // a moment earlier. That must not produce a bar wider than its track.
        let p = Progress::counted(1, Phase::Downloading, 12, 10);
        assert_eq!(p.fraction(), Some(1.0));
    }

    #[test]
    fn fractions_are_what_you_would_expect_in_between() {
        let p = Progress::counted(1, Phase::Downloading, 1, 4);
        assert_eq!(p.fraction(), Some(0.25));
    }

    #[test]
    fn bytes_drive_the_bar_when_they_are_known() {
        // Half the messages but a tenth of the bytes: the honest answer is a tenth, because the
        // message still to come is the 40 MB one.
        let p = Progress::counted(1, Phase::Downloading, 1, 2).with_bytes(1_000, 10_000);
        assert_eq!(p.fraction(), Some(0.1));
    }

    #[test]
    fn the_message_count_still_works_when_no_sizes_are_known() {
        let p = Progress::counted(1, Phase::Downloading, 1, 2);
        assert_eq!(p.fraction(), Some(0.5));
    }

    #[test]
    fn a_byte_total_that_is_overrun_still_clamps() {
        // Servers under-report sizes, and a message grows slightly once headers are added.
        let p = Progress::counted(1, Phase::Downloading, 2, 2).with_bytes(12_000, 10_000);
        assert_eq!(p.fraction(), Some(1.0));
    }

    #[test]
    fn a_silent_reporter_costs_nothing_and_says_nothing() {
        let quiet = Progressing::silent();
        quiet.phase(1, Phase::Connecting);
        quiet.send(Progress::counted(1, Phase::Downloading, 1, 2));
    }

    #[test]
    fn a_reporter_receives_every_report_in_order() {
        let seen: Arc<Mutex<Vec<Phase>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        let p = Progressing::to(Arc::new(move |progress: Progress| {
            sink.lock().unwrap().push(progress.phase);
        }));

        p.phase(1, Phase::Connecting);
        p.phase(1, Phase::Listing);
        p.send(Progress::counted(1, Phase::Downloading, 1, 3));

        assert_eq!(
            *seen.lock().unwrap(),
            vec![Phase::Connecting, Phase::Listing, Phase::Downloading]
        );
    }

    #[test]
    fn phases_are_worded_for_a_person_not_a_protocol() {
        assert_eq!(Phase::Listing.label(), "Checking for new mail");
        assert_eq!(Phase::Connecting.label(), "Connecting");
    }
}
