# Working a shared mailbox as a team

`sales@example.com` is one mailbox that several people work from.

**Use IMAP if your server offers it** — Dovecot does, on port 993 — and most of this document
becomes unnecessary: read state, flags and folders are shared by the server itself. Skip to
[switch to IMAP](#the-better-answer-switch-to-imap).

The rest of this explains what happens on **POP3**, which was never designed for a shared mailbox,
and what Signet does to make it workable anyway. Assignment, priority, work status and follow-ups
travel through the shared file on **either** protocol, because IMAP has no concept of them either.

## The POP3 constraint, stated plainly

POP3 has **no shared state**. Each copy of Signet downloads its own private copy of every
message. Nothing you do to a message — read it, flag it, file it — is visible to anyone else,
because there is nowhere on a POP3 server to put that information.

So assignment, priority and follow-ups travel **out of band**, through a file every machine can
reach.

## How sharing works

Each client keeps a `shared_state` table keyed on the message's RFC822 **`Message-ID`**. That is
the only identifier that means the same thing on two machines: your local row id for a message is
1, your colleague's is 47, but the `Message-ID` is identical because it came from the sender.

On every sync each client:

1. **Reads** the shared file and merges in anything newer than what it has.
2. **Applies** merged state to its own copy of the message, if it has downloaded it yet. If it has
   not, the state waits — and is applied the moment that message arrives.
3. **Writes** the merged result back, atomically via a temporary file and a rename.

Conflicts resolve **last-write-wins per message**, decided on the change's timestamp rather than on
who happened to write the file last. Two people assigning the same message in the same second is
the one case that resolves arbitrarily; the loser sees the winner's choice appear on their next
sync, which is the right way for that to fail.

## Setting it up

1. Pick a path every machine can reach. A UNC share is ideal:
   `\\fileserver\shared\signet\team-state.json`. A synced folder (OneDrive, Dropbox, Syncthing)
   also works, though those introduce their own merge conflicts on the file itself.
2. On each machine: **Team → Shared team file**, paste the same path, press **Done**.
3. On each machine: add the sales team (or let it arrive from the shared file on the first sync),
   then mark **This is me** for whoever sits there. "Me" is deliberately a property of the machine
   and never travels in the shared file.
4. Press **Sync**. Team state exchanges on every mail sync from then on.

Or from the command line, which is easier to roll out to several PCs:

```sh
signet team-add --name "Sarah Khan" --email sarah@example.com
signet team-add --name "Max Rahman" --email max@example.com --me
signet team-sync --path "\\fileserver\shared\signet\team-state.json"
```

## What each person sees

- **Mine** — everything assigned to them.
- **Unassigned** — the queue nobody has picked up. This is the one that matters: it is what stops
  an enquiry being answered twice or not at all.
- **Follow-ups** — every open reminder, overdue ones highlighted.
- Colour-coded initials on each row show who owns it at a glance.
- A red stripe and **High** pill mark high-priority mail. Priority is read from the sender's own
  `X-Priority` / `Importance` headers on arrival, and can be changed by hand.

## Limits you should know about

On **POP3 only** — IMAP fixes the first three:

- **Read and unread do not sync.** Marking a message read is private to your machine. The work
  status (`new` → `in progress` → `waiting` → `done`) is the shared signal instead.
- **Deletions do not sync.** If someone deletes a message locally, it stays on everyone else's
  machine.
- **Everyone must leave mail on the server.** If one machine is set to delete on download, it takes
  the mail away from the rest of the team. The account dialog warns about this.

On **both protocols**:

- **A message with no `Message-ID` cannot be shared.** The reading pane says so when it happens.
  It is rare; nearly every real sender sets one.
- **The shared file is trusted.** Anyone who can write to that path can reassign your mail. Put it
  somewhere with the same access control as the mailbox itself.

## The better answer: switch to IMAP

**IMAP is now supported, and for a shared mailbox it is the right choice.** Every limitation above
exists because POP3 has no server-side state. Your server is Dovecot, which speaks IMAP on the same
host — usually port 993.

With an IMAP account:

- **Read and unread sync**, for everyone, with no shared file at all.
- **Flags sync.** Flagging a message marks it flagged on every machine.
- **Folders come from the server.** Filing something into Archive files it for the team, and any
  folder someone makes in webmail appears here.
- **Deletions and moves are shared** rather than being one person's private opinion.

Set the protocol to IMAP when adding the account. The shared team file is then only needed for the
things IMAP has no concept of — **assignment, priority, work status and follow-up reminders** —
which is a much smaller job, and those still travel through the same `Message-ID`-keyed file.

Local flag changes queue and go out on the next sync rather than opening a connection per click, so
marking mail read while offline works and catches up later.

### What is still outstanding on IMAP

- **No push yet.** IMAP `IDLE` would give near-instant delivery; the sync engine still polls, so
  new-mail latency is the poll interval. The capability is reported as `push: false` so the UI
  does not promise otherwise.
- **Assignment does not use IMAP keywords yet.** Storing it as `$assigned_sarah` on the server
  would remove the shared file entirely for IMAP accounts. That is the natural next step.
- **Moving a message locally does not move it on the server yet.** The plumbing exists
  (`ImapTransport::move_to`), but the UI's move still only files it locally.
