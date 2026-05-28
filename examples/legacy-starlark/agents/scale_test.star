# scale_test.star — renderer stress fixture.
#
# Target: ~2x kitchen_sink node count (~700 after parsing). Not meant
# to be realistic; meant to STRESS the canvas with many local bindings,
# long arithmetic chains, deeply nested literals, and many helper defs.
# If the renderer slows down or misbehaves, this is where we catch it.

config(
    model = "claude-sonnet-4-6",
    temperature = 0.1,
    max_tokens = 2048,
)

# --- Wide module-level literal tables --------------------------------------

TABLE_A = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]
TABLE_B = [2, 4, 6, 8, 10, 12, 14, 16, 18, 20, 22, 24, 26, 28, 30, 32]
TABLE_C = [1, 3, 5, 7, 9, 11, 13, 15, 17, 19, 21, 23, 25, 27, 29, 31]
TABLE_D = [100, 200, 300, 400, 500, 600, 700, 800, 900, 1000]

NESTED_TREE = {
    "root": {
        "left": {
            "left": {"value": 1, "tag": "a"},
            "right": {"value": 2, "tag": "b"},
        },
        "right": {
            "left": {"value": 3, "tag": "c"},
            "right": {
                "left": {"value": 4, "tag": "d"},
                "right": {"value": 5, "tag": "e"},
            },
        },
    },
    "meta": {
        "counts": {"nodes": 9, "leaves": 5, "depth": 4},
        "flags": {"balanced": False, "sorted": True, "indexed": True},
        "weights": [0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8],
    },
}

MATRIX = [
    [1, 2, 3, 4, 5],
    [6, 7, 8, 9, 10],
    [11, 12, 13, 14, 15],
    [16, 17, 18, 19, 20],
    [21, 22, 23, 24, 25],
]

# --- Many small helper defs -------------------------------------------------

def add2(a, b):
    return a + b

def add3(a, b, c):
    return a + b + c

def mul2(a, b):
    return a * b

def mul3(a, b, c):
    return a * b * c

def square(x):
    return x * x

def cube(x):
    return x * x * x

def clamp(x, lo, hi):
    if x < lo:
        return lo
    elif x > hi:
        return hi
    else:
        return x

def sign(x):
    if x > 0:
        return 1
    elif x < 0:
        return -1
    else:
        return 0

def sum_list(values):
    total = 0
    for v in values:
        total += v
    return total

def product_list(values):
    total = 1
    for v in values:
        total *= v
    return total

def max_of(a, b, c, d):
    return max(a, b, c, d)

def min_of(a, b, c, d):
    return min(a, b, c, d)

def describe_row(row):
    return {
        "len": len(row),
        "sum": sum_list(row),
        "first": row[0],
        "last": row[-1],
    }

def weighted_sum(a, b, c, wa, wb, wc):
    return a * wa + b * wb + c * wc

def poly3(x, a, b, c, d):
    return a * x * x * x + b * x * x + c * x + d

def poly4(x, a, b, c, d, e):
    return a * x * x * x * x + b * x * x * x + c * x * x + d * x + e

def blend(a, b, t):
    return a * (1 - t) + b * t

def dot4(a0, a1, a2, a3, b0, b1, b2, b3):
    return a0 * b0 + a1 * b1 + a2 * b2 + a3 * b3

def norm2(x, y):
    return x * x + y * y

def norm3(x, y, z):
    return x * x + y * y + z * z

def manhattan(x1, y1, x2, y2):
    return abs(x1 - x2) + abs(y1 - y2)

def bucket(n):
    if n < 10:
        return "tiny"
    elif n < 50:
        return "small"
    elif n < 200:
        return "medium"
    elif n < 1000:
        return "large"
    else:
        return "huge"

def score_row(row):
    return {
        "sum": sum_list(row),
        "first_squared": row[0] * row[0],
        "last_cubed": row[-1] * row[-1] * row[-1],
        "bucket": bucket(sum_list(row)),
    }

# --- Entrypoint: many local bindings, long chains, nested literals ----------

def agent(seed = 7):
    # Long arithmetic chains — each binop is its own Expression node.
    chain_a = 1 + 2 + 3 + 4 + 5 + 6 + 7 + 8 + 9 + 10
    chain_b = 2 * 3 * 4 * 5 * 6 * 7 * 8
    chain_c = 100 - 5 - 4 - 3 - 2 - 1 - 10 - 20 - 30
    chain_d = seed + 1 + 2 + 3 + 4 + 5 + 6 + 7 + 8
    chain_e = seed * 2 + seed * 3 + seed * 4 + seed * 5
    chain_f = (1 + 2) * (3 + 4) * (5 + 6) * (7 + 8)
    chain_g = 1 + 2 * 3 + 4 * 5 + 6 * 7 + 8 * 9
    chain_h = seed + seed + seed + seed + seed + seed
    chain_i = 1 * 2 + 3 * 4 + 5 * 6 + 7 * 8 + 9 * 10 + 11 * 12
    chain_j = seed + 1 - 2 + 3 - 4 + 5 - 6 + 7 - 8 + 9
    chain_k = (seed + 1) * (seed + 2) * (seed + 3)
    chain_l = (seed - 1) * (seed - 2) + (seed - 3) * (seed - 4)
    chain_m = seed * seed + seed * 2 + seed + 1
    chain_n = seed * seed * seed - seed * seed + seed - 1
    chain_o = 1 + 2 + 3 + 4 + 5 + 6 + 7 + 8 + 9 + 10 + 11 + 12
    chain_p = 2 * 2 * 2 * 2 * 2 * 2 * 2 * 2 * 2 * 2

    # Wide comparisons
    cmp_a = seed > 0 and seed < 100
    cmp_b = seed == 7 or seed == 8 or seed == 9
    cmp_c = seed != 0 and seed != 1 and seed != 2

    # Many helper calls
    s1 = add2(1, 2)
    s2 = add2(3, 4)
    s3 = add2(5, 6)
    s4 = add3(1, 2, 3)
    s5 = add3(4, 5, 6)
    s6 = add3(7, 8, 9)
    m1 = mul2(2, 3)
    m2 = mul2(4, 5)
    m3 = mul3(2, 3, 4)
    m4 = mul3(5, 6, 7)
    sq1 = square(seed)
    sq2 = square(seed + 1)
    sq3 = square(seed + 2)
    cu1 = cube(seed)
    cu2 = cube(seed + 1)
    cl1 = clamp(seed, 0, 10)
    cl2 = clamp(seed * 3, 0, 20)
    cl3 = clamp(seed - 5, -5, 5)
    sg1 = sign(seed)
    sg2 = sign(seed - 10)
    sg3 = sign(0)

    # Even more helper calls
    ws1 = weighted_sum(1, 2, 3, 10, 20, 30)
    ws2 = weighted_sum(seed, seed + 1, seed + 2, 2, 3, 5)
    ws3 = weighted_sum(sq1, sq2, sq3, 1, 1, 1)
    pl1 = poly3(seed, 1, 2, 3, 4)
    pl2 = poly3(seed + 1, 2, 3, 4, 5)
    pl3 = poly4(seed, 1, 1, 1, 1, 1)
    pl4 = poly4(seed - 1, 2, 0, 3, 0, 5)
    bl1 = blend(1, 10, 2)
    bl2 = blend(seed, seed * 2, 3)
    dp1 = dot4(1, 2, 3, 4, 5, 6, 7, 8)
    dp2 = dot4(seed, seed + 1, seed + 2, seed + 3, 2, 3, 5, 7)
    nr1 = norm2(3, 4)
    nr2 = norm2(seed, seed + 1)
    nr3 = norm3(1, 2, 3)
    nr4 = norm3(seed, seed + 1, seed + 2)
    mh1 = manhattan(0, 0, 3, 4)
    mh2 = manhattan(seed, seed, 10, 10)
    bk1 = bucket(chain_a)
    bk2 = bucket(chain_b)
    bk3 = bucket(chain_c)
    bk4 = bucket(chain_k)

    # Build wide intermediate values
    totals_a = sum_list(TABLE_A)
    totals_b = sum_list(TABLE_B)
    totals_c = sum_list(TABLE_C)
    totals_d = sum_list(TABLE_D)
    prods_a = product_list([1, 2, 3, 4])
    prods_b = product_list([2, 3, 5, 7])

    max_a = max_of(s1, s2, s3, s4)
    max_b = max_of(m1, m2, m3, m4)
    min_a = min_of(s1, s2, s3, s4)
    min_b = min_of(sq1, sq2, sq3, cu1)

    # Describe every row of the matrix
    row0 = describe_row(MATRIX[0])
    row1 = describe_row(MATRIX[1])
    row2 = describe_row(MATRIX[2])
    row3 = describe_row(MATRIX[3])
    row4 = describe_row(MATRIX[4])

    # Score every row too
    score0 = score_row(MATRIX[0])
    score1 = score_row(MATRIX[1])
    score2 = score_row(MATRIX[2])
    score3 = score_row(MATRIX[3])
    score4 = score_row(MATRIX[4])

    # Deeply nested literal: each level is its own Collection node.
    deep = {
        "l1": {
            "l2": {
                "l3": {
                    "l4": {
                        "l5": {
                            "value": seed,
                            "chain": chain_a + chain_b + chain_c,
                            "list": [sq1, sq2, sq3, cu1, cu2],
                        },
                        "sibling": [1, 2, 3, 4],
                    },
                    "sibling": [5, 6, 7, 8],
                },
                "sibling": [9, 10, 11, 12],
            },
            "sibling": [13, 14, 15, 16],
        },
        "top_list": [
            {"id": 1, "v": s1},
            {"id": 2, "v": s2},
            {"id": 3, "v": s3},
            {"id": 4, "v": m1},
            {"id": 5, "v": m2},
            {"id": 6, "v": m3},
        ],
    }

    # Many separate augmented-assignment bindings
    acc = 0
    acc += 1
    acc += 2
    acc += 3
    acc += 4
    acc += 5
    acc *= 2
    acc -= 10

    prod = 1
    prod *= 2
    prod *= 3
    prod *= 4
    prod *= 5

    # Host calls
    log("scale test start", seed = seed, chain_a = chain_a, chain_b = chain_b)

    memory("set", key = "scale:seed", value = seed)
    memory("set", key = "scale:chain", value = chain_a)
    memory("set", key = "scale:acc", value = acc)
    recalled = memory("get", key = "scale:seed")

    p1 = prompt("Summarize the number " + str(chain_a))
    p2 = prompt("Summarize the number " + str(chain_b))
    p3 = prompt("Summarize the number " + str(chain_c))

    log("scale test mid", totals_a = totals_a, totals_b = totals_b)

    # Long boolean chain
    flags = (
        cmp_a and cmp_b and cmp_c
        and sg1 != 0
        and sg2 != 0
        and max_a > min_a
        and max_b > min_b
    )

    # Extra scattered bindings to pad node count
    extra_1 = chain_a + 1
    extra_2 = chain_a + 2
    extra_3 = chain_a + 3
    extra_4 = chain_b + 1
    extra_5 = chain_b + 2
    extra_6 = chain_b + 3
    extra_7 = chain_c + 1
    extra_8 = chain_c + 2
    extra_9 = chain_d + 1
    extra_10 = chain_d + 2
    extra_11 = chain_e * 2
    extra_12 = chain_e * 3
    extra_13 = chain_f + chain_g
    extra_14 = chain_f + chain_h
    extra_15 = chain_g + chain_h
    extra_16 = chain_i + 1
    extra_17 = chain_i * 2
    extra_18 = chain_j + chain_k
    extra_19 = chain_j - chain_l
    extra_20 = chain_m + chain_n
    extra_21 = chain_m * 2 + 1
    extra_22 = chain_o + chain_p
    extra_23 = chain_o * chain_p
    extra_24 = ws1 + ws2 + ws3
    extra_25 = pl1 + pl2 + pl3 + pl4
    extra_26 = bl1 + bl2 + dp1 + dp2
    extra_27 = nr1 + nr2 + nr3 + nr4
    extra_28 = mh1 + mh2
    extra_29 = sq1 + sq2 + sq3 + cu1 + cu2
    extra_30 = cl1 + cl2 + cl3 + sg1 + sg2 + sg3

    # Another wide nested literal to exercise Collection nodes
    ledger = {
        "pairs": [
            {"k": "a", "v": extra_1},
            {"k": "b", "v": extra_2},
            {"k": "c", "v": extra_3},
            {"k": "d", "v": extra_4},
            {"k": "e", "v": extra_5},
            {"k": "f", "v": extra_6},
            {"k": "g", "v": extra_7},
            {"k": "h", "v": extra_8},
        ],
        "tuples": [
            (1, extra_9),
            (2, extra_10),
            (3, extra_11),
            (4, extra_12),
            (5, extra_13),
            (6, extra_14),
            (7, extra_15),
            (8, extra_16),
        ],
        "chains": {
            "a": chain_a, "b": chain_b, "c": chain_c, "d": chain_d,
            "e": chain_e, "f": chain_f, "g": chain_g, "h": chain_h,
            "i": chain_i, "j": chain_j, "k": chain_k, "l": chain_l,
            "m": chain_m, "n": chain_n, "o": chain_o, "p": chain_p,
        },
        "derived": {
            "weighted": [ws1, ws2, ws3],
            "polynomials": [pl1, pl2, pl3, pl4],
            "blends": [bl1, bl2],
            "dots": [dp1, dp2],
            "norms": [nr1, nr2, nr3, nr4],
            "manhattans": [mh1, mh2],
            "buckets": [bk1, bk2, bk3, bk4],
        },
    }

    log("scale test end", acc = acc, prod = prod, flags = flags)

    return {
        "seed": seed,
        "chain_a": chain_a,
        "chain_b": chain_b,
        "chain_c": chain_c,
        "chain_d": chain_d,
        "chain_e": chain_e,
        "chain_f": chain_f,
        "chain_g": chain_g,
        "chain_h": chain_h,
        "cmp_a": cmp_a,
        "cmp_b": cmp_b,
        "cmp_c": cmp_c,
        "sums": [s1, s2, s3, s4, s5, s6],
        "muls": [m1, m2, m3, m4],
        "squares": [sq1, sq2, sq3],
        "cubes": [cu1, cu2],
        "clamps": [cl1, cl2, cl3],
        "signs": [sg1, sg2, sg3],
        "totals": {
            "a": totals_a,
            "b": totals_b,
            "c": totals_c,
            "d": totals_d,
        },
        "prods": {"a": prods_a, "b": prods_b},
        "max_a": max_a,
        "max_b": max_b,
        "min_a": min_a,
        "min_b": min_b,
        "rows": [row0, row1, row2, row3, row4],
        "scores": [score0, score1, score2, score3, score4],
        "ledger": ledger,
        "derived_flat": [
            ws1, ws2, ws3, pl1, pl2, pl3, pl4, bl1, bl2,
            dp1, dp2, nr1, nr2, nr3, nr4, mh1, mh2,
            bk1, bk2, bk3, bk4,
        ],
        "deep": deep,
        "acc": acc,
        "prod": prod,
        "recalled": recalled,
        "prompts": [p1, p2, p3],
        "flags": flags,
        "extras": [
            extra_1, extra_2, extra_3, extra_4, extra_5,
            extra_6, extra_7, extra_8, extra_9, extra_10,
            extra_11, extra_12, extra_13, extra_14, extra_15,
            extra_16, extra_17, extra_18, extra_19, extra_20,
            extra_21, extra_22, extra_23, extra_24, extra_25,
            extra_26, extra_27, extra_28, extra_29, extra_30,
        ],
    }
