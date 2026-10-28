//! Headless driver for Signet.
//!
//! The Phase 1 deliverable — add an account, poll it, read the inbox, all without a UI — and
//! permanently the debugging tool that reaches the core directly (PLAN.md §3, "boundary rule").

use std::io::IsTerminal;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use signet_core::App;
use signet_net::pop3::{Pop3Config, Security};
use signet_store::{FolderKind, MessageQuery, NewAccount, Security as StoreSecurity};
use signet_sync::pop3_transport::{Pop3Credentials, Pop3Transport};
use signet_sync::transport::{FolderId, MailTransport};

#[derive(Parser, Debug)]
#[command(name = "signet", version, about = "Signet command line")]
struct Cli {
    /// Log filter, e.g. `debug` or `signet_net=trace`.
    #[arg(long, global = true, default_value = "warn")]
    log: String,

    /// Data directory. Defaults to the one the desktop app uses, so the CLI and the app share a
    /// mailbox rather than quietly diverging.
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Connect to a POP3 server and report what it supports and what is waiting.
    Probe(ProbeArgs),
    /// Round-trip a secret through the platform keystore.
    Vault(VaultArgs),
    /// Add an account to the store.
    AddAccount(AddAccountArgs),
    /// List configured accounts.
    Accounts,
    /// Poll accounts and ingest new mail.
    Sync {
        /// Account id. Omit to sync every enabled account.
        #[arg(long)]
        id: Option<i64>,
    },
    /// Print a folder's messages.
    List {
        #[arg(long, default_value = "inbox")]
        folder: String,
        #[arg(long, default_value_t = 25)]
        limit: i64,
        /// FTS5 search terms.
        #[arg(long, default_value = "")]
        search: String,
    },
    /// Print one message.
    Show { id: i64 },
    /// Report how a message's inline and remote images resolve.
    Images {
        id: i64,
        /// Report the "Show images" path instead of the default blocked view.
        #[arg(long)]
        remote: bool,
    },
    /// Stop an account being synced, without deleting anything it has downloaded.
    DisableAccount { id: i64 },
    /// Sync this account again.
    EnableAccount { id: i64 },
    /// Delete an account and every message downloaded for it. Not reversible.
    RemoveAccount {
        id: i64,
        /// Required. Deleting an account destroys its local mail.
        #[arg(long)]
        yes: bool,
    },
    /// Compose and send a message through the account's SMTP server.
    Send(SendArgs),
    /// Add someone to the sales team.
    TeamAdd {
        #[arg(long)]
        name: String,
        #[arg(long)]
        email: String,
        /// Mark this person as whoever uses this computer.
        #[arg(long)]
        me: bool,
    },
    /// List the sales team.
    Team,
    /// Assign a message to a team member by email, or to nobody.
    Assign {
        message: i64,
        #[arg(long)]
        to: Option<String>,
    },
    /// Set a message's priority.
    Priority {
        message: i64,
        #[arg(value_parser = ["high", "normal", "low"])]
        level: String,
    },
    /// Exchange team state with the shared file.
    TeamSync {
        /// Set the shared file path first.
        #[arg(long)]
        path: Option<String>,
    },
}

#[derive(clap::Args, Debug)]
struct SendArgs {
    #[arg(long)]
    account: i64,
    #[arg(long, required = true, num_args = 1..)]
    to: Vec<String>,
    #[arg(long, num_args = 0..)]
    cc: Vec<String>,
    #[arg(long, num_args = 0..)]
    bcc: Vec<String>,
    #[arg(long, default_value = "")]
    subject: String,
    #[arg(long, default_value = "")]
    body: String,
    /// Files to attach.
    #[arg(long, num_args = 0..)]
    attach: Vec<String>,
}

#[derive(clap::Args, Debug)]
struct ProbeArgs {
    host: String,
    #[arg(short, long)]
    user: String,
    #[arg(short, long, value_enum, default_value_t = SecurityArg::Tls)]
    security: SecurityArg,
    #[arg(short, long)]
    port: Option<u16>,
    #[arg(long, default_value = "SIGNET_POP3_PASSWORD")]
    password_env: String,
    #[arg(long, default_value_t = 3)]
    headers: usize,
    #[arg(long, default_value_t = 30)]
    timeout_secs: u64,
}

#[derive(clap::Args, Debug)]
struct VaultArgs {
    #[arg(long, default_value = "pop3:spike@example.com@test.invalid")]
    handle: String,
}

#[derive(clap::Args, Debug)]
struct AddAccountArgs {
    /// Email address, also used as the From address.
    #[arg(long)]
    email: String,
    /// Incoming protocol. IMAP shares folders and read state across the team; POP3 does not.
    #[arg(long, value_enum, default_value_t = ProtocolArg::Pop3)]
    protocol: ProtocolArg,
    #[arg(long)]
    name: Option<String>,
    #[arg(long)]
    pop_host: String,
    #[arg(long)]
    pop_user: Option<String>,
    #[arg(long, value_enum, default_value_t = SecurityArg::Tls)]
    pop_security: SecurityArg,
    #[arg(long)]
    pop_port: Option<u16>,
    #[arg(long, default_value = "")]
    smtp_host: String,
    #[arg(long, value_enum, default_value_t = SecurityArg::Starttls)]
    smtp_security: SecurityArg,
    #[arg(long)]
    smtp_port: Option<u16>,
    #[arg(long)]
    smtp_user: Option<String>,
    /// Read the password from this environment variable.
    #[arg(long, default_value = "SIGNET_POP3_PASSWORD")]
    password_env: String,
    /// Delete mail from the server once downloaded.
    #[arg(long)]
    delete_on_download: bool,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum ProtocolArg {
    /// Download-and-forget. One folder, no shared flags.
    Pop3,
    /// Server-side folders and flags, shared by everyone on the mailbox.
    Imap,
}

impl From<ProtocolArg> for signet_store::Protocol {
    fn from(a: ProtocolArg) -> Self {
        match a {
            ProtocolArg::Pop3 => signet_store::Protocol::Pop3,
            ProtocolArg::Imap => signet_store::Protocol::Imap,
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum SecurityArg {
    /// Implicit TLS (995 POP3 / 465 SMTP).
    Tls,
    /// STARTTLS (110 POP3 / 587 SMTP).
    Starttls,
    /// No encryption. Lab use only.
    Plain,
}

impl From<SecurityArg> for Security {
    fn from(a: SecurityArg) -> Self {
        match a {
            SecurityArg::Tls => Security::ImplicitTls,
            SecurityArg::Starttls => Security::StartTls,
            SecurityArg::Plain => Security::Plaintext,
        }
    }
}

impl From<SecurityArg> for StoreSecurity {
    fn from(a: SecurityArg) -> Self {
        match a {
            SecurityArg::Tls => StoreSecurity::Tls,
            SecurityArg::Starttls => StoreSecurity::Starttls,
            SecurityArg::Plain => StoreSecurity::Plain,
        }
    }
}

/// The directory the desktop app uses, so both see the same mailbox.
fn default_data_dir() -> PathBuf {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("app.signet.mail")
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| cli.log.clone().into()),
        )
        .with_ansi(std::io::stderr().is_terminal())
        .with_writer(std::io::stderr)
        .init();

    let data_dir = cli.data_dir.clone().unwrap_or_else(default_data_dir);

    match cli.command {
        Command::Probe(args) => probe(args).await,
        Command::Vault(args) => vault(args),
        Command::AddAccount(args) => add_account(&data_dir, args).await,
        Command::Accounts => accounts(&data_dir),
        Command::Sync { id } => sync(&data_dir, id).await,
        Command::List {
            folder,
            limit,
            search,
        } => list(&data_dir, &folder, limit, &search),
        Command::Show { id } => show(&data_dir, id),
        Command::Images { id, remote } => images(&data_dir, id, remote),
        Command::DisableAccount { id } => set_enabled(&data_dir, id, false),
        Command::EnableAccount { id } => set_enabled(&data_dir, id, true),
        Command::RemoveAccount { id, yes } => remove_account(&data_dir, id, yes),
        Command::Send(args) => send(&data_dir, args).await,
        Command::TeamAdd { name, email, me } => team_add(&data_dir, name, email, me),
        Command::Team => team_list(&data_dir),
        Command::Assign { message, to } => assign(&data_dir, message, to),
        Command::Priority { message, level } => set_priority(&data_dir, message, &level),
        Command::TeamSync { path } => team_sync(&data_dir, path),
    }
}

fn team_add(data_dir: &PathBuf, name: String, email: String, me: bool) -> Result<()> {
    let app = open(data_dir)?;
    let id = app.save_team_member(signet_store::TeamMember {
        id: 0,
        name,
        email,
        color: String::new(),
        initials: String::new(),
        is_active: true,
        is_me: me,
        sort_order: 0,
    })?;
    report(&format!("team member {id} added"));
    Ok(())
}

fn team_list(data_dir: &PathBuf) -> Result<()> {
    let app = open(data_dir)?;
    let team = app.list_team()?;
    if team.is_empty() {
        report("nobody on the team yet");
    }
    for m in team {
        report(&format!(
            "[{}] {:<4} {:<22} {:<30} {}{}",
            m.id,
            m.initials,
            m.name,
            m.email,
            m.color,
            if m.is_me { "  (me)" } else { "" }
        ));
    }
    Ok(())
}

fn assign(data_dir: &PathBuf, message: i64, to: Option<String>) -> Result<()> {
    let app = open(data_dir)?;
    let member_id = match &to {
        Some(email) => Some(
            app.list_team()?
                .into_iter()
                .find(|m| m.email.eq_ignore_ascii_case(email.trim()))
                .map(|m| m.id)
                .with_context(|| format!("no team member with email {email}"))?,
        ),
        None => None,
    };
    app.assign(vec![message], member_id)?;
    report(
        match &to {
            Some(e) => format!("message {message} assigned to {e}"),
            None => format!("message {message} unassigned"),
        }
        .as_str(),
    );
    Ok(())
}

fn set_priority(data_dir: &PathBuf, message: i64, level: &str) -> Result<()> {
    let app = open(data_dir)?;
    let priority = match level {
        "high" => signet_store::Priority::High,
        "low" => signet_store::Priority::Low,
        _ => signet_store::Priority::Normal,
    };
    app.set_priority(vec![message], priority)?;
    report(&format!("message {message} set to {level} priority"));
    Ok(())
}

fn team_sync(data_dir: &PathBuf, path: Option<String>) -> Result<()> {
    let app = open(data_dir)?;
    if let Some(p) = path {
        app.set_shared_path(p)?;
    }
    let configured = app.shared_path()?;
    if configured.trim().is_empty() {
        report("no shared team file configured; team state stays on this computer");
        return Ok(());
    }
    let out = app.sync_team()?;
    match out.unreachable {
        Some(e) => report(&format!("shared file unreachable: {e}")),
        None => report(&format!(
            "received {}, published {}, added {} member(s) — {}",
            out.pulled, out.pushed, out.members_added, configured
        )),
    }
    Ok(())
}

async fn send(data_dir: &PathBuf, args: SendArgs) -> Result<()> {
    let app = open(data_dir)?;
    app.send(signet_core::outbox::Draft {
        account_id: args.account,
        to: args.to,
        cc: args.cc,
        bcc: args.bcc,
        subject: args.subject,
        body_text: args.body,
        // The CLI composes plain text; there is no rich version to send.
        body_html: None,
        in_reply_to: None,
        references: Vec::new(),
        attachments: args
            .attach
            .iter()
            .map(|p| signet_core::outbox::DraftAttachment {
                filename: std::path::Path::new(p)
                    .file_name()
                    .map(|f| f.to_string_lossy().to_string())
                    .unwrap_or_else(|| "attachment".into()),
                path: p.clone(),
            })
            .collect(),
        // The CLI has no picker, so the account default applies.
        signature: None,
        draft_message_id: None,
    })
    .await?;
    report("sent, and filed a copy in Sent");
    Ok(())
}

fn open(data_dir: &PathBuf) -> Result<App> {
    App::open(data_dir).with_context(|| format!("opening store at {}", data_dir.display()))
}

async fn add_account(data_dir: &PathBuf, args: AddAccountArgs) -> Result<()> {
    let password = std::env::var(&args.password_env).with_context(|| {
        format!(
            "set {} to the mailbox password (never passed as an argument, where it would land in \
             shell history and the process list)",
            args.password_env
        )
    })?;

    let pop_security: StoreSecurity = args.pop_security.into();
    let smtp_security: StoreSecurity = args.smtp_security.into();
    let protocol: signet_store::Protocol = args.protocol.into();
    let pop_user = args.pop_user.unwrap_or_else(|| args.email.clone());

    let new = NewAccount {
        display_name: args.name.unwrap_or_else(|| args.email.clone()),
        email: args.email.clone(),
        pop_host: args.pop_host,
        pop_port: args.pop_port.unwrap_or(protocol.default_port(pop_security)),
        pop_security,
        pop_user: pop_user.clone(),
        smtp_host: args.smtp_host,
        smtp_port: args.smtp_port.unwrap_or(smtp_security.default_smtp_port()),
        smtp_security,
        smtp_user: args.smtp_user.unwrap_or(pop_user),
        leave_on_server: !args.delete_on_download,
        delete_after_days: None,
        poll_minutes: 5,
        signature: String::new(),
        protocol,
    };

    let app = open(data_dir)?;

    // Both halves are reported, and only the incoming one blocks: an account that cannot collect
    // mail is useless, whereas one that cannot send yet is still worth having while the SMTP
    // details are sorted out.
    let check = app.check_account(&new, &password).await;
    for half in [&check.incoming, &check.outgoing] {
        report(&format!(
            "{} {}: {}",
            if half.ok { "OK  " } else { "FAIL" },
            half.label,
            half.detail
        ));
    }
    if !check.incoming.ok {
        bail!("not adding the account: incoming mail could not be set up");
    }

    let id = app.add_account(new, &password)?;
    report(&format!("account {id} added ({})", args.email));
    Ok(())
}

fn accounts(data_dir: &PathBuf) -> Result<()> {
    let app = open(data_dir)?;
    let accounts = app.list_accounts()?;
    if accounts.is_empty() {
        report("no accounts configured");
        return Ok(());
    }
    for a in accounts {
        report(&format!(
            "[{}] {} <{}>  {}={}:{} smtp={}  {}",
            a.id,
            a.display_name,
            a.email,
            a.protocol.as_str(),
            a.pop_host,
            a.pop_port,
            if a.smtp_host.is_empty() {
                "(none)".into()
            } else {
                format!("{}:{}", a.smtp_host, a.smtp_port)
            },
            if a.leave_on_server {
                "leave on server"
            } else {
                "delete on download"
            }
        ));
    }
    Ok(())
}

async fn sync(data_dir: &PathBuf, id: Option<i64>) -> Result<()> {
    let app = open(data_dir)?;
    match id {
        Some(id) => {
            let r = app.sync_account(id).await?;
            report(&format!(
                "account {id}: {} new, {} already seen, {} bytes, {} deleted from server",
                r.new_messages, r.already_seen, r.bytes_downloaded, r.deleted_from_server
            ));
            for e in &r.errors {
                report(&format!("  error: {e}"));
            }
        }
        None => {
            let outcomes = app.sync_all().await;
            if outcomes.is_empty() {
                report("no accounts to sync");
            }
            for (account_id, result) in outcomes {
                match result {
                    Ok(r) => report(&format!(
                        "account {account_id}: {} new, {} already seen",
                        r.new_messages, r.already_seen
                    )),
                    Err(e) => report(&format!("account {account_id}: FAILED — {e}")),
                }
            }
        }
    }
    Ok(())
}

fn list(data_dir: &PathBuf, folder: &str, limit: i64, search: &str) -> Result<()> {
    let app = open(data_dir)?;
    let messages = app.list_messages(MessageQuery {
        folder_kind: Some(FolderKind::parse(folder)),
        limit,
        search: search.to_string(),
        ..Default::default()
    })?;

    if messages.is_empty() {
        report("no messages");
        return Ok(());
    }
    for m in messages {
        report(&format!(
            "{:>6}  {}{}  {:<28.28}  {:<44.44}  {}",
            m.id,
            if m.is_read { ' ' } else { '*' },
            if m.has_attachments { '@' } else { ' ' },
            m.from_name.chars().take(28).collect::<String>(),
            m.subject.chars().take(44).collect::<String>(),
            m.date_utc
        ));
    }
    Ok(())
}

fn set_enabled(data_dir: &PathBuf, id: i64, enabled: bool) -> Result<()> {
    let app = open(data_dir)?;
    let mut settings = app.account_settings(id)?;
    settings.is_enabled = enabled;
    app.update_account_settings(id, settings)?;
    let account = app.list_accounts()?.into_iter().find(|a| a.id == id);
    report(&format!(
        "account {id}{} is now {}",
        account
            .map(|a| format!(" ({})", a.email))
            .unwrap_or_default(),
        if enabled { "enabled" } else { "disabled" }
    ));
    Ok(())
}

fn remove_account(data_dir: &PathBuf, id: i64, yes: bool) -> Result<()> {
    let app = open(data_dir)?;
    let account = app
        .list_accounts()?
        .into_iter()
        .find(|a| a.id == id)
        .with_context(|| format!("no account with id {id}"))?;

    if !yes {
        bail!(
            "this deletes every message downloaded for {} — pass --yes to confirm",
            account.email
        );
    }
    app.delete_account(id)?;
    report(&format!("account {id} ({}) removed", account.email));
    Ok(())
}

fn images(data_dir: &PathBuf, id: i64, remote: bool) -> Result<()> {
    let app = open(data_dir)?;
    let m = if remote {
        app.get_message_with_remote_content(id)?
    } else {
        app.get_message(id)?
    };
    report(&format!(
        "mode:           {}",
        if remote { "show images" } else { "blocked" }
    ));
    report(&format!(
        "raw_path:       {:?}",
        app.store().raw_path(id).unwrap_or_default()
    ));
    let html = m.body_html.clone().unwrap_or_default();
    let imgs = html.to_lowercase().matches("<img").count();
    let cid_left = html.to_lowercase().matches("cid:").count();
    let data_uris = html.matches("data:image").count();
    report(&format!("subject:        {}", m.subject));
    report(&format!("<img> tags:     {imgs}"));
    report(&format!("inlined (data:) {data_uris}"));
    report(&format!("unresolved cid: {cid_left}"));
    report(&format!("remote blocked: {}", m.has_remote_content));
    let html = m.body_html.clone().unwrap_or_default();
    let remote_srcs = html.matches("src=\"http").count();
    report(&format!("http srcs:      {remote_srcs}"));
    // Print each <img ...> tag so a "nothing appeared" report can be pinned on the actual markup.
    let mut rest = html.as_str();
    while let Some(i) = rest.find("<img") {
        let tag = &rest[i..];
        let end = tag.find('>').map(|e| e + 1).unwrap_or(tag.len().min(240));
        report(&format!("  tag: {}", &tag[..end.min(240)]));
        rest = &tag[end..];
    }
    for a in &m.attachments {
        if a.is_inline || a.content_id.is_some() {
            report(&format!(
                "  inline part: {} ({}, {} bytes) cid={:?}",
                a.filename, a.mime_type, a.size_bytes, a.content_id
            ));
        }
    }
    Ok(())
}

fn show(data_dir: &PathBuf, id: i64) -> Result<()> {
    let app = open(data_dir)?;
    let m = app.get_message(id)?;
    report(&format!("Subject: {}", m.subject));
    report(&format!("From:    {} <{}>", m.from_name, m.from_addr));
    for r in &m.recipients {
        report(&format!("{:<8} {}", format!("{}:", r.kind), r.addr));
    }
    if m.has_remote_content {
        report("[remote images were blocked]");
    }
    for a in &m.attachments {
        report(&format!(
            "Attach:  {} ({}, {} bytes)",
            a.filename, a.mime_type, a.size_bytes
        ));
    }
    report("");
    report(&m.body_text);
    Ok(())
}

/// Phase 0 spike #1: prove POP3 over TLS against a real server.
async fn probe(args: ProbeArgs) -> Result<()> {
    let security: Security = args.security.into();
    let password = std::env::var(&args.password_env).with_context(|| {
        format!(
            "set {} to the mailbox password (it is never read from the command line, \
             where it would land in shell history and the process list)",
            args.password_env
        )
    })?;

    if security == Security::Plaintext {
        tracing::warn!(
            "connecting WITHOUT encryption - credentials will cross the network in the clear"
        );
    }

    let mut config = match security {
        Security::ImplicitTls => Pop3Config::implicit_tls(&args.host),
        Security::StartTls => Pop3Config::starttls(&args.host),
        Security::Plaintext => Pop3Config::plaintext(&args.host),
    }
    .with_timeout(Duration::from_secs(args.timeout_secs));
    if let Some(port) = args.port {
        config = config.with_port(port);
    }

    let credentials = Pop3Credentials {
        user: args.user.clone(),
        password,
        allow_plaintext: security == Security::Plaintext,
    };

    let mut transport = Pop3Transport::new(config.clone(), credentials);
    transport
        .connect()
        .await
        .with_context(|| format!("connecting to {}:{}", config.host, config.port))?;

    let caps = transport.capabilities();
    report(&format!(
        "connected to {}:{} ({:?})",
        config.host, config.port, security
    ));
    report(&format!(
        "capabilities: stable_ids={} partial_fetch={} push={} server_flags={}",
        caps.stable_ids, caps.partial_fetch, caps.push, caps.server_side_flags
    ));
    if caps.is_local_authoritative() {
        report("local store is authoritative: read state and folders will not sync across devices");
    }

    let inbox = FolderId::inbox();
    let messages = transport.list_messages(&inbox).await?;
    let total: u64 = messages.iter().map(|m| m.size).sum();
    report(&format!(
        "{} message(s), {} bytes total",
        messages.len(),
        total
    ));

    let without_uid = messages
        .iter()
        .filter(|m| m.id.0.starts_with("size:"))
        .count();
    if without_uid > 0 {
        report(&format!(
            "WARNING: {without_uid} message(s) had no UIDL; dedupe would be degraded"
        ));
    }

    for msg in messages.iter().take(args.headers) {
        let headers = transport.fetch_headers(&inbox, &msg.id).await?;
        let text = String::from_utf8_lossy(&headers);
        let subject = header(&text, "Subject").unwrap_or_else(|| "(no subject)".into());
        let from = header(&text, "From").unwrap_or_else(|| "(no sender)".into());
        report(&format!(
            "  [{}] {} bytes\n      from: {}\n      subj: {}",
            msg.id.0, msg.size, from, subject
        ));
    }

    // Deliberately committing an empty session: nothing was marked for deletion, so this proves
    // the clean-QUIT path without risking mail.
    Box::new(transport).commit().await?;
    report("session committed cleanly (QUIT)");
    Ok(())
}

/// Phase 0 spike #3: prove the platform keystore.
fn vault(args: VaultArgs) -> Result<()> {
    let secret = signet_auth::Credential::Password("phase-0-spike".into());

    signet_auth::store(&args.handle, &secret).context("storing secret")?;
    report(&format!("stored secret under {:?}", args.handle));

    let loaded = signet_auth::load(&args.handle).context("loading secret")?;
    if loaded.expose() != "phase-0-spike" {
        bail!("keystore returned a different secret than was stored");
    }
    report("loaded secret matches");

    signet_auth::delete(&args.handle).context("deleting secret")?;
    match signet_auth::load(&args.handle) {
        Err(signet_auth::Error::NotFound(_)) => report("deleted; lookup now reports NotFound"),
        Ok(_) => bail!("secret survived deletion"),
        Err(e) => return Err(e).context("unexpected error after deletion"),
    }
    Ok(())
}

/// First non-continuation value of `name` from a header block.
fn header(raw: &str, name: &str) -> Option<String> {
    let prefix = format!("{name}:");
    raw.lines()
        .find(|l| l.len() >= prefix.len() && l[..prefix.len()].eq_ignore_ascii_case(&prefix))
        .map(|l| l[prefix.len()..].trim().to_string())
}

/// Operator-facing output, as distinct from the structured logs on stderr.
fn report(line: &str) {
    #[allow(clippy::print_stdout)]
    {
        println!("{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_lookup_is_case_insensitive() {
        let raw = "from: a@b.c\r\nSUBJECT: Hello\r\nX-Other: no\r\n";
        assert_eq!(header(raw, "From").as_deref(), Some("a@b.c"));
        assert_eq!(header(raw, "Subject").as_deref(), Some("Hello"));
        assert_eq!(header(raw, "Cc"), None);
    }

    #[test]
    fn header_lookup_does_not_match_a_prefix_of_another_header() {
        let raw = "X-Subject-Prefix: no\r\nSubject: yes\r\n";
        assert_eq!(header(raw, "Subject").as_deref(), Some("yes"));
    }

    #[test]
    fn cli_parses_a_typical_probe() {
        let cli = Cli::try_parse_from([
            "signet",
            "probe",
            "mail.example.com",
            "--user",
            "dev@example.com",
            "--security",
            "starttls",
        ])
        .unwrap();
        let Command::Probe(args) = cli.command else {
            panic!("expected probe");
        };
        assert_eq!(args.security, SecurityArg::Starttls);
        assert_eq!(args.host, "mail.example.com");
    }

    #[test]
    fn password_cannot_be_passed_as_an_argument() {
        // Passwords on the command line leak into shell history and the process list.
        assert!(
            Cli::try_parse_from(["signet", "probe", "host", "--user", "u", "--password", "p"])
                .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "signet",
                "add-account",
                "--email",
                "a@b.c",
                "--pop-host",
                "h",
                "--password",
                "p"
            ])
            .is_err()
        );
    }

    #[test]
    fn ports_default_from_the_security_mode() {
        assert_eq!(
            StoreSecurity::from(SecurityArg::Tls).default_pop_port(),
            995
        );
        assert_eq!(
            StoreSecurity::from(SecurityArg::Starttls).default_pop_port(),
            110
        );
        assert_eq!(
            StoreSecurity::from(SecurityArg::Starttls).default_smtp_port(),
            587
        );
        assert_eq!(
            StoreSecurity::from(SecurityArg::Tls).default_smtp_port(),
            465
        );
    }
}
