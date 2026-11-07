//! Software updates from Tools › Check for updates.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { openApp, calls } from './harness.mjs';

const NEWER = { version: '0.1.3', current: '0.1.2', notes: 'Fixes' };

test('a build with no release feed shows no update button', async () => {
  const { page, close } = await openApp();
  try {
    await page.click('[data-tab="tools"]');
    assert.equal(await page.isVisible('#btn-update'), false);
  } finally {
    await close();
  }
});

test('up to date is said only when somebody asked', async () => {
  const { page, close } = await openApp({ answers: { can_update: true, check_update: null } });
  try {
    await page.click('[data-tab="tools"]');
    await page.click('#btn-update');
    await page.waitForTimeout(100);
    assert.match(await page.textContent('#status'), /up to date/);
    assert.equal((await calls(page, 'install_update')).length, 0);
  } finally {
    await close();
  }
});

test('a newer version is offered, and declining installs nothing', async () => {
  const { page, close } = await openApp({ answers: { can_update: true, check_update: NEWER } });
  try {
    await page.click('[data-tab="tools"]');
    await page.click('#btn-update');
    await page.waitForTimeout(100);
    // The harness's confirm answers "Later".
    assert.match(await page.textContent('#update-label'), /0\.1\.3/);
    assert.equal((await calls(page, 'install_update')).length, 0);
  } finally {
    await close();
  }
});

test('accepting the offer installs it', async () => {
  const { page, close } = await openApp({ answers: { can_update: true, check_update: NEWER } });
  try {
    await page.evaluate(() => {
      window.__TAURI__.dialog.confirm = async () => true;
    });
    await page.click('[data-tab="tools"]');
    await page.click('#btn-update');
    await page.waitForTimeout(100);
    assert.equal((await calls(page, 'install_update')).length, 1);
  } finally {
    await close();
  }
});
