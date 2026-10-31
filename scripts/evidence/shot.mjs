// usage: node shot.mjs <url> <out.png> [width] [height] [fullPage]
import { chromium } from 'playwright';
const [url, out, w = '1100', h = '1400', full = 'false'] = process.argv.slice(2);
const b = await chromium.launch();
const p = await b.newPage({ viewport: { width: +w, height: +h }, deviceScaleFactor: 2 });
await p.goto(url);
await p.waitForTimeout(500);
await p.screenshot({ path: out, fullPage: full === 'true' });
await b.close();
console.log('wrote', out);
