#!/usr/bin/env python3
"""Generate `src/unicode_tables.rs` from the Unicode 17.0 UCD files in /tmp/ucd17.

Produces, for RegExp `\\p{...}` property escapes:
  * General_Category values (Lu, Ll, ...) and derived groups (L, LC, C, N, P, S,
    Z, M).
  * Script values (sc).
  * Script_Extensions values (scx).
  * Binary properties (Alphabetic, White_Space, Emoji, ...).
  * Specials: Any, ASCII, Assigned.

Each becomes a sorted/merged `static` `&[(u32,u32)]` range slice. A sorted
`KEYS: &[(&str, &[(u32,u32)])]` maps every loose-matched alias to its slice;
`lookup()` binary-searches it.

Loose matching (UAX44-LM3): lowercase, drop '_', '-', spaces. The `is`-prefix on
binary properties is also accepted.
"""

import os
import re
import sys

UCD = "/tmp/ucd17"
OUT = os.path.join(os.path.dirname(__file__), "..", "src", "unicode_tables.rs")


def loose(s):
    """UAX44-LM3 loose matching key."""
    return re.sub(r"[\s_\-]", "", s).lower()


def parse_ranges_file(path, want_field=1):
    """Yield (lo, hi, value) for each data line. `value` is the `want_field`-th
    semicolon-separated field (0-based after the codepoint field)."""
    out = []
    with open(path, encoding="utf-8") as f:
        for line in f:
            line = line.split("#", 1)[0].strip()
            if not line:
                continue
            parts = [p.strip() for p in line.split(";")]
            cps = parts[0]
            if want_field >= len(parts):
                continue
            value = parts[want_field]
            if ".." in cps:
                lo, hi = cps.split("..")
            else:
                lo = hi = cps
            out.append((int(lo, 16), int(hi, 16), value))
    return out


def merge(ranges):
    """Sort + coalesce a list of (lo, hi) tuples into minimal disjoint ranges."""
    ranges = sorted(ranges)
    merged = []
    for lo, hi in ranges:
        if merged and lo <= merged[-1][1] + 1:
            merged[-1] = (merged[-1][0], max(merged[-1][1], hi))
        else:
            merged.append((lo, hi))
    return merged


def complement(ranges):
    """Complement of merged ranges within [0, 0x10FFFF]."""
    merged = merge(ranges)
    out = []
    cur = 0
    for lo, hi in merged:
        if lo > cur:
            out.append((cur, lo - 1))
        cur = max(cur, hi + 1)
    if cur <= 0x10FFFF:
        out.append((cur, 0x10FFFF))
    return out


# --- Load property/value aliases ---------------------------------------------

def load_property_aliases():
    """Parse PropertyAliases.txt into a list of alias groups (every line is one
    property; all its fields are interchangeable names). Returns
    `groups`: list of (set-of-loose-aliases, short-name)."""
    groups = []
    with open(os.path.join(UCD, "PropertyAliases.txt"), encoding="utf-8") as f:
        for line in f:
            line = line.split("#", 1)[0].strip()
            if not line:
                continue
            names = [p.strip() for p in line.split(";")]
            groups.append(({loose(n) for n in names}, names[0]))
    return groups


def aliases_for(groups, *canonical_names):
    """All loose aliases for whichever group contains any of `canonical_names`."""
    targets = {loose(n) for n in canonical_names}
    out = set(targets)
    for names, _short in groups:
        if names & targets:
            out |= names
    return out


def load_property_alias_names():
    """PropertyAliases.txt as EXACT spellings: list of lists (one per line)."""
    groups = []
    with open(os.path.join(UCD, "PropertyAliases.txt"), encoding="utf-8") as f:
        for line in f:
            line = line.split("#", 1)[0].strip()
            if not line:
                continue
            groups.append([p.strip() for p in line.split(";")])
    return groups


def exact_aliases_for(name_groups, *canonical_names):
    """All EXACT alias spellings for whichever group contains any name."""
    targets = {loose(n) for n in canonical_names}
    out = set(canonical_names)
    for names in name_groups:
        if {loose(n) for n in names} & targets:
            out |= set(names)
    return out


def load_value_aliases(prop):
    """For property `prop` (e.g. 'gc', 'sc'), map loose(alias) -> short value
    name, and short -> list of all aliases."""
    a2short = {}
    short2aliases = {}
    with open(os.path.join(UCD, "PropertyValueAliases.txt"), encoding="utf-8") as f:
        for line in f:
            line = line.split("#", 1)[0].strip()
            if not line:
                continue
            parts = [p.strip() for p in line.split(";")]
            if parts[0] != prop:
                continue
            short = parts[1]
            aliases = parts[1:]
            short2aliases[short] = aliases
            for al in aliases:
                a2short[loose(al)] = short
    return a2short, short2aliases


# --- Build category tables ---------------------------------------------------

def build():
    prop_groups = load_property_aliases()

    # General_Category -----------------------------------------------------
    gc_a2short, gc_short2aliases = load_value_aliases("gc")
    gc_data = parse_ranges_file(os.path.join(UCD, "DerivedGeneralCategory.txt"))
    gc = {}  # short value -> merged ranges
    for lo, hi, val in gc_data:
        gc.setdefault(val, []).append((lo, hi))
    gc = {k: merge(v) for k, v in gc.items()}

    # Derived group categories (union of leaf gc values).
    groups = {
        "L": ["Lu", "Ll", "Lt", "Lm", "Lo"],
        "LC": ["Lu", "Ll", "Lt"],
        "M": ["Mn", "Mc", "Me"],
        "N": ["Nd", "Nl", "No"],
        "P": ["Pc", "Pd", "Ps", "Pe", "Pi", "Pf", "Po"],
        "S": ["Sm", "Sc", "Sk", "So"],
        "Z": ["Zs", "Zl", "Zp"],
        "C": ["Cc", "Cf", "Cs", "Co", "Cn"],
    }
    for g, members in groups.items():
        acc = []
        for m in members:
            acc.extend(gc.get(m, []))
        gc[g] = merge(acc)

    # Scripts --------------------------------------------------------------
    sc_a2short, sc_short2aliases = load_value_aliases("sc")
    sc_data = parse_ranges_file(os.path.join(UCD, "Scripts.txt"))
    sc = {}
    for lo, hi, val in sc_data:
        # Scripts.txt uses long names; normalize to short via alias table.
        short = sc_a2short.get(loose(val), val)
        sc.setdefault(short, []).append((lo, hi))
    sc = {k: merge(v) for k, v in sc.items()}
    # Script=Unknown (Zzzz): code points with no assigned script — the
    # complement of every other Script value (the UCD `@missing` default).
    all_scripted = []
    for ranges in sc.values():
        all_scripted.extend(ranges)
    sc["Zzzz"] = complement(all_scripted)

    # Script_Extensions ----------------------------------------------------
    # Per UAX #24: scx(cp) is the explicit ScriptExtensions.txt value when the
    # code point has one, OTHERWISE it defaults to its Script value. A code
    # point with an explicit scx entry therefore does NOT contribute to its
    # Script value's scx set — important for scx=Common / scx=Inherited, whose
    # explicit-override code points belong to the listed scripts instead.
    scx_explicit = {}     # short script -> list of (lo, hi)
    scx_override_cps = set()  # code points that carry an explicit scx entry
    with open(os.path.join(UCD, "ScriptExtensions.txt"), encoding="utf-8") as f:
        for line in f:
            line = line.split("#", 1)[0].strip()
            if not line:
                continue
            cps, codes = [p.strip() for p in line.split(";")]
            if ".." in cps:
                lo, hi = cps.split("..")
            else:
                lo = hi = cps
            lo, hi = int(lo, 16), int(hi, 16)
            scx_override_cps.update(range(lo, hi + 1))
            for code in codes.split():
                short = sc_a2short.get(loose(code), code)
                scx_explicit.setdefault(short, []).append((lo, hi))

    # Start scx from each Script value MINUS the explicit-override code points,
    # then add the explicit scx entries.
    override_ranges = merge([(c, c) for c in sorted(scx_override_cps)])

    def subtract(ranges, holes):
        """ranges minus holes; both are merged sorted (lo, hi) lists."""
        result = []
        for lo, hi in merge(ranges):
            cur = lo
            for hl, hh in holes:
                if hh < cur or hl > hi:
                    continue
                if hl > cur:
                    result.append((cur, min(hl - 1, hi)))
                cur = max(cur, hh + 1)
                if cur > hi:
                    break
            if cur <= hi:
                result.append((cur, hi))
        return merge(result)

    scx = {}
    for val, ranges in sc.items():
        base = subtract(ranges, override_ranges)
        if base:
            scx[val] = list(base)
    for val, ranges in scx_explicit.items():
        scx.setdefault(val, []).extend(ranges)
    scx = {k: merge(v) for k, v in scx.items()}

    # Binary properties ----------------------------------------------------
    binary = {}  # canonical short name -> merged ranges

    def add_binary_file(path):
        for lo, hi, prop in parse_ranges_file(path):
            if prop == "InCB":
                continue  # enumerated, not binary
            binary.setdefault(prop, []).append((lo, hi))

    add_binary_file(os.path.join(UCD, "PropList.txt"))
    add_binary_file(os.path.join(UCD, "DerivedCoreProperties.txt"))
    add_binary_file(os.path.join(UCD, "DerivedBinaryProperties.txt"))
    add_binary_file(os.path.join(UCD, "emoji-data.txt"))

    # DerivedNormalizationProps.txt holds many non-binary normalization
    # properties (NFKC_CF, NFC_QC, ...) that are NOT `\p{}`-queryable; only
    # Changes_When_NFKC_Casefolded is a binary property test262 escapes.
    dnp = os.path.join(UCD, "DerivedNormalizationProps.txt")
    if os.path.exists(dnp):
        for lo, hi, prop in parse_ranges_file(dnp):
            if prop == "Changes_When_NFKC_Casefolded":
                binary.setdefault(prop, []).append((lo, hi))

    binary = {k: merge(v) for k, v in binary.items()}

    # Specials -------------------------------------------------------------
    specials = {
        "Any": [(0, 0x10FFFF)],
        "ASCII": [(0, 0x7F)],
        "Assigned": complement(gc.get("Cn", [])),
    }

    # ---------------------------------------------------------------------
    # Assemble the slice table: a unique slice per distinct range set, then a
    # KEYS list mapping every loose alias to the right slice index.
    # ---------------------------------------------------------------------
    slices = []          # list of (slice_ident, ranges)

    def add_slice(name, ranges):
        slices.append((name, ranges))

    # Build slices with stable identifiers.
    for val, ranges in sorted(gc.items()):
        add_slice("GC_" + val.upper(), ranges)
    for val, ranges in sorted(sc.items()):
        add_slice("SC_" + re.sub(r"[^A-Za-z0-9]", "_", val).upper(), ranges)
    for val, ranges in sorted(scx.items()):
        add_slice("SCX_" + re.sub(r"[^A-Za-z0-9]", "_", val).upper(), ranges)
    for val, ranges in sorted(binary.items()):
        add_slice("BIN_" + re.sub(r"[^A-Za-z0-9]", "_", val).upper(), ranges)
    for val, ranges in sorted(specials.items()):
        add_slice("SP_" + val.upper(), ranges)

    # ---------------------------------------------------------------------
    # Build KEYS: EXACT key -> slice index. ECMA-262 uses STRICT matching
    # (no UAX44-LM3 loose matching): only the exact spellings listed in
    # PropertyAliases.txt / PropertyValueAliases.txt are valid, the property
    # set is the spec's fixed tables (table-nonbinary-unicode-properties and
    # table-binary-unicode-properties), lone names must be a General_Category
    # value or a listed binary property (NOT a script), and anything else is
    # a SyntaxError.
    # ---------------------------------------------------------------------
    name_groups = load_property_alias_names()
    keys = {}  # exact key -> slice ident

    # ECMA-262 table-binary-unicode-properties (canonical names; their exact
    # aliases come from PropertyAliases.txt). UCD binary properties NOT in
    # this list (Hyphen, Other_*, Grapheme_Link, ...) are SyntaxErrors.
    ES_BINARY = {
        "ASCII_Hex_Digit", "Alphabetic", "Bidi_Control", "Bidi_Mirrored",
        "Case_Ignorable", "Cased", "Changes_When_Casefolded",
        "Changes_When_Casemapped", "Changes_When_Lowercased",
        "Changes_When_NFKC_Casefolded", "Changes_When_Titlecased",
        "Changes_When_Uppercased", "Dash", "Default_Ignorable_Code_Point",
        "Deprecated", "Diacritic", "Emoji", "Emoji_Component",
        "Emoji_Modifier", "Emoji_Modifier_Base", "Emoji_Presentation",
        "Extended_Pictographic", "Extender", "Grapheme_Base",
        "Grapheme_Extend", "Hex_Digit", "ID_Continue", "ID_Start",
        "IDS_Binary_Operator", "IDS_Trinary_Operator", "Ideographic",
        "Join_Control", "Logical_Order_Exception", "Lowercase", "Math",
        "Noncharacter_Code_Point", "Pattern_Syntax", "Pattern_White_Space",
        "Quotation_Mark", "Radical", "Regional_Indicator",
        "Sentence_Terminal", "Soft_Dotted", "Terminal_Punctuation",
        "Unified_Ideograph", "Uppercase", "Variation_Selector",
        "White_Space", "XID_Continue", "XID_Start",
    }

    def exact_value_aliases(short2aliases, val):
        aliases = set(short2aliases.get(val, [val]))
        aliases.add(val)
        return aliases

    # gc: bare value aliases + gc=Value / General_Category=Value (exact).
    gc_prop_names = exact_aliases_for(name_groups, "gc", "General_Category")
    for val in gc:
        ident = "GC_" + val.upper()
        for al in exact_value_aliases(gc_short2aliases, val):
            keys[al] = ident                         # bare \p{Lu}, \p{Letter}
            for pa in gc_prop_names:                 # \p{gc=Lu}, \p{General_Category=Lu}
                keys[pa + "=" + al] = ident

    # sc: Script=Value only — a LONE script name is a SyntaxError in ECMA-262
    # (LoneUnicodePropertyNameOrValue covers gc values + binary props only).
    sc_prop_names = exact_aliases_for(name_groups, "sc", "Script")
    for val in sc:
        ident = "SC_" + re.sub(r"[^A-Za-z0-9]", "_", val).upper()
        for al in exact_value_aliases(sc_short2aliases, val):
            for pa in sc_prop_names:
                keys[pa + "=" + al] = ident

    # scx: Script_Extensions=Value only. Uses sc value aliases.
    scx_prop_names = exact_aliases_for(name_groups, "scx", "Script_Extensions")
    for val in scx:
        ident = "SCX_" + re.sub(r"[^A-Za-z0-9]", "_", val).upper()
        for al in exact_value_aliases(sc_short2aliases, val):
            for pa in scx_prop_names:
                keys[pa + "=" + al] = ident

    # binary: bare exact aliases, restricted to the ES table.
    for prop in binary:
        if prop not in ES_BINARY:
            continue
        ident = "BIN_" + re.sub(r"[^A-Za-z0-9]", "_", prop).upper()
        for al in exact_aliases_for(name_groups, prop):
            keys[al] = ident

    # specials: bare exact names (ECMA-262 lists Any/ASCII/Assigned).
    for val in specials:
        keys[val] = "SP_" + val.upper()

    return slices, keys


def emit(slices, keys):
    lines = []
    lines.append("// @generated by scripts/gen_unicode_tables.py from Unicode 17.0 UCD.")
    lines.append("// Do not edit by hand. Regenerate with:")
    lines.append("//     python3 crates/chidori-js/scripts/gen_unicode_tables.py")
    lines.append("//")
    lines.append("// Range slices are sorted, disjoint, and inclusive on both ends.")
    lines.append("#![allow(clippy::all)]")
    lines.append("")

    for name, ranges in slices:
        body = ", ".join(f"(0x{lo:X},0x{hi:X})" for lo, hi in ranges)
        lines.append(f"static {name}: &[(u32, u32)] = &[{body}];")
    lines.append("")

    # KEYS sorted by exact key.
    lines.append("/// (exact key, ranges), sorted by key for binary search (strict matching).")
    lines.append("static KEYS: &[(&str, &[(u32, u32)])] = &[")
    for key in sorted(keys):
        ident = keys[key]
        esc = key.replace("\\", "\\\\").replace('"', '\\"')
        lines.append(f'    ("{esc}", {ident}),')
    lines.append("];")
    lines.append("")

    lines.append(r'''/// Resolve a RegExp `\p{...}` property name (the brace contents) to its
/// code-point ranges. ECMA-262 uses STRICT matching: only the exact alias
/// spellings from the UCD alias tables are valid (no case folding, no
/// inserted/removed separators — UAX44-LM3 loose matching is a SyntaxError),
/// lone names must be a General_Category value or a spec-listed binary
/// property, and scripts require the `Script=` / `Script_Extensions=` form.
/// Returns `None` for anything else.
pub fn lookup(name: &str) -> Option<&'static [(u32, u32)]> {
    KEYS.binary_search_by(|(k, _)| (*k).cmp(name))
        .ok()
        .map(|i| KEYS[i].1)
}
''')

    with open(OUT, "w", encoding="utf-8") as f:
        f.write("\n".join(lines))


def main():
    slices, keys = build()
    emit(slices, keys)
    print(f"wrote {OUT}: {len(slices)} slices, {len(keys)} keys", file=sys.stderr)


if __name__ == "__main__":
    main()
