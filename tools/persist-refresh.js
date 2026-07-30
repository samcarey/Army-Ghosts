// Refresh the window mid-match and check you land back in the same match.
//
// Offline (no room), so this is the storage path end to end: capture the blob
// the client wrote, reload the page, and prove the world that comes back is
// the one that was written rather than a fresh arena.
const { chromium } = require('playwright');

const PORT = process.env.PORT || '8151';
const URL = `http://127.0.0.1:${PORT}/?bots=3`;
const OUT = process.env.OUT || "target/persist-shots";

const KEY = 'army-ghosts.match.offline';

function parse(blob) {
  if (!blob) return null;
  const [, ...lines] = blob.split('\n');       // drop the timestamp line
  const round = lines.find((l) => l.startsWith('round '));
  const pawns = lines.filter((l) => l.startsWith('pawn '));
  const field = (line, i) => Number(line.split(/\s+/)[i]);
  return {
    roundNumber: field(round, 1),
    roundTicks: field(round, 3),
    pawns: pawns.length,
    // pawn <handle> <bot> <team> <x> <y> ...
    me: pawns.find((p) => field(p, 1) === 0),
    raw: lines.join('\n'),
  };
}

(async () => {
  const browser = await chromium.launch({
    args: ['--use-gl=swiftshader', '--enable-unsafe-swiftshader', '--no-sandbox'],
  });
  const ctx = await browser.newContext({ deviceScaleFactor: 1, viewport: { width: 900, height: 600 } });
  const page = await ctx.newPage();
  const log = [];
  page.on('console', (m) => log.push(m.text()));

  const ready = async () => {
    await page.waitForSelector('body[data-game-ready="1"]', { timeout: 180000 });
  };
  // Nice to have, not part of the verdict: swiftshader can take longer than
  // playwright's screenshot timeout on this debug wasm, and every assertion
  // below reads the stored blob rather than the picture.
  const shoot = async (name) => {
    try {
      await page.screenshot({ path: `${OUT}/${name}.png`, timeout: 60000 });
    } catch (e) {
      console.log(`(screenshot ${name} skipped: ${e.message.split('\n')[0]})`);
    }
  };

  console.log('--- first visit ---');
  await page.goto(URL);
  await ready();
  // Bots arrive over the first few ticks; give them a moment before driving.
  await page.waitForTimeout(3000);

  // Walk the player well off its muster post and lie down in the grass. Both
  // matter: coming back ON the post proves nothing (that is where a fresh
  // arena would put you), and standing up is exactly what the old input
  // encoding did to an absent player.
  await page.keyboard.down('ArrowUp');
  await page.keyboard.down('ArrowRight');
  await page.waitForTimeout(4000);
  await page.keyboard.up('ArrowUp');
  await page.keyboard.up('ArrowRight');

  // Get down, and CHECK rather than assume. Two things make a naive
  // `keyboard.press('KeyC')` unreliable here: the stance buttons read
  // `just_pressed`, and this build renders at single-digit fps under
  // swiftshader — so a press whose keydown and keyup are 10ms apart lands
  // entirely between two frames and is never seen. Hold the key across a frame,
  // then read the stance back out of the blob the client is writing anyway.
  const stanceOf = async () => {
    const blob = await page.evaluate((k) => localStorage.getItem(k), KEY);
    const me = (blob || '').split('\n').find((l) => l.startsWith('pawn 0 '));
    return me ? Number(me.split(/\s+/)[9]) : -1;
  };
  for (let attempt = 0; attempt < 8 && (await stanceOf()) < 2; attempt++) {
    await page.keyboard.down('KeyC');
    await page.waitForTimeout(400);
    await page.keyboard.up('KeyC');
    await page.waitForTimeout(1200);
  }
  await page.waitForTimeout(3000);
  await shoot('before');

  const before = parse(await page.evaluate((k) => localStorage.getItem(k), KEY));
  if (!before) throw new Error('nothing was written to storage at all');
  console.log(`stored: round ${before.roundNumber} tick ${before.roundTicks}, ${before.pawns} pawns`);
  console.log(`  handle 0: ${before.me}`);
  if (before.pawns < 4) throw new Error(`only ${before.pawns} pawns stored; bots never arrived`);
  if (before.roundTicks < 300) throw new Error(`round clock at ${before.roundTicks}; nothing happened`);
  // The setup has to have actually happened, or the assertions below are
  // comparing two identical spawn states and would pass on a broken build.
  const POST = { x: -330 * 256, y: -195 * 256 };
  const field = (line, i) => Number(line.split(/\s+/)[i]);
  const offPost = Math.hypot(field(before.me, 4) - POST.x, field(before.me, 5) - POST.y) / 256;
  console.log(`handle 0 is ${offPost.toFixed(0)} units off its post, stance ${field(before.me, 9)}`);
  if (offPost < 40) throw new Error(`the player never left its post (${offPost.toFixed(0)} units)`);
  if (field(before.me, 9) !== 2) throw new Error(`the player never got prone (stance ${field(before.me, 9)})`);

  console.log('--- refresh ---');
  log.length = 0;
  await page.reload();
  await ready();
  // Read back FAST: the restored world starts moving immediately, so the point
  // is that it starts from where it was, not that it stays there.
  const after = parse(await page.evaluate((k) => localStorage.getItem(k), KEY));
  await page.waitForTimeout(1500);
  await shoot('after');

  const resumed = log.find((l) => l.includes('resuming the stored match'));
  console.log(`resume log: ${resumed || '(none)'}`);
  console.log(`restored: round ${after.roundNumber} tick ${after.roundTicks}, ${after.pawns} pawns`);
  console.log(`  handle 0: ${after.me}`);

  const problems = [];
  if (!resumed) problems.push('the client never said it was resuming a stored match');
  if (after.roundNumber !== before.roundNumber) {
    problems.push(`round ${before.roundNumber} became round ${after.roundNumber}`);
  }
  // The clock may have advanced a few ticks between the reload and the read;
  // what must not happen is it going back to nearly zero (a fresh round).
  if (after.roundTicks < before.roundTicks - 60) {
    problems.push(`round clock went backwards: ${before.roundTicks} → ${after.roundTicks}`);
  }
  if (after.pawns !== before.pawns) {
    problems.push(`${before.pawns} pawns became ${after.pawns}`);
  }
  const stance = (p) => Number(p.split(/\s+/)[9]);
  if (stance(after.me) !== stance(before.me)) {
    problems.push(`came back at stance ${stance(after.me)}, was ${stance(before.me)}`);
  }
  // The thing actually asked for: back where I was, not back on my post.
  const pos = (p) => p && p.split(/\s+/).slice(4, 6).join(',');
  const [wasX, wasY] = pos(before.me).split(',').map(Number);
  const [nowX, nowY] = pos(after.me).split(',').map(Number);
  const moved = Math.hypot(nowX - wasX, nowY - wasY) / 256;
  console.log(`handle 0 moved ${moved.toFixed(1)} world units across the refresh`);
  if (moved > 30) problems.push(`handle 0 came back ${moved.toFixed(0)} units away`);

  await browser.close();
  if (problems.length) {
    console.log('\nFAILED:');
    problems.forEach((p) => console.log(`  - ${p}`));
    process.exit(1);
  }
  console.log('\nPASSED: refreshed back into the same round, same place');
})().catch((e) => {
  console.error(e);
  process.exit(1);
});
