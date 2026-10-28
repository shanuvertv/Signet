-- Explicit sender allow/block lists, and room to record why a message was filed.
--
-- A statistical filter is never going to be right about everyone. These are the escape hatches:
-- one customer whose mail keeps scoring badly, one spammer the classifier keeps forgiving.
CREATE TABLE sender_rules (
    -- Either a full address (buyer@customer.test) or a bare domain (customer.test).
    pattern     TEXT PRIMARY KEY,
    kind        TEXT NOT NULL,        -- allow | block
    created_utc INTEGER NOT NULL
);

CREATE INDEX idx_sender_rules_kind ON sender_rules(kind);

-- Human-readable explanation of the junk verdict, so "why is this in Junk?" has an answer.
ALTER TABLE messages ADD COLUMN spam_reasons TEXT NOT NULL DEFAULT '';
