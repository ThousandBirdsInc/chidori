# Synthesizes the music bed (music.wav) for the sales cut, timed to timings.json.
# Pure numpy, no samples: a minor-key pad that stays sparse and tense through the
# problem section, then a kick + bass pulse from the reveal on, ending on a held chord.
import json, os, wave
import numpy as np

HERE = os.path.dirname(os.path.abspath(__file__))
SR = 44100
T = json.load(open(os.path.join(HERE, "timings.json")))
sc = {s["id"]: s for s in T["scenes"]}
TOTAL = T["total"]
N = int(TOTAL * SR) + SR
t = np.arange(N) / SR
rng = np.random.default_rng(7)

BPM = 104.0
BEAT = 60.0 / BPM
REVEAL = sc["reveal"]["start"]
CTA = sc["cta"]["start"]

def midi(n): return 440.0 * 2 ** ((n - 69) / 12)

def lowpass(x, cutoff):
    # one-pole low-pass, cutoff in Hz (scalar)
    a = np.exp(-2 * np.pi * cutoff / SR)
    acc = 0.0
    out = np.zeros_like(x)
    b0 = 1 - a
    for i in range(0, len(x), 4096):
        seg = x[i:i + 4096]
        o = np.empty_like(seg)
        for j, v in enumerate(seg):
            acc = b0 * v + a * acc
            o[j] = acc
        out[i:i + 4096] = o
    return out

def saw(f, ph=0.0):
    return 2 * ((t * f + ph) % 1.0) - 1

def env_adsr(start, dur, a=1.2, r=1.6):
    e = np.clip((t - start) / a, 0, 1) * np.clip((start + dur + r - t) / r, 0, 1)
    e[t < start] = 0
    return e

# A minor: Am - F - C - G, two bars each
CHORDS = [[57, 60, 64, 69], [53, 57, 60, 65], [48, 55, 60, 64], [55, 59, 62, 67]]
BAR = 4 * BEAT
pad = np.zeros(N)
def add_chord(ch, start, dur):
    global pad
    e = env_adsr(start, dur, a=0.9, r=1.4)
    for n in ch:
        f = midi(n)
        pad += e * (0.5 * saw(f * 0.997, rng.random()) + 0.5 * saw(f * 1.003, rng.random()) + 0.6 * np.sin(2 * np.pi * f / 2 * t))
start, ci = 0.0, 0
while start < CTA - 0.01:
    dur = min(2 * BAR, CTA - start)
    add_chord(CHORDS[ci % 4], start, dur)
    start += dur; ci += 1
add_chord([57, 60, 64, 71], CTA, TOTAL - CTA + 1.5)  # final held chord: Am add9
# brighter after the reveal: crossfade between a dark and a brighter filtered copy
dark = lowpass(pad, 700.0)
bright = lowpass(pad, 1900.0)
k = np.clip((t - REVEAL) / 1.5, 0, 1)
pad = (1 - k) * dark + k * bright * 0.85
pad *= 0.05

# kick on quarters from the reveal to the CTA, plus one hit on the CTA downbeat
kick = np.zeros(N)
def add_kick(at, g=1.0):
    i = int(at * SR); L = int(0.35 * SR)
    if i + L > N: return
    tt = np.arange(L) / SR
    f = 50 + 90 * np.exp(-tt * 30)
    kick[i:i + L] += g * np.sin(2 * np.pi * np.cumsum(f) / SR) * np.exp(-tt * 9)
b = REVEAL
while b < CTA - 0.05:
    add_kick(b, 0.9 if (round((b - REVEAL) / BEAT) % 4 == 0) else 0.7)
    b += BEAT
add_kick(CTA, 1.2)
add_kick(sc["hook"]["start"] + sc["hook"]["cues"][1]["t"], 1.3)  # "Then it hit production."

# bass: 8th-note root pulse after the reveal
bass = np.zeros(N)
b = REVEAL; step = 0
while b < CTA - 0.05:
    bar_idx = int((b - 0) / (2 * BAR)) % 4
    f = midi(CHORDS[bar_idx][0] - 12)
    i = int(b * SR); L = int(BEAT / 2 * SR * 0.9)
    tt = np.arange(L) / SR
    bass[i:i + L] += np.sin(2 * np.pi * f * tt) * np.exp(-tt * 6) * (1.0 if step % 2 == 0 else 0.6)
    b += BEAT / 2; step += 1

# ticking hats through the problem section (clock), steadier after the reveal
hat = np.zeros(N)
noise = rng.standard_normal(int(0.05 * SR))
hp = noise - np.convolve(noise, np.ones(8) / 8, mode="same")
def add_hat(at, g):
    i = int(at * SR); L = len(hp)
    if i + L > N: return
    hat[i:i + L] += g * hp * np.exp(-np.arange(L) / SR * 70)
b = sc["pain"]["start"]
while b < CTA - 0.05:
    in_pain = b < REVEAL
    add_hat(b, 0.10 if in_pain else 0.13)
    b += BEAT / 2

# riser into the reveal
rise = np.zeros(N)
rs, re_ = REVEAL - 2.0, REVEAL
m = (t >= rs) & (t < re_)
rise[m] = rng.standard_normal(m.sum()) * ((t[m] - rs) / (re_ - rs)) ** 2
rise = lowpass(rise, 3000.0) * 0.12

mix = pad + 0.32 * kick + 0.16 * bass + hat + rise
# master fade in/out
mix *= np.clip(t / 0.4, 0, 1) * np.clip((TOTAL + 0.8 - t) / 2.2, 0, 1)
mix = mix[: int((TOTAL + 0.5) * SR)]
mix /= max(1e-9, np.abs(mix).max()) / 0.8
st = np.stack([mix, mix], axis=1)
with wave.open(os.path.join(HERE, "music.wav"), "wb") as w:
    w.setnchannels(2); w.setsampwidth(2); w.setframerate(SR)
    w.writeframes((st * 32767).astype("<i2").tobytes())
print("music.wav", round(len(mix) / SR, 2), "s")
