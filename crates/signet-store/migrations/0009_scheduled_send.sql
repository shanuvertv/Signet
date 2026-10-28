-- Scheduled send.
--
-- The outbox already had `next_attempt_utc` and an index on it, so holding a message back until a
-- chosen time needs no new machinery for the *when* -- only a way to tell the two apart. A row
-- waiting to retry after a network failure and a row deliberately held until Monday morning look
-- identical otherwise, and they must not: one is a problem to report, the other is working as
-- asked, and only the second is something the user can cancel.
--
-- 0 means "not scheduled", which is what every existing row is.
ALTER TABLE outbox ADD COLUMN scheduled_utc INTEGER NOT NULL DEFAULT 0;

-- The composed draft, kept so a scheduled message survives closing the app.
--
-- The built message is already on disk at `raw_path`, but sending is not the whole job: the copy
-- filed in Sent and the removal of the original draft both need the structured draft, and
-- rebuilding it from the raw bytes would be guesswork.
ALTER TABLE outbox ADD COLUMN draft_json TEXT NOT NULL DEFAULT '';

CREATE INDEX idx_outbox_scheduled ON outbox(scheduled_utc) WHERE scheduled_utc > 0;
