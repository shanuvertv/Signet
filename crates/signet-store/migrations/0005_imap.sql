-- IMAP support.
--
-- The existing `pop_*` account columns are reused as the *incoming server* settings — host, port,
-- security, username are the same four facts either way. `protocol` says how to talk to them.
-- Renaming the columns would mean rebuilding a table that already holds real mail on installed
-- machines, for no gain.

ALTER TABLE accounts ADD COLUMN protocol TEXT NOT NULL DEFAULT 'pop3';   -- pop3 | imap

-- IMAP folders live on the server, so a local folder needs to know its server path and the
-- UIDVALIDITY it was synced under.
ALTER TABLE folders ADD COLUMN remote_path TEXT;

-- If the server changes UIDVALIDITY, every UID we hold for that folder is meaningless and the
-- folder must be resynced from scratch. Servers do this rarely but they are entitled to, and a
-- client that ignores it silently duplicates or loses mail.
ALTER TABLE folders ADD COLUMN uid_validity INTEGER;

ALTER TABLE messages ADD COLUMN remote_uid INTEGER;
ALTER TABLE messages ADD COLUMN remote_folder TEXT;

CREATE INDEX idx_messages_remote ON messages(account_id, remote_folder, remote_uid);

-- Generalises `pop_seen`, which was keyed on (account, uidl) because POP3 has exactly one folder.
-- IMAP UIDs are only unique *within* a folder, so the folder has to be part of the key.
CREATE TABLE remote_seen (
    account_id              INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    folder                  TEXT    NOT NULL,
    remote_id               TEXT    NOT NULL,
    first_seen_utc          INTEGER NOT NULL,
    message_id              INTEGER REFERENCES messages(id) ON DELETE SET NULL,
    deleted_from_server_utc INTEGER,
    PRIMARY KEY (account_id, folder, remote_id)
);

-- Carry over everything POP3 has already ingested, so upgrading does not re-download a mailbox.
INSERT INTO remote_seen (account_id, folder, remote_id, first_seen_utc, message_id,
                         deleted_from_server_utc)
SELECT account_id, 'INBOX', uidl, first_seen_utc, message_id, deleted_from_server_utc
FROM pop_seen;

DROP TABLE pop_seen;

CREATE INDEX idx_remote_seen_account ON remote_seen(account_id, folder);
