//! Adding an account: nothing is added unless the connection check shows it works.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { openApp, calls } from './harness.mjs';

const FAIL = { ok: false, label: 'x', detail: 'authentication failed' };
const PASS = { ok: true, label: 'x', detail: 'ok' };

async function fillAndAdd(page, { smtp = 'mail.example.com' } = {}) {
  await page.click('[data-tab="tools"]');
  await page.click('#btn-add-account');
  await page.fill('#account-form [name="display_name"]', 'New');
  await page.fill('#account-form [name="email"]', 'new@example.com');
  await page.fill('#account-form [name="pop_host"]', 'mail.example.com');
  await page.fill('#account-form [name="pop_user"]', 'new@example.com');
  await page.fill('#account-form [name="password"]', 'secret');
  await page.fill('#account-form [name="smtp_host"]', smtp);
  await page.click('#btn-save-account');
  await page.waitForTimeout(150);
}

const dialogOpen = (page) => page.$eval('#account-dialog', (d) => d.open);

test('a failing incoming check adds nothing and keeps the window open', async () => {
  const { page, close } = await openApp({
    answers: { test_account: { ok: false, incoming: FAIL, outgoing: PASS } },
  });
  try {
    await fillAndAdd(page);
    assert.equal((await calls(page, 'add_account')).length, 0, 'a broken account was added');
    assert.ok(await dialogOpen(page), 'the window closed and the settings were lost');
    assert.match(await page.textContent('#account-status'), /Not added/);
  } finally {
    await close();
  }
});

test('a check that could not run at all is not taken as a pass', async () => {
  // No answer for test_account: the call fails, which used to fall through to add_account.
  const { page, close } = await openApp();
  try {
    await fillAndAdd(page);
    assert.equal((await calls(page, 'add_account')).length, 0);
    assert.ok(await dialogOpen(page));
  } finally {
    await close();
  }
});

test('a failing sending server blocks, a blank one is a receive-only mailbox', async () => {
  const answers = { test_account: { ok: false, incoming: PASS, outgoing: FAIL } };
  {
    const { page, close } = await openApp({ answers });
    try {
      await fillAndAdd(page);
      assert.equal((await calls(page, 'add_account')).length, 0);
    } finally {
      await close();
    }
  }
  const { page, close } = await openApp({ answers });
  try {
    await fillAndAdd(page, { smtp: '' });
    assert.equal((await calls(page, 'add_account')).length, 1, 'receive-only mailbox refused');
  } finally {
    await close();
  }
});
