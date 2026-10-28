//! The outbox as a queue with a clock.
//!
//! Two things share this table and must not be confused. A row waiting to retry after a network
//! failure is a problem being worked on; a row held back until Monday morning is working exactly as
//! asked. They differ only by `scheduled_utc`, and the distinction decides what the user is shown
//! and what they are allowed to cancel.

use rusqlite::{OptionalExtension, params};
use serde::Serialize;

use crate::{Result, Store, now_utc};

/// A message waiting in the outbox.
#[derive(Debug, Clone, Serialize)]
pub struct OutboxItem {
    pub id: i64,
    pub account_id: i64,
    pub subject: String,
    pub to_addrs: String,
    pub state: String,
    /// When the user asked for it to go out. 0 for anything not deliberately scheduled.
    pub scheduled_utc: i64,
    pub attempts: i64,
    pub last_error: Option<String>,
    pub created_utc: i64,
    /// The composed draft, as JSON. Empty on rows written before scheduling existed.
    pub draft_json: String,
}

const COLUMNS: &str = "SELECT id, account_id, subject, to_addrs, state, scheduled_utc, attempts,
                              last_error, created_utc, draft_json FROM outbox";

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<OutboxItem> {
    Ok(OutboxItem {
        id: r.get(0)?,
        account_id: r.get(1)?,
        subject: r.get(2)?,
        to_addrs: r.get(3)?,
        state: r.get(4)?,
        scheduled_utc: r.get(5)?,
        attempts: r.get(6)?,
        last_error: r.get(7)?,
        created_utc: r.get(8)?,
        draft_json: r.get(9)?,
    })
}

impl Store {
    /// Queue a message to go out at `scheduled_utc`, or immediately when that is 0.
    pub fn queue_scheduled(
        &self,
        account_id: i64,
        to_addrs: &str,
        subject: &str,
        raw_path: &str,
        draft_json: &str,
        scheduled_utc: i64,
    ) -> Result<i64> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO outbox (account_id, to_addrs, subject, raw_path, created_utc,
                     scheduled_utc, next_attempt_utc, draft_json)
                 VALUES (?1,?2,?3,?4,?5,?6,?6,?7)",
                params![
                    account_id,
                    to_addrs,
                    subject,
                    raw_path,
                    now_utc(),
                    scheduled_utc,
                    draft_json
                ],
            )?;
            Ok(conn.last_insert_rowid())
        })
    }

    /// Everything queued whose time has come.
    ///
    /// Covers both halves of the queue on purpose: a scheduled message whose hour has arrived and a
    /// failed one whose backoff has expired are the same job from here.
    pub fn due_outbox(&self, now: i64) -> Result<Vec<OutboxItem>> {
        self.with_conn(|conn| {
            let sql =
                format!("{COLUMNS} WHERE state = 'queued' AND next_attempt_utc <= ?1 ORDER BY id");
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(params![now], row)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    /// Everything that has not gone out: held back on purpose, waiting to retry, or failed.
    ///
    /// All of it, not only the scheduled rows. A send that failed and was hidden here was a message
    /// the user believed had gone.
    pub fn scheduled_outbox(&self) -> Result<Vec<OutboxItem>> {
        self.with_conn(|conn| {
            let sql = format!(
                "{COLUMNS} WHERE state IN ('queued', 'failed', 'sending')
                 ORDER BY scheduled_utc, id"
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map([], row)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    pub fn outbox_item(&self, id: i64) -> Result<Option<OutboxItem>> {
        self.with_conn(|conn| {
            let sql = format!("{COLUMNS} WHERE id = ?1");
            Ok(conn.query_row(&sql, params![id], row).optional()?)
        })
    }

    /// Bring a scheduled message forward to now.
    pub fn send_scheduled_now(&self, id: i64) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                // A failed row is re-queued too: "Send now" is also how the user retries one.
                "UPDATE outbox SET state = 'queued', next_attempt_utc = ?2,
                     scheduled_utc = CASE WHEN scheduled_utc > 0 THEN ?2 ELSE 0 END
                 WHERE id = ?1 AND state IN ('queued', 'failed')",
                params![id, now_utc()],
            )?;
            Ok(())
        })
    }

    /// Drop a message that has not gone out yet.
    ///
    /// Refuses once the state is `sending` or `sent`: at that point the server may already have it,
    /// and removing the row would only hide a message that is on its way.
    pub fn cancel_outbox(&self, id: i64) -> Result<bool> {
        self.with_conn(|conn| {
            let n = conn.execute(
                "DELETE FROM outbox WHERE id = ?1 AND state IN ('queued', 'failed')",
                params![id],
            )?;
            Ok(n > 0)
        })
    }

    /// Where the built message for a queued row is on disk.
    pub fn outbox_raw_path(&self, id: i64) -> Result<Option<String>> {
        self.with_conn(|conn| {
            Ok(conn
                .query_row(
                    "SELECT raw_path FROM outbox WHERE id = ?1",
                    params![id],
                    |r| r.get(0),
                )
                .optional()?)
        })
    }

    /// Take a queued row for sending. False when someone else already has it.
    ///
    /// The one guard against sending twice: the timer, "Send now" and a second window can all find
    /// the same due row, and only the caller whose update matched may send it.
    pub fn claim_outbox(&self, id: i64) -> Result<bool> {
        self.with_conn(|conn| {
            let n = conn.execute(
                "UPDATE outbox SET state = 'sending' WHERE id = ?1 AND state = 'queued'",
                params![id],
            )?;
            Ok(n > 0)
        })
    }

    /// Rows left `sending` by a run that ended mid-send, parked as failed for the user to decide.
    ///
    /// Not re-queued: the server may have accepted the message before the crash, and sending it
    /// again automatically would be a duplicate in a customer's inbox.
    pub fn recover_interrupted_sends(&self) -> Result<usize> {
        self.with_conn(|conn| {
            Ok(conn.execute(
                "UPDATE outbox SET state = 'failed',
                     last_error = 'Interrupted while sending. Check Sent Items before sending again.'
                 WHERE state = 'sending'",
                [],
            )?)
        })
    }

    /// Push a failed attempt out into the future.
    pub fn defer_outbox(&self, id: i64, next_attempt_utc: i64) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE outbox SET next_attempt_utc = ?2 WHERE id = ?1",
                params![id, next_attempt_utc],
            )?;
            Ok(())
        })
    }
}
