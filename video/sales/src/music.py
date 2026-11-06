# Synthesizes the soundtrack for the sales cut (music + sound design) from timeline.json.
# Pure numpy for the voices, pedalboard for filters, delay, reverb and the master bus.
# 120 BPM in A minor: filtered intro with punch hits, a sidechained drop at the title, a
# half-time verse for the run, dead silence when the worker is killed, a rebuild on resume,
# a groove for the dev loop, a snare-roll build into the feature wall drop, and a held outro.
#   python3 music.py   ->  soundtrack.wav (stereo, 44.1 kHz)
import json, os, wave
import numpy as np
from pedalboard import (Pedalboard, LowpassFilter, HighpassFilter, Reverb, Delay, Compressor,
                        Limiter, Chorus, Bitcrush, Distortion, Gain)

HERE = os.path.dirname(os.path.abspath(__file__))
TL = json.load(open(os.path.join(HERE, "timeline.json")))
SR = 44100
TOTAL = TL["total"]
N = int((TOTAL + 1.0) * SR)
t = np.arange(N, dtype=np.float64) / SR
BEAT = 60.0 / TL["bpm"]
BAR = 4 * BEAT
rng = np.random.default_rng(11)
M = TL["music"]

def midi(n): return 440.0 * 2 ** ((n - 69) / 12)
def fx(board, x): return board(x.astype(np.float32), SR).astype(np.float64)
def lp(x, hz): return fx(Pedalboard([LowpassFilter(cutoff_frequency_hz=hz)]), x)
def hp(x, hz): return fx(Pedalboard([HighpassFilter(cutoff_frequency_hz=hz)]), x)
def curve(keys):
    ks = np.array(keys, dtype=np.float64)
    return np.interp(t, ks[:, 0], ks[:, 1])
def place(buf, shot, at, g=1.0):
    i = int(round(at * SR))
    if i >= len(buf): return
    L = min(len(shot), len(buf) - i)
    buf[i:i + L] += g * shot[:L]
def within(x, name): a, b = M[name]; return a - 1e-6 <= x < b - 1e-6
def beats(a, b, step=BEAT, off=0.0):
    out, x = [], a + off
    while x < b - 1e-6:
        out.append(round(x, 6)); x += step
    return out
def saw_seg(f, n, ph):
    tt = np.arange(n) / SR
    return 2 * ((tt * f + ph) % 1.0) - 1

# ---------- harmony ----------
AM, F, C, G = [57, 60, 64], [53, 57, 60], [55, 60, 64], [55, 59, 62]
ROOT = {id(AM): 33, id(F): 41, id(C): 36, id(G): 43}
PROG = [AM, F, C, G]
def chord_at(x):
    if 56 <= x < 62: return [AM, F, G][min(2, int((x - 56) // BAR))]
    a = 62.0 if x >= 62 else 0.0
    return PROG[int((x - a) // BAR) % 4]
bar_starts = beats(0, 68, BAR)

# ---------- drums ----------
def kick_shot(len_s=0.45):
    tt = np.arange(int(len_s * SR)) / SR
    f = 46 + 120 * np.exp(-tt * 28)
    body = np.sin(2 * np.pi * np.cumsum(f) / SR) * np.exp(-tt * 7.5)
    click = np.exp(-tt * 900) * rng.standard_normal(len(tt)) * .25
    return np.tanh(1.6 * (body + click))
def noise(len_s): return rng.standard_normal(int(len_s * SR))
KICK = kick_shot()
_c = np.zeros(int(.35 * SR))
for d in (0, .011, .022):
    nb = noise(.3); tt = np.arange(len(nb)) / SR
    place(_c, nb * np.exp(-tt * (90 if d < .02 else 18)), d)
CLAP = hp(lp(_c, 3800), 900); CLAP /= np.abs(CLAP).max()
_h = noise(.06); HAT = hp(_h * np.exp(-np.arange(len(_h)) / SR * 75), 7500); HAT /= np.abs(HAT).max()
_o = noise(.25); OHAT = hp(_o * np.exp(-np.arange(len(_o)) / SR * 16), 7000); OHAT /= np.abs(OHAT).max()
_s = noise(.25); _st = np.arange(len(_s)) / SR
SNARE = hp(_s * np.exp(-_st * 22), 1400) * .8 + np.sin(2 * np.pi * 190 * _st) * np.exp(-_st * 30) * .6
SNARE /= np.abs(SNARE).max()

kick = np.zeros(N); clap = np.zeros(N); hats = np.zeros(N); snare = np.zeros(N)
kick_times = []
def K(x, g=1.0): place(kick, KICK, x, g); kick_times.append((x, g))
for x in [2.0, 3.0, 4.0, 5.0, 6.5]: K(x, .8)                         # hook punches
for x in beats(8, 30): K(x)                                         # drop A
for x in beats(30, 39, 2 * BEAT): K(x, .9)                          # half-time verse
for x in beats(40, 42, BAR): K(x, .7)                               # resume: tentative
for x in beats(42, 46): K(x, .9)
for x in beats(46, 56): K(x)
for x in beats(56, 60): K(x)
for x in beats(60, 61.5, BEAT / 2): K(x, .85)
for x in beats(62, 68): K(x, 1.05)
K(68.0, 1.25)
for x in beats(8, 30, BAR, BEAT) + beats(8, 30, BAR, 3 * BEAT): place(clap, CLAP, x, .7)
for x in beats(30, 39, BAR, 2 * BEAT): place(clap, CLAP, x, .75)
for x in beats(46, 56, BAR, BEAT) + beats(46, 56, BAR, 3 * BEAT): place(clap, CLAP, x, .7)
for x in beats(62, 68, BAR, BEAT) + beats(62, 68, BAR, 3 * BEAT): place(clap, CLAP, x, .8)
for x in beats(4, 8, BEAT / 2, BEAT / 2): place(hats, HAT, x, .25 + .25 * (x - 4) / 4)
for a, b in [(8, 30), (42, 46), (46, 56), (62, 68)]:
    for x in beats(a, b, BEAT, BEAT / 2): place(hats, OHAT, x, .45)
    for x in beats(a, b, BEAT / 4):
        if abs((x - a) % (BEAT / 2) - BEAT / 2 + BEAT / 2) > 1e-6: place(hats, HAT, x, .22 + .1 * rng.random())
for x in beats(30, 39, BEAT / 2): place(hats, HAT, x, .3)
# build: accelerating snare roll
x = 56.0
while x < 61.75:
    step = BEAT if x < 58 else BEAT / 2 if x < 60 else BEAT / 4 if x < 61 else BEAT / 8
    place(snare, SNARE, x, .35 + .65 * ((x - 56) / 5.75) ** 1.5); x += step
drums = 1.0 * kick + .55 * fx(Pedalboard([Reverb(room_size=.35, wet_level=.18, dry_level=.9)]), clap) \
        + .35 * hats + .55 * fx(Pedalboard([Reverb(room_size=.5, wet_level=.2, dry_level=.9)]), snare)

# sidechain pump keyed to the kicks
duck = np.zeros(N)
ds = np.arange(int(.45 * SR)) / SR
shape = (1 - np.exp(-ds / .004)) * np.exp(-ds / .12)
for x, g in kick_times:
    i = int(x * SR); L = min(len(shape), N - i); duck[i:i + L] = np.maximum(duck[i:i + L], shape[:L])
pump = 1 - .7 * duck

# ---------- pad (supersaw) ----------
DET = [-.011, -.006, -.002, .002, .006, .011]
padL = np.zeros(N); padR = np.zeros(N)
for b in bar_starts:
    ch = chord_at(b + .01); n = int((BAR + .25) * SR); i = int(b * SR)
    tt = np.arange(n) / SR
    env = np.clip(tt / .03, 0, 1) * np.clip((BAR + .25 - tt) / .25, 0, 1)
    for note in ch + [ch[0] + 12]:
        f = midi(note)
        for k, d in enumerate(DET):
            v = saw_seg(f * (1 + d), n, rng.random()) * env
            L = min(n, N - i)
            (padL if k % 2 == 0 else padR)[i:i + L] += v[:L]
pad_dark = [lp(p, 450) for p in (padL, padR)]
pad_mid = [lp(p, 1400) for p in (padL, padR)]
pad_hi = [lp(p, 4200) for p in (padL, padR)]
bright = curve([(0, 0), (6, .3), (7.9, 1.0), (8, 2.0), (16, 2.0), (16.6, 1.3), (30, 1.3), (30.2, .8), (39, .8),
                (40, .5), (42, .9), (46, 1.4), (56, 1.2), (61.7, 2.0), (62, 2.0), (68, 2.0)])
def blend(d, m, h):
    w0 = np.clip(1 - bright, 0, 1); w1 = 1 - np.abs(bright - 1).clip(0, 1); w2 = np.clip(bright - 1, 0, 1)
    return w0 * d + w1 * m + w2 * h
pad_g = curve([(0, 0), (1.0, .55), (7.8, .7), (8, 1), (16, 1), (16.6, .62), (30, .62), (30.2, .55), (39, .55),
               (40, .35), (46, .62), (56, .6), (61.6, .9), (61.75, 0), (62, 1), (68, 1), (68.01, 0)])
pad = [blend(pad_dark[c], pad_mid[c], pad_hi[c]) * pad_g * np.where(t >= 8, pump, 1.0) * .045 for c in (0, 1)]
pad = [fx(Pedalboard([Chorus(rate_hz=.4, depth=.25, mix=.3), Reverb(room_size=.6, wet_level=.22, dry_level=.85)]), p) for p in pad]

# ---------- bass ----------
bass = np.zeros(N)
def bass_note(x, dur, g, f):
    n = int(dur * SR); tt = np.arange(n) / SR
    env = np.clip(tt / .006, 0, 1) * np.exp(-tt * 3.5) * np.clip((dur - tt) / .02, 0, 1)
    sig = np.sin(2 * np.pi * f * tt) + .25 * np.sin(2 * np.pi * 2 * f * tt)
    place(bass, np.tanh(1.4 * sig) * env, x, g)
for a, b in [(8, 30), (42, 46), (46, 56), (62, 68)]:
    for x in beats(a, b, BEAT / 2, BEAT / 4 * 0 + BEAT / 2 * 0):
        r = midi(ROOT[id(chord_at(x + .01))])
        off = abs(((x - a) / (BEAT / 2)) % 2 - 1) < 1e-6
        bass_note(x, BEAT / 2 * .92, 1.0 if off else .55, r)
for b in beats(30, 39, BAR): bass_note(b, BAR * .98, .8, midi(ROOT[id(chord_at(b + .01))]))
for b in beats(56, 61.75, BEAT / 2): bass_note(b, BEAT / 2 * .9, .4 + .5 * (b - 56) / 6, midi(ROOT[id(chord_at(b + .01))]))
bass = lp(bass, 900) * np.where(t >= 8, .55 + .45 * pump, 1.0)

# ---------- pluck arp ----------
arp = np.zeros(N)
PAT = [0, 1, 2, 3, 2, 1, 2, 3]
def pluck(x, note, g):
    f = midi(note); n = int(.32 * SR); tt = np.arange(n) / SR
    v = (saw_seg(f, n, 0) + saw_seg(f * 1.004, n, .3)) * .5 * np.exp(-tt * 11) * np.clip(tt / .002, 0, 1)
    place(arp, v, x, g)
arp_g = curve([(0, 0), (2, 0), (2.01, .35), (7.9, .7), (8, .9), (16, .9), (16.6, .38), (30, .38), (30.01, 0),
               (46, 0), (46.01, .55), (56, .55), (56.01, .3), (61.7, .6), (61.75, 0), (62, .75), (68, .75), (68.01, 0)])
for x in beats(2, 68, BEAT / 4):
    g = arp_g[min(N - 1, int(x * SR))]
    if g <= 0: continue
    ch = chord_at(x + .001); tones = [ch[0] + 12, ch[1] + 12, ch[2] + 12, ch[0] + 24]
    step = int(round(x / (BEAT / 4)))
    pluck(x, tones[PAT[step % 8]], g * (1.0 if step % 4 == 0 else .7))
arp_dark, arp_hi = lp(arp, 900), lp(arp, 5000)
ab = np.clip(curve([(0, 0), (7.9, .8), (8, 1), (68, 1)]), 0, 1)
arp = (1 - ab) * arp_dark + ab * arp_hi
arp = fx(Pedalboard([Delay(delay_seconds=BEAT * .75, feedback=.35, mix=.25), Reverb(room_size=.5, wet_level=.18, dry_level=.9)]), arp) * .12

# ---------- lead (drop B) ----------
lead = np.zeros(N)
MEL = {0: [(0, 76, 1), (1, 81, .5), (1.5, 79, .5), (2, 76, 1), (3, 72, 1)],
       1: [(0, 77, 1), (1, 81, .5), (1.5, 79, .5), (2, 77, 1), (3, 76, 1)],
       2: [(0, 79, 1), (1, 84, .5), (1.5, 83, .5), (2, 79, 1), (3, 76, 1)]}
for bi in range(3):
    for off, note, dur in MEL[bi]:
        x = 62 + bi * BAR + off * BEAT; d = dur * BEAT * .95; n = int(d * SR); tt = np.arange(n) / SR
        f = midi(note) * (1 + .003 * np.sin(2 * np.pi * 5.5 * tt) * np.clip(tt / .2, 0, 1))
        ph = np.cumsum(f) / SR
        v = (.6 * np.sign(np.sin(2 * np.pi * ph)) + .4 * (2 * (ph % 1) - 1))
        env = np.clip(tt / .008, 0, 1) * (.75 + .25 * np.exp(-tt * 8)) * np.clip((d - tt) / .03, 0, 1)
        place(lead, v * env, x, 1.0)
lead = fx(Pedalboard([LowpassFilter(cutoff_frequency_hz=3800), Chorus(rate_hz=.8, depth=.2, mix=.25),
                      Delay(delay_seconds=BEAT * .75, feedback=.3, mix=.22), Reverb(room_size=.6, wet_level=.25, dry_level=.85)]), lead) * .06

# ---------- risers, downlifters, outro ----------
fxm = np.zeros(N)
def riser(a, b, g):
    n = int((b - a) * SR); tt = np.arange(n) / SR; k = tt / (b - a)
    nz = rng.standard_normal(n)
    sweep = (1 - k) * lp(nz, 900) + k * hp(nz, 2500)
    tone = saw_seg(1, n, 0) * 0
    f = 200 * 2 ** (3 * k); tone = np.sin(2 * np.pi * np.cumsum(f) / SR) * .3
    place(fxm, (sweep + tone) * k ** 2.2, a, g)
def downlifter(a, d, g):
    n = int(d * SR); tt = np.arange(n) / SR; k = tt / d
    nz = rng.standard_normal(n)
    place(fxm, ((1 - k) * hp(nz, 3000) + k * lp(nz, 700)) * (1 - k) ** 2, a, g)
riser(6.0, 8.0, .35); downlifter(8.0, 2.5, .25)
riser(58.0, 61.75, .45); downlifter(62.0, 2.5, .25)
riser(53.6, 56.0, .15)
fxm = fx(Pedalboard([Reverb(room_size=.7, wet_level=.3, dry_level=.8)]), fxm)

outro = np.zeros(N)
n = int(9 * SR); tt = np.arange(n) / SR
env = np.clip(tt / .02, 0, 1) * np.exp(-tt * .35)
for note in [45, 57, 64, 71, 72]:
    f = midi(note)
    outro[int(68 * SR):int(68 * SR) + n] += env * (np.sin(2 * np.pi * f * tt) + .3 * saw_seg(f * 1.003, n, .2)) * .5
for k, note in enumerate([69, 76, 83, 84, 88]):
    x = 69.0 + k * BEAT; m = int(2.5 * SR); u = np.arange(m) / SR
    bell = (np.sin(2 * np.pi * midi(note) * u) + .3 * np.sin(2 * np.pi * midi(note) * 2.76 * u) * np.exp(-u * 6)) * np.exp(-u * 2.2)
    place(outro, bell, x, .45)
outro = fx(Pedalboard([LowpassFilter(cutoff_frequency_hz=3500), Reverb(room_size=.85, wet_level=.4, dry_level=.75)]), outro) * .16

music_mono = drums * .5 + bass * .24 + arp + lead + fxm * .5 + outro
musicL = music_mono + pad[0]; musicR = music_mono + pad[1]
# the crash: everything stops dead for a beat
gap = curve([(0, 1), (38.99, 1), (39.0, 0), (39.98, 0), (40.0, 1), (100, 1)])
fade = curve([(0, 0), (.05, 1), (73.5, 1), (76.5, 0), (100, 0)])
musicL *= gap * fade; musicR *= gap * fade

# ---------- sound design ----------
sfx = np.zeros(N)
def env_t(n, a, dk): tt = np.arange(n) / SR; return np.clip(tt / a, 0, 1) * np.exp(-tt * dk), tt
def S_whoosh(d=.55):
    n = int(d * SR); tt = np.arange(n) / SR; k = tt / d
    nz = rng.standard_normal(n)
    e = np.sin(np.pi * np.clip(k / .7, 0, 1)) ** 2 * np.where(k > .7, np.exp(-(k - .7) * 12), 1)
    return ((1 - k) * lp(nz, 1200) + k * hp(lp(nz, 6000), 1500)) * e * .9
def S_impact():
    n = int(2.2 * SR); tt = np.arange(n) / SR
    f = 38 + 60 * np.exp(-tt * 6)
    boom = np.sin(2 * np.pi * np.cumsum(f) / SR) * np.exp(-tt * 2.2)
    crack = lp(rng.standard_normal(n), 5000) * np.exp(-tt * 14) * .6
    return fx(Pedalboard([Reverb(room_size=.85, wet_level=.35, dry_level=.9)]), np.tanh(1.5 * (boom + crack)))
def S_pop():
    n = int(.09 * SR); tt = np.arange(n) / SR
    f = 260 + 700 * np.exp(-tt * 60)
    return np.sin(2 * np.pi * np.cumsum(f) / SR) * np.exp(-tt * 45) * np.clip(tt / .001, 0, 1)
def S_tick():
    n = int(.03 * SR); tt = np.arange(n) / SR
    return (np.sin(2 * np.pi * 2400 * tt) * .6 + hp(rng.standard_normal(n), 3000) * .4) * np.exp(-tt * 260)
def S_bell(notes, step=.09, dk=5.0):
    n = int(1.4 * SR); out = np.zeros(n + int(step * len(notes) * SR))
    for k, note in enumerate(notes):
        tt = np.arange(n) / SR; f = midi(note)
        b = (np.sin(2 * np.pi * f * tt) + .35 * np.sin(2 * np.pi * 2.0 * f * tt) * np.exp(-tt * 9)) * np.exp(-tt * dk) * np.clip(tt / .002, 0, 1)
        place(out, b, k * step, 1.0)
    return fx(Pedalboard([Reverb(room_size=.5, wet_level=.25, dry_level=.9)]), out)
def S_click():
    n = int(.05 * SR); tt = np.arange(n) / SR
    return hp(lp(rng.standard_normal(n), 6000), 1200) * np.exp(-tt * 240) + np.sin(2 * np.pi * 900 * tt) * np.exp(-tt * 120) * .4
def S_glitch():
    n = int(.5 * SR); out = np.zeros(n); x = 0
    while x < n:
        L = int(rng.integers(600, 3500)); kind = rng.integers(0, 3)
        tt = np.arange(L) / SR
        seg = (np.sign(np.sin(2 * np.pi * rng.uniform(80, 900) * tt)) if kind == 0 else
               rng.standard_normal(L) if kind == 1 else np.sin(2 * np.pi * rng.uniform(1000, 3000) * tt))
        out[x:x + L] = seg[:max(0, min(L, n - x))] * rng.uniform(.4, 1.0); x += L + int(rng.integers(0, 900))
    out = fx(Pedalboard([Bitcrush(bit_depth=5), Distortion(drive_db=10)]), out * .6)
    return out * np.linspace(1, .6, n)
SH = {"whoosh": S_whoosh(), "impact": S_impact(), "pop": S_pop(), "tick": S_tick(),
      "notify": S_bell([88, 93], .11, 4.0), "chime": S_bell([84, 88, 91, 96], .07, 3.5), "click": S_click(), "glitch": S_glitch()}
LEV = {"whoosh": .32, "impact": .9, "pop": .5, "tick": .28, "notify": .28, "chime": .26, "click": .5, "glitch": .55}
for at, kind, g in TL["sfx"]:
    shot = SH[kind]
    if kind == "whoosh": at -= .35            # whooshes peak on the cut
    place(sfx, shot, at, g * LEV[kind])

# typing: mechanical key clicks in each typing window
keys = []
for v in range(8):
    n = int(.045 * SR); tt = np.arange(n) / SR
    c = hp(lp(rng.standard_normal(n), rng.uniform(3500, 6500)), rng.uniform(900, 1800)) * np.exp(-tt * rng.uniform(260, 380))
    c += np.sin(2 * np.pi * rng.uniform(160, 240) * tt) * np.exp(-tt * 140) * .5
    keys.append(c / np.abs(c).max())
for a, b in TL["typing"]:
    x = a
    while x < b:
        place(sfx, keys[int(rng.integers(0, len(keys)))], x, rng.uniform(.06, .12))
        x += max(.035, rng.normal(.075, .02)) + (rng.random() < .06) * rng.uniform(.08, .2)

sfx *= curve([(0, 1), (75, 1), (76.5, .0), (100, 0)])
musicL, musicR = hp(musicL, 32), hp(musicR, 32)
L = musicL + sfx; R = musicR + sfx
st = np.stack([L, R], axis=1).astype(np.float32)
st = Pedalboard([Compressor(threshold_db=-14, ratio=2.5, attack_ms=8, release_ms=120), Gain(gain_db=1.5),
                 Limiter(threshold_db=-1.0, release_ms=80)])(st.T.copy(), SR).T
st = st[: int((TOTAL + .2) * SR)]
st = st / max(1e-9, np.abs(st).max()) * .95
with wave.open(os.path.join(HERE, "soundtrack.wav"), "wb") as w:
    w.setnchannels(2); w.setsampwidth(2); w.setframerate(SR)
    w.writeframes((np.clip(st, -1, 1) * 32767).astype("<i2").tobytes())
print("soundtrack.wav", round(len(st) / SR, 2), "s")
