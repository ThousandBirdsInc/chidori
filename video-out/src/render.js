const { chromium } = require('/opt/node22/lib/node_modules/playwright');
const fs = require('fs'), path = require('path'), { spawn } = require('child_process');
const T = JSON.parse(fs.readFileSync(path.join(__dirname, 'timings.json')));
const FPS = 30;
(async () => {
  const b = await chromium.launch({ args: ['--font-render-hinting=none'] });
  const p = await b.newPage({ viewport: { width: 1920, height: 1080 } });
  await p.addInitScript(t => { window.TIMINGS = t; }, T);
  await p.goto('file://' + path.join(__dirname, 'scenes.html'));
  await p.evaluate(() => document.fonts.ready);
  await p.waitForTimeout(300);
  const mode = process.argv[2];
  if (mode === 'preview') {
    fs.mkdirSync(path.join(__dirname, 'prev'), { recursive: true });
    for (const t of process.argv.slice(3).map(Number)) {
      await p.evaluate(t => setTime(t), t);
      await p.screenshot({ path: path.join(__dirname, 'prev', `t${t.toFixed(1)}.png`) });
    }
  } else {
    const out = path.join(__dirname, 'frames.mp4');
    const ff = spawn('ffmpeg', ['-y', '-loglevel', 'error', '-f', 'image2pipe', '-framerate', String(FPS), '-c:v', 'mjpeg', '-i', '-',
      '-c:v', 'libx264', '-preset', 'medium', '-crf', '18', '-pix_fmt', 'yuv420p', out], { stdio: ['pipe', 'inherit', 'inherit'] });
    const n = Math.ceil(T.total * FPS);
    for (let i = 0; i < n; i++) {
      await p.evaluate(t => setTime(t), i / FPS);
      const buf = await p.screenshot({ type: 'jpeg', quality: 93 });
      if (!ff.stdin.write(buf)) await new Promise(r => ff.stdin.once('drain', r));
      if (i % 300 === 0) console.log(`frame ${i}/${n}`);
    }
    ff.stdin.end();
    await new Promise(r => ff.on('close', r));
  }
  await b.close();
})();
