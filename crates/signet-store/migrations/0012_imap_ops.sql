-- Changes made locally that the IMAP server has not been told about yet.
--
-- Moves, deletes and sent copies used to change only this computer, so a shared mailbox was not
-- shared: a colleague still saw mail you had filed or deleted. Each change is written here in the
-- same transaction as the local change and replayed at the start of the next sync, the same
-- "record now, push on sync" shape flags already use.
CREATE TABLE imap_ops (
    id          INTEGER PRIMARY KEY,
    account_id  INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    kind        TEXT    NOT NULL,          -- move | delete | append
    -- The local message this is about. Not a foreign key: a purged message still owes the server
    -- its delete.
    message_id  INTEGER,
    from_path   TEXT,                      -- move, delete: the mailbox the UID belongs to
    uid         INTEGER,                   -- move, delete
    to_path     TEXT,                      -- move, append: the target mailbox
    raw_path    TEXT,                      -- append: the file to upload
    attempts    INTEGER NOT NULL DEFAULT 0,
    last_error  TEXT,
    created_utc INTEGER NOT NULL
);

CREATE INDEX idx_imap_ops_account ON imap_ops(account_id, id);
