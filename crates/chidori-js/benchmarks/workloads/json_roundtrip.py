# Python twin of json_roundtrip.js — json.dumps / json.loads round-trips over
# a nested object. separators=(",", ":") matches JSON.stringify's compact
# output, and dicts preserve insertion order, so len(s) agrees with JS.
import json

N = 20_000
obj = {
    "id": 0,
    "name": "widget",
    "tags": ["a", "b", "c"],
    "nested": {"x": 1, "y": 2, "z": {"deep": True, "items": [1, 2, 3, 4, 5]}},
    "flag": False,
}
acc = 0
for i in range(N):
    obj["id"] = i
    s = json.dumps(obj, separators=(",", ":"))
    back = json.loads(s)
    acc += back["id"] + len(back["nested"]["z"]["items"]) + len(s)
print("RESULT=" + str(acc))
