//! Tauri shell: maps the [`signet_core::App`] facade onto commands the UI can call.
//!
//! There is deliberately no logic here beyond translation. Anything that decides something
//! belongs in the core, where it can be tested without a window.

use std::sync::Arc;

use signet_core::rules::Rule;
use signet_core::{App, Event, FilingInfo};
use signet_core::{SyncSummary, Triage};
use signet_store::{
    AccountSettings, Folder, MessageDetail, MessageQuery, MessageSummary, NewAccount, Priority,
    Reminder, Signature, TeamMember,
};
use signet_sync::SyncReport;
use signet_sync::outbox::Draft;
use serde::Serialize;
use tauri::{Emitter, Manager};

/// Commands return this rather than a Rust error type, because `anyhow`-style strings are all the
/// UI can act on and `Result<_, String>` is what Tauri serialises cleanly.
type CmdResult<T> = std::result::Result<T, String>;

fn err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

struct AppState {
    app: Arc<App>,
}

#[derive(Serialize)]
struct AccountView {
    id: i64,
    display_name: String,
    email: String,
    pop_host: String,
    is_enabled: bool,
    last_sync_utc: Option<i64>,
    /// Whether this account can send. Composing against an account with no SMTP host should be
    /// blocked in the UI rather than failing at send time.
    can_send: bool,
    /// "pop3" or "imap" — the UI shows it, because the two behave very differently for a team.
    protocol: String,
}

#[tauri::command]
fn list_accounts(state: tauri::State<'_, AppState>) -> CmdResult<Vec<AccountView>> {
    Ok(state
        .app
        .list_accounts()
        .map_err(err)?
        .into_iter()
        .map(|a| AccountView {
            id: a.id,
            display_name: a.display_name,
            email: a.email,
            pop_host: a.pop_host,
            is_enabled: a.is_enabled,
            last_sync_utc: a.last_sync_utc,
            can_send: !a.smtp_host.trim().is_empty(),
            protocol: a.protocol.as_str().to_string(),
        })
        .collect())
}

#[tauri::command]
fn add_account(
    state: tauri::State<'_, AppState>,
    account: NewAccount,
    password: String,
) -> CmdResult<i64> {
    state.app.add_account(account, &password).map_err(err)
}

#[tauri::command]
async fn test_account(
    state: tauri::State<'_, AppState>,
    account: NewAccount,
    password: String,
) -> CmdResult<signet_core::AccountCheck> {
    // Never an Err: a failed check is a result to display, not an error to swallow. Both halves
    // are reported so one failure cannot hide the other.
    Ok(state.app.check_account(&account, &password).await)
}

#[tauri::command]
fn delete_account(state: tauri::State<'_, AppState>, id: i64) -> CmdResult<()> {
    state.app.delete_account(id).map_err(err)
}

#[tauri::command]
async fn sync_account(state: tauri::State<'_, AppState>, id: i64) -> CmdResult<SyncReport> {
    state.app.sync_account(id).await.map_err(err)
}

/// Sync whichever accounts are due, per their own polling interval.
#[tauri::command]
async fn sync_due(state: tauri::State<'_, AppState>) -> CmdResult<Vec<SyncOutcome>> {
    Ok(state
        .app
        .sync_due()
        .await
        .into_iter()
        .map(|(account_id, result)| match result {
            Ok(report) => SyncOutcome {
                account_id,
                new_messages: report.new_messages,
                error: None,
            },
            Err(e) => SyncOutcome {
                account_id,
                new_messages: 0,
                error: Some(e.to_string()),
            },
        })
        .collect())
}

#[tauri::command]
async fn sync_all(state: tauri::State<'_, AppState>) -> CmdResult<Vec<SyncOutcome>> {
    Ok(state
        .app
        .sync_all()
        .await
        .into_iter()
        .map(|(account_id, result)| match result {
            Ok(report) => SyncOutcome {
                account_id,
                new_messages: report.new_messages,
                error: None,
            },
            Err(e) => SyncOutcome {
                account_id,
                new_messages: 0,
                error: Some(e),
            },
        })
        .collect())
}

#[derive(Serialize)]
struct SyncOutcome {
    account_id: i64,
    new_messages: usize,
    error: Option<String>,
}

#[tauri::command]
fn list_folders(
    state: tauri::State<'_, AppState>,
    account_id: Option<i64>,
) -> CmdResult<Vec<Folder>> {
    state.app.list_folders(account_id).map_err(err)
}

#[tauri::command]
fn list_messages(
    state: tauri::State<'_, AppState>,
    query: MessageQuery,
) -> CmdResult<Vec<MessageSummary>> {
    state.app.list_messages(query).map_err(err)
}

#[tauri::command]
fn get_message(state: tauri::State<'_, AppState>, id: i64) -> CmdResult<MessageDetail> {
    state.app.get_message(id).map_err(err)
}

/// Re-render one message with remote images permitted, after the user asks for it.
///
/// On a blocking thread because the images are downloaded here (see `signet_core::remote`).
#[tauri::command]
async fn show_remote_content(
    state: tauri::State<'_, AppState>,
    id: i64,
) -> CmdResult<MessageDetail> {
    let app = state.app.clone();
    blocking(move || app.get_message_with_remote_content(id)).await
}

#[tauri::command]
fn set_read(state: tauri::State<'_, AppState>, ids: Vec<i64>, read: bool) -> CmdResult<()> {
    state.app.set_read(ids, read).map_err(err)
}

#[tauri::command]
fn set_flagged(state: tauri::State<'_, AppState>, ids: Vec<i64>, flagged: bool) -> CmdResult<()> {
    state.app.set_flagged(ids, flagged).map_err(err)
}

#[tauri::command]
fn move_messages(
    state: tauri::State<'_, AppState>,
    ids: Vec<i64>,
    folder_id: i64,
) -> CmdResult<()> {
    state.app.move_messages(ids, folder_id).map_err(err)
}

#[tauri::command]
fn delete_messages(state: tauri::State<'_, AppState>, ids: Vec<i64>) -> CmdResult<()> {
    state.app.delete_messages(ids).map_err(err)
}

#[tauri::command]
fn save_attachment(
    state: tauri::State<'_, AppState>,
    attachment_id: i64,
    dest: String,
) -> CmdResult<()> {
    state
        .app
        .save_attachment(attachment_id, std::path::Path::new(&dest))
        .map_err(err)
}

// ---------------------------------------------------------------- data file

/// Read an archive's manifest without importing it, so the user can be shown what is in the file
/// before agreeing to merge it.
#[tauri::command]
async fn inspect_archive(
    state: tauri::State<'_, AppState>,
    path: String,
) -> CmdResult<signet_core::archive::Manifest> {
    let app = state.app.clone();
    // Archives are read and written on a blocking thread: a full mailbox is tens of thousands of
    // files, and doing that on the UI thread would freeze the window for the duration.
    blocking(move || app.inspect_archive(path)).await
}

#[tauri::command]
async fn export_data_file(
    state: tauri::State<'_, AppState>,
    path: String,
) -> CmdResult<signet_core::archive::ExportSummary> {
    let app = state.app.clone();
    blocking(move || app.export_data_file(path)).await
}

#[tauri::command]
async fn import_data_file(
    state: tauri::State<'_, AppState>,
    path: String,
) -> CmdResult<signet_core::archive::ImportSummary> {
    let app = state.app.clone();
    blocking(move || app.import_data_file(path)).await
}

#[tauri::command]
async fn preview_outlook_file(
    state: tauri::State<'_, AppState>,
    path: String,
) -> CmdResult<signet_core::outlook::OutlookPreview> {
    let app = state.app.clone();
    blocking(move || app.preview_outlook_file(path)).await
}

/// Make a local-only mailbox for an archive to be imported into.
#[tauri::command]
fn create_local_archive(state: tauri::State<'_, AppState>, name: String) -> CmdResult<i64> {
    state.app.create_local_archive(name).map_err(err)
}

#[tauri::command]
async fn import_outlook_file(
    state: tauri::State<'_, AppState>,
    path: String,
    account_id: i64,
) -> CmdResult<signet_core::outlook::OutlookImportSummary> {
    state
        .app
        .import_outlook_file(path, account_id)
        .await
        .map_err(err)
}

/// Open one message in a window of its own.
///
/// Reusing an already-open window rather than stacking duplicates: double-clicking the same message
/// twice should bring its window forward, not open a second copy of it.
/// The product name this build was packaged with, for window titles.
fn app_name<R: tauri::Runtime>(app: &tauri::AppHandle<R>) -> String {
    app.config()
        .product_name
        .clone()
        .unwrap_or_else(|| "Signet".into())
}

#[tauri::command]
async fn open_message_window(app: tauri::AppHandle, id: i64) -> CmdResult<()> {
    use tauri::Manager;

    let label = format!("message-{id}");
    if let Some(existing) = app.get_webview_window(&label) {
        let _ = existing.unminimize();
        let _ = existing.set_focus();
        return Ok(());
    }

    tauri::WebviewWindowBuilder::new(
        &app,
        &label,
        tauri::WebviewUrl::App(format!("index.html?message={id}").into()),
    )
    .title(app_name(&app))
    .inner_size(920.0, 720.0)
    .min_inner_size(520.0, 400.0)
    // Ctrl + / Ctrl − / Ctrl 0, the only way to enlarge text in a webview. Tauri turns it off.
    .zoom_hotkeys_enabled(true)
    .build()
    .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
async fn attachment_preview(
    state: tauri::State<'_, AppState>,
    attachment_id: i64,
) -> CmdResult<signet_core::AttachmentPreview> {
    let app = state.app.clone();
    blocking(move || app.attachment_preview(attachment_id)).await
}

// ---------------------------------------------------------------- scheduled send

#[tauri::command]
async fn schedule_send(
    state: tauri::State<'_, AppState>,
    draft: Draft,
    when_utc: i64,
) -> CmdResult<i64> {
    state.app.schedule_send(draft, when_utc).await.map_err(err)
}

#[tauri::command]
fn scheduled_messages(state: tauri::State<'_, AppState>) -> CmdResult<Vec<signet_store::OutboxItem>> {
    state.app.scheduled_messages().map_err(err)
}

#[tauri::command]
fn cancel_scheduled(state: tauri::State<'_, AppState>, id: i64) -> CmdResult<bool> {
    state.app.cancel_scheduled(id).map_err(err)
}

#[tauri::command]
async fn send_scheduled_now(state: tauri::State<'_, AppState>, id: i64) -> CmdResult<bool> {
    state.app.send_scheduled_now(id).await.map_err(err)
}

/// Send anything whose time has come. Called on a timer, so nothing waits on the user noticing.
#[tauri::command]
async fn flush_outbox(state: tauri::State<'_, AppState>) -> CmdResult<signet_core::OutboxFlush> {
    state.app.flush_outbox().await.map_err(err)
}

// ---------------------------------------------------------------- contacts

#[tauri::command]
fn list_contacts(
    state: tauri::State<'_, AppState>,
    search: String,
) -> CmdResult<Vec<signet_store::Contact>> {
    state.app.list_contacts(search).map_err(err)
}

#[tauri::command]
fn save_contact(state: tauri::State<'_, AppState>, contact: signet_store::Contact) -> CmdResult<()> {
    state.app.save_contact(contact).map_err(err)
}

#[tauri::command]
fn delete_contact(state: tauri::State<'_, AppState>, id: i64) -> CmdResult<()> {
    state.app.delete_contact(id).map_err(err)
}

#[tauri::command]
async fn export_contacts(
    state: tauri::State<'_, AppState>,
    path: String,
    search: String,
) -> CmdResult<i64> {
    let app = state.app.clone();
    blocking(move || app.export_contacts(path, search)).await
}

/// Open an attachment in whatever program Windows uses for it.
#[tauri::command]
async fn open_attachment_externally(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    attachment_id: i64,
) -> CmdResult<()> {
    use tauri_plugin_opener::OpenerExt;

    let core = state.app.clone();
    let path = blocking(move || core.stage_attachment_for_opening(attachment_id)).await?;

    app.opener()
        .open_path(path, None::<&str>)
        .map_err(|e| format!("could not open it: {e}"))
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> signet_core::Result<T> + Send + 'static,
) -> CmdResult<T> {
    tauri::async_runtime::spawn_blocking(f)
        .await
        .map_err(|e| e.to_string())?
        .map_err(err)
}

#[tauri::command]
fn contacts(state: tauri::State<'_, AppState>, prefix: String) -> CmdResult<Vec<(String, String)>> {
    state.app.contacts(&prefix).map_err(err)
}

#[tauri::command]
fn reply_draft(state: tauri::State<'_, AppState>, id: i64, reply_all: bool) -> CmdResult<Draft> {
    state.app.reply_draft(id, reply_all).map_err(err)
}

#[tauri::command]
fn forward_draft(state: tauri::State<'_, AppState>, id: i64) -> CmdResult<Draft> {
    state.app.forward_draft(id).map_err(err)
}

#[tauri::command]
async fn send(state: tauri::State<'_, AppState>, draft: Draft) -> CmdResult<bool> {
    state.app.send(draft).await.map_err(err)
}

// ---------------------------------------------------------------- settings

/// Test a new password against the server and store it only if it works.
#[tauri::command]
async fn change_password(
    state: tauri::State<'_, AppState>,
    id: i64,
    password: String,
) -> CmdResult<()> {
    state.app.change_password(id, &password).await.map_err(err)
}

#[tauri::command]
fn account_settings(state: tauri::State<'_, AppState>, id: i64) -> CmdResult<AccountSettings> {
    state.app.account_settings(id).map_err(err)
}

#[tauri::command]
fn update_account_settings(
    state: tauri::State<'_, AppState>,
    id: i64,
    settings: AccountSettings,
) -> CmdResult<()> {
    state.app.update_account_settings(id, settings).map_err(err)
}

// ---------------------------------------------------------------- rules

#[tauri::command]
fn list_rules(state: tauri::State<'_, AppState>, account_id: Option<i64>) -> CmdResult<Vec<Rule>> {
    state.app.list_rules(account_id).map_err(err)
}

#[tauri::command]
fn save_rule(state: tauri::State<'_, AppState>, rule: Rule) -> CmdResult<i64> {
    state.app.save_rule(rule).map_err(err)
}

#[tauri::command]
fn delete_rule(state: tauri::State<'_, AppState>, id: i64) -> CmdResult<()> {
    state.app.delete_rule(id).map_err(err)
}

#[tauri::command]
fn apply_rules_now(state: tauri::State<'_, AppState>, account_id: i64) -> CmdResult<usize> {
    state.app.apply_rules_now(account_id).map_err(err)
}

// ---------------------------------------------------------------- junk

#[tauri::command]
fn mark_junk(state: tauri::State<'_, AppState>, ids: Vec<i64>, is_junk: bool) -> CmdResult<()> {
    state.app.mark_junk(ids, is_junk).map_err(err)
}

#[tauri::command]
fn filing_info(state: tauri::State<'_, AppState>, id: i64) -> CmdResult<FilingInfo> {
    state.app.filing_info(id).map_err(err)
}

#[derive(Serialize)]
struct JunkCorpus {
    spam: i64,
    ham: i64,
    trained: bool,
}

#[tauri::command]
fn list_sender_rules(state: tauri::State<'_, AppState>) -> CmdResult<Vec<signet_store::SenderRule>> {
    state.app.list_sender_rules().map_err(err)
}

#[tauri::command]
fn set_sender_rule(
    state: tauri::State<'_, AppState>,
    pattern: String,
    kind: String,
) -> CmdResult<()> {
    state.app.set_sender_rule(pattern, kind).map_err(err)
}

#[tauri::command]
fn remove_sender_rule(state: tauri::State<'_, AppState>, pattern: String) -> CmdResult<()> {
    state.app.remove_sender_rule(pattern).map_err(err)
}

#[tauri::command]
fn rule_for_message(
    state: tauri::State<'_, AppState>,
    message_id: i64,
    kind: String,
    whole_domain: bool,
) -> CmdResult<String> {
    state
        .app
        .rule_for_message(message_id, kind, whole_domain)
        .map_err(err)
}

#[tauri::command]
fn junk_corpus(state: tauri::State<'_, AppState>) -> CmdResult<JunkCorpus> {
    let (spam, ham, trained) = state.app.junk_corpus().map_err(err)?;
    Ok(JunkCorpus { spam, ham, trained })
}

// ---------------------------------------------------------------- drafts

#[tauri::command]
fn save_draft(state: tauri::State<'_, AppState>, draft: Draft) -> CmdResult<i64> {
    state.app.save_draft(draft).map_err(err)
}

#[tauri::command]
fn load_draft(state: tauri::State<'_, AppState>, id: i64) -> CmdResult<Draft> {
    state.app.load_draft(id).map_err(err)
}

#[tauri::command]
fn mark_folder_read(state: tauri::State<'_, AppState>, folder_id: i64) -> CmdResult<usize> {
    state.app.mark_folder_read(folder_id).map_err(err)
}

// ---------------------------------------------------------------- team & triage

#[tauri::command]
fn list_team(state: tauri::State<'_, AppState>) -> CmdResult<Vec<TeamMember>> {
    state.app.list_team().map_err(err)
}

#[tauri::command]
fn save_team_member(state: tauri::State<'_, AppState>, member: TeamMember) -> CmdResult<i64> {
    state.app.save_team_member(member).map_err(err)
}

#[tauri::command]
fn delete_team_member(state: tauri::State<'_, AppState>, id: i64) -> CmdResult<()> {
    state.app.delete_team_member(id).map_err(err)
}

#[tauri::command]
fn set_me(state: tauri::State<'_, AppState>, id: i64) -> CmdResult<()> {
    state.app.set_me(id).map_err(err)
}

#[tauri::command]
fn triage(state: tauri::State<'_, AppState>, id: i64) -> CmdResult<Triage> {
    state.app.triage(id).map_err(err)
}

#[tauri::command]
fn set_priority(
    state: tauri::State<'_, AppState>,
    ids: Vec<i64>,
    priority: Priority,
) -> CmdResult<()> {
    state.app.set_priority(ids, priority).map_err(err)
}

#[tauri::command]
fn assign(
    state: tauri::State<'_, AppState>,
    ids: Vec<i64>,
    member_id: Option<i64>,
) -> CmdResult<()> {
    state.app.assign(ids, member_id).map_err(err)
}

#[tauri::command]
fn set_work_status(
    state: tauri::State<'_, AppState>,
    ids: Vec<i64>,
    status: String,
) -> CmdResult<()> {
    state.app.set_work_status(ids, status).map_err(err)
}

#[tauri::command]
fn set_reminder(
    state: tauri::State<'_, AppState>,
    message_id: i64,
    due_utc: i64,
    note: String,
) -> CmdResult<()> {
    state
        .app
        .set_reminder(message_id, due_utc, note)
        .map_err(err)
}

#[tauri::command]
fn complete_reminder(state: tauri::State<'_, AppState>, message_id: i64) -> CmdResult<()> {
    state.app.complete_reminder(message_id).map_err(err)
}

#[tauri::command]
fn clear_reminder(state: tauri::State<'_, AppState>, message_id: i64) -> CmdResult<()> {
    state.app.clear_reminder(message_id).map_err(err)
}

#[tauri::command]
fn open_reminders(state: tauri::State<'_, AppState>) -> CmdResult<Vec<Reminder>> {
    state.app.open_reminders().map_err(err)
}

#[tauri::command]
fn take_due_reminders(state: tauri::State<'_, AppState>) -> CmdResult<Vec<Reminder>> {
    state.app.take_due_reminders().map_err(err)
}

#[tauri::command]
fn sync_team(state: tauri::State<'_, AppState>) -> CmdResult<SyncSummary> {
    state.app.sync_team().map_err(err)
}

#[tauri::command]
fn shared_path(state: tauri::State<'_, AppState>) -> CmdResult<String> {
    state.app.shared_path().map_err(err)
}

#[tauri::command]
fn set_shared_path(state: tauri::State<'_, AppState>, path: String) -> CmdResult<()> {
    state.app.set_shared_path(path).map_err(err)
}

#[tauri::command]
fn get_setting(state: tauri::State<'_, AppState>, key: String) -> CmdResult<Option<String>> {
    state.app.get_setting(key).map_err(err)
}

#[tauri::command]
fn put_setting(state: tauri::State<'_, AppState>, key: String, value: String) -> CmdResult<()> {
    state.app.put_setting(key, value).map_err(err)
}

// ---------------------------------------------------------------- signatures

#[tauri::command]
fn list_signatures(state: tauri::State<'_, AppState>) -> CmdResult<Vec<Signature>> {
    state.app.list_signatures().map_err(err)
}

#[tauri::command]
fn save_signature(state: tauri::State<'_, AppState>, signature: Signature) -> CmdResult<i64> {
    state.app.save_signature(signature).map_err(err)
}

#[tauri::command]
fn delete_signature(state: tauri::State<'_, AppState>, id: i64) -> CmdResult<()> {
    state.app.delete_signature(id).map_err(err)
}

#[tauri::command]
fn set_default_signature(state: tauri::State<'_, AppState>, id: i64) -> CmdResult<()> {
    state.app.set_default_signature(id).map_err(err)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,signet_net=debug".into()),
        )
        .init();

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_opener::init())
        .setup(|handle| {
            let data_dir = handle
                .path()
                .app_data_dir()
                .unwrap_or_else(|_| std::path::PathBuf::from("."));
            tracing::info!(dir = %data_dir.display(), "opening store");

            let app = Arc::new(App::open(data_dir)?);

            // The main window is created here rather than in tauri.conf.json so that
            // `on_new_window` can be attached. That hook is the only way a link inside a message
            // can reach the browser: the reading pane is a sandboxed iframe, WebView2 reports
            // iframe navigation through `FrameNavigationStarting` which Tauri does not surface,
            // and without this a clicked link simply loaded the website *inside* the mail reader.
            let opener_handle = handle.handle().clone();
            tauri::WebviewWindowBuilder::new(
                handle,
                "main",
                tauri::WebviewUrl::App("index.html".into()),
            )
            .title(app_name(handle.handle()))
            .inner_size(1280.0, 820.0)
            .min_inner_size(900.0, 560.0)
            // Ctrl + / Ctrl − / Ctrl 0: people who need larger text have no other way to get it
            // in a webview, and Tauri leaves it off unless asked.
            .zoom_hotkeys_enabled(true)
            .resizable(true)
            .center()
            .on_new_window(move |url, _features| {
                use tauri_plugin_opener::OpenerExt;

                // Only the schemes a message can legitimately ask for. Anything else — `file:`,
                // a custom protocol some installed application registered — is refused rather
                // than handed to Windows to work out, because the URL came from a stranger.
                let scheme = url.scheme();
                if matches!(scheme, "http" | "https" | "mailto") {
                    if let Err(e) = opener_handle
                        .opener()
                        .open_url(url.to_string(), None::<&str>)
                    {
                        tracing::warn!(error = %e, "could not open a link from a message");
                    }
                } else {
                    tracing::warn!(%scheme, "refused to open a link with an unexpected scheme");
                }

                // Never inside the application, whatever happened above.
                tauri::webview::NewWindowResponse::Deny
            })
            .build()?;

            // Fan core events out to every window, so a sync triggered anywhere refreshes
            // everywhere.
            let mut rx = app.subscribe();
            let emitter = handle.handle().clone();
            tauri::async_runtime::spawn(async move {
                while let Ok(event) = rx.recv().await {
                    let name = match &event {
                        Event::SyncStarted { .. } => "sync-started",
                        Event::SyncFinished { .. } => "sync-finished",
                        Event::SyncFailed { .. } => "sync-failed",
                        Event::SyncProgress { .. } => "sync-progress",
                        Event::MessagesChanged { .. } => "messages-changed",
                        Event::AccountsChanged => "accounts-changed",
                    };
                    let _ = emitter.emit(name, &event);
                }
            });

            handle.manage(AppState { app });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            inspect_archive,
            export_data_file,
            import_data_file,
            preview_outlook_file,
            import_outlook_file,
            create_local_archive,
            list_contacts,
            save_contact,
            delete_contact,
            export_contacts,
            open_message_window,
            attachment_preview,
            open_attachment_externally,
            schedule_send,
            scheduled_messages,
            cancel_scheduled,
            send_scheduled_now,
            flush_outbox,
            list_accounts,
            add_account,
            test_account,
            delete_account,
            sync_account,
            sync_all,
            sync_due,
            list_folders,
            list_messages,
            get_message,
            show_remote_content,
            set_read,
            set_flagged,
            move_messages,
            delete_messages,
            save_attachment,
            contacts,
            reply_draft,
            forward_draft,
            send,
            account_settings,
            change_password,
            update_account_settings,
            list_rules,
            save_rule,
            delete_rule,
            apply_rules_now,
            mark_junk,
            filing_info,
            junk_corpus,
            list_sender_rules,
            set_sender_rule,
            remove_sender_rule,
            rule_for_message,
            save_draft,
            load_draft,
            mark_folder_read,
            list_team,
            save_team_member,
            delete_team_member,
            set_me,
            triage,
            set_priority,
            assign,
            set_work_status,
            set_reminder,
            complete_reminder,
            clear_reminder,
            open_reminders,
            take_due_reminders,
            sync_team,
            shared_path,
            set_shared_path,
            get_setting,
            put_setting,
            list_signatures,
            save_signature,
            delete_signature,
            set_default_signature,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Signet");
}
