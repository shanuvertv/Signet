-- Track which shared entries have actually been written onto a local message.
--
-- Without this, team state that arrives *before* the message does is lost: the entry merges into
-- `shared_state` on the first sync, and on every later sync the merge correctly sees nothing newer
-- and skips it — so when the message finally downloads, nobody ever applies the assignment to it.
-- That is the normal case for a shared mailbox, where one salesperson triages mail minutes before
-- a colleague has downloaded it.
--
-- NULL means never applied. Otherwise it records which `updated_utc` was applied, so a later
-- change from a teammate is applied again rather than treated as already done.
ALTER TABLE shared_state ADD COLUMN applied_utc INTEGER;

CREATE INDEX idx_shared_unapplied ON shared_state(applied_utc);
