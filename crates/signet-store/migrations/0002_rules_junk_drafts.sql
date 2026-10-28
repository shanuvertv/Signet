-- Rules, junk filtering and outgoing attachments.

-- Naive-Bayes token counts for the junk filter. One shared corpus across accounts: a phrase that
-- is spam in one company mailbox is spam in all of them, and splitting it would mean every account
-- learning from scratch.
CREATE TABLE spam_tokens (
    token      TEXT    PRIMARY KEY,
    spam_count INTEGER NOT NULL DEFAULT 0,
    ham_count  INTEGER NOT NULL DEFAULT 0
);

-- Corpus size, needed to turn counts into probabilities.
CREATE TABLE spam_meta (
    id             INTEGER PRIMARY KEY CHECK (id = 1),
    spam_messages  INTEGER NOT NULL DEFAULT 0,
    ham_messages   INTEGER NOT NULL DEFAULT 0
);
INSERT INTO spam_meta (id) VALUES (1);

-- 0 = certainly wanted, 1 = certainly junk. Kept so the UI can explain a filing decision and so
-- the threshold can be re-tuned without re-scoring from the raw message.
ALTER TABLE messages ADD COLUMN spam_score REAL NOT NULL DEFAULT 0.0;

-- Whether a message was filed by a rule or the junk filter rather than landing in the Inbox
-- normally. Drives the "moved here by …" banner.
ALTER TABLE messages ADD COLUMN filed_by TEXT NOT NULL DEFAULT '';

ALTER TABLE accounts ADD COLUMN junk_filter_enabled INTEGER NOT NULL DEFAULT 1;

-- Junk score at or above this files the message. Per account, because tolerance differs.
ALTER TABLE accounts ADD COLUMN junk_threshold REAL NOT NULL DEFAULT 0.9;

-- Client-side rules. 0001 described this table but never created it, so it lands here rather than
-- by editing a migration that has already been applied on real installs.
--
-- The rule body is stored as opaque JSON: the grammar belongs to `signet-sync`, next to the engine
-- that evaluates it, and keeping it out of the schema means adding a condition type is not a
-- migration.
CREATE TABLE rules (
    id          INTEGER PRIMARY KEY,
    account_id  INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    name        TEXT    NOT NULL DEFAULT '',
    priority    INTEGER NOT NULL DEFAULT 0,
    enabled     INTEGER NOT NULL DEFAULT 1,
    match_json  TEXT    NOT NULL,
    action_json TEXT    NOT NULL DEFAULT ''
);

CREATE INDEX idx_rules_account ON rules(account_id, priority);

-- Attachments on a draft that has not been sent yet. Separate from `attachments`, which belongs to
-- a stored message and points at blobs we extracted; these point at files the user picked and may
-- still be editing.
CREATE TABLE draft_attachments (
    id         INTEGER PRIMARY KEY,
    message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    filename   TEXT    NOT NULL,
    source_path TEXT   NOT NULL,
    size_bytes INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX idx_draft_attachments_message ON draft_attachments(message_id);
