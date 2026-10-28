# Signet — Outlook-style POP3/SMTP client in Rust (Windows + Android)

**Status:** Windows client working — IMAP + POP3, team triage; Phases 1-4 substantially built · **Owner:** dev@example.com · **Revised:** 2026-08-21

Decisions are recorded in [docs/adr/0001-scope-and-stack.md](docs/adr/0001-scope-and-stack.md).

---

## 0. Scope, locked

| Decision | Choice | What it removed |
|---|---|---|
| Target servers | company / self-hosted only, password auth over TLS | **All OAuth2/XOAUTH2 work** — no provider registration, no Google verification, no CASA assessment |
| Distribution | Internal company use only | Play Console, Data Safety form, foreground-service review, EV certificate |
| IMAP | **Now implemented.** The D3 transport trait made it an implementation, not a rewrite | — |
| Accounts | Multi-account in v1 | — (~1 week of UI, cheaper now than retrofitted) |
| Platform order | Windows first, Android second | — |
| Reference server | Dovecot / cPanel (Mailpit locally) | Exchange and provider-specific quirk handling |
| Updates | Built-in signed auto-updater | Store-managed updates |
| UI | Tauri v2, one codebase, with a Phase 0 fallback gate | — |

Net effect: **both-platform internal beta in roughly 4.5 months, MVP in ~2.5** — down from
5 and 3 respectively before these decisions.

---

## 1. What we are building

A desktop-and-mobile email client with an Outlook-like experience: three-pane reading layout,
folders, search, rules, contacts, rich compose, attachments, offline access — on a shared Rust core,
shipping as a Windows desktop app and an Android app for internal company use.

Transport scope: **IMAP or POP3 (retrieve) + SMTP (send)**. IMAP is the default for new
accounts and the right answer for a shared mailbox.

**Non-goals:** calendar, tasks, Exchange/EWS/MAPI, server-side rules, S/MIME, PGP, shared mailboxes,
iOS, consumer mail providers, public store distribution.

---

## 2. The one constraint that drives the whole design

POP3 is a *download-and-forget* protocol. It has:

- no folders — only a single inbox-like maildrop
- no flags — the server does not know what you've read, replied to, or flagged
- no server-side search
- no sent-items copy (SMTP sends; nothing stores)
- no push — the only way to learn about new mail is to poll
- no cross-device state — two devices polling the same maildrop each get their own copy

Therefore: **the local database is the source of truth, and POP3 is nothing but an ingestion pipe.**
Every Outlook-like feature — folders, read/unread, flags, threads, categories, rules, search,
Sent, Drafts, Deleted Items — is a purely client-side construct we build and own.

Two consequences to accept up front and say plainly in the UI:

1. **Multi-device is lossy.** With "leave mail on server", both Windows and Android download the same
   messages, but read state, folder placement and deletions do **not** sync between them. Only IMAP
   or an account-owned sync channel fixes this — never POP3.
2. **New-mail latency equals the poll interval.** There is no push. On Android, OS background limits
   floor the practical interval at ~15 minutes unless the user opts into a foreground service.

Because we control the server (D1), an eventual IMAP switch is genuinely available to us — which is
exactly why the transport trait (D3) goes in now.

---

## 3. Architecture

Single Cargo workspace. All logic in Rust; the UI is a thin shell over a command/event façade.

```
Signet/
├─ crates/
│  ├─ signet-net        IMAP + POP3 + SMTP clients, TLS, connection handling, timeouts, retry
│  ├─ signet-mime       MIME parse (in) / build (out), charset, encodings, sanitization
│  ├─ signet-store      SQLite schema, migrations, FTS5 index, blob store for attachments
│  ├─ signet-sync       MailTransport trait, IMAP/POP3 ingestion, outbox, rules, junk filter
│  ├─ signet-auth       credential vault over the platform keystore
│  ├─ signet-core       the façade: typed commands + an event stream the UI subscribes to
│  ├─ signet-cli        headless driver — the Phase 1 deliverable and a permanent debugging tool
│  └─ signet-fuzz       fuzz targets for the MIME and protocol parsers
├─ apps/
│  └─ client/        Tauri v2 app (Windows + Android from one source)
├─ docs/adr/         architecture decision records
└─ .github/workflows/
```

**Boundary rule:** the UI never speaks a protocol and never touches SQL. It issues commands
(`sync_account`, `move_to_folder`, `send_message`) and renders projections pushed back over the
event stream. That boundary is what makes `signet-cli`, the test harness, and a future second UI cheap.

**The transport trait (D3).** `signet-sync` depends on `MailTransport`, modelled on the *superset* —
folders, flags, server-side state — with POP3 implementing a degenerate single-folder, no-flags case.
Modelling it on POP3 and widening later would leak POP3 assumptions into every caller, which is the
exact refactor the trait exists to avoid.

**Concurrency:** a `tokio` multi-thread runtime in the core. One sync actor per account owning that
account's connection and cursor state; the UI thread never blocks on I/O.

---

## 4. Tech stack

**Tauri v2** for the UI: Windows and Android from one codebase, `cargo-tauri` already installed here,
Rust core in-process with no IPC serialization tax on the heavy paths. The real cost is that email
HTML renders inside a WebView — WebView2 on Windows, Android System WebView on mobile — two engines
with different quirks. Survivable (every webmail client lives with it), but budget rendering QA.

Fallback if the Phase 0 Android gate fails: Jetpack Compose over the same Rust core via UniFFI.

### Core libraries (verify current versions at implementation time)

| Concern | Choice | Note |
|---|---|---|
| Async runtime | `tokio` | |
| TLS | `rustls` + `rustls-platform-verifier` | **Must** use the platform verifier so the Windows cert store and Android trust anchors (including our internal CA, if any) are honoured |
| SMTP | `lettre` | Mature; STARTTLS + implicit TLS, SASL PLAIN/LOGIN |
| POP3 | **write our own** on `tokio` + `tokio-rustls` | ~10 verbs, ~600 lines, fully testable; the available crates are thin and lightly maintained |
| IMAP | `async-imap` | The opposite call to POP3: literals, untagged responses and a parser that is easy to get subtly wrong. Maintained by the Delta Chat team and exercised against many real servers |
| MIME parse | `mail-parser` | Handles real-world broken MIME, nested multipart, charsets, RFC 2047 |
| MIME build | `mail-builder` | Same author, pairs cleanly |
| Database | `rusqlite`, feature `bundled` | Bundled SQLite avoids all Android dynamic-linking pain. FTS5 for search |
| HTML sanitizing | `ammonia` | Plus a strict CSP and a sandboxed iframe — belt and braces |
| Logging | `tracing` + `tracing-subscriber` | Rolling file appender; a credential-scrubbing layer is mandatory |
| Errors | `thiserror` in libraries, `anyhow` at app edges | |

---

## 5. Data model (sketch)

```sql
accounts(id, display_name, email,
         pop_host, pop_port, pop_security, pop_user,
         smtp_host, smtp_port, smtp_security, smtp_user,
         leave_on_server, delete_after_days, poll_minutes, signature,
         is_enabled, sort_order, created_at)

identities(id, account_id, display_name, addr, signature, is_default)   -- multi-account reply-from

folders(id, account_id, kind, name, parent_id, sort_order)
         -- kind: inbox | sent | drafts | deleted | junk | archive | custom   (all client-side)

messages(id, account_id, folder_id, rfc822_message_id, in_reply_to, thread_id,
         from_name, from_addr, subject, date_utc, received_utc,
         is_read, is_flagged, is_answered, has_attachments, size_bytes,
         preview_text, raw_path, body_html_path, body_text_path)

recipients(message_id, kind, name, addr)            -- to | cc | bcc
attachments(id, message_id, filename, mime_type, size_bytes, content_id, blob_path, is_inline)

pop_seen(account_id, uidl, first_seen_utc, message_id, deleted_from_server_utc)
         -- PRIMARY KEY(account_id, uidl); this table is what makes re-polling idempotent

outbox(id, account_id, identity_id, state, raw_path, attempts, next_attempt_utc, last_error, created_at)
         -- state: queued | sending | sent | failed

rules(id, account_id, priority, enabled, match_json, action_json)
contacts(id, display_name, addr, last_seen_utc, times_seen)     -- auto-harvested for autocomplete

messages_fts   -- FTS5 virtual table over (subject, from, to, body_text), external-content
```

Notes:

- Every query is account-scoped from day one (D4). The unified inbox is a view across accounts, not
  a special case bolted on later.
- Raw RFC822 goes to a file blob, not a BLOB column — keeps the DB small and lets us re-parse after a
  parser upgrade without re-downloading anything.
- `thread_id` is computed at ingest from `Message-ID` / `In-Reply-To` / `References`, falling back to
  normalized-subject + participant heuristics (the JWZ algorithm) when headers are missing or lying.
- Schema migrations from day one (`user_version` plus ordered migration files). You will change this.

---

## 6. Protocol layer

### POP3 client (`signet-net::pop3`)

A state machine over a line-oriented codec: `AUTHORIZATION → TRANSACTION → UPDATE`.
Reference implementation is **Dovecot** (D6).

- Verbs: `CAPA, STLS, USER/PASS, AUTH (PLAIN, LOGIN), APOP, STAT, LIST, UIDL, TOP, RETR, DELE, RSET, NOOP, QUIT`
- Security modes: implicit TLS (995), STARTTLS via `STLS` (110), plaintext (lab only — explicit
  opt-in behind a warning).
- **UIDL is mandatory** for dedupe. Dovecot always provides it; we still fall back to hashing
  `Message-ID + Date + size` and warn, because that costs ten lines and saves a support incident.
- Ingestion loop: `UIDL` → diff against `pop_seen` → `TOP n 0` for headers of each new item → apply
  size and rule filters → `RETR` the ones we want → parse → insert → mark seen → `DELE` only if the
  retention policy says so → `QUIT`. **Deletions commit only on a clean QUIT** — never drop the
  socket after a DELE.
- Stream `RETR` straight to disk through a byte-unstuffing decoder (`..` → `.`); never buffer a
  200 MB message in memory. This matters most on Android.
- Hard timeouts on connect/read/write, bounded exponential backoff, and a circuit breaker so a dead
  server doesn't burn the phone's battery.

### SMTP (`signet-net::smtp`, over `lettre`)

- Submission on 587 STARTTLS or 465 implicit TLS; handle `EHLO`, `SIZE`, `8BITMIME`, `SMTPUTF8`.
- Everything goes through the **outbox**: compose writes a queued row, a worker drains it. That gives
  offline send, retry, and a visible "Sending…" state for free.
- On success, write the copy into the local Sent folder ourselves — no server does this for us.
- Distinguish 4xx (retry with backoff) from 5xx (park as failed, surface the server's text verbatim —
  users need the real bounce reason, not "send failed").

---

## 7. Security model

Email is attacker-controlled input rendered on the user's screen. Internal distribution lowers the
compliance bar, not the threat model — a phishing mail reaching a company inbox is exactly the case
this has to survive.

1. **Credentials never touch the SQLite file.** Windows → Credential Manager (DPAPI-backed);
   Android → Keystore-wrapped key plus EncryptedSharedPreferences. Store only an opaque handle in the DB.
2. **TLS is non-negotiable.** No "ignore certificate errors" toggle, ever. Use the platform verifier.
   Show the negotiated chain in account settings so a self-signed internal server can be pinned
   deliberately — an explicit per-account fingerprint the user confirms, not a blanket bypass.
3. **HTML renders in a sandboxed iframe** (`sandbox="allow-popups"`; no `allow-scripts`, no
   `allow-same-origin`) under a strict CSP forbidding script, frame, form and — initially — all remote
   resources. Sanitize with `ammonia` *before* it reaches the iframe.
4. **Remote images blocked by default**, with a per-message / per-sender "Show images" affordance.
   The single most effective anti-tracking-pixel measure, and what Outlook does too.
5. **Attachments** are never auto-opened and never trusted by extension. Save to a quarantine
   directory, flag executables, scripts and macro-enabled Office types loudly, and apply
   Mark-of-the-Web on Windows so SmartScreen sees them.
6. **Spoofing surface:** POP3 gives us no SPF/DKIM/DMARC verdict of our own. If the receiving server
   left an `Authentication-Results` header we display it — but we never manufacture trust we can't
   verify. Do show the envelope sender when it disagrees with the display name.
7. **Parser hardening:** fuzz targets for the POP3 line codec and the MIME parser, run in CI.
   `#![forbid(unsafe_code)]` in our crates.
8. **Logs and crash reports** pass through a scrubbing layer — no passwords, no tokens, no bodies.
9. **At-rest encryption on Android** (SQLCipher or a per-file key) — assume a lost phone. Windows can
   lean on BitLocker/DPAPI.
10. **The update channel is a supply chain.** The auto-updater (D7) must verify a signature over the
    manifest *and* the payload with a pinned public key. An unauthenticated updater is a remote code
    execution primitive pointed at every company desktop.

---

## 8. Platform specifics

### Windows (ships first)

- WebView2 runtime is a dependency — bundle the evergreen bootstrapper in the installer.
- MSI + NSIS via the Tauri bundler, Authenticode-signed with a standard certificate (no EV needed for
  internal distribution, though unsigned builds will still trip SmartScreen for the first users).
- Tauri updater against a self-hosted signed manifest; keep the update keypair offline and backed up.
- Native toasts for new mail, tray icon with unread badge, minimize-to-tray, run-at-login.
- Per-monitor DPI awareness; dark mode following the system theme.
- Optional: register as a `mailto:` handler; jump list entry for Compose.

### Android (ships second)

- Build with `cargo-ndk` (installed) targeting `aarch64-linux-android`, plus `x86_64` for emulators.
  NDK 28.2 present; JDK at `C:\Program Files\Android\Android Studio\jbr` — `JAVA_HOME` must point there.
- **Background polling is the hard part:**
  - `WorkManager` periodic work — 15-minute floor, respects Doze and app-standby buckets.
  - Opt-in **foreground service** ("keep me connected") for ~1-minute polling with the required
    persistent notification. No Play review to satisfy (D2), so this is purely a battery trade-off
    we can present honestly to internal users.
  - Handle `BOOT_COMPLETED` to reschedule; back off hard on repeated network failure.
- Runtime permission `POST_NOTIFICATIONS` (Android 13+); a notification channel per account.
- Storage in app-private directories; expose attachments through a `FileProvider`; save via SAF
  (`ACTION_CREATE_DOCUMENT`) so we need no broad storage permission.
- **Updates:** no silent self-update outside a store. The app checks the manifest, downloads the APK
  and fires an install intent; the user grants install-from-unknown-sources once. Sign with a
  keystore we own and **never lose** — a lost signing key forces every user to uninstall/reinstall.
- Adaptive layout: list/detail on phones, two-pane on tablets and foldables.

---

## 9. Roadmap

One full-time engineer. Two roughly halve the calendar — except Phase 0.

### Phase 0 — De-risking spikes · 1 week ← *done, except the Android gate*

Prove the three things that could invalidate the plan, in throwaway code:

1. **POP3 + STARTTLS + implicit TLS** against Mailpit in Docker, then against the real cPanel/Dovecot host.
2. **The Android gate:** a Tauri v2 app with bundled `rusqlite` that builds, installs and runs on a
   physical device. *If this fights back, switch the Android UI to Compose + UniFFI now, not in month four.*
3. **Credential vault:** read and write a secret in Windows Credential Manager and the Android Keystore, from Rust.

### Phase 1 — Core: fetch and store · 3 weeks ← *done*

`MailTransport` trait and the POP3 implementation, `signet-mime` ingestion, `signet-store` schema and
migrations, `signet-sync` poll loop with UIDL dedupe and retention policy, multi-account from the start.
**Deliverable:** `signet-cli` adds accounts, polls them, and prints the inbox. No UI — this is the layer
that has to be right.

### Phase 2 — Read experience · 4 weeks ← *core done; threading and drag-to-move outstanding*

Three-pane shell, virtualized message list, reading pane with sanitized HTML and image blocking,
folders, drag-to-move, read/unread/flag, threading, FTS5 search with a real query syntax
(`from:`, `subject:`, `has:attachment`, date ranges), attachment view and save, account switcher and
unified inbox.

### Phase 3 — Write experience · 3 weeks ← *done except rich text: attachments, drafts, signatures and reply all working*

Compose window, rich-text editor producing HTML + plaintext multipart, reply / reply-all / forward
with correct quoting and `References` chains, per-account identity selection, contact autocomplete,
attachments, drafts with autosave, SMTP send through the outbox, Sent folder, signatures.

### Phase 4 — Outlook-ness · 3 weeks ← *rules, junk filter and settings done; categories, conversation view and import/export outstanding*

Client-side rules engine (on ingest), Junk folder with a heuristic filter, categories and colours,
Outlook-compatible keyboard shortcuts, conversation view, import/export (`.eml`, `.mbox`), settings
UI, onboarding pre-seeded with company server defaults.

### Phase 5 — Windows ship · 1.5 weeks

Installer, Authenticode signing, tray and toasts, autostart, **signed auto-updater end to end**,
crash reporting, first-run experience.
**Deliverable:** signed Windows build on the internal update channel.

### Phase 6 — Android ship · 2.5 weeks

Adaptive UI, WorkManager sync, notifications, foreground-service option, share-to-app intent,
FileProvider and SAF, APK signing, in-app update flow.
**Deliverable:** installable APK on the internal update channel.

### Phase 7 — Hardening · 1.5 weeks

Fuzzing in CI, a real security review, large-mailbox performance (target: 50k messages, 60fps list
scroll, search under 200 ms), accessibility pass, opt-in content-free telemetry.

**MVP (phases 0–3 plus minimal packaging): ~11 weeks. Both-platform internal beta: ~18 weeks.**
Be suspicious of any estimate much shorter — mail clients are mostly edge cases.

---

## 10. Testing

- **Unit:** protocol codecs against captured transcripts; MIME parsing against a corpus of nasty
  real-world messages — malformed headers, mixed charsets, 8-bit bodies, deeply nested multipart,
  enormous base64 parts.
- **Protocol doubles:** an in-process fake POP3/SMTP server, so tests cover the error paths a real
  server won't reproduce on demand: mid-transfer disconnect, `-ERR` at every verb, TLS handshake
  failure, UIDL absent, duplicate UIDLs, non-monotonic message numbers.
- **Integration:** **Mailpit** in Docker (SMTP + POP3 in one container) for local runs and CI. Run
  against the real Dovecot host before every release.
- **Property tests:** parse → serialize → parse round-trips over generated MIME.
- **Fuzzing:** `cargo-fuzz` on the line codec and the MIME parser, corpus committed, short run in CI.
- **UI:** component tests for the TS layer; a Windows smoke E2E, plus an instrumented Android run.

---

## 11. Remaining risks

R1 (provider auth) and the store-compliance risks are **closed** by D1 and D2. What's left:

| # | Risk | Impact | Mitigation |
|---|---|---|---|
| R2 | Android background limits put new-mail latency at 15+ minutes | Feels sluggish next to Gmail | Honest UI copy; opt-in foreground service; because we own the server (D1), an IMAP-IDLE-to-FCM relay is genuinely on the table later |
| R3 | POP3 offers no multi-device sync | Confusing across Windows and phone | "Leave on server" by default; explicit UI copy; the D3 trait makes IMAP the real fix when it hurts enough |
| R4 | HTML rendering fidelity across two WebView engines | Visual bugs, support load | Aggressive sanitization removes most divergence; screenshot-diff corpus of common newsletter templates |
| R5 | Tauri Android maturity for an app this size | Could force a UI rewrite | Phase 0 gate #2 exists precisely for this |
| R6 | Auto-updater is a code-execution channel into every company desktop | Severe if compromised | Signature verification over manifest *and* payload, pinned key, offline keypair, backed up |
| R7 | Android signing key loss | Every user must uninstall/reinstall | Keystore backed up to company secret storage on day one of Phase 6 |
| R8 | Mailbox scale (50k+ messages, GB of attachments) on a phone | OOM, sluggish list | Stream everything to disk; header-first ingestion via `TOP`; bodies on demand; size cap with an explicit "download full message" |

**Still open:** the real cPanel/Dovecot hostname, ports and a test mailbox; app id
(`app.signet.mail`?); where the update manifest is hosted; expected user count and typical
mailbox size.

---

## 12. Phase 0 checklist

- [x] Workspace, crate skeleton, CI (fmt, `clippy -D warnings`, test) — 28 tests green, clippy clean
- [x] Mailpit running locally with POP3 on 1110, seeded via `tools/seed-mailpit.mjs`
- [x] **POP3 client written and working** — `CAPA`/`STAT`/`LIST`/`UIDL`/`TOP`/`RETR`/`DELE`/`QUIT`,
      STARTTLS with the injection check, streaming `RETR` with dot-unstuffing
- [x] Verified against Mailpit: dot-stuffed lines round-trip, UIDLs stable across sessions,
      `TOP` returns headers without the body, a 528 KiB message streams without truncation,
      8-bit and encoded-word headers survive
- [x] **TLS verification proven on** — implicit TLS against a self-signed cert is rejected by the
      Windows platform verifier (`UnknownIssuer`). Procedure in `docs/testing.md`
- [x] Credential vault working on Windows (Credential Manager round-trip, including delete)
- [x] Rust core cross-compiles for `aarch64-linux-android`: bundled SQLite with FTS5, `ring`,
      `rustls-platform-verifier`, and `keyring`'s Android Keystore store
- [ ] **The Android gate: Tauri v2 + `rusqlite` running on a physical device.** The native
      dependencies are proven to build; what remains is the Tauri shell on real hardware.
      *Blocked on a device.*
- [ ] POP3 spike against the real cPanel/Dovecot host. *Blocked: need hostname, ports, test mailbox.*
- [ ] `JAVA_HOME` set persistently to `C:\Program Files\Android\Android Studio\jbr`
      (set per-shell so far, not in the environment)
- [ ] Android Keystore verified **at runtime** — it compiles, but the store needs a JNI context
      that only exists once the Tauri shell is running. Folds into the gate above.
