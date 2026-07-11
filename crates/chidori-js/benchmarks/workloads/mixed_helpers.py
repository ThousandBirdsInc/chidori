# Python twin of mixed_helpers.js — small helper functions over dicts and
# strings, the shape of real agent/tool plumbing: key iteration, per-value
# type dispatch, string building, and dict property traffic across calls.
# All strings are ASCII, so Python len() agrees with JS .length.
N = 60_000


def normalize(rec):
    out = {"id": rec["id"], "label": "", "score": 0}
    for k in rec:
        if k == "id":
            continue
        v = rec[k]
        if isinstance(v, int):
            out["score"] = out["score"] + v
        elif isinstance(v, str):
            out["label"] = out["label"] + ":" + v if out["label"] else v
    return out


def render(rec):
    return "[" + str(rec["id"]) + "] " + (rec["label"] or "?") + " => " + str(rec["score"])


def classify(score):
    return "hi" if score > 40 else "mid" if score > 15 else "lo"


buckets = {"hi": 0, "mid": 0, "lo": 0}
text = ""
checksum = 0
for i in range(N):
    rec = {
        "id": i,
        "name": "item" + str(i % 7),
        "a": i % 13,
        "b": (i * 3) % 29,
        "kind": "x" if i % 2 else "y",
    }
    norm = normalize(rec)
    line = render(norm)
    buckets[classify(norm["score"])] += 1
    checksum = (checksum + len(line) + norm["score"]) % 1000000007
    if (i & 1023) == 0:
        text += line + "\n"
print(
    "RESULT="
    + str(checksum)
    + "|"
    + str(buckets["hi"])
    + ","
    + str(buckets["mid"])
    + ","
    + str(buckets["lo"])
    + "|"
    + str(len(text))
)
