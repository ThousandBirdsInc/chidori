# Chidori sales video (≈80 s)

A marketing cut aimed at teams putting agents into production: it opens on the
problem (crashes, unreproducible bugs, re-billed retries, approvals that hold a
process open), shows Chidori's one boundary (every side effect is a recorded host
call), then proves it with real `chidori` 3.8.1 terminal output: crash and resume,
pause-to-disk approval, and a recorded run verified as a $0 CI test. It ends on
the install command and the GitHub repo.

The rendered MP4 is not committed; render it with the steps below.

## Story

| # | Scene | Narration (short) |
|---|-------|-------------------|
| 1 | hook | Your AI agent nailed the demo. Then it hit production. |
| 2 | pain | Crashes lose the run · bugs you can't reproduce · retries re-bill tokens · approvals hold processes open |
| 3 | reveal | Chidori makes agents dependable by default: every LLM/tool/HTTP call goes through one runtime and is recorded |
| 4 | code | Plain async TypeScript; every `await` is a durable checkpoint |
| 5 | crash | Kill mid-run, `chidori resume` in a new process: 1 recorded call replayed, model not called again |
| 6 | human | `chidori.input()` pauses the run to disk; answer days later |
| 7 | test | `chidori export --fixture` + `chidori verify`: replay in CI, 0 model calls, $0 |
| 8 | stack | One Rust binary; Anthropic, OpenAI, OpenRouter, any OpenAI-compatible endpoint; Apache-2.0 |
| 9 | cta | Ship agents you can trust. Install command + github.com/ThousandBirdsInc/chidori |

## Where the on-screen terminal output comes from

`demo/refund_agent.ts` is the agent shown in scene 4. Scenes 5 and 7 reproduce
the output of these commands, run with the chidori 3.8.1 release binary:

```bash
python3 demo/mock_llm.py &                       # local OpenAI-compatible stub for the model
export CHIDORI_OPENAI_COMPAT_URL=http://127.0.0.1:8977 CHIDORI_OPENAI_COMPAT_KEY=sk-test
cd demo
chidori run refund_agent.ts --model gpt-4o-mini --trusted -i order=A-1007 -i complaint="Package arrived crushed"
#   ...kill -9 while it waits at the approval prompt...
echo approve | chidori resume refund_agent.ts <run-id> --trusted
#   -> Resumed from <run-id> (1 recorded calls replayed, 2 executed live)
chidori export <run-id> --fixture tests/fixtures
chidori verify refund_agent.ts <run-id> --runs-dir tests/fixtures
#   -> verified: 3 calls replayed, 0 workspace re-materialization(s), output identical — $0
```

The model's reply is a stub so the run is reproducible offline; everything
Chidori prints is real. (The `--trusted` flag shown in the video's `run` line
stands for the full command above.)

## Rendering

Sources in `src/`:

- `script.json`: narration, one entry per scene, split into caption-sized segments
- `tts.py`: synthesizes narration with [piper](https://github.com/rhasspy/piper)
  (`en-us-ryan-high` voice; set `VOICE=` or put it in `../voices/`), writes
  `narration.wav`, `timings.json` (scene and cue times plus caption text) and `captions.srt`
- `music.py`: synthesizes the music bed `music.wav` from `timings.json` (numpy only)
- `scenes.html`: every scene; animations are scrubbed to exact times via `setTime(t)`,
  captions are burned in from `timings.json`
- `render.js`: Playwright. `node render.js preview 12 40.5` for stills,
  `node render.js full` for `frames.mp4` (set `PLAYWRIGHT=` to the package path and
  `CHROME=` to a Chromium/headless-shell binary if Playwright's own isn't installed)
- `fonts/`: Inter and JetBrains Mono (SIL OFL)

```bash
cd src
python3 tts.py && python3 music.py && node render.js full
ffmpeg -i narration.wav -i music.wav -filter_complex \
  "[0:a]aresample=44100,aformat=channel_layouts=stereo,asplit=2[n1][n2];[1:a]volume=0.28[m];\
   [m][n2]sidechaincompress=threshold=0.02:ratio=5:attack=25:release=400[md];\
   [n1][md]amix=inputs=2:normalize=0:duration=longest,loudnorm=I=-16:TP=-1.5:LRA=11" \
  -ar 48000 -c:a aac -b:a 192k mix.m4a
ffmpeg -i frames.mp4 -i mix.m4a -i captions.srt -map 0:v -map 1:a -map 2:s \
  -c:v copy -c:a copy -c:s mov_text -movflags +faststart -t 79.77 chidori-sales.mp4
```
