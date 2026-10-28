//! Loads the real frontend in a real browser, with the Tauri bridge stubbed out.
//!
//! Everything on the Rust side is checked by a compiler and 300 tests. The frontend had none, and
//! every regression that reached a user in this project has been in it — a grid column resolving
//! to zero width, a sandbox attribute blocking a plugin, a sticky element painting over its
//! sibling. None of those are visible by reading the code, and none are catchable without laying
//! the page out.
//!
//! So this is a real Chromium loading the real `dist/`, asserting *computed* geometry. `invoke` is
//! answered from a small fixture rather than from the app, because what is under test is the
//! interface, not the mail.
//!
//! It is served over HTTP rather than opened as a `file://` path: `app.js` is a module, and a
//! module loaded from `file://` is refused as a cross-origin request.

import { chromium } from 'playwright';
import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import { dirname, join, extname } from 'node:path';

const here = dirname(fileURLToPath(import.meta.url));
export const DIST_DIR = join(here, '..', 'dist');

const TYPES = {
  '.html': 'text/html; charset=utf-8',
  '.js': 'text/javascript; charset=utf-8',
  '.css': 'text/css; charset=utf-8',
};

/// Serve `dist/` on an ephemeral port for the life of one test.
export async function serveDist() {
  const server = createServer(async (req, res) => {
    const rel = (req.url ?? '/').split('?')[0];
    const file = join(DIST_DIR, rel === '/' ? 'index.html' : rel.replace(/^\//, ''));
    try {
      const body = await readFile(file);
      res.writeHead(200, { 'content-type': TYPES[extname(file)] ?? 'application/octet-stream' });
      res.end(body);
    } catch {
      res.writeHead(404).end('not found');
    }
  });

  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  const { port } = server.address();
  return {
    origin: `http://127.0.0.1:${port}`,
    close: () => new Promise((resolve) => server.close(resolve)),
  };
}

/// Enough of an account, folder and message for the UI to have something to draw.
export const FIXTURE = {
  accounts: [
    {
      id: 1,
      email: 'sales@example.com',
      display_name: 'Sales team',
      pop_host: 'mail.example.com',
      protocol: 'imap',
      is_enabled: true,
      can_send: true,
      last_sync_utc: 1_700_000_000,
      poll_minutes: 5,
    },
  ],
  folders: [
    { id: 10, account_id: 1, kind: 'inbox', name: 'Inbox', unread: 1, total: 2 },
    { id: 11, account_id: 1, kind: 'sent', name: 'Sent', unread: 0, total: 0 },
  ],
  messages: [
    {
      id: 100,
      account_id: 1,
      folder_id: 10,
      from_name: 'Amir Rahman',
      from_addr: 'amir@customer.test',
      subject: 'Quote for 40 downlights',
      preview_text: 'Could you price 40 of the 12W downlights please?',
      date_utc: 1_700_000_000,
      is_read: false,
      is_flagged: false,
      has_attachments: true,
      priority: 1,
      work_status: 'new',
    },
    {
      id: 101,
      account_id: 1,
      folder_id: 10,
      from_name: 'Sara Khan',
      from_addr: 'sara@supplier.test',
      subject: 'Revised drawing attached',
      preview_text: 'Please find the revised drawing attached.',
      date_utc: 1_699_900_000,
      is_read: true,
      is_flagged: false,
      has_attachments: false,
      priority: 1,
      work_status: 'new',
    },
  ],
  message: {
    id: 100,
    account_id: 1,
    folder_id: 10,
    from_name: 'Amir Rahman',
    from_addr: 'amir@customer.test',
    subject: 'Quote for 40 downlights',
    body_text: 'Could you price 40 of the 12W downlights please?',
    body_html: '<p>Could you price 40 of the 12W downlights please?</p>'.repeat(40),
    has_remote_content: false,
    date_utc: 1_700_000_000,
    is_read: false,
    is_flagged: false,
    size_bytes: 4096,
    recipients: [{ kind: 'to', name: 'Sales', addr: 'sales@example.com' }],
    attachments: [
      {
        id: 900,
        filename: 'quote.pdf',
        mime_type: 'application/pdf',
        size_bytes: 68000,
        is_inline: false,
      },
      {
        id: 901,
        filename: 'logo.png',
        mime_type: 'image/png',
        size_bytes: 12000,
        is_inline: false,
      },
    ],
  },
  contacts: [
    {
      id: 1,
      display_name: 'Amir Rahman',
      addr: 'amir@customer.test',
      job_title: 'Sales Manager',
      company: 'Gulf Lighting LLC',
      phone: '+971 4 123 4567',
      mobile: '',
      website: '',
      notes: '',
      times_seen: 12,
      last_seen_utc: 1_700_000_000,
      edited_utc: 0,
    },
  ],
};

/// Answers for every command the UI calls during a normal start-up.
export function bridge(fixture = FIXTURE) {
  return {
    list_accounts: fixture.accounts,
    list_folders: fixture.folders,
    list_messages: fixture.messages,
    get_message: fixture.message,
    filing_info: null,
    list_team: [],
    list_signatures: [],
    open_reminders: [],
    take_due_reminders: [],
    scheduled_messages: [],
    list_contacts: fixture.contacts,
    list_sender_rules: [],
    list_rules: [],
    triage: { priority: 'normal', assignee: null, work_status: 'new', shareable: true },
    reply_draft: {
      account_id: 1,
      to: ['rep@pcb-supplier.example'],
      cc: [],
      bcc: [],
      subject: 'Re: PCB order engineer question',
      body_text: '',
      body_html: '<div><br></div><div class="signet-quote"><div><b>From:</b> rep@pcb-supplier.example</div></div>',
      in_reply_to: null,
      references: [],
      attachments: [],
      signature: '',
      draft_message_id: null,
    },
    send: true,
    save_draft: 42,
    delete_messages: null,
    sync_due: [],
    flush_outbox: { sent: 0, failed: 0 },
    setting: '',
    junk_corpus: { spam: 0, ham: 0, trained: false },
    contacts: [],
  };
}

/// Open the app in a real browser with the bridge stubbed, and wait for it to settle.
///
/// Returns `errors` too — anything the page logged or threw. A test that passes while the console
/// is full of exceptions is not telling the truth about the page.
export async function openApp({ width = 1400, height = 900, fixture = FIXTURE, answers = {} } = {}) {
  const site = await serveDist();
  const browser = await chromium.launch();
  const page = await browser.newPage({ viewport: { width, height } });

  const errors = [];
  page.on('pageerror', (e) => errors.push('pageerror: ' + e.message));
  page.on('console', (m) => {
    if (m.type() === 'error') errors.push('console: ' + m.text());
  });

  await page.addInitScript((answers) => {
    // Every call is recorded with its arguments, so a test can assert on what the UI actually
    // asked the core to do rather than on the UI's own idea of what it did.
    window.__CALLS__ = [];

    window.__TAURI__ = {
      core: {
        invoke: async (cmd, args) => {
          window.__CALLS__.push({ cmd, args });
          return cmd in answers ? answers[cmd] : null;
        },
      },
      event: { listen: async () => () => {} },
      dialog: {
        save: async () => null,
        open: async () => null,
        confirm: async () => false,
        message: async () => {},
      },
      notification: {
        isPermissionGranted: async () => false,
        requestPermission: async () => 'denied',
        sendNotification: () => {},
      },
      opener: { openPath: async () => {} },
    };
  }, { ...bridge(fixture), ...answers });

  await page.goto(site.origin);
  await page.waitForSelector('.msg', { timeout: 8000 });

  const close = async () => {
    await browser.close();
    await site.close();
  };

  return { browser, page, errors, close };
}

/// The laid-out size of one element, as the browser actually resolved it.
export async function box(page, selector) {
  return page.evaluate((sel) => {
    const el = document.querySelector(sel);
    if (!el) return null;
    const r = el.getBoundingClientRect();
    return { x: r.x, y: r.y, width: r.width, height: r.height };
  }, selector);
}

/// Every `invoke` the page has made, with its arguments.
export async function calls(page, cmd) {
  const all = await page.evaluate(() => window.__CALLS__);
  return cmd ? all.filter((c) => c.cmd === cmd) : all;
}
