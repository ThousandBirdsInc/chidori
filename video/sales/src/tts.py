import json, subprocess, wave, os, re

HERE = os.path.dirname(os.path.abspath(__file__))
VOICE = os.environ.get("VOICE", os.path.join(HERE, "..", "voices", "en-us-ryan-high.onnx"))
SEG_DIR = os.path.join(HERE, "segs")
os.makedirs(SEG_DIR, exist_ok=True)

LEAD = 0.4   # silence before the first segment of a scene
GAP = 0.32   # silence between segments
PAD = 0.7    # silence after the last segment of a scene
RATE = 22050

CAPTION_FIXES = [
    ("A I", "AI"), ("D S L", "DSL"), ("L L M", "LLM"), ("H T T P", "HTTP"), ("C I", "CI"),
    ("Apache two", "Apache 2.0"),
]

def dur(path):
    with wave.open(path) as w:
        return w.getnframes() / w.getframerate()

def silence(sec):
    return b"\x00\x00" * int(round(sec * RATE))

script = json.load(open(os.path.join(HERE, "script.json")))
pcm = bytearray()
t = 0.0
scenes, subs = [], []
for s in script:
    start = t
    pcm += silence(LEAD); t += LEAD
    cues = []
    for i, text in enumerate(s["segs"]):
        if i:
            pcm += silence(GAP); t += GAP
        out = os.path.join(SEG_DIR, f"{s['id']}_{i}.wav")
        subprocess.run(["python3", "-m", "piper", "-m", VOICE, "--length-scale", "0.97",
                        "-f", out], input=text.encode(), check=True, capture_output=True)
        with wave.open(out) as w:
            assert w.getframerate() == RATE and w.getsampwidth() == 2 and w.getnchannels() == 1
            frames = w.readframes(w.getnframes())
        d = dur(out)
        cap = text
        for a, b in CAPTION_FIXES:
            cap = cap.replace(a, b)
        cues.append({"t": round(t - start, 3), "d": round(d, 3), "text": cap})
        subs.append((t, t + d, cap))
        pcm += frames; t += d
    p = s.get("pad", PAD)
    pcm += silence(p); t += p
    scenes.append({"id": s["id"], "start": round(start, 3), "dur": round(t - start, 3), "cues": cues})

with wave.open(os.path.join(HERE, "narration.wav"), "wb") as w:
    w.setnchannels(1); w.setsampwidth(2); w.setframerate(RATE); w.writeframes(bytes(pcm))

json.dump({"total": round(t, 3), "scenes": scenes}, open(os.path.join(HERE, "timings.json"), "w"), indent=1)

def ts(x):
    ms = int(round(x * 1000)); h, ms = divmod(ms, 3600000); m, ms = divmod(ms, 60000); s_, ms = divmod(ms, 1000)
    return f"{h:02}:{m:02}:{s_:02},{ms:03}"
with open(os.path.join(HERE, "captions.srt"), "w") as f:
    for i, (a, b, c) in enumerate(subs, 1):
        f.write(f"{i}\n{ts(a)} --> {ts(b)}\n{c}\n\n")
print("total", round(t, 2))
for s in scenes:
    print(s["id"], s["start"], s["dur"], [c["t"] for c in s["cues"]])
