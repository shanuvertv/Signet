# Testing

## Layers

| Layer | What it covers | Needs |
|---|---|---|
| Unit tests | Line codec, dot-unstuffing, `LIST`/`UIDL` parsing, capability matching, redaction | nothing |
| Live POP3 tests | The same paths against a server we did not write | Mailpit |
| Keystore test | Windows Credential Manager round-trip | `--ignored`, writes to the real store |
| Android cross-build | Bundled SQLite, `ring`, platform verifier compile for `aarch64-linux-android` | NDK |

`cargo test --workspace` runs everything that needs no external state. The live tests print
`SKIPPED` and pass when `SIGNET_TEST_POP3_HOST` is unset — CI sets it, so they always run there.

## Running the live POP3 tests

Mailpit is the CI server; Dovecot (cPanel) is the reference (ADR 0001 / D6) and should be run
against before every release.

Get Mailpit — either `docker run -p 1025:1025 -p 1110:1110 axllent/mailpit`, or the standalone
binary from <https://github.com/axllent/mailpit/releases>. Then:

```sh
printf 'signet:spike\n' > pop3-auth.txt
mailpit --pop3-auth-file pop3-auth.txt \
        --smtp-auth-accept-any --smtp-auth-allow-insecure \
        --listen 127.0.0.1:8025 --smtp 127.0.0.1:1025 --pop3 127.0.0.1:1110
```

Seed it with the fixture messages, which deliberately include dot-stuffed lines, 8-bit and
encoded-word headers, and a ~528 KiB multipart:

```sh
node tools/seed-mailpit.mjs
```

Then:

```sh
SIGNET_TEST_POP3_HOST=127.0.0.1 cargo test --workspace
```

Overridable: `SIGNET_TEST_POP3_PORT` (1110), `SIGNET_TEST_POP3_USER` (signet), `SIGNET_TEST_POP3_PASSWORD`
(spike).

## Probing a real server by hand

`signet probe` connects, reports capabilities and lists what is waiting, without downloading or
deleting anything:

```sh
export SIGNET_POP3_PASSWORD=...          # never passed as an argument: shell history, process list
cargo run -p signet-cli -- probe mail.example.com --user someone@example.com --security starttls
```

`--security plain` is refused for anything but an explicitly plaintext config, because
`Pop3Client::login` will not send a password over an unencrypted link.

## Verifying that TLS verification is on

This is worth re-running whenever the TLS stack changes, because a client that silently accepts
any certificate looks identical to a working one until it matters.

```sh
openssl req -x509 -newkey rsa:2048 -keyout key.pem -out cert.pem -days 2 -nodes \
        -subj "/CN=127.0.0.1" -addext "subjectAltName=IP:127.0.0.1"
mailpit --pop3-auth-file pop3-auth.txt --pop3-tls-cert cert.pem --pop3-tls-key key.pem \
        --listen 127.0.0.1:8026 --smtp 127.0.0.1:1026 --pop3 127.0.0.1:1995

SIGNET_POP3_PASSWORD=spike cargo run -p signet-cli -- \
    probe 127.0.0.1 --user signet --security tls --port 1995
```

**Expected: failure**, with `invalid peer certificate: UnknownIssuer`. A success means the
platform verifier has been bypassed. (On Git Bash, prefix the `openssl` line with
`MSYS_NO_PATHCONV=1` or `-subj` gets rewritten into a Windows path.)

## Android cross-build

```sh
export JAVA_HOME="/c/Program Files/Android/Android Studio/jbr"
export ANDROID_NDK_HOME="$HOME/AppData/Local/Android/Sdk/ndk/28.2.13676358"
cargo ndk -t arm64-v8a build -p signet-store -p signet-net -p signet-sync -p signet-auth
```

This proves the native dependencies build for Android. It does **not** prove the app runs there —
that is the Phase 0 gate, and it needs a physical device.

## End-to-end

`crates/signet-core/tests/end_to_end.rs` drives the whole stack through the same facade the UI uses:
add an account, sync from a real POP3 server, read folders and messages back, search, mark read,
delete, then re-sync and assert nothing was duplicated. It runs under the same
`SIGNET_TEST_POP3_HOST` gate as the other live tests.

## Driving the app headlessly

`signet` shares the desktop app's data directory by default, so the CLI is a way to set up or inspect
exactly what the app sees:

```sh
export SIGNET_POP3_PASSWORD=spike
signet add-account --email signet@example.com --pop-host 127.0.0.1 --pop-user signet \
                --pop-security plain --pop-port 1110 \
                --smtp-host 127.0.0.1 --smtp-security plain --smtp-port 1025
signet sync
signet list
signet show 3
signet send --account 1 --to someone@example.com --subject Hi --body "..."
```

Pass `--data-dir` to work against a scratch mailbox instead of the real one.

## Junk filter

The filter is checked two ways. Unit tests in `crates/signet-sync/src/junk.rs` cover the maths — that
ordinary business mail scores low, that one rare token cannot decide a message, that a long message
does not underflow to a false verdict, and that training shifts the outcome. Feature tests in
`crates/signet-core/tests/features.rs` cover the behaviour: marking junk moves *and* trains, marking
not-junk withdraws the vote, and marking the same message twice does not double-count.

To watch it work end to end, send an obvious spam and an ordinary message through Mailpit and sync:

```sh
signet --data-dir /tmp/junk-demo add-account --email you@example.com --pop-host 127.0.0.1 \
    --pop-user signet --pop-security plain --pop-port 1110
signet --data-dir /tmp/junk-demo sync
signet --data-dir /tmp/junk-demo list --folder inbox
signet --data-dir /tmp/junk-demo list --folder junk
```

With no training at all, a lottery-scam message lands in Junk and a "Re: PO 4471" does not.

## IMAP

The IMAP suite runs against **GreenMail**, a Java test server that speaks IMAP, POP3 and SMTP. It
needs a JDK; on a machine with Android Studio, its bundled JBR will do.

```sh
curl -sSL -o greenmail.jar \
  https://repo1.maven.org/maven2/com/icegreen/greenmail-standalone/2.1.0/greenmail-standalone-2.1.0.jar
java -Dgreenmail.setup.test.all -Dgreenmail.users=signet:spike@example.com -jar greenmail.jar &
node tools/seed-greenmail.mjs

SIGNET_TEST_IMAP_HOST=127.0.0.1 cargo test --workspace -- --test-threads=1
```

`--test-threads=1` matters: the live tests share one mailbox on the server, and running them
concurrently makes them fight over its flags.

The interesting tests open **two independent installations** against the same mailbox and assert
that read state and flags set on one appear on the other. That is the entire argument for IMAP
over POP3 for a shared mailbox, so it is tested directly rather than assumed.

### Why the tests use plaintext

GreenMail's IMAPS port presents a self-signed certificate, which the platform verifier correctly
refuses. Rather than weaken verification for tests — which would defeat the check that matters
most — the client keeps an explicit plaintext lab mode, exactly as POP3 does, and the tests opt
into it. One test asserts that a STARTTLS-configured account will *not* silently fall back to
plaintext.
