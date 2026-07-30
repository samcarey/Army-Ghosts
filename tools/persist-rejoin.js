// Two browser tabs in one room; one of them refreshes mid-match.
//
// This is the half the stored blob cannot do: the returning tab's own storage
// is stale by however long the reload took, so what it must come back into is
// the world the OTHER tab is still playing. Everything here is read off the
// console, not the screen.
const { chromium } = require('playwright');

const PORT = process.env.PORT || '8151';
const ROOM = process.env.ROOM || `rj${Date.now().toString(36)}`;
const URL = `http://127.0.0.1:${PORT}/?room=${ROOM}&players=2&bots=2`;

const strip = (s) => s.replace(/%c/g, '').replace(/color:[^%]*/g, '').trim();

(async () => {
  const browser = await chromium.launch({
    args: [
      '--use-gl=swiftshader',
      '--enable-unsafe-swiftshader',
      '--no-sandbox',
      // Headless Chrome cannot resolve mDNS-obfuscated host candidates, and
      // the srflx fallback needs NAT hairpinning — without this the data
      // channels never open. See CLAUDE.md.
      '--disable-features=WebRtcHideLocalIpsWithMdns',
    ],
  });

  // Separate contexts so the two tabs get separate localStorage, and therefore
  // separate player identities — the whole mechanism turns on that id.
  const tabs = [];
  for (const name of ['A', 'B']) {
    const ctx = await browser.newContext({ deviceScaleFactor: 1, viewport: { width: 800, height: 500 } });
    const page = await ctx.newPage();
    const log = [];
    page.on('console', (m) => log.push(strip(m.text())));
    page.on('pageerror', (e) => log.push(`PAGEERROR ${e.message.split('\n')[0]}`));
    tabs.push({ name, ctx, page, log });
  }
  const [A, B] = tabs;

  const await_ = async (tab, needle, limit = 150) => {
    for (let i = 0; i < limit; i++) {
      if (tab.log.some((l) => l.includes(needle))) return true;
      await tab.page.waitForTimeout(1000);
    }
    console.log(`\n--- tab ${tab.name}: last 15 lines ---`);
    tab.log.slice(-15).forEach((l) => console.log(`  ${l}`));
    return false;
  };
  const die = (msg) => { console.log(`\nFAILED: ${msg}`); process.exit(1); };

  console.log(`room ${ROOM}`);
  console.log('--- both tabs join ---');
  await A.page.goto(URL);
  await A.page.waitForSelector('body[data-game-ready="1"]', { timeout: 240000 });
  await A.page.waitForTimeout(2000);
  await B.page.goto(URL);
  await B.page.waitForSelector('body[data-game-ready="1"]', { timeout: 240000 });

  if (!(await await_(A, 'starting generation 0'))) die('tab A never started the match');
  if (!(await await_(B, 'starting generation 0'))) die('tab B never started the match');
  console.log('both tabs are in generation 0');

  const idOf = (tab) => (tab.log.find((l) => l.includes('player id:')) || '').split('player id:')[1]?.trim();
  console.log(`  A is player ${idOf(A)}`);
  console.log(`  B is player ${idOf(B)}`);

  // Let the match run so there is something worth coming back to, and drive
  // tab B off its post so "back where I was" is a claim with teeth.
  await B.page.keyboard.down('ArrowUp');
  await B.page.waitForTimeout(4000);
  await B.page.keyboard.up('ArrowUp');
  await B.page.waitForTimeout(4000);

  console.log('--- tab B refreshes ---');
  A.log.length = 0;
  B.log.length = 0;
  await B.page.reload();
  await B.page.waitForSelector('body[data-game-ready="1"]', { timeout: 240000 });

  if (!(await await_(A, 'is back — resyncing'))) die('tab A never recognised the returning player');
  console.log(`  ${A.log.find((l) => l.includes('is back'))}`);
  if (!(await await_(A, 'starting generation 1'))) die('tab A never moved to generation 1');
  if (!(await await_(B, 'starting generation 1'))) die('tab B never moved to generation 1');
  if (!(await await_(B, 'resuming generation 1'))) die('tab B built a fresh world instead of resuming');

  const round = (tab) => {
    const line = tab.log.find((l) => l.includes('resuming generation 1 at round'));
    return line && line.match(/at round (\d+)/)?.[1];
  };
  await await_(A, 'resuming generation 1');
  console.log(`  A: ${A.log.find((l) => l.includes('resuming generation 1'))}`);
  console.log(`  B: ${B.log.find((l) => l.includes('resuming generation 1'))}`);
  if (!round(A) || round(A) !== round(B)) {
    die(`peers resumed into different rounds: A=${round(A)} B=${round(B)}`);
  }
  console.log(`both resumed into round ${round(A)}`);

  // A resumed session that immediately desyncs would have passed everything
  // above, so this is the assertion that matters — and it is only assertable
  // at all since the round-clock fix. Before it, two tabs desynced at frame 10
  // with nobody refreshing and no bots (`persist-control.js` is that
  // reduction), so this had to report rather than assert. Run the control if
  // this ever starts failing again: it says whether the baseline is broken or
  // the rejoin is.
  await A.page.waitForTimeout(15000);

  // Both peers autosave their world, so a divergence can be read off as "which
  // pawn, which field" instead of as a checksum. Note the two blobs are taken
  // at whatever moment each peer last autosaved, so compare the ROUND TICK
  // before comparing anything else.
  const KEY = `army-ghosts.match.${ROOM}`;
  for (const tab of tabs) {
    const blob = await tab.page.evaluate((k) => localStorage.getItem(k), KEY);
    console.log(`--- tab ${tab.name} world ---`);
    console.log((blob || '(none)').split('\n').slice(1).join('\n'));
  }
  for (const tab of tabs) {
    const desync = tab.log.find((l) => l.includes('DESYNC'));
    if (desync) die(`tab ${tab.name} desynced after the resume: ${desync}`);
  }

  await browser.close();
  console.log(`\nPASSED: tab B refreshed and rejoined into round ${round(A)}, no desync`);
})().catch((e) => { console.error(e); process.exit(1); });
