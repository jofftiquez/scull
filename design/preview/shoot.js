// Render every design screen (and key interaction states) to PNG.
const { chromium } = require('playwright');
const fs = require('fs');
const path = require('path');

const SRC = process.env.SRC || path.join(__dirname, '../source');
const PAGES = path.join(__dirname, 'pages');
const OUT = process.argv[2] || path.join(__dirname, 'out');
fs.mkdirSync(PAGES, { recursive: true });
fs.mkdirSync(OUT, { recursive: true });
fs.copyFileSync(path.join(__dirname, 'support.js'), path.join(PAGES, 'support.js'));

const fontDir = path.join(__dirname, 'node_modules/@fontsource');
const face = (fam, file, w) => `@font-face{font-family:'${fam}';font-weight:${w};font-style:normal;src:url('file://${fontDir}/${file}') format('woff2')}`;
const FONTS = '<style>' + [400, 500, 600, 700].map((w) => face('DM Sans', `dm-sans/files/dm-sans-latin-${w}-normal.woff2`, w)).join('') +
  [400, 500].map((w) => face('DM Mono', `dm-mono/files/dm-mono-latin-${w}-normal.woff2`, w)).join('') + '</style>';

for (const f of fs.readdirSync(SRC).filter((x) => x.endsWith('.dc.html'))) {
  let html = fs.readFileSync(path.join(SRC, f), 'utf8');
  html = html.replace(/<link rel="preconnect"[^>]*>\n?/g, '').replace(/<link rel="stylesheet" href="https:\/\/fonts\.googleapis[^>]*>\n?/g, '');
  html = html.replace('<script src="./support.js"></script>', FONTS + '<script src="./support.js"></script>');
  fs.writeFileSync(path.join(PAGES, f), html);
}

const wait = (ms) => new Promise((r) => setTimeout(r, ms));
const SHOTS = [
  { file: 'Main', name: '01-overview' },
  { file: 'Main', name: '01b-overview-scanning', act: async (p) => { await p.getByRole('button', { name: /^Rescan/ }).click(); await wait(1700); } },
  { file: 'Main', name: '01c-overview-quick-clean-done', act: async (p) => { await p.getByRole('button', { name: /^Clean 4 caches/ }).click(); await wait(3600); } },
  { file: 'Onboarding', name: '02-onboarding' },
  { file: 'Onboarding', name: '02b-onboarding-scan-done', act: async (p) => { await p.getByRole('button', { name: 'Start first scan' }).click(); await wait(4000); } },
  { file: 'CleanupReview', name: '03-cleanup-review' },
  { file: 'CleanupReview', name: '03b-cleanup-review-done', act: async (p) => { await p.getByRole('button', { name: /^Delete 4 items/ }).click(); await wait(5200); } },
  { file: 'Worktrees', name: '04-worktrees' },
  { file: 'Worktrees', name: '04b-worktrees-removed', act: async (p) => { await p.getByRole('button', { name: /^Remove worktree, keep branch/ }).click(); await wait(1500); } },
  { file: 'Insights', name: '05-insights' },
  { file: 'Projects', name: '06-projects' },
  { file: 'Projects', name: '06b-projects-cleaned', act: async (p) => { await p.getByRole('button', { name: /^Clean regenerable/ }).click(); await wait(2600); } },
  { file: 'Explorer', name: '07-explorer-drives' },
  { file: 'Explorer', name: '07b-explorer-drilldown', act: async (p) => { await p.getByRole('button', { name: /^Open folder Macintosh HD/ }).click(); await wait(500); await p.getByRole('button', { name: /^Open folder code/ }).click(); await wait(600); } },
  { file: 'Caches', name: '08-caches' },
  { file: 'Caches', name: '08b-caches-done', act: async (p) => { await p.getByRole('button', { name: /^Clean 4 caches/ }).click(); await wait(3600); } },
  { file: 'History', name: '09-history' },
  { file: 'History', name: '09b-history-restored', act: async (p) => { await p.getByRole('button', { name: 'Restore' }).first().click(); await wait(1500); } },
  { file: 'Settings', name: '10-settings-general' },
  { file: 'Settings', name: '10b-settings-drives', act: async (p) => { await p.getByRole('tab', { name: 'Drives' }).click(); await wait(500); } },
  { file: 'Settings', name: '10c-settings-agents', act: async (p) => { await p.getByRole('tab', { name: 'AI agents' }).click(); await wait(500); } },
  { file: 'MenuBar', name: '11-menu-bar-widget', w: 380, h: 660 },
  { file: 'MenuBar', name: '11b-menu-bar-quick-clean-done', w: 380, h: 660, act: async (p) => { await p.getByRole('button', { name: /^Quick Clean/ }).click(); await wait(3000); } }
];

(async () => {
  const browser = await chromium.launch();
  const results = [];
  for (const s of SHOTS) {
    const page = await browser.newPage({ viewport: { width: s.w || 1440, height: s.h || 900 }, deviceScaleFactor: 1, reducedMotion: 'no-preference' });
    const errors = [];
    page.on('pageerror', (e) => errors.push(e.message));
    await page.goto('file://' + path.join(PAGES, s.file + '.dc.html'));
    await page.waitForFunction(() => window.__dcRenders > 0, null, { timeout: 5000 }).catch(() => errors.push('never rendered'));
    await page.evaluate(() => document.fonts.ready);
    await wait(900);
    try { if (s.act) await s.act(page); } catch (e) { errors.push('action: ' + e.message.split('\n')[0]); }
    await wait(400);
    const out = path.join(OUT, s.name + '.png');
    await page.screenshot({ path: out, fullPage: !s.w });
    results.push({ shot: s.name, errors: errors });
    await page.close();
  }
  await browser.close();
  for (const r of results) console.log((r.errors.length ? 'WARN ' : 'ok   ') + r.shot + (r.errors.length ? '  ' + r.errors.join(' | ') : ''));
})();
