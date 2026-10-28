# ADR 0001 — Scope and stack

- **Status:** accepted
- **Date:** 2026-08-21
- **Deciders:** dev@example.com

## Context

Greenfield build of an Outlook-style POP3/SMTP mail client in Rust for Windows and Android.
Before any code, eight decisions materially changed the shape of the work. They are recorded
here so later contributors do not re-litigate them from scratch.

## Decisions

### D1 — Target servers: company / self-hosted only

The client connects only to servers we control or to generic business hosts, authenticating with
a password over TLS. Consumer providers (Gmail, Outlook.com) are **not** supported.

**Consequence:** OAuth2 / XOAUTH2 is removed from v1 entirely. No OAuth client registration with
Google or Microsoft, no OAuth app verification, no annual CASA security assessment, no token
refresh machinery. This is the single largest scope reduction of the eight.

**Reversal cost:** moderate. `signet-auth` keeps a `Credential` enum so an `XOAuth2` variant can be
added without touching call sites, but the interactive browser flow, token store and refresh
scheduler would all be new work (~2-3 weeks, plus provider paperwork).

### D2 — Distribution: internal company use only

Not published to the Microsoft Store or Google Play.

**Consequence:** no Play Console account, no Data Safety declaration, no foreground-service
justification review, no target-API deadline treadmill, no EV code-signing certificate. A standard
Authenticode certificate suffices on Windows; the Android APK is self-signed with a keystore we own
and keep for the life of the product (losing it means users must uninstall/reinstall to upgrade).

**Consequence:** the polish bar is "colleagues will use this daily", not "strangers will judge it in
30 seconds". Onboarding can assume the user is told their server settings rather than discovered.

### D3 — Transport abstraction from day one

v1 speaks POP3 only, but `signet-sync` depends on a `MailTransport` trait rather than a concrete POP3
client.

**Rationale:** an afternoon now versus roughly a fortnight of refactoring later. POP3's inability to
sync read state across devices (D-consequence of the protocol, see PLAN.md §2) makes an eventual
IMAP addition likely, and the trait keeps that door open at near-zero cost.

**Consequence:** the trait must be modelled on the *superset* (folders, flags, server-side state)
with POP3 implementing a degenerate single-folder, no-flags case — not modelled on POP3 and widened
later, which would leak POP3 assumptions into every caller.

### D4 — Multi-account in v1

Account switcher, unified inbox, per-account folders and identities, per-account notifications.

**Consequence:** roughly a week of extra UI work in Phase 2, and every list/query in `signet-store`
takes an account scope from the start. Retrofitting this later is materially worse than paying now.

### D5 — Windows ships first

The Tauri UI is shared, so Android becomes largely packaging plus background sync once the Windows
client is real.

**Rationale:** faster feedback loop — no device deployment, no background-execution limits, better
debugging. The Android *build* risk is still de-risked in Phase 0 (see D8) so the fallback decision
is made early even though the Android *ship* is late.

### D6 — Dovecot (cPanel) is the reference server

Code against Dovecot semantics: reliable `UIDL`, `STLS` on 110, implicit TLS on 995, `SASL
PLAIN`/`LOGIN`. Mailpit stands in for local development and CI.

**Consequence:** we may still be tolerant of missing `UIDL` and odd `CAPA` responses (cheap
insurance), but we do not contort the design for hypothetical servers we will never meet.

### D7 — Built-in auto-updater

The app checks a signed manifest on a self-hosted endpoint and updates itself.

**Consequence (Windows):** Tauri's updater handles this well. Needs a signing keypair and a static
endpoint we host.

**Consequence (Android):** there is no silent self-update outside a store. The app can check the
manifest, download the APK and fire an install intent, but the user must grant
`REQUEST_INSTALL_PACKAGES` / allow install-from-unknown-sources once. Document that in the rollout
notes; it is a first-run speed bump, not a blocker.

### D8 — Tauri v2 for both platforms, with a Phase 0 gate

One TypeScript UI over a shared Rust core, in-process.

**Gate:** if a Tauri v2 app with bundled `rusqlite` cannot be built and run on a physical Android
device during Phase 0, the Android UI switches to Jetpack Compose over the same Rust core via
UniFFI. That decision is made in week 1, not in month four.

## Status of the alternatives considered

Rejected: native UIs on both platforms (roughly 2x UI effort), egui/Slint (no HTML-email rendering
story — disqualifying), Flutter or React Native over Rust FFI (a second runtime for no gain over
Tauri here).
