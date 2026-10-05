# Chidori explainer video

- `chidori-explainer.mp4` — 3:04, 1920×1080 @ 30fps, narrated, soft English subtitle track
- `chidori-explainer.srt` — standalone captions

## Re-rendering

Sources live in `src/`:

- `script.json` — narration, split into per-sentence cues
- `tts.py` — synthesizes narration with [piper](https://github.com/rhasspy/piper) (`en_US-ryan-high` voice, expected at `../voices/`), writes `narration.wav`, `timings.json`, `captions.srt`
- `scenes.html` — every scene; all animation is scrubbed to exact times via `setTime(t)`
- `render.js` — Playwright: `node render.js preview <t>...` for stills, `node render.js full` → `frames.mp4`
- `fonts/` — Inter and JetBrains Mono (SIL OFL)

Then mux: `ffmpeg -i frames.mp4 -i mix.m4a -i captions.srt -map 0:v -map 1:a -map 2:s -c:v copy -c:a copy -c:s mov_text out.mp4`
(`mix.m4a` = narration over a quiet ambient pad, loudness-normalized to -16 LUFS).
