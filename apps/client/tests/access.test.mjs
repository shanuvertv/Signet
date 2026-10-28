//! Keyboard reach and the controls the design review found too small or hidden.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { openApp, box, calls } from './harness.mjs';

test('folders and filters can be reached and used from the keyboard', async () => {
  const { page, close } = await openApp();
  try {
    const folders = await page.$$eval('#folders .folder', (els) => els.map((e) => e.tagName));
    assert.ok(folders.length > 0 && folders.every((t) => t === 'BUTTON'), `not buttons: ${folders}`);

    const chip = await page.$('.fchip[data-filter="unread"]');
    await chip.focus();
    await page.keyboard.press('Enter');
    assert.equal(await chip.getAttribute('aria-pressed'), 'true', 'Enter did not toggle the filter');
  } finally {
    await close();
  }
});

test('the More menu is operable with arrow keys and hands focus back on Escape', async () => {
  const { page, close } = await openApp();
  try {
    await page.click('.msg');
    await page.waitForSelector('[data-act="more"]');
    await page.click('[data-act="more"]');
    await page.keyboard.press('ArrowDown');
    const second = await page.evaluate(() => document.activeElement?.dataset.ctx);
    assert.equal(second, 'unread', 'arrow key did not move through the menu');
    await page.keyboard.press('Escape');
    assert.ok(await page.$eval('#ctx-menu', (m) => m.hidden));
    assert.equal(await page.evaluate(() => document.activeElement?.dataset.act), 'more');
  } finally {
    await close();
  }
});

test('the row flag is at least a 20px target', async () => {
  const { page, close } = await openApp();
  try {
    const b = await box(page, '.msg .flag-toggle');
    assert.ok(b.width >= 20 && b.height >= 20, `flag is ${b.width}x${b.height}`);
  } finally {
    await close();
  }
});

test('Insert flags the selected message, as the tooltip says', async () => {
  const { page, close } = await openApp();
  try {
    await page.click('.msg');
    await page.waitForSelector('[data-act="more"]');
    await page.focus('#message-list');
    await page.keyboard.press('Insert');
    await page.waitForTimeout(100);
    const flagged = await calls(page, 'set_flagged');
    assert.equal(flagged.length, 1, 'Insert did nothing');
    assert.equal(flagged[0].args.flagged, true);
  } finally {
    await close();
  }
});
