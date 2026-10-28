//! Layout regressions.
//!
//! Every test here corresponds to a bug that reached a user in this project. They are written
//! against computed geometry rather than markup, because in each case the markup was correct and
//! the rendering was not.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { openApp, box, calls } from './harness.mjs';

test('the reading pane occupies real width', async () => {
  // Shipped broken: a fourth grid column was added and the panes were left to auto-place. A
  // `display: none` element takes no grid cell, so every later pane slid one track left and the
  // reader landed in the zero-width track. The markup was right; only the layout was wrong.
  const { page, close } = await openApp();
  try {
    const reader = await box(page, '.read-pane');
    assert.ok(reader, 'there is no reading pane at all');
    assert.ok(
      reader.width > 400,
      `the reading pane is ${reader.width}px wide, which means it is in the wrong grid column`,
    );
  } finally {
    await close();
  }
});

test('collapsing the list widens the reader and leaves a usable way back', async () => {
  // Shipped broken twice: first the restore strip had zero width, then it was a 28px sliver in the
  // page's own greys that read as a border. Both left no way back except a keyboard shortcut.
  const { page, close } = await openApp();
  try {
    const before = await box(page, '.read-pane');

    await page.click('#btn-collapse-list');
    await page.waitForTimeout(60);

    const after = await box(page, '.read-pane');
    assert.ok(
      after.width > before.width,
      `collapsing did not widen the reader (${before.width} -> ${after.width})`,
    );

    const strip = await box(page, '#list-restore');
    assert.ok(strip, 'nothing was left behind to bring the list back');
    assert.ok(
      strip.width >= 24 && strip.height > 100,
      `the way back is ${strip.width}x${strip.height}, too small to find or hit`,
    );

    // And it has to actually work.
    await page.click('#list-restore');
    await page.waitForTimeout(60);
    const list = await box(page, '.list-pane');
    assert.ok(list.width > 200, 'the list did not come back');
  } finally {
    await close();
  }
});

test('attachments sit above the message body and never overlap it', async () => {
  // Shipped broken: the strip was `position: sticky`, so it pinned itself over the message text as
  // the body scrolled. Being above the body in the DOM was not enough to be above it on screen.
  const { page, close } = await openApp();
  try {
    await page.click('.msg');
    await page.waitForSelector('.attachments', { timeout: 5000 });

    // Scrolled, not at rest. A sticky element sits in its natural place until the container
    // scrolls, so asserting on a freshly opened message proves nothing — the first version of this
    // test passed against the very bug it was written for.
    await page.evaluate(() => {
      const reader = document.querySelector('.reader');
      reader.scrollTop = reader.scrollHeight;
    });
    await page.waitForTimeout(60);

    const strip = await box(page, '.attachments');
    const body = await box(page, '#reader-body');

    assert.ok(strip && body, 'the reader did not render an attachment strip and a body');
    assert.ok(
      strip.y + strip.height <= body.y + 1,
      `after scrolling, the attachment strip (ends ${Math.round(strip.y + strip.height)}) ` +
        `overlaps the body (starts ${Math.round(body.y)})`,
    );
  } finally {
    await close();
  }
});

test('the reading pane is not pushed off-screen by its own header', async () => {
  // The complaint that started the compaction work: five rows of chrome above the message left
  // very little of the window for the thing being read.
  const { page, close } = await openApp({ height: 900 });
  try {
    await page.click('.msg');
    await page.waitForSelector('#reader-body', { timeout: 5000 });

    const body = await box(page, '#reader-body');
    assert.ok(
      body.height > 300,
      `only ${Math.round(body.height)}px is left for the message itself`,
    );
  } finally {
    await close();
  }
});

test('nothing in the chrome scrolls the window sideways', async () => {
  const { page, close } = await openApp({ width: 1100 });
  try {
    const overflow = await page.evaluate(
      () => document.documentElement.scrollWidth - document.documentElement.clientWidth,
    );
    assert.equal(overflow, 0, `the page scrolls ${overflow}px sideways`);
  } finally {
    await close();
  }
});

test('the app starts without a single console error', async () => {
  // A thrown error during start-up leaves a half-drawn window, which is exactly what "unrefined"
  // looks like from the outside.
  const { errors, close } = await openApp();
  try {
    assert.deepEqual(errors, [], `start-up logged errors:\n${errors.join('\n')}`);
  } finally {
    await close();
  }
});

test('scrolling to the end of a long folder loads the next page', async () => {
  const { FIXTURE } = await import('./harness.mjs');
  const many = Array.from({ length: 200 }, (_, i) => ({ ...FIXTURE.messages[0], id: 1000 + i }));
  const { page, close } = await openApp({ fixture: { ...FIXTURE, messages: many } });
  try {
    await page.$eval('#message-list', (l) => l.scrollTo(0, l.scrollHeight));
    await page.waitForTimeout(200);
    const pages = await calls(page, 'list_messages');
    const next = pages.find((c) => c.args.query.offset === 200);
    assert.ok(next, 'no second page was requested; mail past the first page is unreachable');
    assert.equal(next.args.query.limit, 200);
  } finally {
    await close();
  }
});
