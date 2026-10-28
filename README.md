# Signet

An Outlook-style POP3/SMTP mail client in Rust, for Windows and Android, for internal company use.

- **[PLAN.md](PLAN.md)** — scope, architecture, roadmap, risks
- **[docs/adr/0001-scope-and-stack.md](docs/adr/0001-scope-and-stack.md)** — the eight decisions the design rests on
- **[docs/testing.md](docs/testing.md)** — running the tests, including the live POP3 suite

## Status

**The Windows desktop app works**, and installers are built. 118 tests pass, including suites that
run against a real POP3/SMTP server.

Working: multi-account POP3 sync · three-pane reading with sanitized HTML and blocked remote
images · full-text search · flag, move, delete · signatures · file attachments in and out · drafts ·
client-side rules · a learning junk filter · reply/reply-all · SMTP send with a Sent copy ·
per-account settings · **light/dark/system themes** · **priority colour-coding** ·
**per-salesperson assignment** · **follow-up reminders** · **shared team state across machines**.

For a sales team sharing one mailbox, read **[docs/team-workflow.md](docs/team-workflow.md)** —
it explains what POP3 can and cannot share, and how to set the shared file up.

Outstanding: the Android on-device gate (native deps cross-compile; the Tauri shell is untested on
hardware), a run against the real cPanel/Dovecot host, and rich-text compose.

## Installers

```
target/release/bundle/nsis/Signet_0.1.0_x64-setup.exe    (~4.5 MB)
target/release/bundle/msi/Signet_0.1.0_x64_en-US.msi     (~6.3 MB)
```

Built with `cargo tauri build` from `apps/client/src-tauri`. Neither is code-signed yet, so
SmartScreen will warn on first run until an Authenticode certificate is in place.

## Choosing a protocol

**IMAP is the default and the right answer for a shared mailbox.** Folders, read state and flags
live on the server, so everyone working `sales@` sees the same thing with no extra machinery.

**POP3 is download-and-forget**: no folders, no flags, no server-side search, no push, no
cross-device state. Every Outlook-like feature is then a client-side construct we build and own,
and read state does not sync between machines. It is still supported, and for a single-user
mailbox it is fine — but a sales team on POP3 needs the shared-file workaround described in
[docs/team-workflow.md](docs/team-workflow.md).

The two sit behind one `MailTransport` trait (ADR 0001 / D3), which was modelled on IMAP's
superset from the start precisely so this could be added without reshaping the sync engine or the
UI.

## Layout

```
crates/
  signet-net      IMAP + POP3 + SMTP clients, TLS, framing
  signet-store    SQLite, migrations, FTS5 search
  signet-mime     MIME parsing and HTML sanitization
  signet-sync     MailTransport trait, IMAP/POP3 ingestion, outbox, rules, junk filter
  signet-auth     platform keystore credential vault
  signet-core     command/event facade for the UI
  signet-cli      headless driver (`signet`)
apps/client    Tauri v2 app - Windows working, Android pending the gate
```

The UI never speaks a protocol and never touches SQL. It issues commands and renders projections
from an event stream — which is what makes `signet-cli` and the test harness cheap.

## Quick start

Run the desktop app:

```sh
cargo run -p signet-app          # or: cargo tauri dev --config apps/client/src-tauri
```

Everything the app does is also available headlessly, against the same mailbox:

```sh
export SIGNET_POP3_PASSWORD=...
signet probe mail.example.com --user someone@example.com --security starttls   # inspect, change nothing
signet add-account --email you@example.com --pop-host mail.example.com --smtp-host mail.example.com
signet sync
signet list --search "budget"
signet show 42
signet send --account 1 --to someone@example.com --subject Hello --body "..."
```

`signet probe` connects, reports what the server supports and lists what is waiting. It downloads
nothing and deletes nothing.

## Things that are load-bearing

- **TLS verification cannot be disabled.** `signet-net` uses the platform verifier, so the Windows
  certificate store and Android trust anchors apply, including a private company CA. There is no
  ignore-certificate-errors switch, deliberately. `docs/testing.md` has a procedure for proving
  verification is still on.
- **`DELE` is never committed without a clean `QUIT`.** `MailTransport::commit` consumes the
  transport, so dropping it on an error path is what *prevents* deletion rather than risking it.
- **Passwords never reach the database, the logs, or a `Debug` output.** They live in the platform
  keystore; the database stores an opaque handle. `Credential` and `Pop3Credentials` have
  hand-written `Debug` impls, and there are tests asserting secrets do not leak through them.
- **Bcc never reaches the headers.** It exists only in the SMTP envelope; emitting it as a header
  would disclose blind recipients to everyone else on the message. Reply-all drops Bcc recipients
  and your own address for the same reason. Both are covered by tests, and verified against a real
  SMTP server.
- **Outlook `.pst` files import.** The MS-PST reading is Microsoft's own `outlook-pst` crate;
  everything above it is ours. A PST holds MAPI properties rather than the message that arrived, so
  an imported message is *reconstructed* into real MIME and then fed through the same ingestion
  pipeline as downloaded mail — which is what keeps the search index, contact list, sanitisation
  and inline images consistent between the two. Where Outlook kept `PR_TRANSPORT_MESSAGE_HEADERS`
  the original `Message-ID` is recovered from it, so an imported message is the same message to the
  rest of the app as the copy a colleague downloaded over IMAP.
- **`.ost` files are refused, and say why.** They use a different NDB version, and an OST is a cache
  of a mailbox that is still on the server — so the answer is to add the account, not import a
  file. A file Outlook still has open says *that* rather than complaining about the format, because
  it is the first thing anyone hits.
- **Scheduled send builds the message now and holds it.** Not at the moment of sending: a signature
  or account setting changed in between must not silently rewrite a message that has already been
  written and approved. The draft is stored beside the built bytes so a scheduled message survives
  closing the app, and the outbox is drained on a timer, which also gives failed sends the retry
  with backoff they never had.
- **Both halves of an account are checked, separately.** Incoming and outgoing are configured
  independently and fail independently, so `Test connection` reports them independently and neither
  failure hides the other. Sending was previously never tested at all, which meant an account could
  pass its test and still be unable to send a reply.
- **Attachments open in the window.** Images and PDFs get a viewer rather than a save-and-open round
  trip through Explorer. SVG is deliberately excluded: it is a document that can carry script, and
  the viewer is no place to render untrusted markup from a stranger.
- **The data file exports as an ordinary zip.** `Tools → Export data file` writes a `.signet`
  holding a consistent snapshot of the database plus every raw message and attachment, laid out
  exactly like the data directory. It is a plain zip on purpose: a backup you need this program to
  read is a worse backup, and restoring by hand is a matter of unzipping it into place.
- **The snapshot uses `VACUUM INTO`, not a file copy.** The live database runs in WAL mode, so the
  bytes in `mail.sqlite` are only half the story and copying it alone would produce an archive
  missing whatever is still in the log. `VACUUM INTO` runs inside SQLite's own locking, so an
  export is safe while a sync is running.
- **Import merges and never destroys.** It adds accounts it does not have and messages it has not
  seen, matching accounts on their email address and messages on `Message-ID`. Importing the same
  archive twice adds nothing the second time. Replacing would have been far less code, but then
  importing the wrong file would be unrecoverable — and the reasons to import (a new laptop, a
  restored backup, a colleague's archive) are all served by merging.
- **Imported mail goes in through the same writer as downloaded mail.** The search index and the
  contact list are maintained in code rather than by triggers, so an import writing rows directly
  would produce mail that exists but cannot be found. There is a test that imports an archive and
  then searches for what was in it.
- **Passwords are never in the archive.** They live in Windows Credential Manager, and writing them
  into a file that gets copied onto a memory stick would undo the reason for keeping them there.
  Imported accounts arrive disabled and say which ones need a password.
- **An archive is treated as untrusted input.** Entry paths go through `enclosed_name`, which
  rejects absolute paths and anything climbing out with `..`, so a crafted zip cannot write outside
  the extraction directory. Blob names collide across machines — they come from server-chosen ids,
  unique only within one server -- so an imported file that would land on an existing one is given
  a new name rather than overwriting a message that is not being imported at all.
- **The junk filter never deletes.** It moves mail to Junk and nothing else, the threshold defaults
  high, and the classifier is biased toward "not junk" when uncertain — a spam in the Inbox costs a
  click, a real order silently filed costs more. Before it has seen 20 examples of each, built-in
  heuristics do the work; after that, what you mark takes over.
- **Words are only half the filter.** Alongside the statistics it reads structure: SPF/DKIM/DMARC
  results, whether a link goes where its text claims, executable attachments, `Reply-To` pointing
  at another domain, a missing `Message-ID`. Each carries a weight and a sentence, so the Junk
  folder can always answer "why is this here?" in plain words rather than a number.
- **Two hard signals together are not a coincidence.** Forwarding breaks DMARC and marketing tools
  rewrite links, so any one of those stays a nudge. Two at once — a failed DMARC *and* a lying
  link, say — escalate, because otherwise a textbook phish sits just under the threshold on a
  fresh install where the word statistics are still empty. Broken DKIM or SPF alone is deliberately
  not counted: customers forward mail from their phones every day.
- **Knowing the sender still outranks all of it.** Someone you have written to is pulled well clear
  of the threshold. When such a message *does* show the shape of a forgery it is delivered anyway,
  but carries a warning naming what is wrong — that combination is what a taken-over supplier
  account looks like, and it is the case where filing and silence are both the wrong answer.
- **Block and allow lists are decisions, not guesses.** They beat every signal, they are excluded
  from training because a rule teaches the classifier nothing, and a block still files mail when
  the statistical filter is switched off — otherwise the button would quietly do nothing. Both
  lists are reviewable and reversible under **Tools → Junk options**.
- **Rules never destroy mail either.** A rule's "Delete" moves to Deleted Items, and a rule with no
  conditions matches nothing rather than everything.
- **Team state is keyed on `Message-ID`, not our row ids.** Two people who each downloaded the
  same message over POP3 hold different local ids for it but the same `Message-ID`, so that is the
  only join key that means anything across machines. State that arrives before the message does is
  held and applied when the message downloads.
- **The transport trait is modelled on the superset** — folders, flags, server-side state — with
  POP3 as the degenerate case. That direction is the point: it keeps POP3's limitations from
  leaking into every caller, and leaves room for IMAP.
