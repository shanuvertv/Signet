// Signet UI.
//
// This layer renders and dispatches. It never speaks a protocol and never composes SQL — every
// decision lives in the Rust core behind `invoke` (PLAN.md §3).

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const dialogApi = window.__TAURI__.dialog;

/// The product name this build was made with (`productName` in the Tauri config), so one codebase
/// can be packaged under more than one name. Filled in by `loadAppName` before anything is shown.
let APP_NAME = 'Signet';

async function loadAppName() {
  APP_NAME = (await window.__TAURI__.app?.getName?.().catch(() => null)) || APP_NAME;
  document.title = APP_NAME;
  const brand = document.getElementById('brand-name');
  if (brand) brand.textContent = APP_NAME;
  document.querySelector('.brand-mark')?.setAttribute('data-initial', APP_NAME[0].toUpperCase());
}
const notifyApi = window.__TAURI__.notification;

const state = {
  /// Messages the reader switched back to a white background in the dark theme.
  lightBodies: new Set(),
  /// The message whose body is on screen, so a theme change can redraw it.
  bodyMessage: null,
  accounts: [],
  folders: [],
  team: [],
  me: null,
  selectedFolderId: null,
  selectedFolderKind: null,
  selectedAccountId: null,
  /** null = a real folder; otherwise a saved view: 'mine' | 'unassigned' | 'followups' */
  view: null,
  messages: [],
  contacts: [],
  selectedMessageId: null,
  search: '',
  filters: { unread: false, flagged: false, high: false, mine: false, unassigned: false, reminder: false },
  attachments: [],
  draftMessageId: null,
  signatures: [],
  /// Which signature is currently in the compose body, so switching replaces rather than stacks.
  appliedSignatureId: null,
};

const $ = (id) => document.getElementById(id);

// ---------------------------------------------------------------- utilities

/// Escape text for insertion into HTML. Everything user- or sender-controlled goes through here.
function esc(s) {
  return String(s ?? '')
    .replaceAll('&', '&amp;')
    .replaceAll('<', '&lt;')
    .replaceAll('>', '&gt;')
    .replaceAll('"', '&quot;')
    .replaceAll("'", '&#39;');
}

function formatDate(unixSeconds) {
  if (!unixSeconds) return '';
  const d = new Date(unixSeconds * 1000);
  const now = new Date();
  if (d.toDateString() === now.toDateString())
    return d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
  if (d.getFullYear() === now.getFullYear())
    return d.toLocaleDateString([], { month: 'short', day: 'numeric' });
  return d.toLocaleDateString([], { year: '2-digit', month: 'short', day: 'numeric' });
}

/// "in 2h", "3d overdue" — a relative phrase is what you actually want on a follow-up.
function relativeDue(unixSeconds) {
  const diff = unixSeconds - Math.floor(Date.now() / 1000);
  const abs = Math.abs(diff);
  const unit =
    abs < 3600
      ? [Math.round(abs / 60), 'min']
      : abs < 86400
        ? [Math.round(abs / 3600), 'h']
        : [Math.round(abs / 86400), 'd'];
  return diff < 0 ? `${unit[0]}${unit[1]} overdue` : `in ${unit[0]}${unit[1]}`;
}

function formatSize(bytes) {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(0)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

/// A stable colour for a sender with no team member behind them.
function hashColor(seed) {
  let h = 0;
  for (const c of String(seed)) h = (h * 31 + c.charCodeAt(0)) >>> 0;
  const palette = ['#2563eb', '#dc2626', '#059669', '#d97706', '#7c3aed', '#0891b2', '#db2777', '#65a30d'];
  return palette[h % palette.length];
}

function initialsOf(text) {
  const words = String(text ?? '').trim().split(/\s+/).filter(Boolean);
  if (!words.length) return '?';
  if (words.length === 1) return words[0].slice(0, 2).toUpperCase();
  return (words[0][0] + words[1][0]).toUpperCase();
}

let statusTimer;
function status(message, isError = false) {
  const el = $('status');
  el.textContent = message;
  el.classList.toggle('error', isError);
  el.classList.add('show');
  clearTimeout(statusTimer);
  statusTimer = setTimeout(() => el.classList.remove('show'), isError ? 6000 : 2600);
}

async function call(command, args) {
  try {
    return await invoke(command, args);
  } catch (e) {
    status(String(e), true);
    throw e;
  }
}

// ---------------------------------------------------------------- theme

const THEME_KEY = 'ui.theme';

function applyTheme(theme) {
  // "system" means remove the attribute and let prefers-color-scheme decide.
  if (theme === 'light' || theme === 'dark') {
    document.documentElement.setAttribute('data-theme', theme);
  } else {
    document.documentElement.removeAttribute('data-theme');
  }
  document.querySelectorAll('#theme-seg button').forEach((b) => {
    b.classList.toggle('on', b.dataset.theme === (theme || 'system'));
  });
  rerenderForTheme();
}

/// The message body is drawn for one theme at a time, so a theme change redraws it.
function rerenderForTheme() {
  const m = state.bodyMessage;
  if (m && document.getElementById('reader-body')) renderMessage(m);
}
window.matchMedia('(prefers-color-scheme: dark)').addEventListener('change', rerenderForTheme);

async function loadTheme() {
  const saved = await call('get_setting', { key: THEME_KEY }).catch(() => null);
  applyTheme(saved || 'system');
}

// ---------------------------------------------------------------- folders

/// One icon from the sprite in index.html (Fluent UI System Icons, 20px Regular).
///
/// Emoji used to stand in for icons, and each one arrived in a different weight and style. These
/// share one stroke, follow the text colour in both themes, and are hidden from screen readers:
/// the label next to them says what the control does.
function icon(name) {
  return `<svg class="ic" aria-hidden="true"><use href="#i-${name}"></use></svg>`;
}

const FOLDER_ICONS = {
  inbox: 'inbox',
  drafts: 'drafts',
  sent: 'send',
  deleted: 'delete',
  junk: 'junk',
  archive: 'archive',
  custom: 'folder',
};

/// How a work status reads to a person. The stored values are identifiers, not labels.
const STATUS_LABELS = {
  new: 'New',
  in_progress: 'In progress',
  waiting: 'Waiting',
  done: 'Done',
};

async function loadFolders() {
  state.accounts = await call('list_accounts');
  state.folders = await call('list_folders', { accountId: null });
  state.team = await call('list_team').catch(() => []);
  state.me = state.team.find((m) => m.is_me) ?? null;
  renderFolders();
  refreshRibbonPickers();
  refreshRibbonState();

  if (state.accounts.length === 0) {
    $('reader').innerHTML = `
      <div class="empty">
        <span class="empty-icon">${icon('mail')}</span>
        <h3>No accounts yet</h3>
        <p>Add your mailbox to get started. You will need the POP3 and SMTP server names from
        your mail administrator.</p>
        <button class="btn btn-primary" id="btn-empty-add">Add account</button>
      </div>`;
    $('btn-empty-add').addEventListener('click', () => $('btn-add-account').click());
    return;
  }

  if (state.selectedFolderId === null && state.view === null) {
    const inbox = state.folders.find((f) => f.kind === 'inbox');
    if (inbox) selectFolder(inbox);
  }
}

function renderFolders() {
  const nav = $('folders');
  nav.innerHTML = '';
  if (!state.accounts.length) return;

  // Saved views come first: for a shared mailbox these are what people actually work from.
  const views = document.createElement('div');
  views.className = 'views';
  const viewDefs = [
    { key: 'mine', icon: 'person', label: state.me ? `Mine (${state.me.name})` : 'Mine' },
    { key: 'unassigned', icon: 'unassigned', label: 'Unassigned' },
    { key: 'followups', icon: 'followup', label: 'Follow-ups' },
    { key: 'scheduled', icon: 'outbox', label: 'Outbox' },
    { key: 'contacts', icon: 'contacts', label: 'Contacts' },
  ];
  for (const v of viewDefs) {
    // Buttons, so Tab reaches every folder and Enter or Space opens it.
    const el = document.createElement('button');
    el.type = 'button';
    el.className = 'folder' + (state.view === v.key ? ' active' : '');
    if (state.view === v.key) el.setAttribute('aria-current', 'page');
    el.innerHTML = `<span class="icon">${icon(v.icon)}</span><span class="name">${esc(v.label)}</span>
      <span class="count" data-count="${v.key}"></span>`;
    el.addEventListener('click', () => selectView(v.key));
    views.appendChild(el);
  }
  nav.appendChild(views);

  for (const account of state.accounts) {
    const group = document.createElement('div');
    group.className = 'account-group';
    // A disabled account keeps its downloaded mail readable but is skipped by Sync, so say so
    // rather than leaving someone wondering why it never updates.
    // A mailbox with no incoming server is an imported archive: it lives only on this computer,
    // and saying "not syncing" about it would read as a fault rather than as what it is.
    const isArchive = !account.pop_host;
    const synced = isArchive
      ? 'local archive'
      : !account.is_enabled
        ? 'not syncing'
        : account.last_sync_utc
          ? `synced ${formatDate(account.last_sync_utc)}`
          : 'never synced';
    const dim = account.is_enabled || isArchive ? '' : ' style="opacity:.55"';
    group.innerHTML = `<div class="account-name"${dim}>
        <span>${isArchive ? icon('database') + ' ' : ''}${esc(account.display_name || account.email)}</span>
        <span class="sync-time">${esc(isArchive ? '' : (account.protocol ?? ''))}${
          isArchive ? '' : ' · '
        }${esc(synced)}</span>
      </div>`;

    for (const folder of state.folders.filter((f) => f.account_id === account.id)) {
      const el = document.createElement('button');
      el.type = 'button';
      el.className = 'folder';
      if (folder.id === state.selectedFolderId && state.view === null) {
        el.classList.add('active');
        el.setAttribute('aria-current', 'page');
      }
      if (folder.unread > 0) el.classList.add('has-unread');
      el.innerHTML = `<span class="icon">${icon(FOLDER_ICONS[folder.kind] ?? 'folder')}</span>
        <span class="name">${esc(folder.name)}</span>
        ${folder.unread > 0 ? `<span class="count">${folder.unread}</span>` : ''}`;
      el.title = `${folder.total} message(s), ${folder.unread} unread`;
      el.addEventListener('click', () => selectFolder(folder));
      group.appendChild(el);
    }
    nav.appendChild(group);
  }

  refreshViewCounts();
}

/// Counts on the saved views, fetched separately because they are cross-folder.
async function refreshViewCounts() {
  const set = (key, n, urgent) => {
    const el = document.querySelector(`[data-count="${key}"]`);
    if (!el) return;
    el.textContent = n > 0 ? n : '';
    el.classList.toggle('urgent', Boolean(urgent) && n > 0);
  };

  if (state.me) {
    const mine = await call('list_messages', {
      query: { ...baseQuery(), folder_kind: 'inbox', assignee_id: state.me.id, limit: 999 },
    }).catch(() => []);
    set('mine', mine.length);
  }
  const unassigned = await call('list_messages', {
    query: { ...baseQuery(), folder_kind: 'inbox', unassigned_only: true, limit: 999 },
  }).catch(() => []);
  set('unassigned', unassigned.length);

  const reminders = await call('open_reminders').catch(() => []);
  const now = Math.floor(Date.now() / 1000);
  set('followups', reminders.length, reminders.some((r) => r.due_utc < now));

  const scheduled = await call('scheduled_messages').catch(() => []);
  set('scheduled', scheduled.length, scheduled.some((s) => s.state === 'failed'));
}

function selectFolder(folder) {
  state.view = null;
  state.selectedFolderId = folder.id;
  state.selectedFolderKind = folder.kind;
  state.selectedAccountId = folder.account_id;
  state.selectedMessageId = null;
  document.body.dataset.pane = 'list';
  renderFolders();
  loadMessages();
}

function selectView(key) {
  state.view = key;
  state.selectedFolderId = null;
  state.selectedFolderKind = null;
  state.selectedMessageId = null;
  document.body.dataset.pane = 'list';
  renderFolders();
  if (key === 'followups') renderFollowups();
  else if (key === 'scheduled') renderScheduled();
  else if (key === 'contacts') renderContacts();
  else loadMessages();
}

// ---------------------------------------------------------------- message list

function baseQuery() {
  return {
    account_id: null,
    folder_id: null,
    folder_kind: null,
    unread_only: false,
    flagged_only: false,
    assignee_id: null,
    unassigned_only: false,
    high_priority_only: false,
    has_reminder_only: false,
    search: '',
    limit: PAGE_SIZE,
    offset: 0,
  };
}

/// Rows fetched per request. The rest arrive as the list is scrolled, so a 30,000-message Inbox is
/// all reachable without building 30,000 rows up front.
const PAGE_SIZE = 200;

/// Bumped by every fresh load, so a page still in flight for the previous folder is dropped
/// instead of appended to the new one.
let listGeneration = 0;

async function loadMessages() {
  if (state.view === 'followups') return renderFollowups();
  if (state.view === 'scheduled') return renderScheduled();
  if (state.view === 'contacts') return renderContacts();

  const q = {
    ...baseQuery(),
    folder_id: state.selectedFolderId,
    folder_kind: state.view ? 'inbox' : null,
    unread_only: state.filters.unread,
    flagged_only: state.filters.flagged,
    high_priority_only: state.filters.high,
    has_reminder_only: state.filters.reminder,
    unassigned_only: state.filters.unassigned || state.view === 'unassigned',
    assignee_id: state.filters.mine || state.view === 'mine' ? (state.me?.id ?? null) : null,
    search: state.search,
  };

  if ((state.filters.mine || state.view === 'mine') && !state.me) {
    $('message-list').innerHTML =
      '<div class="empty small">Nobody on the team is marked as you yet — open Team and tick "This is me".</div>';
    $('list-count').textContent = '';
    return;
  }

  const generation = ++listGeneration;
  const page = await call('list_messages', { query: q });
  if (generation !== listGeneration) return;
  state.messages = page;
  state.listQuery = q;
  state.hasMoreMessages = page.length === PAGE_SIZE;
  renderMessages();
}

/// Fetch the next page when the list is scrolled near its end.
async function loadMoreMessages() {
  if (!state.hasMoreMessages || state.loadingMore || !state.listQuery) return;
  state.loadingMore = true;
  const generation = listGeneration;
  try {
    const q = { ...state.listQuery, offset: state.messages.length };
    const page = await call('list_messages', { query: q });
    if (generation !== listGeneration) return;
    // Mail that arrived meanwhile shifts the window by a row or two; never show one twice.
    const seen = new Set(state.messages.map((m) => m.id));
    const fresh = page.filter((m) => !seen.has(m.id));
    state.messages.push(...fresh);
    state.hasMoreMessages = page.length === PAGE_SIZE;
    const list = $('message-list');
    const now = Math.floor(Date.now() / 1000);
    for (const m of fresh) list.appendChild(messageRow(m, now));
    updateListCount();
  } finally {
    state.loadingMore = false;
  }
}

function updateListCount() {
  const n = state.messages.length;
  $('list-count').textContent = `${n}${state.hasMoreMessages ? '+' : ''} message${n === 1 ? '' : 's'}`;
}

/// Reflect selection and read state on the rows already drawn, without rebuilding the list.
///
/// Rebuilding on every click was two full re-renders per message opened, which is slow once a few
/// thousand rows are loaded and throws away the scroll position.
function refreshRowStates() {
  refreshRibbonState();
  const byId = new Map(state.messages.map((m) => [m.id, m]));
  for (const el of $('message-list').querySelectorAll('.msg')) {
    const m = byId.get(Number(el.dataset.id));
    el.classList.toggle('selected', m?.id === state.selectedMessageId);
    el.classList.toggle('unread', m ? !m.is_read : false);
  }
}

function renderMessages() {
  refreshRibbonState();
  refreshRestoreCount();
  const list = $('message-list');
  list.innerHTML = '';
  updateListCount();

  if (state.messages.length === 0) {
    list.innerHTML = `<div class="empty small">${
      state.search ? 'Nothing matches that search.' : 'Nothing here.'
    }</div>`;
    return;
  }

  const now = Math.floor(Date.now() / 1000);
  for (const m of state.messages) list.appendChild(messageRow(m, now));
}

function messageRow(m, now) {
  const el = document.createElement('div');
  el.className = 'msg';
  el.dataset.id = m.id;
  if (!m.is_read) el.classList.add('unread');
  if (m.id === state.selectedMessageId) el.classList.add('selected');
  if (m.priority === 2) el.classList.add('p-high');
  if (m.priority === 0) el.classList.add('p-low');

  // Who has it, and how far along it is, read as one thing: this is what a shared inbox is for.
  const statusLabel =
    m.work_status && m.work_status !== 'new' ? (STATUS_LABELS[m.work_status] ?? m.work_status) : '';
  const owner = m.assignee_initials
    ? `<span class="own-pill status-${esc(m.work_status ?? 'new')}"
            title="Assigned to ${esc(m.assignee_name ?? '')}${statusLabel ? ' · ' + esc(statusLabel) : ''}">
         <span class="who" style="background:${esc(m.assignee_color ?? '#6b7280')}">${esc(
           m.assignee_initials,
         )}</span>${statusLabel ? `<span class="own-status">${esc(statusLabel)}</span>` : ''}
       </span>`
    : `${
        statusLabel
          ? `<span class="pill status-${esc(m.work_status)}">${esc(statusLabel)}</span>`
          : ''
      }<span class="who unassigned" title="Nobody has picked this up">+</span>`;

  const due = m.reminder_due_utc
    ? `<span class="due ${m.reminder_due_utc < now ? 'overdue' : ''}">${icon('followup')} ${esc(
        relativeDue(m.reminder_due_utc),
      )}</span>`
    : '';

  el.innerHTML = `
    <div class="prio-bar"></div>
    <div class="from">${esc(m.from_name || m.from_addr || '(unknown sender)')}</div>
    <div class="date">${esc(formatDate(m.date_utc))}</div>
    <div class="subject">${m.priority === 2 ? '<span class="pill high">High</span> ' : ''}${esc(
      m.subject || '(no subject)',
    )}</div>
    <div class="preview">${esc(m.preview_text)}</div>
    <div class="badges">
      ${due}
      ${m.has_attachments ? `<span class="badge-ic" title="Has attachments">${icon('attach')}</span>` : ''}
      <button type="button" class="flag-toggle ${m.is_flagged ? 'flagged' : 'unflagged'}"
              tabindex="-1" aria-pressed="${m.is_flagged}"
              aria-label="${m.is_flagged ? 'Remove flag' : 'Flag this message'}"
              title="${m.is_flagged ? 'Remove flag' : 'Flag this message'} (Insert)">${icon(
                m.is_flagged ? 'flagfilled' : 'flag',
              )}</button>
      ${owner}
    </div>`;

  el.addEventListener('click', (e) => {
    // The flag is its own control, not part of the row's click target. Out of the Tab order so a
    // long list is not two hundred stops; the keyboard route is Insert, and the ribbon's Flag.
    if (e.target.closest('.flag-toggle')) {
      e.stopPropagation();
      toggleFlag(m);
      return;
    }
    openMessage(m.id);
  });
  // Double-click opens it in its own window, which is what Outlook does and what people reach
  // for when they want two messages side by side.
  el.addEventListener('dblclick', (e) => {
    e.preventDefault();
    popOutMessage(m.id);
  });
  return el;
}

async function toggleFlag(m) {
  await call('set_flagged', { ids: [m.id], flagged: !m.is_flagged });
  m.is_flagged = !m.is_flagged;
  renderMessages();
}

// ---------------------------------------------------------------- follow-ups

async function renderFollowups() {
  const reminders = await call('open_reminders');
  const list = $('message-list');
  $('list-count').textContent = `${reminders.length} follow-up${reminders.length === 1 ? '' : 's'}`;

  if (!reminders.length) {
    list.innerHTML =
      '<div class="empty small">No follow-ups. Set one from the Follow up menu on any message.</div>';
    return;
  }

  const now = Math.floor(Date.now() / 1000);
  list.innerHTML = `<div class="followups" style="padding:12px">${reminders
    .map(
      (r) => `<div class="followup ${r.due_utc < now ? 'overdue' : ''}" data-id="${r.message_id}">
        <div class="followup-main">
          <div class="followup-subject">${esc(r.subject || '(no subject)')}</div>
          <div class="muted small">${esc(r.from_addr)}${r.note ? ' · ' + esc(r.note) : ''}</div>
        </div>
        <div style="text-align:right">
          <div class="due ${r.due_utc < now ? 'overdue' : ''}">${esc(relativeDue(r.due_utc))}</div>
          <button type="button" class="btn btn-quiet small" data-done="${r.message_id}">Done</button>
        </div>
      </div>`,
    )
    .join('')}</div>`;

  list.querySelectorAll('[data-done]').forEach((b) =>
    b.addEventListener('click', async (e) => {
      e.stopPropagation();
      await call('complete_reminder', { messageId: Number(b.dataset.done) });
      status('Follow-up cleared');
      renderFollowups();
      refreshViewCounts();
    }),
  );
  list.querySelectorAll('.followup').forEach((el) =>
    el.addEventListener('click', () => openMessage(Number(el.dataset.id))),
  );
}

// ---------------------------------------------------------------- reader

async function openMessage(id) {
  state.selectedMessageId = id;
  document.body.dataset.pane = 'read';
  refreshRowStates();

  // A draft is something you continue writing, not something you read.
  if (state.selectedFolderKind === 'drafts') {
    openCompose(await call('load_draft', { id }), 'Edit draft');
    return;
  }

  const m = await call('get_message', { id });
  m.filing = await call('filing_info', { id }).catch(() => null);
  m.triage = await call('triage', { id }).catch(() => null);
  renderMessage(m);

  if (!m.is_read) {
    await call('set_read', { ids: [id], read: true });
    const summary = state.messages.find((x) => x.id === id);
    if (summary) summary.is_read = true;
    refreshRowStates();
    loadFolders();
  }
}

/// Extensions Windows will execute, or that carry macros. Flagged loudly rather than blocked:
/// a legitimate installer does occasionally arrive by mail, and the user knows which.
const RISKY = new Set([
  'exe', 'scr', 'com', 'pif', 'bat', 'cmd', 'ps1', 'psm1', 'vbs', 'vbe', 'js', 'jse', 'wsf',
  'wsh', 'msi', 'msp', 'hta', 'cpl', 'jar', 'reg', 'lnk', 'dll', 'docm', 'xlsm', 'pptm', 'dotm',
  'xlam', 'iso', 'img',
]);

const isRisky = (f) => RISKY.has(f.split('.').pop()?.toLowerCase() ?? '');

function renderMessage(m) {
  const reader = $('reader');
  const to = m.recipients.filter((r) => r.kind === 'to').map((r) => r.addr);
  const cc = m.recipients.filter((r) => r.kind === 'cc').map((r) => r.addr);
  const t = m.triage;

  const senderMismatch =
    m.from_name && m.from_name.includes('@') &&
    !m.from_name.toLowerCase().includes(m.from_addr.toLowerCase());

  const teamOptions = state.team
    .filter((x) => x.is_active)
    .map(
      (x) =>
        `<option value="${x.id}" ${t?.assignee?.id === x.id ? 'selected' : ''}>${esc(x.name)}</option>`,
    )
    .join('');

  const status = t?.work_status ?? 'new';
  const owner = t?.assignee;

  reader.innerHTML = `
    <div class="reader-head">
      <h1 class="reader-subject">${esc(m.subject || '(no subject)')}</h1>
      <div class="reader-from">
        <span class="avatar" style="background:${esc(hashColor(m.from_addr))}">${esc(
          initialsOf(m.from_name || m.from_addr),
        )}</span>
        <div class="reader-meta">
          <strong>${esc(m.from_name || m.from_addr)}</strong>
          <span>${esc(m.from_addr)}</span>
          <span>${esc(new Date(m.date_utc * 1000).toLocaleString())} · ${esc(
            formatSize(m.size_bytes),
          )}${to.length ? ' · to ' + esc(to.join(', ')) : ''}${
            cc.length ? ' · cc ' + esc(cc.join(', ')) : ''
          }</span>
        </div>
      </div>
      <!-- The three things people do to a message most. Everything else is in More, the ribbon,
           and the right-click menu, as in Outlook's own reading pane. -->
      <div class="reader-actions">
        <button class="btn btn-primary" data-act="reply">${icon('reply')} Reply</button>
        <button class="btn" data-act="reply-all">${icon('replyall')} Reply all</button>
        <button class="btn" data-act="forward">${icon('forward')} Forward</button>
        <span class="spacer"></span>
        ${
          isDarkTheme()
            ? `<button class="btn btn-quiet btn-icon" data-act="body-light"
                 aria-label="${lightBody(m) ? 'Show the message on a dark background' : 'Show the message on a light background'}"
                 title="${lightBody(m) ? 'Dark background' : 'Light background'}">${icon(
                   lightBody(m) ? 'moon' : 'sun',
                 )}</button>`
            : ''
        }
        <button class="btn btn-quiet btn-icon" data-act="more" aria-haspopup="menu"
                aria-label="More actions" title="More actions">${icon('more')}</button>
      </div>
    </div>

    <!-- Who owns this and where it stands: the reason a shared inbox exists, so it is one bar
         that reads as a sentence rather than a row of unrelated fields. -->
    <div class="own-bar status-${esc(status)}">
      <div class="own-who">
        ${
          owner
            ? `<span class="avatar avatar-sm" style="background:${esc(owner.color)}">${esc(
                owner.initials,
              )}</span>`
            : '<span class="avatar avatar-sm avatar-empty" aria-hidden="true">+</span>'
        }
        <label class="own-field">
          <span class="own-lbl">Owner</span>
          <select data-triage="assignee">
            <option value="">Unassigned</option>
            ${teamOptions}
          </select>
        </label>
        ${
          state.me && owner?.id !== state.me.id
            ? '<button class="btn btn-sm" data-act="claim">Take it</button>'
            : ''
        }
      </div>
      <label class="own-field">
        <span class="own-lbl">Status</span>
        <select data-triage="status">
          ${Object.entries(STATUS_LABELS)
            .map(
              ([value, label]) =>
                `<option value="${value}" ${status === value ? 'selected' : ''}>${label}</option>`,
            )
            .join('')}
        </select>
      </label>
      <label class="own-field">
        <span class="own-lbl">Priority</span>
        <select data-triage="priority">
          <option value="high" ${t?.priority === 'high' ? 'selected' : ''}>High</option>
          <option value="normal" ${t?.priority === 'normal' ? 'selected' : ''}>Normal</option>
          <option value="low" ${t?.priority === 'low' ? 'selected' : ''}>Low</option>
        </select>
      </label>
      <label class="own-field">
        <span class="own-lbl">Follow up</span>
        <select data-triage="due-preset">
          <option value="">${
            t?.reminder ? esc(relativeDue(t.reminder.due_utc)) : 'None'
          }</option>
          <option value="tomorrow">Tomorrow, 9:00</option>
          <option value="monday">Next Monday, 9:00</option>
          <option value="pick">Pick a date and time…</option>
          ${t?.reminder ? '<option value="clear">Clear follow-up</option>' : ''}
        </select>
      </label>
      <input type="datetime-local" data-triage="due" aria-label="Follow-up date and time" hidden />
    </div>

    ${
      t && !t.shareable
        ? `<div class="banner banner-info">
             <span>This message has no Message-ID, so its owner and follow-up stay on this
             computer and will not reach the rest of the team.</span>
           </div>`
        : ''
    }
    ${
      senderMismatch
        ? `<div class="banner banner-danger"><span>${icon('warning')} The display name contains an address that does
             not match the actual sender (<strong>${esc(m.from_addr)}</strong>). Treat with
             caution.</span></div>`
        : ''
    }
    ${
      m.filing?.reasons && !m.filing.filed_by
        ? `<div class="banner banner-danger">
             <span>${icon('warning')} <strong>Treat this message with caution.</strong> ${esc(
               m.filing.reasons,
             )}. It was delivered because you have emailed this sender before — which is also what
             a taken-over supplier account looks like. Check bank details by phone before acting on
             them.</span>
             <button class="btn" data-act="junk">Move to Junk</button>
           </div>`
        : ''
    }
    ${
      m.filing?.filed_by
        ? `<div class="banner ${m.filing.is_junk_folder ? '' : 'banner-info'}">
             <span><strong>Filed here by ${esc(m.filing.filed_by.split(':')[0])}.</strong>${
               m.filing.reasons ? ' ' + esc(m.filing.reasons) + '.' : ''
             }</span>
             ${
               m.filing.is_junk_folder
                 ? '<button class="btn" data-act="not-junk">Not junk</button>'
                 : ''
             }
           </div>`
        : ''
    }
    ${
      m.filing?.sender_rule
        ? `<div class="banner banner-info">
             <span>You have chosen to always ${
               m.filing.sender_rule === 'allow' ? 'deliver' : 'file'
             } mail from this sender.</span>
             <button class="btn" data-act="clear-sender-rule">Undo</button>
           </div>`
        : ''
    }
    ${
      m.has_remote_content
        ? `<div class="banner">
             <span>Remote images were blocked to protect your privacy.</span>
             <button class="btn" data-act="show-images">Show images</button>
           </div>`
        : ''
    }

    ${
      m.attachments.length
        ? `<div class="attachments">${m.attachments
            .map(
              (a) => `<div class="attachment" data-att="${a.id}" data-name="${esc(a.filename)}">
                ${isRisky(a.filename) ? `<span class="risky">${icon('warning')}</span>` : icon('attach')}
                ${esc(a.filename)} <span class="muted">(${esc(formatSize(a.size_bytes))})</span>
              </div>`,
            )
            .join('')}</div>`
        : ''
    }
    <div class="reader-body" id="reader-body"></div>`;

  renderBody(m);

  reader.querySelectorAll('[data-act]').forEach((btn) =>
    btn.addEventListener('click', () => {
      if (btn.dataset.act === 'more') {
        const r = btn.getBoundingClientRect();
        showContextMenu(r.right - 220, r.bottom + 4, readerMenu(m), btn);
        return;
      }
      messageAction(btn.dataset.act, m);
    }),
  );
  // Images and PDFs open in the window; everything else is a save, because opening it means
  // handing it to another program anyway.
  // Images open in the built-in viewer, which is instant and needs no temp file. PDFs go to
  // whatever program you already read PDFs in — an invoice usually wants printing, signing or
  // filing, and a read-only pane cannot do any of those.
  const viewable = m.attachments.filter((a) => viewerKind(a) === 'image');
  reader.querySelectorAll('.attachment').forEach((el) =>
    el.addEventListener('click', () => {
      const id = Number(el.dataset.att);
      const att = m.attachments.find((a) => a.id === id);
      const at = viewable.findIndex((a) => a.id === id);

      if (at >= 0) {
        openViewer(viewable, at);
      } else if (viewerKind(att) === 'pdf') {
        openAttachmentExternally(id, el.dataset.name);
      } else {
        saveAttachment(id, el.dataset.name);
      }
    }),
  );
  reader.querySelectorAll('[data-triage]').forEach((el) =>
    el.addEventListener('change', () => triageChanged(el.dataset.triage, el, m)),
  );
}

async function triageChanged(field, el, m) {
  switch (field) {
    case 'priority':
      await call('set_priority', { ids: [m.id], priority: el.value });
      status(`Priority set to ${el.selectedOptions[0]?.textContent ?? el.value}`);
      break;
    case 'assignee':
      await call('assign', { ids: [m.id], memberId: el.value ? Number(el.value) : null });
      status(el.value ? 'Assigned' : 'Unassigned');
      break;
    case 'status':
      await call('set_work_status', { ids: [m.id], status: el.value });
      status(`Marked ${STATUS_LABELS[el.value] ?? el.value}`);
      break;
    case 'due-preset': {
      const choice = el.value;
      if (choice === 'pick') {
        // A date field only when someone asks for one; the presets cover most follow-ups.
        const input = el.closest('.own-bar').querySelector('[data-triage="due"]');
        input.hidden = false;
        input.focus();
        input.showPicker?.();
        return;
      }
      if (choice === 'clear') {
        await call('clear_reminder', { messageId: m.id });
        status('Follow-up cleared');
        break;
      }
      if (!choice) return;
      const d = new Date();
      if (choice === 'tomorrow') d.setDate(d.getDate() + 1);
      else d.setDate(d.getDate() + (((8 - d.getDay()) % 7) || 7)); // next Monday
      d.setHours(9, 0, 0, 0);
      await call('set_reminder', { messageId: m.id, dueUtc: Math.floor(d.getTime() / 1000), note: '' });
      status(`Follow-up set for ${d.toLocaleString()}`);
      break;
    }
    case 'due': {
      if (!el.value) return;
      const due = Math.floor(new Date(el.value).getTime() / 1000);
      await call('set_reminder', { messageId: m.id, dueUtc: due, note: '' });
      status(`Follow-up set for ${new Date(el.value).toLocaleString()}`);
      break;
    }
  }
  await loadMessages();
  await loadFolders();
  openMessage(m.id);
}

/// Render the body.
///
/// HTML goes into a sandboxed iframe via `srcdoc`. `sandbox=""` with no tokens means no scripts,
/// no same-origin access, no form submission, no navigation — so even if the Rust sanitizer let
/// something through, it has nothing to act on. That is the second of the two layers in
/// PLAN.md §7.3; the sanitizer is the first.
/// Whether the app is dark right now, whether chosen or inherited from Windows.
function isDarkTheme() {
  const t = document.documentElement.getAttribute('data-theme');
  return t ? t === 'dark' : window.matchMedia('(prefers-color-scheme: dark)').matches;
}

/// Whether this message was switched to a light background by the person reading it.
function lightBody(m) {
  return state.lightBodies.has(m.id);
}

/// Styles that turn a message dark without touching the sender's markup.
///
/// Mail is written for white paper and sets its own colours inline, so recolouring it rule by rule
/// is hopeless. Inverting the whole page and turning the hue back round keeps every colour's
/// meaning, and pictures are inverted a second time so they come out exactly as sent. The page is
/// painted a neutral grey that inverts to near the app's own pane colour, so the message sits in
/// the pane instead of glowing inside it; a tinted grey would come out brown after the hue turn. The sun button puts any message back on white.
const DARK_BODY_CSS = `
  html { background: #ebebeb; filter: invert(1) hue-rotate(180deg); }
  body { background: transparent !important; }
  img, picture, video, svg, [style*="background-image"], [background] {
    filter: invert(1) hue-rotate(180deg);
  }`;

function renderBody(m) {
  state.bodyMessage = m;
  const body = $('reader-body');
  if (m.body_html) {
    const frame = document.createElement('iframe');
    // `allow-popups` and `allow-popups-to-escape-sandbox` are what let a clicked link ask for a new
    // window instead of replacing the message in place. Scripts, forms and same-origin access stay
    // forbidden — the two tokens added here grant a message no ability to *do* anything, only to
    // ask the application to open something, which the Rust side then vets and hands to the
    // browser. Without them, clicking a link loaded the website inside the reading pane.
    frame.setAttribute('sandbox', 'allow-popups allow-popups-to-escape-sandbox');
    frame.setAttribute('referrerpolicy', 'no-referrer');
    frame.srcdoc = `<!doctype html><html><head><meta charset="utf-8">
      <base target="_blank">
      <style>
        body { font: 14px/1.6 "Segoe UI", system-ui, sans-serif; color: #14181f;
               background: #fff; margin: 18px 22px; word-break: break-word; }
        img { max-width: 100%; height: auto; }
        table { max-width: 100%; }
        blockquote { border-left: 3px solid #dfe3e8; margin-left: 0; padding-left: 12px;
                     color: #667085; }
        ${isDarkTheme() && !lightBody(m) ? DARK_BODY_CSS : ''}
      </style></head><body>${m.body_html}</body></html>`;
    body.replaceChildren(frame);
  } else {
    const pre = document.createElement('div');
    pre.className = 'reader-text';
    pre.textContent = m.body_text || '(This message has no body.)';
    body.replaceChildren(pre);
  }
}

async function messageAction(action, m) {
  switch (action) {
    case 'reply':
    case 'reply-all': {
      const draft = await call('reply_draft', { id: m.id, replyAll: action === 'reply-all' });
      openCompose(draft, action === 'reply-all' ? 'Reply all' : 'Reply');
      break;
    }
    case 'forward':
      openCompose(await call('forward_draft', { id: m.id }), 'Forward');
      break;
    case 'claim':
      await call('assign', { ids: [m.id], memberId: state.me.id });
      status(`Assigned to you`);
      await loadMessages();
      await loadFolders();
      openMessage(m.id);
      break;
    case 'due-tomorrow': {
      const d = new Date();
      d.setDate(d.getDate() + 1);
      d.setHours(9, 0, 0, 0);
      await call('set_reminder', {
        messageId: m.id,
        dueUtc: Math.floor(d.getTime() / 1000),
        note: '',
      });
      status('Follow-up set for tomorrow 9am');
      await loadMessages();
      await loadFolders();
      openMessage(m.id);
      break;
    }
    case 'clear-reminder':
      await call('clear_reminder', { messageId: m.id });
      status('Follow-up cleared');
      await loadMessages();
      await loadFolders();
      openMessage(m.id);
      break;
    case 'flag':
      await call('set_flagged', { ids: [m.id], flagged: !m.is_flagged });
      await loadMessages();
      openMessage(m.id);
      break;
    case 'unread':
      await call('set_read', { ids: [m.id], read: false });
      await loadMessages();
      await loadFolders();
      break;
    case 'junk':
      await call('mark_junk', { ids: [m.id], isJunk: true });
      status('Moved to Junk and the filter has learned from it');
      state.selectedMessageId = null;
      $('reader').innerHTML = '<div class="empty">Select a message to read it.</div>';
      await loadMessages();
      await loadFolders();
      break;
    case 'not-junk':
      await call('mark_junk', { ids: [m.id], isJunk: false });
      status('Moved to the Inbox and the filter has learned from it');
      await loadMessages();
      await loadFolders();
      break;
    case 'delete':
      await deleteSelected([m.id]);
      break;
    case 'allow-sender':
    case 'block-sender': {
      const kind = action === 'allow-sender' ? 'allow' : 'block';
      const domain = await dialogApi.confirm(
        `Apply this to everyone at ${esc(m.from_addr.split('@')[1] ?? '')}, rather than just ` +
          `${esc(m.from_addr)}?`,
        { title: APP_NAME, kind: 'info' },
      );
      const pattern = await call('rule_for_message', {
        messageId: m.id,
        kind,
        wholeDomain: domain,
      });
      status(`Mail from ${pattern} will always be ${kind === 'allow' ? 'delivered' : 'filed as junk'}`);
      await loadMessages();
      openMessage(m.id);
      break;
    }
    case 'clear-sender-rule': {
      const addr = m.from_addr.toLowerCase();
      // The rule may be on the address or on the whole domain; clear whichever exists.
      await call('remove_sender_rule', { pattern: addr });
      await call('remove_sender_rule', { pattern: addr.split('@')[1] ?? '' });
      status('Sender rule removed');
      await loadMessages();
      openMessage(m.id);
      break;
    }
    case 'body-light': {
      if (state.lightBodies.has(m.id)) state.lightBodies.delete(m.id);
      else state.lightBodies.add(m.id);
      renderMessage(m);
      break;
    }
    case 'show-images': {
      renderBody(await call('show_remote_content', { id: m.id }));
      status('Remote images loaded for this message');
      break;
    }
  }
}

async function deleteSelected(ids) {
  if (!ids.length) return;
  const inDeleted = state.selectedFolderKind === 'deleted';
  if (inDeleted) {
    const ok = await dialogApi.confirm(
      `Permanently delete ${ids.length} message(s)? This cannot be undone.`,
      { title: APP_NAME, kind: 'warning' },
    );
    if (!ok) return;
  }
  await call('delete_messages', { ids });
  state.selectedMessageId = null;
  $('reader').innerHTML = '<div class="empty">Select a message to read it.</div>';
  await loadMessages();
  await loadFolders();
  status(inDeleted ? 'Deleted permanently' : 'Moved to Deleted Items');
}

async function saveAttachment(attachmentId, filename) {
  const dest = await dialogApi.save({ defaultPath: filename });
  if (!dest) return;
  await call('save_attachment', { attachmentId: Number(attachmentId), dest });
  status(`Saved ${filename}`);
}

// ---------------------------------------------------------------- syncing

async function syncAll() {
  const btn = $('btn-sync');
  btn.disabled = true;
  for (const a of state.accounts) {
    if (a.is_enabled) beginProgress(a.id);
  }
  // Busy state goes on a class, never on textContent: the button holds an icon span and a label
  // span, and assigning textContent replaces both of them for the rest of the session.
  btn.classList.add('is-busy');
  try {
    const outcomes = await call('sync_all');
    const total = outcomes.reduce((n, o) => n + o.new_messages, 0);
    const failed = outcomes.filter((o) => o.error);

    for (const o of outcomes) {
      finishProgress(o.account_id, { error: o.error, newMessages: o.new_messages });
    }
    // Name the account that failed. "Sync failed: connection refused" is useless once there is
    // more than one account, because it does not say which one is broken.
    const nameOf = (id) => {
      const a = state.accounts.find((x) => x.id === id);
      return a ? a.email : `account ${id}`;
    };

    // Team state travels with every sync, so nobody has to remember to press a second button.
    const team = await call('sync_team').catch(() => null);
    if (failed.length) {
      const detail = failed.map((f) => `${nameOf(f.account_id)}: ${f.error}`).join(' · ');
      const ok = total > 0 ? ` (${total} new message${total === 1 ? '' : 's'} elsewhere)` : '';
      status(`Sync failed — ${detail}${ok}`, true);
    } else if (team?.unreachable) {
      status(`Mail synced, but the shared team file is unreachable: ${team.unreachable}`, true);
    } else {
      status(total > 0 ? `${total} new message${total === 1 ? '' : 's'}` : 'No new mail');
    }
    await loadFolders();
    await loadMessages();
  } finally {
    btn.disabled = false;
    btn.classList.remove('is-busy');
  }
}

/// Fire desktop notifications for reminders that have come due.
async function checkReminders() {
  const due = await call('take_due_reminders').catch(() => []);
  for (const r of due) {
    // A follow-up you set for 3pm is no use as a toast you only see at 5pm when you next look at
    // the window, so this goes to Windows rather than to the status line alone.
    await notify('Follow-up due', r.subject || '(no subject)');
  }
  if (due.length) refreshViewCounts();
}

// ---------------------------------------------------------------- accounts

function accountFormValues() {
  const data = new FormData($('account-form'));
  const days = data.get('delete_after_days');
  return {
    account: {
      display_name: data.get('display_name')?.trim() ?? '',
      email: data.get('email')?.trim() ?? '',
      protocol: data.get('protocol') ?? 'imap',
      pop_host: data.get('pop_host')?.trim() ?? '',
      pop_port: Number(data.get('pop_port')) || 995,
      pop_security: data.get('pop_security'),
      pop_user: data.get('pop_user')?.trim() ?? '',
      smtp_host: data.get('smtp_host')?.trim() ?? '',
      smtp_port: Number(data.get('smtp_port')) || 587,
      smtp_security: data.get('smtp_security'),
      smtp_user: data.get('smtp_user')?.trim() ?? '',
      leave_on_server: data.get('leave_on_server') === 'on',
      delete_after_days: days ? Number(days) : null,
      poll_minutes: 5,
      signature: data.get('signature') ?? '',
    },
    password: data.get('password') ?? '',
  };
}

function setAccountStatus(text, kind) {
  const el = $('account-status');
  el.textContent = text;
  el.className = `account-status ${kind ?? ''}`;
}

function wireAccountDialog() {
  const dlg = $('account-dialog');
  const form = $('account-form');

  // Incoming port follows both the protocol and the security mode, because 993/143 and 995/110
  // are different pairs.
  const INCOMING_PORTS = {
    imap: { tls: 993, starttls: 143, plain: 143 },
    pop3: { tls: 995, starttls: 110, plain: 110 },
  };

  const currentProtocol = () => form.elements.protocol.value || 'imap';

  const refreshIncomingPort = () => {
    form.elements.pop_port.value =
      INCOMING_PORTS[currentProtocol()][form.elements.pop_security.value];
  };

  const applyProtocol = (protocol) => {
    form.elements.protocol.value = protocol;
    document.querySelectorAll('#protocol-seg button').forEach((b) =>
      b.classList.toggle('on', b.dataset.protocol === protocol),
    );
    // Retention is a POP3 concept: with IMAP, reading a message never removes it from the server.
    $('pop3-retention').style.display = protocol === 'pop3' ? '' : 'none';
    $('protocol-note').textContent =
      protocol === 'imap'
        ? 'Recommended for a shared mailbox. Folders, read state and flags live on the server, so the whole team sees the same thing.'
        : 'Each computer downloads its own private copy. Read state, folders and deletions do NOT sync between people.';
    refreshIncomingPort();
  };

  document.querySelectorAll('#protocol-seg button').forEach((b) =>
    b.addEventListener('click', () => applyProtocol(b.dataset.protocol)),
  );

  form.elements.pop_security.addEventListener('change', refreshIncomingPort);
  form.elements.smtp_security.addEventListener('change', (e) => {
    form.elements.smtp_port.value = { tls: 465, starttls: 587, plain: 25 }[e.target.value];
  });

  applyProtocol('imap');

  $('btn-add-account').addEventListener('click', () => {
    form.reset();
    applyProtocol('imap');
    setAccountStatus('');
    $('account-check').hidden = true;
    $('account-check').innerHTML = '';
    dlg.showModal();
  });
  $('btn-cancel-account').addEventListener('click', () => dlg.close());

  $('btn-test').addEventListener('click', () => runAccountCheck());

  // Esc must not throw away a half-filled form while a check is running.
  dlg.addEventListener('cancel', (e) => {
    if ($('btn-save-account').disabled) e.preventDefault();
  });

  $('btn-save-account').addEventListener('click', async () => {
    const { account, password } = accountFormValues();
    if (!form.reportValidity()) return;

    const save = $('btn-save-account');
    save.disabled = true;
    try {
      // An account is only added once it has been shown to work. Server names cannot be changed
      // after adding, so a broken account could only be deleted and typed in again; and a check
      // that could not even run used to count as a pass.
      const check = await runAccountCheck();
      if (!check) return; // runAccountCheck has already said why
      if (!check.incoming.ok) {
        setAccountStatus(
          'Not added: incoming mail could not connect with these settings. Correct the line marked ✕ above and try again.',
          'bad',
        );
        return;
      }
      // A blank sending server is a choice (a receive-only mailbox); a failing one is a mistake.
      if (!check.outgoing.ok && account.smtp_host) {
        setAccountStatus(
          'Not added: sending mail could not connect with these settings. Correct the line marked ✕ above, or clear the sending server to add a receive-only mailbox.',
          'bad',
        );
        return;
      }

      await invoke('add_account', { account, password });
      dlg.close();
      status('Account added');
      state.selectedFolderId = null;
      await loadFolders();
      await syncAll();
    } catch (e) {
      setAccountStatus(`Not added: ${e}`, 'bad');
    } finally {
      save.disabled = false;
    }
  });
}

/// Run both halves of the connection check and show each one.
async function runAccountCheck() {
  const form = $('account-form');
  const panel = $('account-check');
  const { account, password } = accountFormValues();

  if (!account.pop_host || !account.pop_user || !password) {
    setAccountStatus('Server, username and password are all needed to test.', 'bad');
    return null;
  }

  setAccountStatus('');
  panel.hidden = false;
  panel.innerHTML = `
    ${checkRow('busy', `Incoming mail — ${esc(account.pop_host)}:${account.pop_port}`, 'Connecting…')}
    ${checkRow('busy', `Sending mail — ${esc(account.smtp_host || 'not set')}:${account.smtp_port}`, 'Waiting…')}`;

  const testing = $('btn-test');
  testing.disabled = true;
  try {
    const check = await invoke('test_account', { account, password });
    panel.innerHTML = [check.incoming, check.outgoing]
      .map((r) => checkRow(r.ok ? 'ok' : 'bad', esc(r.label), esc(r.detail)))
      .join('');
    setAccountStatus(
      check.ok ? 'Both checks passed.' : 'Correct the settings above and test again.',
      check.ok ? 'ok' : 'bad',
    );
    return check;
  } catch (e) {
    panel.hidden = true;
    setAccountStatus(String(e), 'bad');
    return null;
  } finally {
    testing.disabled = false;
  }
}

function checkRow(kind, label, detail) {
  const mark = { ok: '✓', bad: '✕', busy: '…' }[kind];
  return `<div class="check-row ${kind}">
    <span class="check-mark">${mark}</span>
    <span><span class="check-label">${label}</span>
      <div class="check-detail">${detail}</div></span>
  </div>`;
}

// ---------------------------------------------------------------- team

async function renderTeam() {
  const team = await call('list_team');
  state.team = team;
  state.me = team.find((m) => m.is_me) ?? null;
  const list = $('team-list');

  if (!team.length) {
    list.innerHTML = '<p class="muted small">Nobody yet. Add your sales team below.</p>';
    return;
  }

  list.innerHTML = team
    .map(
      (m) => `<div class="team-item">
        <div class="team-main">
          <span class="who" style="background:${esc(m.color)}">${esc(m.initials)}</span>
          <div>
            <div class="team-name">${esc(m.name)} ${
              m.is_me ? '<span class="me-badge">me</span>' : ''
            }</div>
            <div class="muted small">${esc(m.email)}</div>
          </div>
        </div>
        <div class="rule-row">
          ${m.is_me ? '' : `<button type="button" class="btn btn-quiet" data-me="${m.id}">This is me</button>`}
          <button type="button" class="btn btn-quiet" data-rm="${m.id}">Remove</button>
        </div>
      </div>`,
    )
    .join('');

  list.querySelectorAll('[data-me]').forEach((b) =>
    b.addEventListener('click', async () => {
      await call('set_me', { id: Number(b.dataset.me) });
      status('Set as you — the "Mine" view now works');
      await renderTeam();
      await loadFolders();
    }),
  );
  list.querySelectorAll('[data-rm]').forEach((b) =>
    b.addEventListener('click', async () => {
      await call('delete_team_member', { id: Number(b.dataset.rm) });
      await renderTeam();
      await loadFolders();
    }),
  );
}

function wireTeamDialog() {
  const dlg = $('team-dialog');
  const form = $('team-form');

  $('btn-team').addEventListener('click', async () => {
    $('team-status').textContent = '';
    form.elements.shared_path.value = await call('shared_path').catch(() => '');
    await renderTeam();
    dlg.showModal();
  });

  $('btn-close-team').addEventListener('click', async () => {
    await call('set_shared_path', { path: form.elements.shared_path.value });
    dlg.close();
  });

  $('btn-add-member').addEventListener('click', async () => {
    const email = form.elements.email.value.trim();
    if (!email) {
      $('team-status').textContent = 'An email address is needed — it is how assignments travel.';
      $('team-status').className = 'account-status bad';
      return;
    }
    try {
      await invoke('save_team_member', {
        member: {
          id: 0,
          name: form.elements.name.value.trim(),
          email,
          color: '',
          initials: '',
          is_active: true,
          is_me: false,
          sort_order: 0,
        },
      });
      form.elements.name.value = '';
      form.elements.email.value = '';
      $('team-status').textContent = '';
      await renderTeam();
    } catch (e) {
      $('team-status').textContent = String(e);
      $('team-status').className = 'account-status bad';
    }
  });

  $('btn-sync-team').addEventListener('click', async () => {
    await call('set_shared_path', { path: form.elements.shared_path.value });
    const out = await call('sync_team');
    $('team-sync-status').textContent = out.unreachable
      ? `Unreachable: ${out.unreachable}`
      : `Received ${out.pulled}, published ${out.pushed}, added ${out.members_added} member(s).`;
    await renderTeam();
    await loadFolders();
    await loadMessages();
  });
}

// ---------------------------------------------------------------- settings

function describeSensitivity(threshold) {
  if (threshold >= 0.95) return 'Cautious — only very obvious junk is moved';
  if (threshold >= 0.85) return 'Balanced';
  if (threshold >= 0.7) return 'Aggressive — check the Junk folder regularly';
  return 'Very aggressive — real mail will be caught';
}

async function loadSettingsFor(accountId) {
  const s = await call('account_settings', { id: Number(accountId) });
  const form = $('settings-form');
  form.elements.display_name.value = s.display_name;
  form.elements.poll_minutes.value = s.poll_minutes;
  form.elements.leave_on_server.checked = s.leave_on_server;
  form.elements.delete_after_days.value = s.delete_after_days ?? '';
  form.elements.junk_filter_enabled.checked = s.junk_filter_enabled;
  form.elements.junk_threshold.value = s.junk_threshold;
  $('junk-threshold-label').textContent = describeSensitivity(s.junk_threshold);
  form.elements.is_enabled.checked = s.is_enabled;

  const corpus = await call('junk_corpus').catch(() => null);
  $('junk-corpus-note').textContent = corpus
    ? corpus.trained
      ? `Learned from ${corpus.spam} junk and ${corpus.ham} wanted messages.`
      : `Learning: ${corpus.spam} junk and ${corpus.ham} wanted so far. Until there are 20 of each, ` +
        'built-in rules of thumb do most of the work. Use Junk and Not junk to teach it.'
    : '';
}

function fillAccountSelect(select, selected) {
  select.innerHTML = state.accounts
    .map((a) => `<option value="${a.id}">${esc(a.email)}</option>`)
    .join('');
  if (selected) select.value = String(selected);
  return select.value;
}

function wireSettingsDialog() {
  const dlg = $('settings-dialog');
  const form = $('settings-form');

  document.querySelectorAll('#theme-seg button').forEach((b) =>
    b.addEventListener('click', async () => {
      applyTheme(b.dataset.theme);
      await call('put_setting', { key: THEME_KEY, value: b.dataset.theme });
    }),
  );

  $('btn-settings').addEventListener('click', async () => {
    if (!state.accounts.length) {
      status('Add an account first', true);
      return;
    }
    $('settings-status').textContent = '';
    await loadSettingsFor(fillAccountSelect($('settings-account'), state.selectedAccountId));
    await renderSignatureList();
    dlg.showModal();
  });

  $('settings-account').addEventListener('change', (e) => loadSettingsFor(e.target.value));

  $('btn-change-password').addEventListener('click', async () => {
    const input = $('settings-password');
    const note = $('password-status');
    const btn = $('btn-change-password');
    btn.disabled = true;
    note.textContent = 'Checking with the server…';
    try {
      await invoke('change_password', {
        id: Number($('settings-account').value),
        password: input.value,
      });
      input.value = '';
      note.textContent = 'Password saved.';
    } catch (e) {
      note.textContent = String(e);
    } finally {
      btn.disabled = false;
    }
  });
  form.elements.junk_threshold.addEventListener('input', (e) => {
    $('junk-threshold-label').textContent = describeSensitivity(Number(e.target.value));
  });
  $('btn-cancel-settings').addEventListener('click', () => dlg.close());

  $('btn-save-settings').addEventListener('click', async () => {
    const days = form.elements.delete_after_days.value;
    try {
      await invoke('update_account_settings', {
        id: Number($('settings-account').value),
        settings: {
          display_name: form.elements.display_name.value,
          poll_minutes: Number(form.elements.poll_minutes.value) || 5,
          leave_on_server: form.elements.leave_on_server.checked,
          delete_after_days: days ? Number(days) : null,
          junk_filter_enabled: form.elements.junk_filter_enabled.checked,
          junk_threshold: Number(form.elements.junk_threshold.value),
          is_enabled: form.elements.is_enabled.checked,
        },
      });
      dlg.close();
      status('Settings saved');
      await loadFolders();
    } catch (e) {
      $('settings-status').textContent = String(e);
      $('settings-status').className = 'account-status bad';
    }
  });

  $('btn-delete-account').addEventListener('click', async () => {
    const id = Number($('settings-account').value);
    const account = state.accounts.find((a) => a.id === id);
    const ok = await dialogApi.confirm(
      `Remove ${account?.email}? Every message downloaded to this computer for it will be deleted. ` +
        'Mail still on the server is not affected.',
      { title: APP_NAME, kind: 'warning' },
    );
    if (!ok) return;
    await call('delete_account', { id });
    dlg.close();
    state.selectedFolderId = null;
    status('Account removed');
    await loadFolders();
  });
}

// ---------------------------------------------------------------- rules

const RULE_ACTIONS = {
  archive: { kind: 'move_to', folder: 'archive' },
  junk: { kind: 'move_to', folder: 'junk' },
  deleted: { kind: 'delete' },
  mark_read: { kind: 'mark_read' },
  flag: { kind: 'flag' },
  never_junk: { kind: 'never_junk' },
};

function describeRule(rule) {
  const c = rule.conditions[0];
  const ops = {
    contains: 'contains',
    not_contains: 'does not contain',
    equals: 'is exactly',
    starts_with: 'starts with',
    ends_with: 'ends with',
  };
  const actions = rule.actions
    .map((a) => (a.kind === 'move_to' ? `move to ${a.folder}` : a.kind.replace(/_/g, ' ')))
    .join(', ');
  return `${c.field} ${ops[c.op] ?? c.op} "${c.value}" → ${actions}`;
}

async function renderRules(accountId) {
  const rules = await call('list_rules', { accountId: Number(accountId) });
  const list = $('rules-list');
  if (!rules.length) {
    list.innerHTML = '<p class="muted small">No rules yet.</p>';
    return;
  }
  list.innerHTML = rules
    .map(
      (r) => `<div class="rule-item">
        <label class="row">
          <input type="checkbox" data-toggle="${r.id}" ${r.enabled ? 'checked' : ''} />
          <span><strong>${esc(r.name)}</strong><br /><span class="muted small">${esc(
            describeRule(r),
          )}</span></span>
        </label>
        <button type="button" class="btn btn-quiet" data-del="${r.id}">Remove</button>
      </div>`,
    )
    .join('');

  list.querySelectorAll('[data-del]').forEach((b) =>
    b.addEventListener('click', async () => {
      await call('delete_rule', { id: Number(b.dataset.del) });
      renderRules(accountId);
    }),
  );
  list.querySelectorAll('[data-toggle]').forEach((b) =>
    b.addEventListener('change', async () => {
      const rule = rules.find((r) => r.id === Number(b.dataset.toggle));
      rule.enabled = b.checked;
      await call('save_rule', { rule });
    }),
  );
}

// ---------------------------------------------------------------- rich composer

/// The composer is a `contenteditable`, so what is edited is exactly what is sent — including the
/// quoted original, which the user can trim the way they can in Outlook.
function editor() {
  return $('compose-body');
}

/// Flatten one part of the editor to plain text.
///
/// Built from the DOM rather than by stripping tags with a regular expression, so block elements
/// become line breaks and a quoted table does not collapse into one run-on line.
function flattenToText(node) {
  const clone = node.cloneNode(true);
  clone.querySelectorAll('br').forEach((br) => br.replaceWith('\n'));
  clone
    .querySelectorAll('p, div, li, tr, blockquote, h1, h2, h3, h4')
    .forEach((el) => el.append('\n'));

  return (clone.textContent ?? '')
    // HTML layout uses runs of whitespace for spacing. Flattened literally they arrive as pages of
    // `=20` in the quoted-printable body, which is what made the text half unreadable.
    .replace(/[ \t\u00a0]{2,}/g, ' ')
    .split('\n')
    // Both ends. HTML source is indented for the benefit of whoever wrote it, and carrying that
    // indentation into a text quotation produces `>   Hi dear customer,` on every other line.
    .map((line) => line.trim())
    .join('\n')
    .replace(/\n{3,}/g, '\n\n');
}

/// A plain-text version of the composed message, for the `text/plain` alternative.
///
/// The quoted original is marked with `> `, the way every mail client has written a text quotation
/// for thirty years. Flattening the HTML quote header verbatim instead produced an unprefixed
/// `From:/Sent:/To:` block that reads like a second message rather than a quotation.
function editorPlainText() {
  const root = editor();
  const quote = root.querySelector('.signet-quote');

  if (!quote) return flattenToText(root).trim();

  // Everything the user actually wrote, which is everything above the quote.
  const written = document.createElement('div');
  for (const child of root.childNodes) {
    if (child === quote || quote.contains(child)) break;
    written.append(child.cloneNode(true));
  }

  const mine = flattenToText(written).trim();
  const quoted = flattenToText(quote)
    .trim()
    .split('\n')
    .map((line) => (line ? `> ${line}` : '>'))
    .join('\n');

  return `${mine}\n\n${quoted}`.trim();
}

function setEditorHtml(html) {
  editor().innerHTML = html ?? '';
}

/// Put the cursor where the reply is written — at the very top, above the quote.
function focusEditorStart() {
  const el = editor();
  el.focus();
  const range = document.createRange();
  // Inside the first line, not before it: a caret at the editor's edge lands in the quote below.
  range.selectNodeContents(el.firstElementChild?.classList.contains('signet-quote') ? el : el.firstElementChild ?? el);
  range.collapse(true);
  const sel = window.getSelection();
  sel.removeAllRanges();
  sel.addRange(range);
}

function wireEditorToolbar() {
  $('editor-toolbar').addEventListener('mousedown', (e) => {
    // `mousedown` rather than `click`, so pressing a button never takes the caret out of the
    // editor before the command runs.
    const button = e.target.closest('[data-fmt]');
    if (!button) return;
    e.preventDefault();

    const command = button.dataset.fmt;
    if (command === 'createLink') {
      const url = window.prompt('Link to:', 'https://');
      if (url) document.execCommand('createLink', false, url);
      return;
    }
    document.execCommand(command, false, null);
  });

  // Paste as text by default. Pasting from a browser otherwise drags in the source page's fonts,
  // colours and background, and the reply arrives looking like a web page.
  editor().addEventListener('paste', (e) => {
    if (e.clipboardData?.types.includes('text/html') && !e.shiftKey) {
      e.preventDefault();
      const text = e.clipboardData.getData('text/plain');
      document.execCommand('insertText', false, text);
    }
  });
}

// ---------------------------------------------------------------- contacts

const CONTACT_FILTERS = [
  { name: 'vCard — Outlook, phones, Google', extensions: ['vcf'] },
  { name: 'CSV — Excel', extensions: ['csv'] },
];

let selectedContactId = null;

/// The address book, in the list pane.
async function renderContacts() {
  const contacts = await call('list_contacts', { search: state.search });
  state.contacts = contacts;

  const list = $('message-list');
  $('list-count').textContent = `${contacts.length} contact${contacts.length === 1 ? '' : 's'}`;

  if (!contacts.length) {
    list.innerHTML = state.search
      ? '<div class="empty small">Nobody matches that.</div>'
      : `<div class="empty small">No contacts yet. They are collected automatically from the
         people who write to you.</div>`;
    $('reader').innerHTML = '';
    return;
  }

  list.innerHTML = contacts
    .map((c) => {
      const name = c.display_name || c.addr;
      const sub = [c.job_title, c.company].filter(Boolean).join(' · ') || c.addr;
      return `<div class="contact-row ${c.id === selectedContactId ? 'selected' : ''}"
                   data-contact="${c.id}">
        <span class="avatar" style="background:${esc(hashColor(c.addr))}">${esc(initialsOf(name))}</span>
        <span>
          <div class="contact-name">${esc(name)}</div>
          <div class="contact-sub">${esc(sub)}</div>
        </span>
        <span class="contact-seen">${c.times_seen}</span>
      </div>`;
    })
    .join('');

  list.querySelectorAll('[data-contact]').forEach((el) =>
    el.addEventListener('click', () => showContact(Number(el.dataset.contact))),
  );

  if (selectedContactId && contacts.some((c) => c.id === selectedContactId)) {
    showContact(selectedContactId);
  } else {
    $('reader').innerHTML =
      `<div class="empty"><span class="empty-icon">${icon('contacts')}</span>Select someone to see their details.</div>`;
  }
}

function showContact(id) {
  selectedContactId = id;
  const c = state.contacts.find((x) => x.id === id);
  if (!c) return;

  document.querySelectorAll('[data-contact]').forEach((el) =>
    el.classList.toggle('selected', Number(el.dataset.contact) === id),
  );

  const name = c.display_name || c.addr;
  const field = (key, label, value, type = 'input') =>
    `<label for="c-${key}">${label}</label>
     ${
       type === 'textarea'
         ? `<textarea id="c-${key}" rows="3" data-field="${key}">${esc(value)}</textarea>`
         : `<input id="c-${key}" data-field="${key}" value="${esc(value)}" />`
     }`;

  $('reader').innerHTML = `
    <div class="contact-detail">
      <div class="contact-head">
        <span class="avatar" style="background:${esc(hashColor(c.addr))}">${esc(initialsOf(name))}</span>
        <h2>${esc(name)}</h2>
      </div>

      <div class="contact-fields">
        ${field('display_name', 'Name', c.display_name)}
        <label>Email</label><div class="muted small">${esc(c.addr)}</div>
        ${field('job_title', 'Job title', c.job_title)}
        ${field('company', 'Company', c.company)}
        ${field('phone', 'Phone', c.phone)}
        ${field('mobile', 'Mobile', c.mobile)}
        ${field('website', 'Website', c.website)}
        ${field('notes', 'Notes', c.notes, 'textarea')}
      </div>

      <p class="contact-origin">${
        c.edited_utc
          ? 'You have edited this contact, so it is left exactly as you typed it.'
          : `Filled in automatically from this person's email signature, which is guesswork — check
             it before relying on it. Editing anything here stops it being changed again.`
      }</p>

      <div class="contact-actions">
        <button class="btn btn-primary" data-cact="save">Save</button>
        <button class="btn" data-cact="compose">Write to them</button>
        <button class="btn" data-cact="messages">Show their mail</button>
        <span class="spacer"></span>
        <button class="btn btn-danger" data-cact="delete">Remove</button>
      </div>
      <p class="muted small">Seen in ${c.times_seen} message${c.times_seen === 1 ? '' : 's'}.</p>
    </div>`;

  $('reader')
    .querySelectorAll('[data-cact]')
    .forEach((b) => b.addEventListener('click', () => contactAction(b.dataset.cact, c)));
}

async function contactAction(action, c) {
  switch (action) {
    case 'save': {
      const edited = { ...c };
      $('reader')
        .querySelectorAll('[data-field]')
        .forEach((el) => {
          edited[el.dataset.field] = el.value;
        });
      await call('save_contact', { contact: edited });
      status('Contact saved');
      await renderContacts();
      break;
    }
    case 'compose':
      openCompose({ to: [c.addr] });
      break;
    case 'messages':
      // Their address in the search box is the quickest honest answer: the index already covers
      // sender and recipients, so this finds both sides of the conversation.
      state.view = null;
      $('search').value = c.addr;
      state.search = c.addr;
      selectedContactId = null;
      state.selectedFolderId = null;
      renderFolders();
      await loadMessages();
      break;
    case 'delete': {
      const ok = await dialogApi.confirm(
        `Remove ${c.display_name || c.addr} from contacts? Their messages are not touched, and ` +
          'they will be collected again the next time they write.',
        { title: APP_NAME, kind: 'warning' },
      );
      if (!ok) return;
      await call('delete_contact', { id: c.id });
      selectedContactId = null;
      status('Contact removed');
      await renderContacts();
      break;
    }
  }
}

async function exportContacts() {
  const dest = await dialogApi.save({
    defaultPath: `${APP_NAME} contacts ${todayStamp()}.vcf`,
    filters: CONTACT_FILTERS,
  });
  if (!dest) return;

  const n = await call('export_contacts', { path: dest, search: state.search });
  const vcard = dest.toLowerCase().endsWith('.vcf');
  await dialogApi.message(
    `${n} contact${n === 1 ? '' : 's'} written.\n\n` +
      (vcard
        ? 'A vCard file. Outlook, a phone and Google Contacts all import it directly.'
        : 'A CSV file, ready for Excel or Outlook’s import wizard.') +
      (state.search ? `\n\nOnly those matching "${state.search}" were included.` : ''),
    { title: 'Contacts exported', kind: 'info' },
  );
  status(`Exported ${n} contacts`);
}

// ---------------------------------------------------------------- collapsing the list

const LIST_COLLAPSED_KEY = 'signet.listCollapsed';

/// Fold the message list away, or bring it back.
///
/// Remembered across restarts: someone who works with the list collapsed wants it collapsed
/// tomorrow too, and having to fold it again every morning would make the control not worth using.
function setListCollapsed(collapsed) {
  document.body.classList.toggle('list-collapsed', collapsed);

  // The ribbon button says what pressing it will do, not what the current state is.
  const label = $('toggle-list-label');
  if (label) label.innerHTML = collapsed ? 'Show<br />list' : 'Hide<br />list';

  try {
    localStorage.setItem(LIST_COLLAPSED_KEY, collapsed ? '1' : '0');
  } catch {
    // Private window, or storage switched off. The choice still applies for this session.
  }
  refreshRestoreCount();
}

function toggleListCollapsed() {
  setListCollapsed(!document.body.classList.contains('list-collapsed'));
}

/// Keep the unread count on the collapsed strip, so folding the list does not mean losing track of
/// what is waiting in it.
function refreshRestoreCount() {
  const el = $('list-restore-count');
  if (!el) return;
  const unread = state.messages.filter((m) => !m.is_read).length;
  el.textContent = unread ? `${unread} unread` : '';
}

function wireListCollapse() {
  $('btn-collapse-list').addEventListener('click', toggleListCollapsed);
  $('list-restore').addEventListener('click', toggleListCollapsed);

  let remembered = false;
  try {
    remembered = localStorage.getItem(LIST_COLLAPSED_KEY) === '1';
  } catch {
    remembered = false;
  }
  if (remembered) setListCollapsed(true);
}

// ---------------------------------------------------------------- send & receive progress

/// One row per account, keyed by account id. Kept after finishing so the panel can say what
/// happened for a moment rather than vanishing the instant the work does.
const progressRows = new Map();
let progressHideTimer = null;

function progressPanel() {
  return $('progress-panel');
}

/// Show the panel and make sure `accountId` has a row.
function beginProgress(accountId) {
  clearTimeout(progressHideTimer);
  if (!progressRows.has(accountId)) {
    progressRows.set(accountId, {
      phase: 'Starting',
      done: 0,
      total: 0,
      folder: '',
      state: '',
      bytesDone: 0,
      bytesTotal: 0,
    });
  }
  progressPanel().hidden = false;
  renderProgress();
}

function updateProgress(p) {
  const row = progressRows.get(p.account_id) ?? {
    phase: '',
    done: 0,
    total: 0,
    folder: '',
    state: '',
    bytesDone: 0,
    bytesTotal: 0,
  };
  row.phase = p.phase;
  row.done = p.done;
  row.total = p.total;
  row.folder = p.folder ?? '';
  row.bytesDone = p.bytes_done ?? 0;
  row.bytesTotal = p.bytes_total ?? 0;
  progressRows.set(p.account_id, row);

  clearTimeout(progressHideTimer);
  progressPanel().hidden = false;
  renderProgress();
}

function finishProgress(accountId, { error, newMessages } = {}) {
  const row = progressRows.get(accountId);
  if (!row) return;

  row.state = error ? 'failed' : 'done';
  row.folder = '';
  row.done = row.total; // a finished row shows a full bar, whatever it was mid-flight
  row.bytesDone = row.bytesTotal;
  row.phase = error
    ? error
    : newMessages
      ? `${newMessages} new message${newMessages === 1 ? '' : 's'}`
      : 'No new mail';
  renderProgress();
  scheduleProgressHide();
}

/// Take the panel away once everything has settled — but leave a failure up for longer, since it
/// is the case somebody actually needs to read.
function scheduleProgressHide() {
  const rows = [...progressRows.values()];
  if (rows.some((r) => !r.state)) return;

  clearTimeout(progressHideTimer);
  const anyFailed = rows.some((r) => r.state === 'failed');
  progressHideTimer = setTimeout(
    () => {
      progressPanel().hidden = true;
      progressRows.clear();
    },
    anyFailed ? 12_000 : 2_500,
  );
}

function renderProgress() {
  const rows = $('progress-rows');
  rows.innerHTML = [...progressRows.entries()]
    .map(([id, r]) => {
      // Bytes drive the bar wherever they are known, because they are what takes the time: one
      // 40 MB drawing among forty short replies makes a message count nearly meaningless.
      // An unknown total gets a sweep rather than a width — a bar cannot show a position within a
      // total nobody has taken yet, and showing one anyway is the bar that jumps to full and waits.
      const byBytes = r.bytesTotal > 0;
      const known = byBytes || r.total > 0;
      const pct = byBytes
        ? Math.min(100, Math.round((r.bytesDone / r.bytesTotal) * 100))
        : r.total > 0
          ? Math.min(100, Math.round((r.done / r.total) * 100))
          : 100;

      // "12 of 47 · 4.1 MB of 18.3 MB" — the count says how far through the queue, the size says
      // how long that is likely to take.
      const parts = [];
      if (r.total > 0 && !r.state) parts.push(`${r.done} of ${r.total}`);
      if (byBytes) {
        parts.push(
          r.state
            ? formatSize(r.bytesTotal)
            : `${formatSize(r.bytesDone)} of ${formatSize(r.bytesTotal)}`,
        );
      }
      const counts = parts.join(' · ');
      const where = r.folder ? ` · ${esc(r.folder)}` : '';

      return `<div class="progress-row ${r.state}">
        <div class="progress-line">
          <span class="progress-who">${esc(accountEmail(id))}</span>
          <span class="progress-what">${esc(counts)}</span>
        </div>
        <div class="progress-track ${known || r.state ? '' : 'unknown'}">
          <div class="progress-fill" style="width:${known || r.state ? pct : 35}%"></div>
        </div>
        <div class="progress-line">
          <span class="muted small">${esc(r.phase)}${where}</span>
        </div>
      </div>`;
    })
    .join('');
}

function wireProgress() {
  $('progress-hide').addEventListener('click', () => {
    clearTimeout(progressHideTimer);
    progressPanel().hidden = true;
    progressRows.clear();
  });
}

// ---------------------------------------------------------------- desktop notifications

/// Whether Windows has agreed to show our toasts. Asked once, then remembered.
let notifyAllowed = null;

async function ensureNotifyPermission() {
  if (notifyAllowed !== null) return notifyAllowed;
  try {
    notifyAllowed = await notifyApi.isPermissionGranted();
    if (!notifyAllowed) {
      notifyAllowed = (await notifyApi.requestPermission()) === 'granted';
    }
  } catch {
    // No notification service, or the user has them switched off system-wide. Not an error worth
    // interrupting anyone over -- the in-app status line still says everything.
    notifyAllowed = false;
  }
  return notifyAllowed;
}

/// Raise a real Windows toast, and mirror it to the status line.
///
/// Both, deliberately: the toast is what reaches you when the window is behind something else, and
/// the status line is what you can still read a moment later when the toast has gone.
async function notify(title, body) {
  status(body);
  if (!(await ensureNotifyPermission())) return;
  try {
    notifyApi.sendNotification({ title, body });
  } catch (e) {
    console.warn('notification failed', e);
  }
}

/// Announce new mail, naming who it is from when there is one obvious answer.
///
/// A count on its own ("3 new messages") makes you open the window to find out whether it matters.
/// A sender and a subject usually answers that from the toast alone.
async function announceNewMail(count) {
  if (count <= 0) return;

  const newest = state.messages.find((m) => !m.is_read) ?? null;
  const who = newest?.from_name || newest?.from_addr || '';
  const what = newest?.subject || '(no subject)';

  if (count === 1 && who) {
    await notify('New message', `${who} — ${what}`);
  } else if (who) {
    await notify(`${count} new messages`, `Latest: ${who} — ${what}`);
  } else {
    await notify(`${count} new messages`, `Open ${APP_NAME} to read them.`);
  }
}

/// Poll each account on its own schedule.
///
/// `poll_minutes` is per account and the decision about which are due is made in the core, so this
/// only has to come round often enough to notice. Once a minute is far more often than any sensible
/// interval, and costs nothing when nothing is due.
function startPolling() {
  const tick = async () => {
    const outcomes = await call('sync_due').catch(() => []);
    if (!outcomes.length) return;

    for (const o of outcomes) {
      finishProgress(o.account_id, { error: o.error, newMessages: o.new_messages });
    }

    const failed = outcomes.filter((o) => o.error);
    const total = outcomes.reduce((n, o) => n + o.new_messages, 0);

    if (total > 0) {
      await loadFolders();
      await loadMessages();
      await announceNewMail(total);
    }

    // A failing background poll is reported quietly. It is usually a laptop that has moved off the
    // office network, and a toast every minute about it would be worse than the problem.
    if (failed.length && total === 0) {
      const names = failed.map((f) => accountEmail(f.account_id)).join(', ');
      status(`Could not check ${names}`, true);
    }
  };

  tick();
  setInterval(tick, 60_000);
}

function accountEmail(id) {
  return state.accounts.find((a) => a.id === id)?.email ?? `account ${id}`;
}

// ---------------------------------------------------------------- context menu

/// Where the browser's own menu is the right answer.
///
/// In a text field, Cut/Copy/Paste are exactly what a right-click should offer, and reimplementing
/// them badly would be worse than leaving them alone. Everywhere else the WebView's menu offers
/// Back -- which navigates the webview off the application -- along with Save as, Print and Send
/// tab to your devices, none of which mean anything in a mail client.
function wantsNativeMenu(target) {
  if (!target || !target.closest) return false;
  return !!target.closest('input, textarea, [contenteditable="true"]');
}

let menuOpener = null;

function hideContextMenu() {
  const menu = $('ctx-menu');
  if (menu.hidden) return;
  const hadFocus = menu.contains(document.activeElement);
  menu.hidden = true;
  // Back to whatever opened it, so a keyboard user is not dropped at the top of the page.
  if (hadFocus) menuOpener?.focus();
  menuOpener = null;
}

/// Show `items` at a point, nudged so the menu never hangs off the screen.
///
/// Items are buttons in a `role="menu"`: Tab-free, arrow keys move, Enter or Space runs, Escape
/// closes and returns focus to `opener`.
function showContextMenu(x, y, items, opener = null) {
  const menu = $('ctx-menu');
  menuOpener = opener ?? (document.activeElement instanceof HTMLElement ? document.activeElement : null);
  menu.setAttribute('role', 'menu');
  menu.innerHTML = items
    .map((it) =>
      it === '-'
        ? '<div class="ctx-sep" role="separator"></div>'
        : `<button type="button" role="menuitem" tabindex="-1"
                   class="ctx-item ${it.danger ? 'danger' : ''}" data-ctx="${it.id}">
             <span class="ctx-icon">${it.icon ? icon(it.icon) : ''}</span>
             <span>${esc(it.label)}</span>
             ${it.key ? `<span class="ctx-key">${esc(it.key)}</span>` : ''}
           </button>`,
    )
    .join('');

  // Placed off-screen first so it can be measured, then moved: a menu opened near the bottom right
  // would otherwise open partly outside the window and be unusable.
  menu.hidden = false;
  menu.style.left = '-9999px';
  menu.style.top = '-9999px';
  const { width, height } = menu.getBoundingClientRect();
  menu.style.left = `${Math.min(x, window.innerWidth - width - 6)}px`;
  menu.style.top = `${Math.min(y, window.innerHeight - height - 6)}px`;

  menu.querySelectorAll('[data-ctx]').forEach((el) =>
    el.addEventListener('click', () => {
      const item = items.find((i) => i !== '-' && i.id === el.dataset.ctx);
      hideContextMenu();
      item?.run();
    }),
  );
  menu.querySelector('[data-ctx]')?.focus();
}

/// Arrow keys inside an open menu.
function menuKeys(e) {
  const menu = $('ctx-menu');
  if (menu.hidden || !menu.contains(document.activeElement)) return;
  const items = [...menu.querySelectorAll('[data-ctx]')];
  const at = items.indexOf(document.activeElement);
  const go = (i) => items[(i + items.length) % items.length]?.focus();
  if (e.key === 'ArrowDown') go(at + 1);
  else if (e.key === 'ArrowUp') go(at - 1);
  else if (e.key === 'Home') go(0);
  else if (e.key === 'End') go(items.length - 1);
  else if (e.key === 'Tab') hideContextMenu();
  else return;
  e.preventDefault();
}

/// The reading pane's More menu: everything the three visible buttons leave out.
function readerMenu(m) {
  return [
    {
      id: 'flag',
      icon: 'flag',
      label: m.is_flagged ? 'Remove flag' : 'Flag',
      key: 'Insert',
      run: () => messageAction('flag', m),
    },
    { id: 'unread', icon: 'unread', label: 'Mark as unread', run: () => messageAction('unread', m) },
    { id: 'popout', icon: 'popout', label: 'Open in new window', run: () => popOutMessage(m.id) },
    '-',
    m.filing?.is_junk_folder
      ? { id: 'not-junk', icon: 'check', label: 'Not junk', run: () => messageAction('not-junk', m) }
      : { id: 'junk', icon: 'junk', label: 'Move to Junk', run: () => messageAction('junk', m) },
    {
      id: 'allow',
      icon: 'shield',
      label: 'Always allow this sender',
      run: () => messageAction('allow-sender', m),
    },
    {
      id: 'block',
      icon: 'dismiss',
      label: 'Block this sender',
      run: () => messageAction('block-sender', m),
    },
    '-',
    {
      id: 'delete',
      icon: 'delete',
      label: 'Delete',
      key: 'Del',
      danger: true,
      run: () => messageAction('delete', m),
    },
  ];
}

/// The menu for one message in the list.
function messageMenu(m) {
  return [
    { id: 'open', icon: 'folderopen', label: 'Open', run: () => openMessage(m.id) },
    {
      id: 'popout',
      icon: 'popout',
      label: 'Open in new window',
      run: () => popOutMessage(m.id),
    },
    '-',
    { id: 'reply', icon: 'reply', label: 'Reply', key: 'Ctrl+R', run: () => ribbonCommand('reply') },
    {
      id: 'reply-all',
      icon: 'replyall',
      label: 'Reply all',
      key: 'Ctrl+Shift+R',
      run: () => ribbonCommand('reply-all'),
    },
    { id: 'forward', icon: 'forward', label: 'Forward', run: () => ribbonCommand('forward') },
    '-',
    {
      id: 'read',
      icon: m.is_read ? 'unread' : 'readunread',
      label: m.is_read ? 'Mark as unread' : 'Mark as read',
      run: async () => {
        await call('set_read', { ids: [m.id], read: !m.is_read });
        await loadMessages();
        refreshViewCounts();
      },
    },
    {
      id: 'flag',
      icon: 'flag',
      label: m.is_flagged ? 'Remove flag' : 'Flag',
      run: () => toggleFlag(m),
    },
    '-',
    { id: 'junk', icon: 'junk', label: 'Move to Junk', run: () => ribbonCommand('junk') },
    {
      id: 'delete',
      icon: 'delete',
      label: 'Delete',
      key: 'Del',
      danger: true,
      run: () => deleteSelected([m.id]),
    },
  ];
}

function wireContextMenu() {
  document.addEventListener('contextmenu', (e) => {
    if (wantsNativeMenu(e.target)) return;

    // Everything else gets ours, or nothing at all -- never the WebView's.
    e.preventDefault();

    const row = e.target.closest?.('.msg');
    if (row) {
      const id = Number(row.dataset.id);
      const m = state.messages.find((x) => x.id === id);
      if (m) {
        // Right-clicking a message selects it too, so the menu and the reading pane agree about
        // which message is being acted on.
        if (state.selectedMessageId !== id) openMessage(id);
        showContextMenu(e.clientX, e.clientY, messageMenu(m));
        return;
      }
    }

    const att = e.target.closest?.('.attachment');
    if (att) {
      const id = Number(att.dataset.att);
      const name = att.dataset.name;
      showContextMenu(e.clientX, e.clientY, [
        {
          id: 'open-att',
          icon: 'folderopen',
          label: 'Open',
          run: () => att.click(),
        },
        {
          id: 'open-att-ext',
          icon: 'popout',
          label: 'Open in default program',
          run: () => openAttachmentExternally(id, name),
        },
        { id: 'save-att', icon: 'save', label: 'Save as…', run: () => saveAttachment(id, name) },
      ]);
      return;
    }

    hideContextMenu();
  });

  // Anything that moves the page out from under the menu closes it.
  for (const ev of ['click', 'scroll', 'resize']) {
    window.addEventListener(ev, hideContextMenu, true);
  }
  // The window losing focus, not an element: focus moving into the menu is how it is used.
  window.addEventListener('blur', (e) => {
    if (e.target === window) hideContextMenu();
  });
  document.addEventListener('keydown', (e) => {
    if (e.key === 'Escape') hideContextMenu();
    menuKeys(e);
  });
}

// ---------------------------------------------------------------- scheduled send

/// Messages built and waiting for their moment.
async function renderScheduled() {
  const items = await call('scheduled_messages');
  const list = $('message-list');
  $('list-count').textContent = `${items.length} in Outbox`;

  if (!items.length) {
    list.innerHTML =
      '<div class="empty small">Outbox is empty. Everything has been sent.</div>';
    return;
  }

  const now = Math.floor(Date.now() / 1000);
  list.innerHTML = `<div style="padding:12px">${items
    .map((s) => {
      // 0 is a message sent normally that has not gone yet: it is retrying, not scheduled.
      const when =
        s.state === 'sending'
          ? 'Sending…'
          : s.state === 'failed'
            ? 'Not sent'
            : s.scheduled_utc
              ? new Date(s.scheduled_utc * 1000).toLocaleString()
              : 'Waiting to retry';
      const late = s.state === 'failed' || (s.scheduled_utc > 0 && s.scheduled_utc < now);
      return `<div class="followup ${s.state === 'failed' ? 'overdue' : ''}">
        <div class="followup-main">
          <div class="followup-subject">${esc(s.subject || '(no subject)')}</div>
          <div class="muted small">to ${esc(s.to_addrs)}</div>
          ${
            s.state === 'failed'
              ? `<div class="muted small" style="color:var(--danger)">${esc(
                  s.last_error ?? 'could not be sent',
                )}</div>`
              : ''
          }
        </div>
        <div style="text-align:right">
          <div class="due ${late ? 'overdue' : ''}">${esc(when)}</div>
          ${
            s.state === 'sending'
              ? ''
              : `<button type="button" class="btn btn-quiet small" data-send-now="${s.id}">${
                  s.state === 'failed' ? 'Retry' : 'Send now'
                }</button>
          <button type="button" class="btn btn-quiet small" data-cancel-sched="${s.id}">Cancel</button>`
          }
        </div>
      </div>`;
    })
    .join('')}</div>`;

  list.querySelectorAll('[data-send-now]').forEach((b) =>
    b.addEventListener('click', async () => {
      b.disabled = true;
      const sent = await call('send_scheduled_now', { id: Number(b.dataset.sendNow) });
      status(sent ? 'Sent' : 'Still not sent — see the reason in the Outbox', !sent);
      renderScheduled();
      refreshViewCounts();
    }),
  );
  list.querySelectorAll('[data-cancel-sched]').forEach((b) =>
    b.addEventListener('click', async () => {
      const ok = await dialogApi.confirm(
        'Cancel this scheduled message? It will not be sent, and the text is not put back into a ' +
          'compose window.',
        { title: APP_NAME, kind: 'warning' },
      );
      if (!ok) return;
      await call('cancel_scheduled', { id: Number(b.dataset.cancelSched) });
      status('Message removed from Outbox');
      renderScheduled();
      refreshViewCounts();
    }),
  );
}

/// Round to the next whole minute: seconds in a "send at 9am" are noise.
function atTime(daysAhead, hour) {
  const d = new Date();
  d.setDate(d.getDate() + daysAhead);
  d.setHours(hour, 0, 0, 0);
  return d;
}

function nextMonday9() {
  const d = new Date();
  // 0 is Sunday. Always at least one day ahead, so "Monday" on a Monday means next Monday.
  const ahead = ((8 - d.getDay()) % 7) || 7;
  return atTime(ahead, 9);
}

function toLocalInput(date) {
  const pad = (n) => String(n).padStart(2, '0');
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}T${pad(
    date.getHours(),
  )}:${pad(date.getMinutes())}`;
}

function wireSendLater() {
  const dlg = $('later-dialog');

  $('btn-send-later').addEventListener('click', () => {
    $('later-status').textContent = '';
    $('later-when').value = toLocalInput(atTime(1, 9));
    dlg.showModal();
  });
  $('btn-cancel-later').addEventListener('click', () => dlg.close());

  dlg.querySelectorAll('[data-later]').forEach((b) =>
    b.addEventListener('click', () => {
      const when = {
        'tomorrow-9': () => atTime(1, 9),
        'monday-9': nextMonday9,
        hour: () => new Date(Date.now() + 3600_000),
      }[b.dataset.later]();
      $('later-when').value = toLocalInput(when);
    }),
  );

  $('btn-confirm-later').addEventListener('click', async () => {
    const value = $('later-when').value;
    if (!value) {
      $('later-status').textContent = 'Pick a time first.';
      $('later-status').className = 'account-status bad';
      return;
    }
    const whenUtc = Math.floor(new Date(value).getTime() / 1000);

    try {
      await invoke('schedule_send', { draft: currentDraft(), whenUtc });
    } catch (e) {
      $('later-status').textContent = String(e?.message ?? e);
      $('later-status').className = 'account-status bad';
      return;
    }

    dlg.close();
    $('compose-dialog').close();
    status(`Scheduled for ${new Date(value).toLocaleString()}`);
    await loadFolders();
  });
}

// ---------------------------------------------------------------- attachment viewer

/// What can be shown in the window rather than handed to another program.
function viewerKind(att) {
  const mime = (att.mime_type || '').toLowerCase();
  const name = (att.filename || '').toLowerCase();
  if (mime.startsWith('image/') || /\.(png|jpe?g|gif|webp|bmp|avif)$/.test(name)) {
    // SVG is deliberately excluded: it is a document that can carry script, and the viewer is not
    // the place to be rendering untrusted markup from a stranger's email.
    return mime === 'image/svg+xml' || name.endsWith('.svg') ? null : 'image';
  }
  if (mime === 'application/pdf' || name.endsWith('.pdf')) return 'pdf';
  return null;
}

const viewer = {
  items: [],
  index: 0,
  url: null,
};

function revokeViewerUrl() {
  if (viewer.url) {
    URL.revokeObjectURL(viewer.url);
    viewer.url = null;
  }
}

function closeViewer() {
  revokeViewerUrl();
  $('viewer-body').innerHTML = '';
  $('viewer').hidden = true;
}

async function openViewer(items, index) {
  viewer.items = items;
  viewer.index = index;
  $('viewer').hidden = false;
  await showViewerItem();
}

async function showViewerItem() {
  const att = viewer.items[viewer.index];
  const body = $('viewer-body');

  revokeViewerUrl();
  body.className = 'viewer-body';
  body.innerHTML = '<div class="viewer-message">Opening…</div>';
  $('viewer-name').textContent = att.filename;
  $('viewer-count').textContent =
    viewer.items.length > 1 ? `${viewer.index + 1} of ${viewer.items.length}` : '';
  $('viewer-prev').disabled = viewer.index === 0;
  $('viewer-next').disabled = viewer.index >= viewer.items.length - 1;

  let data;
  try {
    data = await invoke('attachment_preview', { attachmentId: att.id });
  } catch (e) {
    // Too large, or gone from disk. Saying so beats an empty frame.
    body.innerHTML = `<div class="viewer-message">${esc(String(e?.message ?? e))}</div>`;
    $('viewer-zoom').hidden = true;
    return;
  }

  // Through a blob rather than a data: URL. A 20 MB PDF as a data: URL is a 27 MB string sitting
  // in an attribute, and WebView2 will not navigate a frame to one that size.
  //
  // `data.mime_type` is what the core read out of the file's own leading bytes, never what the
  // sender declared. That distinction is the whole defence here: this blob is same-origin, so a
  // file that claimed to be a PDF and was really HTML would otherwise be rendered as a document
  // inside this application.
  const bytes = Uint8Array.from(atob(data.base64), (c) => c.charCodeAt(0));
  viewer.url = URL.createObjectURL(new Blob([bytes], { type: data.mime_type }));

  // The kind follows the sniffed type too, so the element and the content always agree.
  const kind = data.mime_type === 'application/pdf' ? 'pdf' : 'image';
  $('viewer-zoom').hidden = kind !== 'image';
  $('viewer-zoom').textContent = 'Full size';
  // No `sandbox` on the frame, deliberately. Edge's built-in PDF viewer cannot start inside a
  // sandboxed frame and shows its own "this page has been blocked" instead of the document.
  //
  // It is safe to leave off because the check that matters happened before this point: the core
  // read the file's leading bytes and refused to return anything whose content was not genuinely
  // an image or a PDF. Sandboxing was defending against HTML wearing a `.pdf` name, and that can
  // no longer reach here at all.
  body.innerHTML =
    kind === 'image'
      ? `<img src="${viewer.url}" alt="${esc(att.filename)}" />`
      : `<iframe src="${viewer.url}" title="${esc(att.filename)}"></iframe>`;
}

function wireViewer() {
  $('viewer-close').addEventListener('click', closeViewer);

  $('viewer-prev').addEventListener('click', async () => {
    if (viewer.index > 0) {
      viewer.index -= 1;
      await showViewerItem();
    }
  });
  $('viewer-next').addEventListener('click', async () => {
    if (viewer.index < viewer.items.length - 1) {
      viewer.index += 1;
      await showViewerItem();
    }
  });

  $('viewer-zoom').addEventListener('click', () => {
    const body = $('viewer-body');
    const actual = body.classList.toggle('actual');
    $('viewer-zoom').textContent = actual ? 'Fit to window' : 'Full size';
  });

  $('viewer-save').addEventListener('click', async () => {
    const att = viewer.items[viewer.index];
    await saveAttachment(att.id, att.filename);
  });

  // The viewer is a layer over everything, so it takes the keyboard while it is open.
  document.addEventListener(
    'keydown',
    (e) => {
      if ($('viewer').hidden) return;
      if (e.key === 'Escape') {
        e.stopPropagation();
        closeViewer();
      } else if (e.key === 'ArrowLeft') {
        e.stopPropagation();
        $('viewer-prev').click();
      } else if (e.key === 'ArrowRight') {
        e.stopPropagation();
        $('viewer-next').click();
      }
    },
    true,
  );
}

/// Hand an attachment to the program Windows uses for it.
async function openAttachmentExternally(id, name) {
  status(`Opening ${name}…`);
  try {
    await invoke('open_attachment_externally', { attachmentId: Number(id) });
  } catch (e) {
    // The core refuses anything that is not provably a PDF or an image, and says why.
    await dialogApi.message(String(e?.message ?? e), {
      title: 'Cannot open that here',
      kind: 'warning',
    });
  }
}

// ---------------------------------------------------------------- pop-out windows

/// Open the current message in a window of its own, the way double-clicking does in Outlook.
async function popOutMessage(id) {
  if (!id) return;
  await call('open_message_window', { id });
}

/// This window was opened to show one message: strip it back to just that.
async function enterPopoutMode(id) {
  document.body.classList.add('popout');
  document.body.dataset.pane = 'read';
  await openMessage(id);
  const subject = document.querySelector('.reader-subject')?.textContent?.trim();
  if (subject) document.title = subject;
}

// ---------------------------------------------------------------- data file

const ARCHIVE_FILTER = [{ name: 'Data file', extensions: ['signet'] }];
/// Opening also offers every file, so a backup made before the extension changed still shows up.
const ARCHIVE_OPEN_FILTER = [...ARCHIVE_FILTER, { name: 'All files', extensions: ['*'] }];

function todayStamp() {
  const d = new Date();
  const pad = (n) => String(n).padStart(2, '0');
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
}

function wireDataFile() {
  $('btn-export').addEventListener('click', async () => {
    const dest = await dialogApi.save({
      defaultPath: `${APP_NAME} ${todayStamp()}.signet`,
      filters: ARCHIVE_FILTER,
    });
    if (!dest) return;

    status('Exporting — this can take a while for a large mailbox…');
    const out = await call('export_data_file', { path: dest });
    await dialogApi.message(
      `Saved ${out.messages} message${out.messages === 1 ? '' : 's'} from ${out.accounts} ` +
        `account${out.accounts === 1 ? '' : 's'} (${formatSize(out.bytes)}).\n\n` +
        'Passwords are not included, so this file is safe to copy to another machine — but the ' +
        'account will ask for its password when it gets there.',
      { title: 'Export complete', kind: 'info' },
    );
    status(`Exported to ${out.path}`);
  });

  $('btn-import').addEventListener('click', async () => {
    const picked = await dialogApi.open({ multiple: false, filters: ARCHIVE_OPEN_FILTER });
    if (!picked) return;
    const path = typeof picked === 'string' ? picked : picked.path;

    // Read the manifest first, so the choice to merge is made with the contents in view rather
    // than on the strength of a file name.
    const manifest = await call('inspect_archive', { path });
    const listed = manifest.accounts
      .map((a) => `  • ${a.email} — ${a.messages} message${a.messages === 1 ? '' : 's'}`)
      .join('\n');
    const ok = await dialogApi.confirm(
      `This file holds ${manifest.messages} message${manifest.messages === 1 ? '' : 's'}:\n\n` +
        `${listed}\n\n` +
        'Importing adds what is missing and leaves everything you already have alone. Mail ' +
        'already here is skipped, and nothing is deleted.',
      { title: 'Import data file', kind: 'info' },
    );
    if (!ok) return;

    status('Importing…');
    const out = await call('import_data_file', { path });

    const lines = [
      `${out.messages_imported} message${out.messages_imported === 1 ? '' : 's'} imported.`,
      out.messages_skipped ? `${out.messages_skipped} already here and skipped.` : '',
      out.accounts_added ? `${out.accounts_added} account(s) added.` : '',
      out.signatures ? `${out.signatures} signature(s) added.` : '',
      out.sender_rules ? `${out.sender_rules} sender rule(s) added.` : '',
      out.needs_password.length
        ? `\nEnter the password for ${out.needs_password.join(', ')} in Settings before it can ` +
          'collect mail — passwords are never written into an archive.'
        : '',
    ].filter(Boolean);

    await dialogApi.message(lines.join('\n'), { title: 'Import complete', kind: 'info' });
    await loadFolders();
    status('Import complete');
  });
}

const PST_FILTER = [{ name: 'Outlook data file', extensions: ['pst'] }];

function wireOutlookImport() {
  const dlg = $('pst-dialog');
  let pending = null;

  $('btn-import-pst').addEventListener('click', async () => {
    if (!state.accounts.length) {
      status('Add an account first — imported mail has to belong to one', true);
      return;
    }

    const picked = await dialogApi.open({ multiple: false, filters: PST_FILTER });
    if (!picked) return;
    const path = typeof picked === 'string' ? picked : picked.path;

    let preview;
    try {
      preview = await invoke('preview_outlook_file', { path });
    } catch (e) {
      // Outlook holding the file open, an .ost, or something that is not a PST at all. The core's
      // message says which of those it is and what to do about it.
      await dialogApi.message(String(e?.message ?? e), {
        title: 'Cannot read that file',
        kind: 'error',
      });
      return;
    }

    if (!preview.messages) {
      await dialogApi.message('That Outlook file holds no messages.', {
        title: 'Nothing to import',
        kind: 'info',
      });
      return;
    }

    pending = path;
    // Default the archive's name to whatever Outlook called the store, which is usually the
    // address it belonged to and exactly what someone would type anyway.
    $('pst-archive-name').value = preview.display_name || 'Outlook archive';
    $('pst-summary').textContent =
      `${preview.display_name} — ${preview.messages} message${
        preview.messages === 1 ? '' : 's'
      } in ${preview.folders.length} folder${preview.folders.length === 1 ? '' : 's'}.`;

    $('pst-folders').innerHTML = preview.folders
      .slice()
      .sort((a, b) => b.messages - a.messages)
      .map(
        (f) => `<div class="rule-item"><span>${esc(f.path)}</span>
          <span class="muted small">${f.messages}</span></div>`,
      )
      .join('');

    // Which mailbox the archive belongs to is not something the file records, so it is asked
    // rather than guessed — filing an archive under the wrong account is tedious to undo.
    fillAccountSelect($('pst-account'), state.selectedAccountId);
    $('pst-status').textContent = '';
    dlg.showModal();
  });

  $('btn-cancel-pst').addEventListener('click', () => dlg.close());

  $('btn-run-pst').addEventListener('click', async () => {
    if (!pending) return;
    const run = $('btn-run-pst');
    const intoNew =
      document.querySelector('input[name="pst_target"]:checked')?.value === 'new';

    run.disabled = true;
    $('pst-status').className = 'account-status';
    $('pst-status').textContent = 'Importing — this can take a while for a large file…';

    try {
      const accountId = intoNew
        ? await invoke('create_local_archive', { name: $('pst-archive-name').value })
        : Number($('pst-account').value);

      // Reading the PST is most of the wait on a large one and cannot be counted until the tree
      // has been walked, so the panel opens on a sweep and gets real numbers when they exist.
      beginProgress(accountId);
      dlg.close();

      const out = await invoke('import_outlook_file', { path: pending, accountId });
      finishProgress(accountId, { newMessages: out.messages_imported });

      // An import that produced nothing must not leave a mailbox behind that looks like it worked.
      if (intoNew && out.messages_imported === 0) {
        await call('delete_account', { id: accountId });
        await loadFolders();
        await dialogApi.message(
          `Nothing was imported, so no mailbox was created.\n\n` +
            `${out.failed} message(s) could not be read.` +
            (out.first_error ? `\n\nThe first problem was: ${out.first_error}` : '') +
            '\n\nIf the file opens in Outlook, close Outlook completely and try again — it keeps ' +
            'its data files locked while it is running.',
          { title: 'Import found nothing', kind: 'error' },
        );
        return;
      }

      const lines = [
        `${out.messages_imported} message(s) imported from ${out.folders} folder(s).`,
        out.messages_skipped
          ? `${out.messages_skipped} were already here and were skipped.`
          : '',
        out.failed
          ? `${out.failed} were not mail, or could not be read, and were left behind.` +
            (out.first_error ? ` The first was: ${out.first_error}` : '')
          : '',
      ].filter(Boolean);

      await dialogApi.message(lines.join('\n'), { title: 'Import complete', kind: 'info' });
      await loadFolders();
      status('Outlook import complete');
    } catch (e) {
      for (const id of progressRows.keys()) {
        finishProgress(id, { error: 'Import failed' });
      }
      $('pst-status').textContent = String(e?.message ?? e);
      $('pst-status').className = 'account-status bad';
      dlg.showModal();
    } finally {
      run.disabled = false;
    }
  });
}

// ---------------------------------------------------------------- junk options

async function renderSenderRules() {
  const rules = await call('list_sender_rules');
  const groups = { block: $('junk-blocked'), allow: $('junk-allowed') };
  const empty = {
    block: 'Nothing is blocked. Use <strong>Block sender</strong> on a message, or add one above.',
    allow: 'Nothing is on the safe list yet.',
  };

  for (const kind of ['block', 'allow']) {
    const mine = rules.filter((r) => r.kind === kind);
    groups[kind].innerHTML = mine.length
      ? mine
          .map(
            (r) => `<div class="rule-item">
              <span>${esc(r.pattern)}${
                r.pattern.includes('@') ? '' : ' <span class="muted small">(whole domain)</span>'
              }</span>
              <button type="button" class="btn btn-quiet" data-drop="${esc(r.pattern)}">Remove</button>
            </div>`,
          )
          .join('')
      : `<p class="muted small">${empty[kind]}</p>`;

    groups[kind].querySelectorAll('[data-drop]').forEach((b) =>
      b.addEventListener('click', async () => {
        await call('remove_sender_rule', { pattern: b.dataset.drop });
        await renderSenderRules();
        await loadMessages();
      }),
    );
  }
}

function wireJunkDialog() {
  const dlg = $('junk-dialog');
  const form = $('junk-form');
  const note = $('junk-status');

  $('btn-junk-options').addEventListener('click', async () => {
    note.textContent = '';
    form.elements.pattern.value = '';
    await renderSenderRules();
    dlg.showModal();
  });
  $('btn-close-junk').addEventListener('click', () => dlg.close());

  const add = async (kind) => {
    const pattern = form.elements.pattern.value.trim();
    try {
      await invoke('set_sender_rule', { pattern, kind });
    } catch (e) {
      note.textContent = String(e?.message ?? e);
      note.className = 'account-status bad';
      return;
    }
    form.elements.pattern.value = '';
    note.textContent = `Mail from ${pattern} will always be ${
      kind === 'allow' ? 'delivered' : 'filed as junk'
    }.`;
    note.className = 'account-status ok';
    await renderSenderRules();
    await loadMessages();
  };

  $('btn-junk-allow').addEventListener('click', () => add('allow'));
  $('btn-junk-block').addEventListener('click', () => add('block'));
}

function wireRulesDialog() {
  const dlg = $('rules-dialog');
  const form = $('rules-form');

  $('btn-rules').addEventListener('click', async () => {
    if (!state.accounts.length) {
      status('Add an account first', true);
      return;
    }
    $('rules-status').textContent = '';
    await renderRules(fillAccountSelect($('rules-account'), state.selectedAccountId));
    dlg.showModal();
  });

  $('rules-account').addEventListener('change', (e) => renderRules(e.target.value));
  $('btn-close-rules').addEventListener('click', () => dlg.close());

  $('btn-add-rule').addEventListener('click', async () => {
    const accountId = Number($('rules-account').value);
    const value = form.elements.value.value.trim();
    if (!value) {
      $('rules-status').textContent = 'Enter some text for the rule to match.';
      $('rules-status').className = 'account-status bad';
      return;
    }
    try {
      await invoke('save_rule', {
        rule: {
          id: 0,
          account_id: accountId,
          name: form.elements.name.value.trim() || `Rule for "${value}"`,
          priority: 0,
          enabled: true,
          match_all: true,
          conditions: [{ field: form.elements.field.value, op: form.elements.op.value, value }],
          actions: [RULE_ACTIONS[form.elements.action.value]],
        },
      });
      form.elements.name.value = '';
      form.elements.value.value = '';
      $('rules-status').textContent = '';
      await renderRules(accountId);
    } catch (e) {
      $('rules-status').textContent = String(e);
      $('rules-status').className = 'account-status bad';
    }
  });

  $('btn-apply-rules').addEventListener('click', async () => {
    const accountId = Number($('rules-account').value);
    const moved = await call('apply_rules_now', { accountId });
    $('rules-status').textContent = `Moved ${moved} existing message(s).`;
    $('rules-status').className = 'account-status ok';
    await loadFolders();
    await loadMessages();
  });
}

// ---------------------------------------------------------------- signatures

/// The separator RFC 3676 defines. Everything from this line down is the signature, which is how
/// a switch can replace the old one instead of piling them up.
const SIG_SEPARATOR = '\n-- \n';

function bodyWithoutSignature(body) {
  const at = body.lastIndexOf(SIG_SEPARATOR);
  return at === -1 ? body : body.slice(0, at);
}

/// Whether the composer already carries a signature block.
function editorHasSignature() {
  return !!editor().querySelector('.signet-signature');
}

/// Put `signature` at the bottom of the compose body, replacing whatever was there before.
function applySignatureToBody(signature) {
  // Marked with a class rather than a text separator, because the body is HTML now and searching
  // for `-- ` in markup would match inside the quoted message.
  const el = editor();
  el.querySelector('.signet-signature')?.remove();
  if (!signature) return;

  const block = document.createElement('div');
  block.className = 'signet-signature';
  block.innerHTML = `<br>--<br>${esc(signature).replaceAll('\n', '<br>')}`;

  // Above the quoted original, which is where a signature belongs in a reply.
  const quote = el.querySelector('.signet-quote');
  if (quote) el.insertBefore(block, quote);
  else el.append(block);
}

async function loadSignatures() {
  state.signatures = await call('list_signatures').catch(() => []);
  return state.signatures;
}

function fillSignaturePicker(selectedId) {
  const sel = $('compose-signature');
  sel.innerHTML =
    '<option value="">None</option>' +
    state.signatures
      .map(
        (g) =>
          `<option value="${g.id}" ${g.id === selectedId ? 'selected' : ''}>${esc(g.name)}${
            g.is_default ? ' (default)' : ''
          }</option>`,
      )
      .join('');
}

async function renderSignatureList() {
  await loadSignatures();
  const list = $('signature-list');
  if (!state.signatures.length) {
    list.innerHTML = '<p class="muted small">No signatures yet. Add one below.</p>';
    return;
  }
  list.innerHTML = state.signatures
    .map(
      (g) => `<div class="sig-item">
        <div style="min-width:0;flex:1">
          <div class="team-name">${esc(g.name)} ${
            g.is_default ? '<span class="me-badge">default</span>' : ''
          }</div>
          <div class="sig-body">${esc(g.body)}</div>
        </div>
        <div class="rule-row">
          <button type="button" class="btn btn-quiet" data-sig-edit="${g.id}">Edit</button>
          ${
            g.is_default
              ? ''
              : `<button type="button" class="btn btn-quiet" data-sig-default="${g.id}">Make default</button>`
          }
          <button type="button" class="btn btn-quiet" data-sig-del="${g.id}">Remove</button>
        </div>
      </div>`,
    )
    .join('');

  list.querySelectorAll('[data-sig-edit]').forEach((b) =>
    b.addEventListener('click', () => {
      const g = state.signatures.find((x) => x.id === Number(b.dataset.sigEdit));
      $('sig-name').value = g.name;
      $('sig-body').value = g.body;
      $('sig-status').textContent = `editing "${g.name}"`;
      $('btn-save-signature').dataset.editing = String(g.id);
    }),
  );
  list.querySelectorAll('[data-sig-default]').forEach((b) =>
    b.addEventListener('click', async () => {
      await call('set_default_signature', { id: Number(b.dataset.sigDefault) });
      renderSignatureList();
    }),
  );
  list.querySelectorAll('[data-sig-del]').forEach((b) =>
    b.addEventListener('click', async () => {
      await call('delete_signature', { id: Number(b.dataset.sigDel) });
      renderSignatureList();
    }),
  );
}

function wireSignatures() {
  const clear = () => {
    $('sig-name').value = '';
    $('sig-body').value = '';
    $('sig-status').textContent = '';
    delete $('btn-save-signature').dataset.editing;
  };

  $('btn-new-signature').addEventListener('click', clear);

  $('btn-save-signature').addEventListener('click', async () => {
    const editing = Number($('btn-save-signature').dataset.editing ?? 0);
    const existing = state.signatures.find((x) => x.id === editing);
    try {
      await invoke('save_signature', {
        signature: {
          id: editing,
          name: $('sig-name').value,
          body: $('sig-body').value,
          // Editing must not silently demote a signature that was the default.
          is_default: existing?.is_default ?? false,
          sort_order: existing?.sort_order ?? 0,
        },
      });
      clear();
      await renderSignatureList();
      status('Signature saved');
    } catch (e) {
      $('sig-status').textContent = String(e);
    }
  });

  $('compose-signature').addEventListener('change', (e) => {
    const id = Number(e.target.value) || null;
    state.appliedSignatureId = id;
    const chosen = state.signatures.find((x) => x.id === id);
    applySignatureToBody(chosen ? chosen.body : '');
  });
}

// ---------------------------------------------------------------- compose

const MAX_ATTACHMENT_TOTAL = 25 * 1024 * 1024;

function renderAttachments() {
  const list = $('attach-list');
  const total = state.attachments.reduce((n, a) => n + (a.size ?? 0), 0);
  $('attach-summary').textContent = state.attachments.length
    ? `${state.attachments.length} file(s)${
        total > MAX_ATTACHMENT_TOTAL ? ' — too large, most servers will refuse this' : ''
      }`
    : '';
  $('attach-summary').classList.toggle('over-limit', total > MAX_ATTACHMENT_TOTAL);

  list.innerHTML = state.attachments
    .map(
      (a, i) => `<span class="chip">${icon('attach')} ${esc(a.filename)}
          <button type="button" class="chip-x" data-i="${i}" title="Remove">×</button>
        </span>`,
    )
    .join('');

  list.querySelectorAll('.chip-x').forEach((btn) =>
    btn.addEventListener('click', () => {
      state.attachments.splice(Number(btn.dataset.i), 1);
      renderAttachments();
    }),
  );
}

function currentDraft() {
  const form = $('compose-form');
  const dlg = $('compose-dialog');
  return {
    account_id: Number(form.elements.account_id.value),
    to: splitAddresses(form.elements.to.value),
    cc: splitAddresses(form.elements.cc.value),
    bcc: splitAddresses(form.elements.bcc.value),
    subject: form.elements.subject.value,
    body_text: editorPlainText(),
    body_html: editor().innerHTML.trim() ? editor().innerHTML : null,
    in_reply_to: dlg.dataset.inReplyTo || null,
    references: JSON.parse(dlg.dataset.references || '[]'),
    attachments: state.attachments.map((a) => ({ filename: a.filename, path: a.path })),
    // The composer owns the signature: it is already in the body above, so the send path must
    // append nothing or the recipient gets it twice.
    signature: '',
    draft_message_id: state.draftMessageId,
  };
}

/// Split a recipient field into entries on `;`, `,` or a new line, the way Outlook does.
///
/// Separators inside quotes or angle brackets are part of the entry, so `"Doe, John" <j@x.org>`
/// stays one recipient. Splitting on every comma turned it into two, neither of them an address.
function splitAddresses(value) {
  const out = [];
  let cur = '';
  let quoted = false;
  let angled = false;
  for (const ch of value) {
    if (ch === '"') quoted = !quoted;
    else if (ch === '<' && !quoted) angled = true;
    else if (ch === '>' && !quoted) angled = false;
    if (!quoted && !angled && (ch === ',' || ch === ';' || ch === '\n')) {
      out.push(cur);
      cur = '';
    } else {
      cur += ch;
    }
  }
  out.push(cur);
  return out.map((s) => s.trim()).filter(Boolean);
}

function openCompose(draft, title = 'New message') {
  const dlg = $('compose-dialog');
  const form = $('compose-form');
  form.reset();
  $('compose-title').textContent = title;
  $('compose-status').textContent = '';
  $('compose-status').className = 'account-status';

  const from = $('compose-from');
  from.innerHTML = state.accounts
    .map(
      (a) =>
        `<option value="${a.id}" ${a.can_send ? '' : 'disabled'}>${esc(a.email)}${
          a.can_send ? '' : ' — outgoing mail not set up'
        }</option>`,
    )
    .join('');
  const accountId = draft?.account_id ?? state.selectedAccountId ?? state.accounts[0]?.id;
  if (accountId) from.value = String(accountId);
  // Fall back to one that can send, rather than opening on an account that cannot.
  if (!state.accounts.find((a) => a.id === Number(from.value))?.can_send) {
    const sender = state.accounts.find((a) => a.can_send);
    if (sender) from.value = String(sender.id);
  }

  form.elements.to.value = (draft?.to ?? []).join(', ');
  form.elements.cc.value = (draft?.cc ?? []).join(', ');
  form.elements.bcc.value = (draft?.bcc ?? []).join(', ');
  form.elements.subject.value = draft?.subject ?? '';

  // A reply arrives with HTML already built — the quote block and a space above it. A new message
  // starts empty. A saved draft has only text, so it is put back as text.
  setEditorHtml(
    draft?.body_html ??
      (draft?.body_text
        ? `<div style="white-space:pre-wrap">${esc(draft.body_text)}</div>`
        : ''),
  );

  dlg.dataset.inReplyTo = draft?.in_reply_to ?? '';
  dlg.dataset.references = JSON.stringify(draft?.references ?? []);
  state.attachments = (draft?.attachments ?? []).map((a) => ({ ...a }));
  state.draftMessageId = draft?.draft_message_id ?? null;
  updateSendAvailability();
  // A draft this window created by autosaving, as opposed to one the user reopened. Discard removes
  // the first and leaves the second alone.
  state.autosavedDraftId = null;
  state.composeDirty = false;
  renderAttachments();
  dlg.showModal();

  // Insert the default signature so it is visible and editable before sending, rather than
  // appearing only in the recipient's copy.
  loadSignatures().then(() => {
    const reopening = draft?.draft_message_id != null;
    const already = editorHasSignature();
    const def = state.signatures.find((g) => g.is_default);
    if (def && !reopening && !already) {
      state.appliedSignatureId = def.id;
      applySignatureToBody(def.body);
    } else {
      state.appliedSignatureId = null;
    }
    fillSignaturePicker(state.appliedSignatureId);
  });
  if (draft?.to?.length) focusEditorStart();
  else form.elements.to.focus();
}

/// Send is only offered when the chosen account can send, and says why when it cannot.
function updateSendAvailability() {
  const account = state.accounts.find((a) => a.id === Number($('compose-from').value));
  const ok = Boolean(account?.can_send);
  $('btn-send').disabled = !ok;
  $('btn-send-later').disabled = !ok;
  if (!ok) {
    $('compose-status').textContent =
      "Can't send yet: set up outgoing mail for this account in Settings. You can still save a draft.";
    $('compose-status').className = 'account-status bad';
  }
}

/// True when the composer holds anything worth keeping.
function composeHasContent(d) {
  return d.to.length + d.cc.length + d.bcc.length > 0 || d.subject.trim() || d.body_text.trim();
}

/// Save the open message to Drafts if it changed. False only when a save was needed and failed.
///
/// Called every 30 seconds and when the window is dismissed, so a crash, a stray Esc or a closed
/// app costs at most half a minute of typing — never the whole message.
async function autosaveCompose() {
  if (!$('compose-dialog').open || !state.composeDirty) return true;
  const d = currentDraft();
  if (!composeHasContent(d)) return true;
  try {
    const existed = state.draftMessageId != null;
    state.draftMessageId = await invoke('save_draft', { draft: d });
    if (!existed) state.autosavedDraftId = state.draftMessageId;
    state.composeDirty = false;
    const at = new Date().toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
    $('compose-status').textContent = `Draft saved at ${at}`;
    $('compose-status').className = 'account-status';
    return true;
  } catch (e) {
    $('compose-status').textContent = `Could not save draft: ${e}`;
    $('compose-status').className = 'account-status bad';
    return false;
  }
}

function wireComposeDialog() {
  const dlg = $('compose-dialog');
  $('compose-from').addEventListener('change', () => {
    $('compose-status').textContent = '';
    updateSendAvailability();
  });
  // Typing in any field, the editor included, makes the message worth saving.
  $('compose-form').addEventListener('input', () => {
    state.composeDirty = true;
  });
  setInterval(autosaveCompose, 30_000);

  // Esc keeps the message, the way Outlook does: it goes to Drafts rather than into the void.
  dlg.addEventListener('cancel', async (e) => {
    e.preventDefault();
    const hadWork = state.composeDirty && composeHasContent(currentDraft());
    if (!(await autosaveCompose())) return;
    dlg.close();
    if (hadWork || state.autosavedDraftId) status('Saved to Drafts');
  });

  // Discard really discards, but only after asking, and it takes the autosaved copy with it.
  $('btn-cancel-compose').addEventListener('click', async () => {
    const d = currentDraft();
    if (composeHasContent(d) && (state.composeDirty || state.autosavedDraftId)) {
      const ok = await dialogApi.confirm('Discard this message? What you wrote will not be kept.', {
        title: APP_NAME,
        kind: 'warning',
      });
      if (!ok) return;
    }
    if (state.autosavedDraftId) {
      await call('delete_messages', { ids: [state.autosavedDraftId] }).catch(() => {});
    }
    state.composeDirty = false;
    dlg.close();
  });
  $('btn-compose').addEventListener('click', () => openCompose(null));

  $('btn-attach').addEventListener('click', async () => {
    const picked = await dialogApi.open({ multiple: true });
    if (!picked) return;
    for (const path of Array.isArray(picked) ? picked : [picked]) {
      state.attachments.push({ filename: path.split(/[\\/]/).pop(), path, size: 0 });
    }
    renderAttachments();
  });

  $('btn-save-draft').addEventListener('click', async () => {
    try {
      state.draftMessageId = await invoke('save_draft', { draft: currentDraft() });
      dlg.close();
      status('Draft saved');
      await loadFolders();
      await loadMessages();
    } catch (e) {
      $('compose-status').textContent = String(e);
      $('compose-status').className = 'account-status bad';
    }
  });

  $('btn-send').addEventListener('click', async () => {
    const btn = $('btn-send');
    const draft = currentDraft();
    if (draft.to.length + draft.cc.length + draft.bcc.length === 0) {
      $('compose-status').textContent = 'Add at least one recipient.';
      $('compose-status').className = 'account-status bad';
      return;
    }
    btn.disabled = true;
    $('compose-status').textContent = 'Sending…';
    $('compose-status').className = 'account-status';
    try {
      const sent = await invoke('send', { draft });
      dlg.close();
      // Not sent is not lost: it is in the Outbox and the timer retries it.
      status(sent ? 'Message sent' : 'Mail server unreachable — saved in Outbox, will retry');
      refreshViewCounts();
      await loadFolders();
      await loadMessages();
    } catch (e) {
      $('compose-status').textContent = String(e);
      $('compose-status').className = 'account-status bad';
    } finally {
      btn.disabled = false;
    }
  });
}

// ---------------------------------------------------------------- ribbon

/// Commands that act on the selected message are disabled until there is one, rather than
/// failing when pressed. Outlook does the same, and it is the difference between a toolbar that
/// teaches you what is possible and one that just errors.
function refreshRibbonState() {
  const has = state.selectedMessageId != null;
  document.querySelectorAll('[data-needs-message]').forEach((el) => {
    el.disabled = !has;
  });

  const info = $('ribbon-sync-info');
  if (info) {
    const lines = state.accounts.map((acc) => {
      const when = acc.last_sync_utc ? formatDate(acc.last_sync_utc) : 'never';
      return `${acc.email}: ${acc.is_enabled ? when : 'not syncing'}`;
    });
    info.textContent = lines.join(' · ') || 'No accounts yet';
  }
}

/// Populate the Move-to and Owner pickers from whatever exists right now.
function refreshRibbonPickers() {
  const move = $('ribbon-move');
  if (move) {
    const account = state.selectedAccountId;
    move.innerHTML =
      '<option value="">Move to…</option>' +
      state.folders
        .filter((f) => account == null || f.account_id === account)
        .map((f) => `<option value="${f.id}">${esc(f.name)}</option>`)
        .join('');
  }

}

async function ribbonCommand(cmd) {
  const id = state.selectedMessageId;

  switch (cmd) {
    // --- respond ---------------------------------------------------------
    case 'reply':
    case 'reply-all':
      if (!id) return;
      openCompose(
        await call('reply_draft', { id, replyAll: cmd === 'reply-all' }),
        cmd === 'reply-all' ? 'Reply all' : 'Reply',
      );
      return;
    case 'forward':
      if (!id) return;
      openCompose(await call('forward_draft', { id }), 'Forward');
      return;

    case 'view-contacts':
      selectView('contacts');
      return;

    case 'export-contacts':
      await exportContacts();
      return;

    case 'toggle-list':
      toggleListCollapsed();
      return;

    case 'pop-out':
      if (id) await popOutMessage(id);
      return;

    // --- delete ----------------------------------------------------------
    case 'delete':
      if (id) await deleteSelected([id]);
      return;
    case 'archive': {
      if (!id) return;
      const target = state.folders.find(
        (f) => f.kind === 'archive' && f.account_id === state.selectedAccountId,
      );
      if (!target) return status('This account has no Archive folder', true);
      await call('move_messages', { ids: [id], folderId: target.id });
      status('Archived');
      break;
    }
    case 'junk':
      if (!id) return;
      await call('mark_junk', { ids: [id], isJunk: true });
      status('Moved to Junk and the filter has learned from it');
      break;
    case 'not-junk':
      if (!id) return;
      await call('mark_junk', { ids: [id], isJunk: false });
      status('Moved to the Inbox and the filter has learned from it');
      break;

    // --- tags ------------------------------------------------------------
    case 'toggle-read': {
      if (!id) return;
      const m = state.messages.find((x) => x.id === id);
      await call('set_read', { ids: [id], read: !(m?.is_read ?? false) });
      break;
    }
    case 'toggle-flag': {
      if (!id) return;
      const m = state.messages.find((x) => x.id === id);
      await call('set_flagged', { ids: [id], flagged: !(m?.is_flagged ?? false) });
      break;
    }
    case 'followup': {
      if (!id) return;
      const d = new Date();
      d.setDate(d.getDate() + 1);
      d.setHours(9, 0, 0, 0);
      await call('set_reminder', { messageId: id, dueUtc: Math.floor(d.getTime() / 1000), note: '' });
      status('Follow-up set for tomorrow 9am');
      break;
    }

    // --- send / receive ---------------------------------------------------
    case 'sync-team': {
      const out = await call('sync_team');
      status(
        out.unreachable
          ? `Shared team file unreachable: ${out.unreachable}`
          : `Team state synced — received ${out.pulled}, published ${out.pushed}`,
        Boolean(out.unreachable),
      );
      break;
    }

    // --- view -------------------------------------------------------------
    case 'view-inbox': {
      const inbox = state.folders.find((f) => f.kind === 'inbox');
      if (inbox) selectFolder(inbox);
      return;
    }
    case 'view-mine':
      selectView('mine');
      return;
    case 'view-unassigned':
      selectView('unassigned');
      return;
    case 'view-followups':
      selectView('followups');
      return;
    case 'clear-filters':
      for (const k of Object.keys(state.filters)) state.filters[k] = false;
      state.search = '';
      $('search').value = '';
      document.querySelectorAll('.fchip').forEach((c) => {
        c.classList.remove('on');
        c.setAttribute('aria-pressed', 'false');
      });
      await loadMessages();
      status('Filters cleared');
      return;
  }

  await loadMessages();
  await loadFolders();
  if (state.selectedMessageId) openMessage(state.selectedMessageId);
}

function wireRibbon() {
  $('ribbon-tabs').addEventListener('click', (e) => {
    const tab = e.target.closest('.rtab');
    if (!tab) return;
    document.querySelectorAll('.rtab').forEach((t) => t.classList.toggle('on', t === tab));
    document.querySelectorAll('.rpanel').forEach((p) =>
      p.classList.toggle('on', p.dataset.panel === tab.dataset.tab),
    );
  });

  document.querySelectorAll('[data-cmd]').forEach((b) =>
    b.addEventListener('click', () => ribbonCommand(b.dataset.cmd)),
  );

  $('ribbon-move').addEventListener('change', async (e) => {
    const folderId = Number(e.target.value);
    if (!folderId || !state.selectedMessageId) return;
    await call('move_messages', { ids: [state.selectedMessageId], folderId });
    e.target.value = '';
    state.selectedMessageId = null;
    $('reader').innerHTML = '<div class="empty">Select a message to read it.</div>';
    status('Moved');
    await loadMessages();
    await loadFolders();
  });

  refreshRibbonState();
}

// ---------------------------------------------------------------- wiring

function wireFilters() {
  $('filter-chips').addEventListener('click', (e) => {
    const chip = e.target.closest('.fchip');
    if (!chip) return;
    const key = chip.dataset.filter;
    state.filters[key] = !state.filters[key];
    // "Mine" and "Unassigned" are contradictory; turning one on clears the other.
    if (key === 'mine' && state.filters.mine) state.filters.unassigned = false;
    if (key === 'unassigned' && state.filters.unassigned) state.filters.mine = false;
    document.querySelectorAll('.fchip').forEach((c) => {
      c.classList.toggle('on', Boolean(state.filters[c.dataset.filter]));
      c.setAttribute('aria-pressed', String(Boolean(state.filters[c.dataset.filter])));
    });
    loadMessages();
  });

  $('btn-mark-all-read').addEventListener('click', async () => {
    if (state.selectedFolderId === null) return;
    const n = await call('mark_folder_read', { folderId: state.selectedFolderId });
    status(n > 0 ? `Marked ${n} message(s) read` : 'Nothing unread here');
    await loadMessages();
    await loadFolders();
  });

  let debounce;
  $('search').addEventListener('input', (e) => {
    state.search = e.target.value;
    clearTimeout(debounce);
    debounce = setTimeout(loadMessages, 160);
  });
}

/// Outlook-compatible bindings, so muscle memory carries over.
function wireKeyboard() {
  document.addEventListener('keydown', (e) => {
    const inField = ['INPUT', 'TEXTAREA', 'SELECT'].includes(e.target.tagName);
    const dialogOpen = document.querySelector('dialog[open]');

    if (e.key === 'F9') {
      e.preventDefault();
      return void syncAll();
    }
    // Ctrl+Shift+L folds the list away. Shift is deliberate: Ctrl+L alone is the address bar in
    // every browser, and this runs in one.
    if (e.ctrlKey && e.shiftKey && e.key.toLowerCase() === 'l') {
      e.preventDefault();
      toggleListCollapsed();
      return;
    }

    if (e.ctrlKey && e.key.toLowerCase() === 'e') {
      e.preventDefault();
      $('search').focus();
      return void $('search').select();
    }
    if (e.ctrlKey && e.key.toLowerCase() === 'n' && !dialogOpen) {
      e.preventDefault();
      return void openCompose(null);
    }
    if (e.ctrlKey && e.key.toLowerCase() === 'r' && state.selectedMessageId && !dialogOpen) {
      e.preventDefault();
      return void call('reply_draft', { id: state.selectedMessageId, replyAll: e.shiftKey }).then(
        (d) => openCompose(d, e.shiftKey ? 'Reply all' : 'Reply'),
      );
    }
    if (e.ctrlKey && e.key.toLowerCase() === 'f' && state.selectedMessageId && !dialogOpen) {
      // Outlook binds Ctrl+F to Forward, not Find. Search is Ctrl+E, as it is there too.
      e.preventDefault();
      return void call('forward_draft', { id: state.selectedMessageId }).then((d) =>
        openCompose(d, 'Forward'),
      );
    }
    // An open menu owns the arrow keys; without this, moving through it also moved through the
    // message list underneath and redrew the pane the menu belonged to.
    if (inField || dialogOpen || !$('ctx-menu').hidden) return;

    const index = state.messages.findIndex((m) => m.id === state.selectedMessageId);
    if (e.key === 'ArrowDown' || e.key === 'j') {
      e.preventDefault();
      const next = state.messages[Math.min(index + 1, state.messages.length - 1)];
      if (next) openMessage(next.id);
    } else if (e.key === 'ArrowUp' || e.key === 'k') {
      e.preventDefault();
      const prev = state.messages[Math.max(index - 1, 0)];
      if (prev) openMessage(prev.id);
    } else if (e.key === 'Delete' && state.selectedMessageId) {
      e.preventDefault();
      deleteSelected([state.selectedMessageId]);
    } else if (e.key === 'Insert' && state.selectedMessageId) {
      // Outlook's key for Flag, and the one the flag's tooltip and the More menu name.
      e.preventDefault();
      const m = state.messages.find((x) => x.id === state.selectedMessageId);
      if (m) toggleFlag(m);
    } else if (e.key === 'Escape' && document.body.dataset.pane === 'read') {
      document.body.dataset.pane = 'list';
    }
  });
}

async function main() {
  // A window opened with ?message=<id> exists to show that one message, and nothing else.
  const popout = Number(new URLSearchParams(location.search).get('message')) || null;

  await loadAppName();
  await loadTheme();
  wireAccountDialog();
  wireComposeDialog();
  // Next page when the list is within a screen of its end.
  $('message-list').addEventListener('scroll', (e) => {
    const l = e.currentTarget;
    if (l.scrollTop + l.clientHeight > l.scrollHeight - l.clientHeight) loadMoreMessages();
  });
  wireTeamDialog();
  wireSettingsDialog();
  wireSignatures();
  wireRulesDialog();
  wireJunkDialog();
  wireDataFile();
  wireOutlookImport();
  wireViewer();
  wireSendLater();
  wireContextMenu();
  wireProgress();
  wireListCollapse();
  wireEditorToolbar();
  wireFilters();
  wireRibbon();
  wireKeyboard();
  $('btn-sync').addEventListener('click', syncAll);

  // The core broadcasts; every window re-reads rather than trusting its own copy.
  await listen('messages-changed', () => {
    loadMessages();
    loadFolders();
  });
  await listen('accounts-changed', loadFolders);
  await listen('sync-progress', (e) => updateProgress(e.payload));
  await listen('sync-started', (e) => beginProgress(e.payload.account_id));
  await listen('sync-failed', (e) => status(`Sync failed: ${e.payload.error}`, true));

  $('reader').innerHTML =
    `<div class="empty"><span class="empty-icon">${icon('mail')}</span>Select a message to read it.</div>`;
  await loadFolders();

  if (popout) {
    await enterPopoutMode(popout);
    return;
  }

  // Reminders are checked on a timer rather than only at sync, so a follow-up due at 3pm is
  // announced at 3pm even if no mail arrives.
  checkReminders();
  setInterval(checkReminders, 60_000);

  // Mail arrives on its own from here. Until now `poll_minutes` was a setting nothing read, and
  // nothing checked for mail unless somebody pressed Send/Receive.
  startPolling();

  // Scheduled messages and retries both live in the outbox, and both need something to come round
  // and look at the clock.
  const drainOutbox = async () => {
    const out = await call('flush_outbox').catch(() => null);
    if (out?.sent) {
      status(`${out.sent} scheduled message${out.sent === 1 ? '' : 's'} sent`);
      refreshViewCounts();
    }
  };
  drainOutbox();
  setInterval(drainOutbox, 60_000);
}

main();
