-- Local flag changes waiting to be pushed to an IMAP server.
--
-- Opening a TCP+TLS+LOGIN session every time someone reads a message would be ruinous — a busy
-- inbox would spend its life reconnecting. Instead the change is marked here and pushed in a batch
-- on the next sync, which also means marking mail read while offline just works.
ALTER TABLE messages ADD COLUMN flags_dirty INTEGER NOT NULL DEFAULT 0;

CREATE INDEX idx_messages_flags_dirty ON messages(account_id, flags_dirty);
