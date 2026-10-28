-- Named signatures.
--
-- Replaces the single `accounts.signature` field, which allowed exactly one and gave no way to
-- pick a different one per message. That column stays as the per-account default so nothing
-- breaks, and its contents are carried into this table below.
CREATE TABLE signatures (
    id          INTEGER PRIMARY KEY,
    name        TEXT    NOT NULL,
    body        TEXT    NOT NULL,
    -- Offered first in the compose window and inserted automatically on a new message.
    is_default  INTEGER NOT NULL DEFAULT 0,
    sort_order  INTEGER NOT NULL DEFAULT 0,
    created_utc INTEGER NOT NULL
);

CREATE INDEX idx_signatures_order ON signatures(sort_order, name);

-- Carry across whatever each account already had, so upgrading loses nobody's signature.
INSERT INTO signatures (name, body, is_default, sort_order, created_utc)
SELECT email, signature, 0, id, strftime('%s', 'now')
FROM accounts
WHERE TRIM(signature) <> '';

-- The first one becomes the default; the user can change it.
UPDATE signatures SET is_default = 1
WHERE id = (SELECT MIN(id) FROM signatures);
