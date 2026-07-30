// Control: two tabs, bots, NO refresh. Does a plain p2p match with bots stay
// in sync at all? If not, the rejoin is innocent.
const { chromium } = require('playwright');
const PORT = process.env.PORT || '8151';
const BOTS = process.env.BOTS || '2';
const ROOM = `ct${Date.now().toString(36)}`;
const URL = `http://127.0.0.1:${PORT}/?room=${ROOM}&players=2&bots=${BOTS}`;
const strip = (s) => s.replace(/%c/g, '').replace(/color:[^%]*/g, '').trim();
(async () => {
  const browser = await chromium.launch({ args: ['--use-gl=swiftshader','--enable-unsafe-swiftshader','--no-sandbox','--disable-features=WebRtcHideLocalIpsWithMdns'] });
  const tabs = [];
  for (const name of ['A','B']) {
    const ctx = await browser.newContext({ deviceScaleFactor: 1, viewport: { width: 800, height: 500 } });
    const page = await ctx.newPage(); const log = [];
    page.on('console', (m) => log.push(strip(m.text())));
    tabs.push({ name, page, log });
  }
  const [A,B] = tabs;
  await A.page.goto(URL); await A.page.waitForSelector('body[data-game-ready="1"]', { timeout: 240000 });
  await A.page.waitForTimeout(2000);
  await B.page.goto(URL); await B.page.waitForSelector('body[data-game-ready="1"]', { timeout: 240000 });
  for (let i = 0; i < 150; i++) {
    if (A.log.some(l => l.includes('starting generation 0'))) break;
    await A.page.waitForTimeout(1000);
  }
  console.log(`bots=${BOTS}: generation 0 up, watching 60s for a desync`);
  for (let i = 0; i < 12; i++) {
    await A.page.waitForTimeout(5000);
    const d = [...A.log, ...B.log].find(l => l.includes('DESYNC'));
    if (d) { console.log(`DESYNCED after ~${(i+1)*5}s: ${d}`); await browser.close(); process.exit(1); }
  }
  console.log('60s with no desync');
  await browser.close();
})().catch(e => { console.error(e); process.exit(1); });
