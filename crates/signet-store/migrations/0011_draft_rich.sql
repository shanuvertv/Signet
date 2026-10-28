-- A saved draft keeps its thread references, so a reply reopened from Drafts still threads.
-- Its rich body goes in the existing messages.body_html column.
ALTER TABLE messages ADD COLUMN draft_references TEXT NOT NULL DEFAULT '';
