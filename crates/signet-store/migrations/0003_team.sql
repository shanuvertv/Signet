-- Team triage: priority, assignment, reminders, and the shared state that makes them work across
-- a sales team sharing one POP3 mailbox.

-- The people mail can be assigned to.
--
-- Deliberately not tied to accounts: the whole point is that several people work one shared
-- mailbox, so the team list is a property of the installation, not of the mail account.
CREATE TABLE team_members (
    id         INTEGER PRIMARY KEY,
    name       TEXT    NOT NULL,
    email      TEXT    NOT NULL UNIQUE,
    -- Hex colour used for the initials chip in the list. Chosen at creation, stable thereafter,
    -- so a person keeps the same colour on everyone's screen.
    color      TEXT    NOT NULL DEFAULT '#6b7280',
    initials   TEXT    NOT NULL DEFAULT '',
    is_active  INTEGER NOT NULL DEFAULT 1,
    -- Marks which member is the person sitting at this computer, so "My mail" means something.
    is_me      INTEGER NOT NULL DEFAULT 0,
    sort_order INTEGER NOT NULL DEFAULT 0
);

-- 0 = low, 1 = normal, 2 = high. Set from the sender's headers at ingest, overridable by hand.
ALTER TABLE messages ADD COLUMN priority INTEGER NOT NULL DEFAULT 1;

-- Who is dealing with this. NULL means nobody has picked it up.
ALTER TABLE messages ADD COLUMN assignee_id INTEGER REFERENCES team_members(id) ON DELETE SET NULL;

-- Where a message is in the workflow, for a shared mailbox used as a queue.
-- new | in_progress | waiting | done
ALTER TABLE messages ADD COLUMN work_status TEXT NOT NULL DEFAULT 'new';

CREATE INDEX idx_messages_assignee ON messages(assignee_id, work_status);
CREATE INDEX idx_messages_priority ON messages(folder_id, priority DESC, date_utc DESC);

-- Follow-ups. A message can have at most one open reminder; completing it sets done_utc rather
-- than deleting, so "what did we chase and when" survives.
CREATE TABLE reminders (
    id          INTEGER PRIMARY KEY,
    message_id  INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    due_utc     INTEGER NOT NULL,
    note        TEXT    NOT NULL DEFAULT '',
    created_utc INTEGER NOT NULL,
    done_utc    INTEGER,
    -- Set once the desktop notification has fired, so it does not fire again every poll.
    notified_utc INTEGER
);

CREATE INDEX idx_reminders_due ON reminders(done_utc, due_utc);
CREATE UNIQUE INDEX idx_reminders_open ON reminders(message_id) WHERE done_utc IS NULL;

-- Team state as it applies to a message, keyed by the RFC822 Message-ID rather than our local
-- row id.
--
-- This is the join key that makes sharing possible at all: two salespeople who each downloaded the
-- same message over POP3 have different local ids for it, but the same Message-ID. Rows can exist
-- here for messages this machine has not downloaded yet, and are adopted when it does.
CREATE TABLE shared_state (
    message_key   TEXT PRIMARY KEY,
    assignee_email TEXT,
    priority      INTEGER,
    work_status   TEXT,
    reminder_due_utc INTEGER,
    reminder_note TEXT,
    -- Last-write-wins is resolved on this, not on file order.
    updated_utc   INTEGER NOT NULL,
    updated_by    TEXT    NOT NULL DEFAULT '',
    -- Cleared once the change has been written to the shared file.
    dirty         INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX idx_shared_dirty ON shared_state(dirty);

-- Simple key/value for app-wide preferences: theme, shared-state file path, and so on.
CREATE TABLE settings (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
