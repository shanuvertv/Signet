-- Contact details read out of signatures.
--
-- `contacts` has existed since the first migration, harvested from every sender so that compose
-- could autocomplete. It held a name and an address; everything a signature actually offers — the
-- direct line, the mobile, the job title, the company — arrived in every message and was thrown
-- away.
ALTER TABLE contacts ADD COLUMN job_title TEXT NOT NULL DEFAULT '';
ALTER TABLE contacts ADD COLUMN company   TEXT NOT NULL DEFAULT '';
ALTER TABLE contacts ADD COLUMN phone     TEXT NOT NULL DEFAULT '';
ALTER TABLE contacts ADD COLUMN mobile    TEXT NOT NULL DEFAULT '';
ALTER TABLE contacts ADD COLUMN website   TEXT NOT NULL DEFAULT '';
ALTER TABLE contacts ADD COLUMN notes     TEXT NOT NULL DEFAULT '';

-- When a person last edited this contact by hand. Non-zero means the row is theirs: the signature
-- reader may fill blanks but must never correct what somebody typed. A parser working from
-- heuristics over other people's formatting will be wrong sometimes, and being wrong is tolerable
-- only while it cannot overwrite a human.
ALTER TABLE contacts ADD COLUMN edited_utc INTEGER NOT NULL DEFAULT 0;

CREATE INDEX idx_contacts_company ON contacts(company) WHERE company <> '';
