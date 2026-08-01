// Photograph the concealment rig — `Scenario::GrassStrip`, a wall of grass with
// a pawn either side of it and nothing else on the map. Driven by
// tools/grass-shots.sh; see that for usage.
//
//   node grass-shots.js <table.md> <outdir> <baseurl>
//
// The table comes from tools/grass-table.sh, and it is what decides which
// pictures get taken: the depths are its rows and the alpha printed under each
// picture is its measurement of that exact frame. One source of truth
// (STRIP_DEPTHS in client/src/vision/strip_table.rs) drives both.
//
// Four pictures per depth, because those are the four that differ:
//   A  both standing           — the baseline
//   B  east prone              — what going flat buys the target
//   C  west (the camera) prone — what it costs the viewer
//   D  both prone              — the case the model is anchored on
// The east pawn has no player driving it, so its stance is set by the scenario
// (`?scenario=strip:<depth>:<level>`); the west pawn is the local handle and
// goes down with the C key like any other player.
const fs = require('fs');
const path = require('path');
const { chromium } = require('playwright');

const [tableFile, outDir, baseUrl] = process.argv.slice(2);

/// Tall enough that the crop below clears the HUD at top and bottom. The height
/// is set by the BOTTOM of the HUD, which is the taller end: the stance column
/// stands 153 px off the bottom edge and the crop reaches 180 below the camera,
/// so anything under ~706 puts a chevron in every frame. It was 620 while the
/// stance buttons lived on the right edge, out of the crop's width entirely —
/// see the note in CLAUDE.md's Testing section about re-checking this whenever
/// the HUD moves, which is exactly what happened.
const VIEW = { width: 900, height: 760 };
/// The crop: centred on the camera, which the rig parks on the middle of the
/// wall — so it is written as a half-size off the middle of the window rather
/// than as corners, and stays centred whatever the window does. Wide enough for
/// both pawns (96 units apart at STRIP_ZOOM = 0.33, so ~291 px) with ground
/// either side; clear of MENU/health bar/roster above and the stance column
/// below.
const HALF = { x: 320, y: 180 };
const SHOT = {
  x: VIEW.width / 2 - HALF.x,
  y: VIEW.height / 2 - HALF.y,
  width: HALF.x * 2,
  height: HALF.y * 2,
};
/// Camera lerp, TILE_EASE and the fog's move threshold.
const SETTLE = 1200;

const STANCE_LEVEL = { standing: 0, crouching: 1, prone: 2 };

// Rows of the markdown table: depth, west stance, east stance, then the two
// alphas. Keyed "<depth>/<west>/<east>" → east's alpha.
function readTable(text) {
  const alpha = new Map();
  const depths = [];
  for (const line of text.split('\n')) {
    const cells = line.split('|').map((c) => c.trim());
    if (cells.length < 7 || !/^\d+ /.test(cells[1])) continue;
    const [depth, ...label] = cells[1].split(/\s+/);
    if (!depths.some((d) => d.depth === Number(depth))) {
      depths.push({ depth: Number(depth), label: label.join(' ') });
    }
    alpha.set(`${depth}/${cells[2]}/${cells[3]}`, Number(cells[4]));
  }
  return { depths, alpha };
}

/// Playwright's own default is 30 s, which a 900x760 window of software-rendered
/// grass does not reliably make — it started timing out partway through the
/// sheet when the window grew to clear the relocated stance buttons.
const SHOT_TIMEOUT = 120000;

async function shoot(page, file) {
  await page.waitForTimeout(SETTLE);
  await page.screenshot({ path: `${outDir}/${file}`, clip: SHOT, timeout: SHOT_TIMEOUT });
}

// One page per (depth, east stance): the east pawn's stance is baked into the
// URL because nothing can drive it at runtime.
async function scene(browser, depth, eastStance, shots) {
  const page = await browser.newPage({ viewport: VIEW, deviceScaleFactor: 1 });
  try {
    const url = `${baseUrl}/?scenario=strip:${depth}:${STANCE_LEVEL[eastStance]}`;
    await page.goto(url, { waitUntil: 'domcontentloaded' });
    await page.waitForSelector('body[data-game-ready="1"]', { timeout: 180000 });
    // The grass field mesh and the tuft bake land after the game reports ready.
    await page.waitForTimeout(4000);
    for (const { westStance, file } of shots) {
      // C drops the local pawn one level per press and each takes
      // STANCE_DOWN_TICKS to play out; V would bring it back up.
      for (let i = 0; i < STANCE_LEVEL[westStance]; i++) {
        await page.keyboard.press('KeyC');
        await page.waitForTimeout(700);
      }
      await shoot(page, file);
      console.log(`shot depth ${depth}: west ${westStance}, east ${eastStance}`);
    }
  } finally {
    await page.close();
  }
}

// The contact sheet. Each picture is captioned with the alpha the table
// measured for that exact pairing — the picture and the number are the same
// scene, so a tuning change that moves one moves the other.
function sheet({ depths, alpha }, columns) {
  const rows = depths
    .map(({ depth, label }) => {
      const cells = columns
        .map(({ west, east, title }) => {
          const a = alpha.get(`${depth}/${west}/${east}`);
          return `<td><img src="d${depth}-${west}-${east}.png">
            <figcaption>${title}<br><b>alpha ${a === undefined ? '?' : a.toFixed(3)}</b></figcaption></td>`;
        })
        .join('');
      return `<tr><td class="meta"><h2>${depth} deep</h2><p>${label}</p></td>${cells}</tr>`;
    })
    .join('\n');
  return `<!doctype html><meta charset="utf-8"><style>
    body { background:#16181a; color:#e8e6e1; font:14px/1.5 -apple-system,Helvetica,sans-serif; margin:0; padding:28px 32px; }
    h1 { font-size:20px; margin:0 0 4px; }
    .lede, .foot { color:#9a9a96; max-width:1500px; margin:0 0 22px; }
    table { border-collapse:separate; border-spacing:0 16px; }
    td { vertical-align:top; }
    td.meta { width:150px; padding-right:20px; }
    h2 { font-size:17px; margin:0 0 2px; color:#cfe3b8; }
    .meta p { color:#8d8d88; font-size:12px; margin:0; }
    img { display:block; width:400px; border:1px solid #2e3134; }
    figcaption { color:#8d8d88; font-size:12px; padding:4px 0 0; }
    figcaption b { color:#e8e6e1; font-weight:600; }
    code { color:#cfe3b8; }
  </style>
  <h1>The concealment rig, photographed</h1>
  <p class="lede">One hex-wide wall of grass, a pawn one clear hex either side of it, and nothing
  else on the map — the scene <code>client/src/vision/strip_table.rs</code> tabulates, built in the
  game by <code>?scenario=strip:&lt;depth&gt;:&lt;east stance&gt;</code>. The camera is the WEST pawn
  (always drawn solid — you never fade yourself); the EAST pawn is faded by exactly the alpha under
  each frame, which is the table's number for that pairing. Ground darkening is the same
  concealment asked about the terrain.</p>
  <table>${rows}</table>
  <p class="foot">Regenerate: <code>tools/grass-shots.sh</code>. Depths come from
  <code>STRIP_DEPTHS</code>; the wall's width and the standoff from <code>STRIP_HALF_W</code> /
  <code>STRIP_STANDOFF</code> in the sim.</p>`;
}

(async () => {
  const table = readTable(fs.readFileSync(tableFile, 'utf8'));
  if (!table.depths.length) throw new Error('no depths parsed from the table');

  const columns = [
    { west: 'standing', east: 'standing', title: 'both standing' },
    { west: 'standing', east: 'prone', title: 'east prone' },
    { west: 'prone', east: 'standing', title: 'west (camera) prone' },
    // The case the model is built around: two pawns lying either side of the
    // wall must not see each other at all.
    { west: 'prone', east: 'prone', title: 'both prone' },
  ];

  const browser = await chromium.launch({
    args: ['--use-gl=swiftshader', '--enable-unsafe-swiftshader', '--no-sandbox'],
  });
  for (const { depth } of table.depths) {
    // Group by east stance: that one costs a page load, the west stance is a
    // keypress. Within a page the west pawn only ever goes further down, so the
    // shots are ordered by how far down it has to be.
    for (const east of ['standing', 'prone']) {
      const shots = columns
        .filter((c) => c.east === east)
        .sort((a, b) => STANCE_LEVEL[a.west] - STANCE_LEVEL[b.west])
        .map((c) => ({ westStance: c.west, file: `d${depth}-${c.west}-${c.east}.png` }));
      if (shots.length) await scene(browser, depth, east, shots);
    }
  }

  fs.writeFileSync(`${outDir}/sheet.html`, sheet(table, columns));
  const page = await browser.newPage({
    viewport: { width: 2000, height: 900 },
    deviceScaleFactor: 1.5,
  });
  await page.goto(`file://${path.resolve(outDir)}/sheet.html`);
  await page.waitForTimeout(500);
  await page.screenshot({ path: `${outDir}/grass-strip.png`, fullPage: true });
  await browser.close();
  console.log(`contact sheet: ${outDir}/grass-strip.png`);
})();
