-- Initial schema. See PLAN.md §5.
--
-- The local store is the source of truth: POP3 has no folders, flags, or search, so everything
-- Outlook-like here is a client-side construct we own. Every table that holds mail is
-- account-scoped from the start (ADR 0001 / D4).

CREATE TABLE accounts (
    id                INTEGER PRIMARY KEY,
    display_name      TEXT    NOT NULL,
    email             TEXT    NOT NULL,

    pop_host          TEXT    NOT NULL,
    pop_port          INTEGER NOT NULL,
    pop_security      TEXT    NOT NULL,          -- tls | starttls | plain
    pop_user          TEXT    NOT NULL,

    smtp_host         TEXT    NOT NULL DEFAULT '',
    smtp_port         INTEGER NOT NULL DEFAULT 587,
    smtp_security     TEXT    NOT NULL DEFAULT 'starttls',
    smtp_user         TEXT    NOT NULL DEFAULT '',

    -- The keystore handle. The password itself is never stored here (PLAN.md §7.1).
    credential_handle TEXT    NOT NULL,

    leave_on_server   INTEGER NOT NULL DEFAULT 1,
    delete_after_days INTEGER,                   -- NULL = never delete from the server
    poll_minutes      INTEGER NOT NULL DEFAULT 5,
    signature         TEXT    NOT NULL DEFAULT '',
    is_enabled        INTEGER NOT NULL DEFAULT 1,
    sort_order        INTEGER NOT NULL DEFAULT 0,
    last_sync_utc     INTEGER,
    created_utc       INTEGER NOT NULL
);

CREATE UNIQUE INDEX idx_accounts_email ON accounts(email, pop_host);

CREATE TABLE folders (
    id          INTEGER PRIMARY KEY,
    account_id  INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    kind        TEXT    NOT NULL,                -- inbox|sent|drafts|deleted|junk|archive|custom
    name        TEXT    NOT NULL,
    parent_id   INTEGER REFERENCES folders(id) ON DELETE CASCADE,
    sort_order  INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX idx_folders_account ON folders(account_id, sort_order);

CREATE TABLE messages (
    id                INTEGER PRIMARY KEY,
    account_id        INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    folder_id         INTEGER NOT NULL REFERENCES folders(id) ON DELETE CASCADE,

    rfc822_message_id TEXT,
    in_reply_to       TEXT,
    thread_id         TEXT    NOT NULL,

    from_name         TEXT    NOT NULL DEFAULT '',
    from_addr         TEXT    NOT NULL DEFAULT '',
    subject           TEXT    NOT NULL DEFAULT '',
    date_utc          INTEGER NOT NULL,
    received_utc      INTEGER NOT NULL,

    is_read           INTEGER NOT NULL DEFAULT 0,
    is_flagged        INTEGER NOT NULL DEFAULT 0,
    is_answered       INTEGER NOT NULL DEFAULT 0,
    has_attachments   INTEGER NOT NULL DEFAULT 0,
    has_remote_content INTEGER NOT NULL DEFAULT 0,
    size_bytes        INTEGER NOT NULL DEFAULT 0,

    preview_text      TEXT    NOT NULL DEFAULT '',
    body_text         TEXT    NOT NULL DEFAULT '',
    body_html         TEXT,
    -- Raw RFC822 lives on disk so the DB stays small and a parser upgrade can re-read it without
    -- re-downloading anything.
    raw_path          TEXT    NOT NULL DEFAULT ''
);

CREATE INDEX idx_messages_folder_date ON messages(folder_id, date_utc DESC);
CREATE INDEX idx_messages_account_date ON messages(account_id, date_utc DESC);
CREATE INDEX idx_messages_thread ON messages(account_id, thread_id);
CREATE INDEX idx_messages_unread ON messages(folder_id, is_read);

CREATE TABLE recipients (
    message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    kind       TEXT    NOT NULL,                 -- to | cc | bcc
    name       TEXT    NOT NULL DEFAULT '',
    addr       TEXT    NOT NULL
);

CREATE INDEX idx_recipients_message ON recipients(message_id);

CREATE TABLE attachments (
    id          INTEGER PRIMARY KEY,
    message_id  INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    filename    TEXT    NOT NULL,
    mime_type   TEXT    NOT NULL DEFAULT 'application/octet-stream',
    size_bytes  INTEGER NOT NULL DEFAULT 0,
    content_id  TEXT,
    blob_path   TEXT    NOT NULL,
    is_inline   INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX idx_attachments_message ON attachments(message_id);

-- What makes re-polling idempotent. Keyed on the server's UIDL, never on the message number,
-- which servers renumber between sessions.
CREATE TABLE pop_seen (
    account_id             INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    uidl                   TEXT    NOT NULL,
    first_seen_utc         INTEGER NOT NULL,
    message_id             INTEGER REFERENCES messages(id) ON DELETE SET NULL,
    deleted_from_server_utc INTEGER,
    PRIMARY KEY (account_id, uidl)
);

CREATE TABLE outbox (
    id               INTEGER PRIMARY KEY,
    account_id       INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    state            TEXT    NOT NULL DEFAULT 'queued',   -- queued|sending|sent|failed
    to_addrs         TEXT    NOT NULL,                    -- comma separated envelope recipients
    subject          TEXT    NOT NULL DEFAULT '',
    raw_path         TEXT    NOT NULL,
    attempts         INTEGER NOT NULL DEFAULT 0,
    next_attempt_utc INTEGER NOT NULL DEFAULT 0,
    last_error       TEXT,
    created_utc      INTEGER NOT NULL
);

CREATE INDEX idx_outbox_pending ON outbox(state, next_attempt_utc);

CREATE TABLE contacts (
    id            INTEGER PRIMARY KEY,
    display_name  TEXT    NOT NULL DEFAULT '',
    addr          TEXT    NOT NULL UNIQUE,
    last_seen_utc INTEGER NOT NULL,
    times_seen    INTEGER NOT NULL DEFAULT 1
);

-- Search index. A plain (not external-content) FTS5 table: the body text lives on disk rather than
-- in a shadow column, so there is nothing for external-content mode to point at. rowid is the
-- message id, so deletes stay cheap and exact.
CREATE VIRTUAL TABLE messages_fts USING fts5(
    subject,
    sender,
    recipients,
    body,
    tokenize = 'unicode61 remove_diacritics 2'
);
