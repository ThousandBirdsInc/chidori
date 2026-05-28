# kitchen_sink.star — deliberately exercises as many Starlark language
# constructs and chidori host functions as possible.
#
# Intended as the canonical fixture for bi-directional renderer tests:
# any construct a renderer needs to round-trip through a visual editor
# should appear here at least once. Keep syntactic variety high even if
# the resulting values are contrived.

# --- Agent-level config -----------------------------------------------------

config(
    model = "claude-sonnet-4-6",
    temperature = 0.2,
    max_tokens = 1024,
    max_turns = 5,
    timeout = 30,
)

# --- Module-level constants: literals of every supported data type ----------

NONE_VALUE = None
TRUE_VALUE = True
FALSE_VALUE = False

INT_DECIMAL = 42
INT_HEX = 0xFF
INT_OCTAL = 0o17
INT_BINARY = 0b1010
INT_NEGATIVE = -7
INT_LARGE = 1000000

FLOAT_BASIC = 3.14
FLOAT_EXP = 1.5e-3
FLOAT_NEG = -0.5
FLOAT_ZERO = 0.0

STR_SINGLE = 'single quotes'
STR_DOUBLE = "double quotes"
STR_ESCAPES = "tab:\t newline:\n quote:\" backslash:\\ unicode:\u00e9"
STR_TRIPLE_DOUBLE = """triple-double
spans multiple lines"""
STR_TRIPLE_SINGLE = '''triple-single
also multiline'''
STR_EMPTY = ""

LIST_EMPTY = []
LIST_HOMOG = [1, 2, 3, 4, 5]
LIST_MIXED = [1, "two", 3.0, None, True, [4, 5]]
LIST_NESTED = [[1, 2], [3, 4], [5, 6]]

TUPLE_EMPTY = ()
TUPLE_SINGLE = (1,)
TUPLE_MIXED = (1, "two", 3.0, None)
TUPLE_NESTED = ((1, 2), (3, 4))

DICT_EMPTY = {}
DICT_NESTED = {
    "name": "chidori",
    "version": 1,
    "flags": {"debug": True, "trace": False, "verbose": True},
    "numbers": [1, 2, 3],
    "pairs": [(1, "a"), (2, "b")],
}

# --- Helper functions: default, keyword, *args, **kwargs --------------------

def greet(name, greeting = "Hello", punctuation = "!", *extras, **fields):
    pieces = [greeting, " ", name]
    for e in extras:
        pieces.append(" ")
        pieces.append(str(e))
    pieces.append(punctuation)
    if fields:
        tags = sorted(fields.keys())
        pieces.append(" [")
        pieces.append(",".join([k + "=" + str(fields[k]) for k in tags]))
        pieces.append("]")
    return "".join(pieces)

def identity(x):
    return x

def classify(n):
    if n == 0:
        return "zero"
    elif 0 < n and n < 10:
        return "small"
    elif 10 <= n and n <= 100:
        return "medium"
    elif n > 100:
        return "large"
    else:
        return "negative"

def fibonacci(n):
    a, b = 0, 1
    out = []
    for _ in range(n):
        out.append(a)
        a, b = b, a + b
    return out

def _sum(values):
    total = 0
    for v in values:
        total += v
    return total

def stats(values):
    total = _sum(values)
    n = len(values)
    return {
        "count": n,
        "sum": total,
        "min": min(values) if n > 0 else None,
        "max": max(values) if n > 0 else None,
        "mean": (total / n) if n > 0 else None,
    }

def always_fails():
    fail("deliberate failure for retry demo")

# --- Agent entrypoint -------------------------------------------------------

def agent(name = "world", items = [1, 2, 3, 4, 5], mode = "demo"):
    # f-strings (identifiers only in braces under this dialect)
    polished = name.strip().title()
    full_name = f"{polished} (mode={mode})"
    shout = greet(name, greeting = "Hi", punctuation = "!!", tag = "v1", build = 7)

    # Arithmetic: + - * / // % **
    math_ops = {
        "add": 2 + 3,
        "sub": 10 - 4,
        "mul": 6 * 7,
        "div": 10 / 4,
        "floordiv": 10 // 4,
        "mod": 10 % 3,
        "pow_manual": 2 * 2 * 2 * 2 * 2 * 2 * 2 * 2,
        "neg": -(-5),
        "paren": (1 + 2) * (3 + 4),
        "str_repeat": "ab" * 3,
        "list_repeat": [0] * 4,
    }

    # Bitwise: | & ^ ~ << >>
    bit_ops = {
        "or": 0b1100 | 0b1010,
        "and": 0b1100 & 0b1010,
        "xor": 0b1100 ^ 0b1010,
        "not": ~0b0001,
        "shl": 1 << 4,
        "shr": 256 >> 2,
    }

    # Comparison: == != < > <= >= in, not in, chained
    cmp_ops = {
        "eq": 1 == 1,
        "ne": 1 != 2,
        "lt": 1 < 2,
        "gt": 2 > 1,
        "le": 1 <= 1,
        "ge": 2 >= 2,
        "chain": 0 < 5 and 5 < 10,
        "in_list": 3 in [1, 2, 3],
        "not_in_list": 4 not in [1, 2, 3],
        "in_str": "ell" in "hello",
        "in_dict": "name" in DICT_NESTED,
    }

    # Logical + short-circuit
    logic_ops = {
        "and": True and False,
        "or": True or False,
        "not": not False,
        "combined": (True and not False) or (1 < 2),
        "sc_or": None or "default",
        "sc_and": True and "value",
    }

    # Ternary
    parity = "even" if len(items) % 2 == 0 else "odd"

    # Comprehensions: list, dict, multi-clause, filtered
    squares = [x * x for x in items]
    even_squares = [x * x for x in items if x % 2 == 0]
    pairs = [(i, v) for i, v in enumerate(items)]
    flattened = [y for row in LIST_NESTED for y in row]
    cartesian = [(a, b) for a in [1, 2] for b in ["x", "y"]]
    index_map = {v: i for i, v in enumerate(items)}
    filtered_map = {k: v for k, v in DICT_NESTED["flags"].items() if v}

    # Slicing
    seq = list(range(10))
    slices = {
        "head": seq[:3],
        "tail": seq[-3:],
        "middle": seq[2:7],
        "every_other": seq[::2],
        "reversed": seq[::-1],
        "copy": seq[:],
        "index_first": seq[0],
        "index_last": seq[-1],
        "str_slice": "abcdefg"[1:5],
    }

    # String methods
    phrase = "  The Quick Brown Fox  "
    strings = {
        "stripped": phrase.strip(),
        "lstripped": phrase.lstrip(),
        "rstripped": phrase.rstrip(),
        "lower": phrase.lower(),
        "upper": phrase.upper(),
        "replaced": phrase.replace("Quick", "Slow"),
        "split": phrase.strip().split(" "),
        "rsplit": phrase.strip().rsplit(" ", 1),
        "joined": "-".join(["a", "b", "c"]),
        "starts": phrase.strip().startswith("The"),
        "ends": phrase.strip().endswith("Fox"),
        "count": "abracadabra".count("a"),
        "find": "abracadabra".find("cad"),
        "rfind": "abracadabra".rfind("a"),
        "capitalize": "hello world".capitalize(),
        "title": "hello world".title(),
    }

    # List mutation methods
    acc = []
    acc.append(1)
    acc.append(2)
    acc.extend([3, 4])
    acc.insert(0, 0)
    popped = acc.pop()

    # Dict mutation + methods
    d = {}
    d["a"] = 1
    d["b"] = 2
    d.update({"c": 3, "d": 4})
    got_a = d.get("a", -1)
    got_missing = d.get("missing", "fallback")
    keys_sorted = sorted(d.keys())
    values_list = list(d.values())
    items_list = list(d.items())

    # Dict merge via explicit copy + update
    merged = {}
    for k, v in DICT_NESTED["flags"].items():
        merged[k] = v
    merged["verbose"] = False
    merged["new"] = True

    # Augmented assignment
    counter = 0
    counter += 5
    counter -= 1
    counter *= 2
    counter //= 3
    total = 0
    for x in items:
        total += x

    # Tuple unpacking in loop header + swap
    swapped = []
    a, b = 1, 2
    a, b = b, a
    for i, v in enumerate(items):
        swapped.append((v, i))

    # Built-ins: any / all / reversed / sorted / zip / enumerate
    any_big = any([x > 3 for x in items])
    all_positive = all([x > 0 for x in items])
    reversed_items = list(reversed(items))
    sorted_desc = sorted(items, reverse = True)
    zipped = list(zip(items, ["a", "b", "c", "d", "e"]))

    # Lambdas used as values
    double = lambda x: x * 2
    doubled = [double(x) for x in items]

    # Type coercion built-ins
    conversions = {
        "to_int": int("42"),
        "to_float": float("1.5"),
        "to_str": str(123),
        "to_bool_truthy": bool(1),
        "to_bool_falsy": bool(0),
        "to_list": list((1, 2, 3)),
        "to_tuple": tuple([1, 2, 3]),
        "to_dict": dict(a = 1, b = 2),
        "abs_neg": abs(-7),
        "hash_str": hash("stable") != 0,
        "type_of_list": type([]),
        "type_of_dict": type({}),
        "type_of_int": type(0),
        "repr_list": repr([1, 2]),
        "chr_ord": chr(ord("A") + 1),
    }

    # Host functions: log, env, memory, parallel, try_call, retry, exec_expr
    log("running kitchen sink", name = name, mode = mode, count = len(items))

    home_env = env("HOME") or "/"

    memory("set", key = "ks:last_name", value = name)
    recalled = memory("get", key = "ks:last_name")

    fanout = parallel([
        lambda: _sum(items),
        lambda: [x * x for x in items],
        lambda: greet(name, greeting = "Parallel"),
    ])

    safe_div = try_call(lambda: exec_expr("10 / 0"))
    retry_result = try_call(
        lambda: retry(
            always_fails,
            max_attempts = 2,
            backoff = "exponential",
            initial_delay_ms = 5,
        ),
    )

    expr_result = exec_expr(
        "if a > b then a * 2 else b * 2",
        vars = {"a": 7, "b": 4},
    )

    # Positional + keyword args mixed, nested calls
    greeted_many = [
        greet("alice"),
        greet("bob", greeting = "Hi"),
        greet("carol", "Hey", "?"),
        greet("dan", greeting = "Yo", punctuation = ".", extra1 = 1, extra2 = 2),
    ]

    # Nested function + direct use
    def describe(label, data):
        kind = type(data)
        size = len(data) if kind in ("list", "dict", "string", "tuple") else -1
        return f"{label}: {kind} size={size}"

    descriptions = [
        describe("items", items),
        describe("phrase", phrase),
        describe("math_ops", math_ops),
        describe("counter", counter),
    ]

    # Pass / continue / break in a loop
    collected = []
    for x in range(10):
        if x == 7:
            break
        if x % 2 == 0:
            continue
        collected.append(x)
    if not collected:
        pass

    return {
        "full_name": full_name,
        "shout": shout,
        "parity": parity,
        "classify_examples": {
            "zero": classify(0),
            "small": classify(5),
            "medium": classify(50),
            "large": classify(500),
            "negative": classify(-1),
        },
        "fibonacci_8": fibonacci(8),
        "stats": stats([float(x) for x in items]),
        "math_ops": math_ops,
        "bit_ops": bit_ops,
        "cmp_ops": cmp_ops,
        "logic_ops": logic_ops,
        "squares": squares,
        "even_squares": even_squares,
        "pairs": pairs,
        "flattened": flattened,
        "cartesian": cartesian,
        "index_map": index_map,
        "filtered_map": filtered_map,
        "slices": slices,
        "strings": strings,
        "list_after_mutation": acc,
        "popped": popped,
        "dict_after_mutation": d,
        "got_a": got_a,
        "got_missing": got_missing,
        "keys_sorted": keys_sorted,
        "values_list": values_list,
        "items_list": items_list,
        "merged": merged,
        "counter": counter,
        "total": total,
        "swapped": swapped,
        "swap_result": (a, b),
        "any_big": any_big,
        "all_positive": all_positive,
        "reversed": reversed_items,
        "sorted_desc": sorted_desc,
        "zipped": zipped,
        "doubled": doubled,
        "conversions": conversions,
        "home_env": home_env,
        "recalled_memory": recalled,
        "parallel_results": fanout,
        "safe_div_error": safe_div["error"] != None,
        "retry_error": retry_result["error"] != None,
        "expr_result": expr_result,
        "greeted_many": greeted_many,
        "descriptions": descriptions,
        "collected": collected,
        "literal_samples": {
            "none": NONE_VALUE,
            "true": TRUE_VALUE,
            "false": FALSE_VALUE,
            "int_decimal": INT_DECIMAL,
            "int_hex": INT_HEX,
            "int_octal": INT_OCTAL,
            "int_binary": INT_BINARY,
            "int_large": INT_LARGE,
            "int_negative": INT_NEGATIVE,
            "float_basic": FLOAT_BASIC,
            "float_exp": FLOAT_EXP,
            "float_neg": FLOAT_NEG,
            "float_zero": FLOAT_ZERO,
            "str_single": STR_SINGLE,
            "str_double": STR_DOUBLE,
            "str_empty": STR_EMPTY,
            "str_escapes": STR_ESCAPES,
            "str_triple_double": STR_TRIPLE_DOUBLE,
            "str_triple_single": STR_TRIPLE_SINGLE,
            "tuple_empty": TUPLE_EMPTY,
            "tuple_single": TUPLE_SINGLE,
            "tuple_mixed": TUPLE_MIXED,
            "tuple_nested": TUPLE_NESTED,
            "list_empty": LIST_EMPTY,
            "list_homog": LIST_HOMOG,
            "list_mixed": LIST_MIXED,
            "list_nested": LIST_NESTED,
            "dict_empty": DICT_EMPTY,
            "dict_nested": DICT_NESTED,
        },
        "identity_result": identity(INT_DECIMAL),
    }
