// Renders scenes.html frame by frame (every animation scrubbed to an exact time) and pipes JPEGs to ffmpeg.
//   node render.js preview 3.2 10 41.5   -> prev/t3.2.png ...
//   node render.js full [workers]        -> frames.mp4 (chunks rendered in parallel, then joined)
// Env: PLAYWRIGHT (path to the playwright package), CHROME (optional executable path).
const { chromium } = require(process.env.PLAYWRIGHT || 'playwright');
const fs = require('fs'), path = require('path'), { spawn, execFileSync } = require('child_process');
const TL = JSON.parse(fs.readFileSync(path.join(__dirname, 'timeline.json')));
const FPS = TL.fps;
async function page(b) {
  const p = await b.newPage({ viewport: { width: 1920, height: 1080 } });
  p.on('pageerror', e => console.error('PAGE ERROR', e.message));
  await p.addInitScript(t => { window.TL = t; }, TL);
  await p.goto('file://' + path.join(__dirname, 'scenes.html'));
  await p.evaluate(() => document.fonts.ready);
  await p.waitForTimeout(300);
  return p;
}
async function chunk(b, from, to, out) {
  const p = await page(b);
  const ff = spawn('ffmpeg', ['-y', '-loglevel', 'error', '-f', 'image2pipe', '-framerate', String(FPS), '-c:v', 'mjpeg', '-i', '-',
    '-c:v', 'libx264', '-preset', 'medium', '-crf', '17', '-pix_fmt', 'yuv420p', '-r', String(FPS), out], { stdio: ['pipe', 'inherit', 'inherit'] });
  for (let i = from; i < to; i++) {
    await p.evaluate(t => setTime(t), i / FPS);
    const buf = await p.screenshot({ type: 'jpeg', quality: 94 });
    if (!ff.stdin.write(buf)) await new Promise(r => ff.stdin.once('drain', r));
    if ((i - from) % 300 === 0) console.log(`${path.basename(out)} frame ${i - from}/${to - from}`);
  }
  ff.stdin.end();
  await new Promise(r => ff.on('close', r));
  await p.close();
}
(async () => {
  const b = await chromium.launch({ executablePath: process.env.CHROME || undefined, args: ['--font-render-hinting=none'] });
  const mode = process.argv[2];
  if (mode === 'preview') {
    const p = await page(b);
    fs.mkdirSync(path.join(__dirname, 'prev'), { recursive: true });
    for (const t of process.argv.slice(3).map(Number)) {
      await p.evaluate(t => setTime(t), t);
      await p.screenshot({ path: path.join(__dirname, 'prev', `t${t.toFixed(1)}.png`) });
    }
  } else {
    const W = Number(process.argv[3] || 4), n = Math.ceil(TL.total * FPS), per = Math.ceil(n / W);
    const dir = path.join(__dirname, 'segs'); fs.mkdirSync(dir, { recursive: true });
    const parts = [...Array(W).keys()].map(k => path.join(dir, `part${k}.mp4`));
    await Promise.all(parts.map((o, k) => chunk(b, k * per, Math.min(n, (k + 1) * per), o)));
    fs.writeFileSync(path.join(dir, 'list.txt'), parts.map(p => `file '${p}'`).join('\n'));
    execFileSync('ffmpeg', ['-y', '-loglevel', 'error', '-f', 'concat', '-safe', '0', '-i', path.join(dir, 'list.txt'), '-c', 'copy', path.join(__dirname, 'frames.mp4')]);
    console.log('frames.mp4', n, 'frames');
  }
  await b.close();
})();
