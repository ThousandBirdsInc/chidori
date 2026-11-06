# Chidori sales video (76 s)

A music-driven cut for developers: it builds one agent with Chidori from scratch
and shows what that is like at each step, then lands on the features teams need
in production. No voiceover; the on-screen type carries the story, cut to a
120 BPM soundtrack.

The rendered MP4 is not committed; render it with the steps below.

## Story

| Time | Scene | What's on screen |
|------|-------|------------------|
| 0:00 | hook | An empty editor. "Let's build an agent. That calls tools. Waits for a human. Survives a crash. Never does anything twice. In plain TypeScript." |
| 0:08 | title | Chidori wordmark on the drop: "Agents you can trust in production." |
| 0:12 | 01 Start | `chidori init support-bot --template worker` and its real output. One binary, no Node, no config. |
| 0:16 | 02 Write | The agent typed out: a `defineTool` tool, `chidori.prompt` with any model, `chidori.input` for approval, a journaled `fetch`. Every `await` is a durable checkpoint. |
| 0:30 | 03 Run | `chidori run`; the run pauses at the approval and saves to disk until someone answers. |
| 0:38 | 04 Recover | The worker is killed mid-request. `chidori resume` replays the finished steps (the model isn't called again, the approval is kept) and runs only the unfinished one live. |
| 0:46 | 05 Iterate | `chidori dev` re-runs on save and says which recorded step changed; `chidori trace` steps through the run. |
| 0:56 | 06 Ship | `chidori export --fixture` and `chidori verify` turn the run into a CI test; `chidori serve` puts it behind HTTP. |
| 1:02 | features | Twelve capabilities: crash recovery, human-in-the-loop, exactly-once effects, time-travel replay, edit-and-resume, branching runs, supervised actors, signals and webhooks, durable memory, saga rollback, npm without Node, OpenTelemetry. |
| 1:08 | end | Install command and github.com/ThousandBirdsInc/chidori. |

## What's real

- `demo/support-bot/agent.ts` is the agent typed on screen (it adds only the `API`
  constant the video leaves out) and passes `chidori check`.
- Every command and flag on screen exists in chidori 3.8.1 (`init --template`, `run -i`,
  `resume`, `dev`, `trace`, `export --fixture`, `verify --runs-dir`, `serve --port`).
- The `init` output is verbatim. The other terminal panels follow chidori's output
  format (`Run id:`, `seq N: …`, `Resumed from <id> (N recorded calls replayed, M
  executed live)`, `verified: N calls replayed …`) but are condensed for the screen,
  not captured from one run; the REPLAYED/LIVE tags and the trace timeline are
  visual stand-ins for what `resume` and `trace` report.

## Rendering

Sources in `src/`:

- `timeline.json`: the single clock. Scene boundaries, music sections, typing
  windows and every sound effect cue; `scenes.html` and `music.py` both read it.
- `scenes.html`: all scenes. Every animation is a pure function of time, scrubbed
  with `setTime(t)`; `window.TL` is the timeline.
- `music.py`: synthesizes `soundtrack.wav` (music + sound design + key clicks) with
  numpy and [pedalboard](https://github.com/spotify/pedalboard).
- `render.js`: Playwright. `node render.js preview 12 40.5` writes stills to `prev/`;
  `node render.js full 4` renders 60 fps `frames.mp4` in 4 parallel chunks. Set
  `PLAYWRIGHT=` to the package path and `CHROME=` to a Chromium or headless-shell
  binary if Playwright's own isn't installed.
- `fonts/`: Geist and Geist Mono (SIL OFL).

```bash
cd src
pip install numpy pedalboard
python3 music.py && node render.js full 4
ffmpeg -i frames.mp4 -i soundtrack.wav -map 0:v -map 1:a -c:v copy \
  -af loudnorm=I=-14:TP=-1.5:LRA=11 -ar 48000 -c:a aac -b:a 256k \
  -movflags +faststart -t 76 chidori-sales.mp4
```
