//! What a reply actually turns into when it is sent.
//!
//! The plain-text half of a reply is the part nobody looks at until a recipient's client shows it
//! instead of the HTML — and then it is the whole message. A real reply went out with an unmarked
//! `From:/Sent:/To:` block and screens of encoded spaces from a flattened table, so these drive the
//! composer the way a person does and read back what the app asked the core to send.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { openApp, calls } from './harness.mjs';

const QUOTE = `<div>yes please cut off the pads as per the hole</div>
  <div class="signet-quote">
    <div><b>From:</b> rep@pcb-supplier.example<br><b>Sent:</b> 2026-09-25 01:49<br>
         <b>To:</b> purchase@example.com<br><b>Subject:</b> PCB order</div>
    <blockquote><p>Hi dear customer,</p><p>Shall we cut off the pad?</p></blockquote>
  </div>`;

/// Open a reply, put `html` in the composer, press Send, and return the draft that was sent.
async function sentDraft(page, html) {
  await page.click('.msg');
  await page.waitForSelector('[data-act="reply"]', { timeout: 5000 });
  await page.click('[data-act="reply"]');
  await page.waitForSelector('#compose-body', { state: 'visible', timeout: 5000 });

  await page.evaluate((markup) => {
    document.getElementById('compose-body').innerHTML = markup;
  }, html);

  await page.click('#btn-send');
  await page.waitForTimeout(120);

  const sent = await calls(page, 'send');
  assert.ok(sent.length, 'the composer never asked the core to send anything');
  return sent[sent.length - 1].args.draft;
}

test('the quoted original is marked with > the way every client writes it', async () => {
  const { page, close } = await openApp();
  try {
    const draft = await sentDraft(page, QUOTE);
    assert.ok(
      draft.body_text.includes('> From: rep@pcb-supplier.example'),
      `the quote was not marked:\n${draft.body_text}`,
    );
    assert.ok(
      draft.body_text.includes('> Hi dear customer,'),
      `the quoted body was not marked:\n${draft.body_text}`,
    );
  } finally {
    await close();
  }
});

test('what the user wrote stays unquoted and comes first', async () => {
  const { page, close } = await openApp();
  try {
    const { body_text: text } = await sentDraft(page, QUOTE);
    assert.ok(
      text.startsWith('yes please cut off the pads'),
      `the reply does not start with what was written:\n${text}`,
    );
    assert.ok(!text.includes('> yes please'), 'the reply was quoted back at its own author');
  } finally {
    await close();
  }
});

test('whitespace used for HTML layout does not become pages of spaces', async () => {
  // A quoted table flattened literally arrived as screens of `=20` in the sent message.
  const { page, close } = await openApp();
  try {
    const { body_text: text } = await sentDraft(
      page,
      '<div>hi</div><div class="signet-quote"><table><tr><td>     </td>' +
        '<td>          </td></tr><tr><td>x</td></tr></table></div>',
    );
    assert.ok(!/ {3}/.test(text), `runs of spaces survived:\n${JSON.stringify(text)}`);
    assert.ok(!/\n{3}/.test(text), `runs of blank lines survived:\n${JSON.stringify(text)}`);
  } finally {
    await close();
  }
});

test('a message with no quote is left exactly as written', async () => {
  // A new message must not acquire quote markers from a path meant for replies.
  const { page, close } = await openApp();
  try {
    const { body_text: text } = await sentDraft(
      page,
      '<div>Hello Amir</div><div>Please see attached.</div>',
    );
    assert.equal(text, 'Hello Amir\nPlease see attached.');
  } finally {
    await close();
  }
});

test('the rich version is sent alongside the plain one', async () => {
  // Both halves, or a client that prefers HTML shows nothing and one that prefers text shows the
  // fallback. `multipart/alternative` needs the pair.
  const { page, close } = await openApp();
  try {
    const draft = await sentDraft(page, QUOTE);
    assert.ok(draft.body_html, 'no HTML body was sent');
    assert.ok(draft.body_html.includes('signet-quote'), 'the quote block was lost');
    assert.ok(draft.body_text.length > 0, 'no plain-text alternative was sent');
  } finally {
    await close();
  }
});

/// Open a reply and type into it the way a person does, so the composer sees real input events.
async function typedReply(page, words) {
  await page.click('.msg');
  await page.waitForSelector('[data-act="reply"]', { timeout: 5000 });
  await page.click('[data-act="reply"]');
  await page.waitForSelector('#compose-body', { state: 'visible', timeout: 5000 });
  await page.click('#compose-body');
  await page.keyboard.type(words);
}

test('Esc keeps the message in Drafts instead of throwing it away', async () => {
  const { page, close } = await openApp();
  try {
    await typedReply(page, 'Price confirmed at 12.50 per unit');
    await page.keyboard.press('Escape');
    await page.waitForTimeout(150);

    const saved = await calls(page, 'save_draft');
    assert.equal(saved.length, 1, 'Esc lost the message');
    assert.ok(saved[0].args.draft.body_text.includes('Price confirmed'));
    assert.ok(!(await page.$eval('#compose-dialog', (d) => d.open)), 'the window stayed open');
  } finally {
    await close();
  }
});

test('Discard asks first, and declining keeps the window open', async () => {
  const { page, close } = await openApp();
  try {
    await typedReply(page, 'half-written answer');
    // The harness's confirm answers "no" and records nothing, so count it here.
    await page.evaluate(() => {
      window.__ASKED__ = 0;
      window.__TAURI__.dialog.confirm = async () => {
        window.__ASKED__ += 1;
        return false;
      };
    });
    await page.click('#btn-cancel-compose');
    await page.waitForTimeout(100);

    assert.equal(await page.evaluate(() => window.__ASKED__), 1, 'discarded written text without asking');
    assert.ok(await page.$eval('#compose-dialog', (d) => d.open), 'closed although the user said no');
  } finally {
    await close();
  }
});

test('several recipients, including names pasted from Outlook, reach the core as separate entries', async () => {
  const { page, close } = await openApp();
  try {
    await page.click('#btn-compose');
    await page.waitForSelector('#compose-body', { state: 'visible' });
    await page.fill(
      '#compose-form [name="to"]',
      'Janice Carroll <janice@x.org>; "Doe, John" <j@x.org>, plain@x.org',
    );
    await page.fill('#compose-form [name="cc"]', 'a@x.org;b@x.org');
    await page.click('#btn-send');
    await page.waitForTimeout(120);

    const sent = await calls(page, 'send');
    assert.ok(sent.length, 'nothing was sent');
    const { to, cc } = sent[sent.length - 1].args.draft;
    assert.deepEqual(to, ['Janice Carroll <janice@x.org>', '"Doe, John" <j@x.org>', 'plain@x.org']);
    assert.deepEqual(cc, ['a@x.org', 'b@x.org']);
  } finally {
    await close();
  }
});
