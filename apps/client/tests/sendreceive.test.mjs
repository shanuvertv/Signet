//! Send/Receive for one mailbox, from its header in the folder pane.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { openApp, calls } from './harness.mjs';

const OUTCOME = { account_id: 1, new_messages: 2, error: null, sent: 1, send_error: null };

test('the mailbox button sends and receives that mailbox only', async () => {
  const { page, close, errors } = await openApp({ answers: { send_receive_account: OUTCOME } });
  try {
    await page.click('.acct-sync[data-account="1"]');
    await page.waitForTimeout(150);
    const made = await calls(page, 'send_receive_account');
    assert.equal(made.length, 1);
    assert.equal(made[0].args.id, 1);
    assert.equal((await calls(page, 'sync_all')).length, 0, 'every mailbox was synced');
    assert.match(await page.textContent('#status'), /sales@example\.com: 1 sent, 2 new messages/);
    assert.deepEqual(errors, []);
  } finally {
    await close();
  }
});

test('right-clicking a mailbox header offers Send/Receive for it', async () => {
  const { page, close } = await openApp({ answers: { send_receive_account: OUTCOME } });
  try {
    await page.click('.account-name', { button: 'right' });
    await page.click('#ctx-menu >> text=Send/receive this mailbox');
    await page.waitForTimeout(150);
    assert.equal((await calls(page, 'send_receive_account')).length, 1);
  } finally {
    await close();
  }
});

test('a receive failure is reported against the mailbox that failed', async () => {
  const { page, close } = await openApp({
    answers: { send_receive_account: { ...OUTCOME, sent: 0, error: 'connection refused' } },
  });
  try {
    await page.click('.acct-sync');
    await page.waitForTimeout(150);
    assert.match(await page.textContent('#status'), /sales@example\.com: receiving failed — connection refused/);
  } finally {
    await close();
  }
});
