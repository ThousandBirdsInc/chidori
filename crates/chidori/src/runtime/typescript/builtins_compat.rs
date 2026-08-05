//! Shim sources for the rest of the `node:` builtin module suite.
//!
//! `builtins.rs` holds the original, heavily-exercised shims (`process`,
//! `buffer`, `fs`, `crypto`, `http`, …); this module completes coverage so
//! *every* Node builtin specifier resolves and links. Modules whose behavior
//! is expressible deterministically (`querystring`, `stream`,
//! `string_decoder`, `punycode`, `diagnostics_channel`, `zlib` via the
//! flate2-backed native, …) get working implementations; modules that
//! require capabilities the runtime deliberately does not grant
//! (subprocesses, raw sockets, threads, arbitrary eval contexts) get
//! fail-loud stubs: the import
//! links, the module exposes Node's surface, and each entry point throws a
//! clear "not supported in the Chidori runtime" error at first use — never a
//! silent no-op.
//!
//! Registered through `builtins::shim_source`, which consults this module
//! after its own table. `BUILTIN_NAMES`/`NODE_BUILTIN_ALLOWLIST` list every
//! name served from either file.

// node:querystring — pure logic, full parse/stringify surface. Unlike
// URLSearchParams, querystring encodes spaces as %20 and supports custom
// separators.
const QUERYSTRING_SHIM: &str = r#"
import { Buffer } from "node:buffer";
// Node's escape() is not encodeURIComponent: it walks UTF-16 units itself, so
// a lone surrogate at the end of the input is an `ERR_INVALID_URI` URIError
// (and a lone surrogate followed by any unit is folded into a 4-byte sequence,
// exactly as Node's `encodeStr` does). The literal set is Node's `noEscape`
// table: unreserved ASCII plus !'()*-._~.
const hexTable = [];
for (let i = 0; i < 256; i++) {
    hexTable.push("%" + (i < 16 ? "0" : "") + i.toString(16).toUpperCase());
}
const noEscape = new Uint8Array(128);
{
    const literal = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789!'()*-._~";
    for (let i = 0; i < literal.length; i++) noEscape[literal.charCodeAt(i)] = 1;
}
function invalidURI() {
    const err = new URIError("URI malformed");
    err.code = "ERR_INVALID_URI";
    return err;
}
function encodeStr(str) {
    const len = str.length;
    if (len === 0) return "";
    let out = "";
    let lastPos = 0;
    for (let i = 0; i < len; i++) {
        let c = str.charCodeAt(i);
        if (c < 0x80) {
            if (noEscape[c] === 1) continue;
            if (lastPos < i) out += str.slice(lastPos, i);
            lastPos = i + 1;
            out += hexTable[c];
            continue;
        }
        if (lastPos < i) out += str.slice(lastPos, i);
        if (c < 0x800) {
            lastPos = i + 1;
            out += hexTable[0xc0 | (c >> 6)] + hexTable[0x80 | (c & 0x3f)];
            continue;
        }
        if (c < 0xd800 || c >= 0xe000) {
            lastPos = i + 1;
            out += hexTable[0xe0 | (c >> 12)] + hexTable[0x80 | ((c >> 6) & 0x3f)] +
                hexTable[0x80 | (c & 0x3f)];
            continue;
        }
        // Surrogate: consume the next unit unconditionally (Node does the same).
        i++;
        if (i >= len) throw invalidURI();
        const c2 = str.charCodeAt(i) & 0x3ff;
        lastPos = i + 1;
        c = 0x10000 + (((c & 0x3ff) << 10) | c2);
        out += hexTable[0xf0 | (c >> 18)] + hexTable[0x80 | ((c >> 12) & 0x3f)] +
            hexTable[0x80 | ((c >> 6) & 0x3f)] + hexTable[0x80 | (c & 0x3f)];
    }
    if (lastPos === 0) return str;
    if (lastPos < len) return out + str.slice(lastPos);
    return out;
}
function qsEscape(str) {
    if (typeof str !== "string") {
        // Node coerces objects with String() (toString/valueOf may throw a
        // TypeError, which is the intended surface) and everything else — a
        // Symbol included — through `+ ""`.
        if (typeof str === "object") str = String(str);
        else str += "";
    }
    return encodeStr(str);
}
// Node's `querystring.unescapeBuffer` — a byte-level percent-decoder that
// never throws: an unterminated or non-hex escape is emitted verbatim (and
// re-scanned, so `%%2a` decodes to `%*`), which is why `unescape()` can fall
// back to it when `decodeURIComponent` rejects the input. Mirrors
// lib/querystring.js's state machine, `hasHex` short-circuit included.
function unhexDigit(code) {
    if (code >= 0x30 && code <= 0x39) return code - 0x30;
    if (code >= 0x41 && code <= 0x46) return code - 0x41 + 10;
    if (code >= 0x61 && code <= 0x66) return code - 0x61 + 10;
    return -1;
}
function unescapeBuffer(s, decodeSpaces) {
    s = String(s);
    const out = Buffer.allocUnsafe(s.length);
    let index = 0;
    let outIndex = 0;
    const maxLength = s.length - 2;
    let hasHex = false;
    while (index < s.length) {
        let currentChar = s.charCodeAt(index);
        if (currentChar === 43 && decodeSpaces) {
            out[outIndex++] = 32;
            index++;
            continue;
        }
        if (currentChar === 37 && index < maxLength) {
            currentChar = s.charCodeAt(++index);
            const hexHigh = unhexDigit(currentChar);
            if (hexHigh < 0) {
                out[outIndex++] = 37;
                continue;
            } else {
                const nextChar = s.charCodeAt(++index);
                const hexLow = unhexDigit(nextChar);
                if (hexLow < 0) {
                    out[outIndex++] = 37;
                    index--;
                } else {
                    hasHex = true;
                    currentChar = hexHigh * 16 + hexLow;
                }
            }
        }
        out[outIndex++] = currentChar;
        index++;
    }
    return hasHex ? out.slice(0, outIndex) : out;
}
function qsUnescape(str, decodeSpaces) {
    const s = String(str);
    try { return decodeURIComponent(s); } catch { return unescapeBuffer(s, decodeSpaces).toString(); }
}
function stringifyPrimitive(v) {
    if (typeof v === "string") return v;
    if (typeof v === "number" && isFinite(v)) return String(v);
    if (typeof v === "bigint") return String(v);
    if (typeof v === "boolean") return String(v);
    return "";
}
function parse(qs, sep, eq, options) {
    sep = sep || "&";
    eq = eq || "=";
    const obj = Object.create(null);
    if (typeof qs !== "string" || qs.length === 0) return obj;
    let maxKeys = 1000;
    if (options && typeof options.maxKeys === "number") maxKeys = options.maxKeys;
    // Node reads the decoder off the module object, so replacing
    // `querystring.unescape` re-points `parse` too — and counts as a *custom*
    // decoder, which changes the gating below.
    let decode = querystring.unescape;
    if (options && typeof options.decodeURIComponent === "function") {
        decode = options.decodeURIComponent;
    }
    const customDecode = decode !== qsUnescape;
    // Node only reaches for the decoder when the slice actually *looks*
    // encoded — a `%` followed by two hex digits (its `encodeCheck` state
    // machine). That gate is observable: `parse('%Ā=%ā')` keeps its
    // keys verbatim precisely because the decoder is never called. A custom
    // decoder is always called (Node seeds `keyEncoded = customDecode`), and
    // it sees `+` as `%20` rather than a literal space.
    const plusChar = customDecode ? "%20" : " ";
    const looksEncoded = /%[0-9A-Fa-f]{2}/;
    function decodeSlice(s) {
        if (s.length === 0) return s;
        if (!customDecode && !looksEncoded.test(s)) return s;
        // A throwing decoder is not fatal: Node retries through its own
        // never-throwing `unescape` (with `+` → space enabled).
        try { return decode(s); } catch { return qsUnescape(s, true); }
    }
    const parts = qs.split(sep);
    const limit = maxKeys > 0 ? Math.min(parts.length, maxKeys) : parts.length;
    for (let i = 0; i < limit; i++) {
        const part = parts[i];
        if (part.length === 0) continue;
        const idx = part.indexOf(eq);
        let key, value;
        if (idx === -1) {
            key = decodeSlice(part.split("+").join(plusChar));
            value = "";
        } else {
            key = decodeSlice(part.slice(0, idx).split("+").join(plusChar));
            value = decodeSlice(part.slice(idx + eq.length).split("+").join(plusChar));
        }
        if (Object.prototype.hasOwnProperty.call(obj, key)) {
            if (Array.isArray(obj[key])) obj[key].push(value);
            else obj[key] = [obj[key], value];
        } else {
            obj[key] = value;
        }
    }
    return obj;
}
function stringify(obj, sep, eq, options) {
    sep = sep || "&";
    eq = eq || "=";
    const encode = options && typeof options.encodeURIComponent === "function"
        ? options.encodeURIComponent
        : qsEscape;
    if (obj === null || typeof obj !== "object") return "";
    const parts = [];
    for (const key of Object.keys(obj)) {
        const value = obj[key];
        const ek = encode(stringifyPrimitive(key));
        if (Array.isArray(value)) {
            for (const item of value) parts.push(ek + eq + encode(stringifyPrimitive(item)));
        } else {
            parts.push(ek + eq + encode(stringifyPrimitive(value)));
        }
    }
    return parts.join(sep);
}
const querystring = {
    parse, stringify, decode: parse, encode: stringify,
    escape: qsEscape, unescape: qsUnescape, unescapeBuffer,
};
export { parse, stringify, parse as decode, stringify as encode, qsEscape as escape, qsUnescape as unescape, unescapeBuffer };
export default querystring;
"#;

// node:string_decoder — a StringDecoder that buffers incomplete multi-byte
// sequences across write() calls, matching Node's streaming semantics for the
// encodings Buffer supports here. The state machine mirrors Node's own
// (`lastChar`/`lastNeed`/`lastTotal`, undocumented but asserted on by Node's
// tests): partial characters are held in `lastChar` and completed by the next
// write, `end()` flushes whatever is left (a replacement character for utf8,
// the buffered half of a surrogate for utf16le, padded base64) and resets the
// state so the decoder is reusable afterwards. Written as a function + prototype
// rather than a class so `StringDecoder.call(obj)` works, like Node.
const STRING_DECODER_SHIM: &str = r#"
import { Buffer } from "node:buffer";

const kState = Symbol("kStringDecoderState");

function normalizeEncoding(encoding) {
    if (encoding === undefined || encoding === null) return "utf8";
    if (typeof encoding === "string") {
        const e = encoding.toLowerCase();
        if (e === "utf8" || e === "utf-8") return "utf8";
        if (e === "utf16le" || e === "utf-16le" || e === "ucs2" || e === "ucs-2") return "utf16le";
        if (e === "latin1" || e === "binary") return "latin1";
        if (e === "ascii") return "ascii";
        if (e === "base64") return "base64";
        if (e === "base64url") return "base64url";
        if (e === "hex") return "hex";
    }
    const err = new TypeError("Unknown encoding: " + String(encoding));
    err.code = "ERR_UNKNOWN_ENCODING";
    throw err;
}
function receivedHelper(input) {
    if (input === null) return " Received null";
    if (input === undefined) return " Received undefined";
    const t = typeof input;
    if (t === "function") return " Received function " + (input.name || "(anonymous)");
    if (t === "object") {
        const ctor = input.constructor;
        if (ctor && typeof ctor.name === "string" && ctor.name.length !== 0) {
            return " Received an instance of " + ctor.name;
        }
        return " Received [Object: null prototype] {}";
    }
    let inspected = t === "string" ? "'" + input + "'"
        : t === "bigint" ? String(input) + "n"
        : t === "symbol" ? input.toString()
        : String(input);
    if (inspected.length > 25) inspected = inspected.slice(0, 25) + "...";
    return " Received type " + t + " (" + inspected + ")";
}
function checkThis(self) {
    if (self === undefined || self === null || !self[kState]) {
        const err = new TypeError('Value of "this" must be of type StringDecoder');
        err.code = "ERR_INVALID_THIS";
        throw err;
    }
}
// StringDecoder works in bytes; anything ArrayBufferView-shaped is reinterpreted
// as a Buffer over the same memory so the encoding helpers below can use
// Buffer.prototype.toString.
function asBuffer(view) {
    if (Buffer.isBuffer(view)) return view;
    return Buffer.from(view.buffer, view.byteOffset, view.byteLength);
}
function copyInto(src, dst, dstStart, srcStart, srcEnd) {
    dst.set(src.subarray(srcStart, srcEnd), dstStart);
}

// Total width and the legal range of the *second* byte for a UTF-8 lead byte,
// or null when the byte cannot start a sequence (a stray continuation byte, or
// one of C0/C1/F5..FF which UTF-8 never uses). The second-byte range is what
// rules out overlong encodings, surrogate code points and anything above
// U+10FFFF, so partial sequences are rejected as early as Buffer.toString does.
function utf8Lead(b) {
    if (b >= 0xc2 && b <= 0xdf) return [2, 0x80, 0xbf];
    if (b >= 0xe0 && b <= 0xef) return [3, b === 0xe0 ? 0xa0 : 0x80, b === 0xed ? 0x9f : 0xbf];
    if (b >= 0xf0 && b <= 0xf4) return [4, b === 0xf0 ? 0x90 : 0x80, b === 0xf4 ? 0x8f : 0xbf];
    return null;
}
// How many trailing bytes of `buf` form a sequence that is still valid but not
// yet complete, and so must wait for the next chunk. Anything already known to
// be malformed stays in place for the decoder to turn into U+FFFD.
function utf8IncompleteTail(buf, i) {
    const len = buf.length;
    for (let back = 1; back <= 3 && len - back >= i; back++) {
        const p = len - back;
        const lead = utf8Lead(buf[p]);
        if (lead === null) {
            // A continuation byte may still belong to a lead further back.
            if (buf[p] >= 0x80 && buf[p] <= 0xbf) continue;
            return 0;
        }
        if (lead[0] <= back) return 0;
        for (let k = 1; k < back; k++) {
            const b = buf[p + k];
            const lo = k === 1 ? lead[1] : 0x80;
            const hi = k === 1 ? lead[2] : 0xbf;
            if (b < lo || b > hi) return 0;
        }
        return back;
    }
    return 0;
}
function utf8Text(self, buf, i) {
    const keep = utf8IncompleteTail(buf, i);
    if (keep === 0) {
        self.lastNeed = 0;
        self.lastTotal = 0;
        return buf.toString("utf8", i);
    }
    const start = buf.length - keep;
    self.lastTotal = utf8Lead(buf[start])[0];
    self.lastNeed = self.lastTotal - keep;
    copyInto(buf, self.lastChar, 0, start, buf.length);
    return buf.toString("utf8", i, start);
}
// UTF-8 carries the held-back bytes forward by prepending them to the next
// chunk: they produced no output on their own, so re-decoding them is free of
// side effects and keeps invalid-sequence handling identical to a single write.
function utf8Write(self, buf) {
    if (self.lastTotal !== 0) {
        const seen = self.lastTotal - self.lastNeed;
        const merged = Buffer.allocUnsafe(seen + buf.length);
        copyInto(self.lastChar, merged, 0, 0, seen);
        copyInto(buf, merged, seen, 0, buf.length);
        self.lastNeed = 0;
        self.lastTotal = 0;
        buf = merged;
    }
    return utf8Text(self, buf, 0);
}
// UTF-16LE needs byte pairs, and a trailing high surrogate must wait for its
// low half before it can be emitted.
function utf16Text(self, buf, i) {
    const n = buf.length;
    if ((n - i) % 2 === 0) {
        // Inspect the trailing code unit as bytes rather than as the last
        // character of the decoded string: an unpaired surrogate cannot
        // survive a round trip through this engine's UTF-8 strings.
        if (n - i >= 2) {
            const c = buf[n - 2] | (buf[n - 1] << 8);
            if (c >= 0xd800 && c <= 0xdbff) {
                self.lastNeed = 2;
                self.lastTotal = 4;
                self.lastChar[0] = buf[n - 2];
                self.lastChar[1] = buf[n - 1];
                return buf.toString("utf16le", i, n - 2);
            }
        }
        return buf.toString("utf16le", i);
    }
    self.lastNeed = 1;
    self.lastTotal = 2;
    self.lastChar[0] = buf[buf.length - 1];
    return buf.toString("utf16le", i, buf.length - 1);
}
// base64 emits four characters per three bytes, so hold back the remainder.
function base64Text(self, buf, i) {
    const n = (buf.length - i) % 3;
    if (n === 0) return buf.toString(self.encoding, i);
    self.lastNeed = 3 - n;
    self.lastTotal = 3;
    if (n === 1) {
        self.lastChar[0] = buf[buf.length - 1];
    } else {
        self.lastChar[0] = buf[buf.length - 2];
        self.lastChar[1] = buf[buf.length - 1];
    }
    return buf.toString(self.encoding, i, buf.length - n);
}
function fillLast(self, buf) {
    if (self.lastNeed <= buf.length) {
        copyInto(buf, self.lastChar, self.lastTotal - self.lastNeed, 0, self.lastNeed);
        return self.lastChar.toString(self.encoding, 0, self.lastTotal);
    }
    copyInto(buf, self.lastChar, self.lastTotal - self.lastNeed, 0, buf.length);
    self.lastNeed -= buf.length;
    return undefined;
}

function StringDecoder(encoding) {
    this.encoding = normalizeEncoding(encoding);
    this.lastNeed = 0;
    this.lastTotal = 0;
    // 4 bytes covers the widest utf8 sequence and a utf16le surrogate pair;
    // base64 only ever holds back two of a three-byte group.
    this.lastChar = Buffer.alloc(
        this.encoding === "base64" || this.encoding === "base64url" ? 3 : 4
    );
    this[kState] = true;
}

// Returns only the complete characters in `buf` from index `i`, buffering any
// trailing partial character. Undocumented in Node but part of its surface.
StringDecoder.prototype.text = function text(buf, i) {
    checkThis(this);
    const bytes = asBuffer(buf);
    if (this.encoding === "utf8") return utf8Text(this, bytes, i);
    if (this.encoding === "utf16le") return utf16Text(this, bytes, i);
    if (this.encoding === "base64" || this.encoding === "base64url") {
        return base64Text(this, bytes, i);
    }
    return bytes.toString(this.encoding, i);
};

StringDecoder.prototype.write = function write(buf) {
    if (typeof buf === "string") return buf;
    if (!ArrayBuffer.isView(buf)) {
        const err = new TypeError(
            'The "buf" argument must be an instance of Buffer, TypedArray, or DataView.' +
            receivedHelper(buf)
        );
        err.code = "ERR_INVALID_ARG_TYPE";
        throw err;
    }
    checkThis(this);
    const bytes = asBuffer(buf);
    if (bytes.length === 0) return "";
    if (this.encoding === "utf8") return utf8Write(this, bytes);
    let r;
    let i;
    if (this.lastNeed) {
        r = fillLast(this, bytes);
        if (r === undefined) return "";
        i = this.lastNeed;
        this.lastNeed = 0;
    } else {
        i = 0;
    }
    if (i < bytes.length) {
        const rest = this.text(bytes, i);
        return r ? r + rest : rest;
    }
    return r || "";
};

StringDecoder.prototype.end = function end(buf) {
    checkThis(this);
    let r = buf === undefined || buf === null ? "" : this.write(buf);
    if (this.lastNeed) {
        const enc = this.encoding;
        if (enc === "utf8") {
            // One replacement character for the truncated sequence, however
            // many of its bytes had already arrived.
            r += "�";
        } else if (enc === "utf16le") {
            r += this.lastChar.toString("utf16le", 0, this.lastTotal - this.lastNeed);
        } else if (enc === "base64" || enc === "base64url") {
            r += this.lastChar.toString(enc, 0, 3 - this.lastNeed);
        }
        // Flushing resets the decoder: a write() after end() starts clean.
        this.lastNeed = 0;
        this.lastTotal = 0;
    }
    return r;
};

export { StringDecoder };
export default { StringDecoder };
"#;

// node:punycode — the RFC 3492 bootstring algorithm plus the RFC 3490
// domain-mapping helpers. Deprecated in Node but still imported by URL/domain
// tooling, and fully expressible as pure deterministic JS.
const PUNYCODE_SHIM: &str = r#"
const maxInt = 2147483647;
const base = 36;
const tMin = 1;
const tMax = 26;
const skew = 38;
const damp = 700;
const initialBias = 72;
const initialN = 128;
const delimiter = "-";
const baseMinusTMin = base - tMin;

// Node's punycode uses these exact RangeError messages; tests match on them.
const errorMessages = {
    "overflow": "Overflow: input needs wider integers to process",
    "not-basic": "Illegal input >= 0x80 (not a basic code point)",
    "invalid-input": "Invalid input",
};
function error(type) {
    throw new RangeError(errorMessages[type]);
}
function ucs2decode(string) {
    const output = [];
    let counter = 0;
    const length = string.length;
    while (counter < length) {
        const value = string.charCodeAt(counter++);
        if (value >= 0xd800 && value <= 0xdbff && counter < length) {
            const extra = string.charCodeAt(counter++);
            if ((extra & 0xfc00) === 0xdc00) {
                output.push(((value & 0x3ff) << 10) + (extra & 0x3ff) + 0x10000);
            } else {
                output.push(value);
                counter--;
            }
        } else {
            output.push(value);
        }
    }
    return output;
}
function ucs2encode(codePoints) {
    // Astral code points go through fromCodePoint: the engine's strings are
    // code-point based, so emitting a hand-built surrogate pair as two
    // fromCharCode halves would yield two replacement characters instead of
    // the character. BMP units (including a bare surrogate) stay on
    // fromCharCode, which round-trips them the same way a source literal does.
    let out = "";
    for (const cp of codePoints) {
        out += cp > 0xffff ? String.fromCodePoint(cp) : String.fromCharCode(cp);
    }
    return out;
}
function basicToDigit(codePoint) {
    if (codePoint >= 0x30 && codePoint < 0x3a) return 26 + (codePoint - 0x30);
    if (codePoint >= 0x41 && codePoint < 0x5b) return codePoint - 0x41;
    if (codePoint >= 0x61 && codePoint < 0x7b) return codePoint - 0x61;
    return base;
}
function digitToBasic(digit, flag) {
    return digit + 22 + 75 * (digit < 26) - ((flag != 0) << 5);
}
function adapt(delta, numPoints, firstTime) {
    let k = 0;
    delta = firstTime ? Math.floor(delta / damp) : delta >> 1;
    delta += Math.floor(delta / numPoints);
    for (; delta > (baseMinusTMin * tMax) >> 1; k += base) {
        delta = Math.floor(delta / baseMinusTMin);
    }
    return Math.floor(k + ((baseMinusTMin + 1) * delta) / (delta + skew));
}
function decode(input) {
    const output = [];
    const inputLength = input.length;
    let i = 0;
    let n = initialN;
    let bias = initialBias;
    let basic = input.lastIndexOf(delimiter);
    if (basic < 0) basic = 0;
    for (let j = 0; j < basic; ++j) {
        if (input.charCodeAt(j) >= 0x80) error("not-basic");
        output.push(input.charCodeAt(j));
    }
    for (let index = basic > 0 ? basic + 1 : 0; index < inputLength; ) {
        const oldi = i;
        for (let w = 1, k = base; ; k += base) {
            if (index >= inputLength) error("invalid-input");
            const digit = basicToDigit(input.charCodeAt(index++));
            // A non-basic (or out-of-alphabet) code point is invalid input,
            // not an overflow — the two RangeErrors are distinguishable.
            if (digit >= base) error("invalid-input");
            if (digit > Math.floor((maxInt - i) / w)) error("overflow");
            i += digit * w;
            const t = k <= bias ? tMin : k >= bias + tMax ? tMax : k - bias;
            if (digit < t) break;
            const baseMinusT = base - t;
            if (w > Math.floor(maxInt / baseMinusT)) error("overflow");
            w *= baseMinusT;
        }
        const out = output.length + 1;
        bias = adapt(i - oldi, out, oldi === 0);
        if (Math.floor(i / out) > maxInt - n) error("overflow");
        n += Math.floor(i / out);
        i %= out;
        output.splice(i++, 0, n);
    }
    return ucs2encode(output);
}
function encode(input) {
    const output = [];
    const decoded = ucs2decode(String(input));
    const inputLength = decoded.length;
    let n = initialN;
    let delta = 0;
    let bias = initialBias;
    for (const currentValue of decoded) {
        if (currentValue < 0x80) output.push(String.fromCharCode(currentValue));
    }
    const basicLength = output.length;
    let handledCPCount = basicLength;
    if (basicLength) output.push(delimiter);
    while (handledCPCount < inputLength) {
        let m = maxInt;
        for (const currentValue of decoded) {
            if (currentValue >= n && currentValue < m) m = currentValue;
        }
        const handledCPCountPlusOne = handledCPCount + 1;
        if (m - n > Math.floor((maxInt - delta) / handledCPCountPlusOne)) error("overflow");
        delta += (m - n) * handledCPCountPlusOne;
        n = m;
        for (const currentValue of decoded) {
            if (currentValue < n && ++delta > maxInt) error("overflow");
            if (currentValue === n) {
                let q = delta;
                for (let k = base; ; k += base) {
                    const t = k <= bias ? tMin : k >= bias + tMax ? tMax : k - bias;
                    if (q < t) break;
                    const qMinusT = q - t;
                    const baseMinusT = base - t;
                    output.push(String.fromCharCode(digitToBasic(t + (qMinusT % baseMinusT), 0)));
                    q = Math.floor(qMinusT / baseMinusT);
                }
                output.push(String.fromCharCode(digitToBasic(q, 0)));
                bias = adapt(delta, handledCPCountPlusOne, handledCPCount === basicLength);
                delta = 0;
                ++handledCPCount;
            }
        }
        ++delta;
        ++n;
    }
    return output.join("");
}
function mapDomain(domain, callback) {
    const parts = String(domain).split("@");
    let result = "";
    if (parts.length > 1) {
        result = parts[0] + "@";
        domain = parts[1];
    }
    const labels = String(domain).split(/[.。．｡]/);
    const encoded = [];
    for (const label of labels) encoded.push(callback(label));
    return result + encoded.join(".");
}
function toASCII(input) {
    // Case is significant to the encoder (RFC 3492 case flags), so the label
    // is encoded verbatim — lowercasing here would corrupt mixed-case labels.
    return mapDomain(input, function (label) {
        return /[^\0-\x7f]/.test(label) ? "xn--" + encode(label) : label;
    });
}
function toUnicode(input) {
    return mapDomain(input, function (label) {
        return /^xn--/.test(label.toLowerCase()) ? decode(label.toLowerCase().slice(4)) : label;
    });
}
const ucs2 = { decode: ucs2decode, encode: ucs2encode };
const version = "2.1.0";
const punycode = { decode, encode, toASCII, toUnicode, ucs2, version };
export { decode, encode, toASCII, toUnicode, ucs2, version };
export default punycode;
"#;

// node:console — the engine installs a `console` global; this module
// re-exports it (Node's node:console default IS the global console). The
// Console class returns the same global sink: there are no per-stream
// consoles in this runtime.
const CONSOLE_SHIM: &str = r#"
class Console {
    constructor() {
        return globalThis.console;
    }
}
const consoleModule = globalThis.console;
if (consoleModule && consoleModule.Console === undefined) {
    try { consoleModule.Console = Console; } catch {}
}
export { Console };
export default consoleModule;
"#;

// node:constants — the legacy merged constants module (fs flags + errno +
// signals). Values match Linux/glibc, the reference platform for the numbers
// Node itself reports; they are fixed constants, so record/replay agrees.
const CONSTANTS_SHIM: &str = r#"
export const F_OK = 0;
export const R_OK = 4;
export const W_OK = 2;
export const X_OK = 1;
export const O_RDONLY = 0;
export const O_WRONLY = 1;
export const O_RDWR = 2;
export const O_CREAT = 64;
export const O_EXCL = 128;
export const O_TRUNC = 512;
export const O_APPEND = 1024;
export const O_NONBLOCK = 2048;
export const S_IFMT = 61440;
export const S_IFREG = 32768;
export const S_IFDIR = 16384;
export const S_IFLNK = 40960;
export const SIGHUP = 1;
export const SIGINT = 2;
export const SIGQUIT = 3;
export const SIGABRT = 6;
export const SIGKILL = 9;
export const SIGUSR1 = 10;
export const SIGUSR2 = 12;
export const SIGPIPE = 13;
export const SIGALRM = 14;
export const SIGTERM = 15;
export const SIGCHLD = 17;
export const EPERM = 1;
export const ENOENT = 2;
export const EINTR = 4;
export const EIO = 5;
export const EBADF = 9;
export const EAGAIN = 11;
export const ENOMEM = 12;
export const EACCES = 13;
export const EEXIST = 17;
export const ENOTDIR = 20;
export const EISDIR = 21;
export const EINVAL = 22;
export const EMFILE = 24;
export const EPIPE = 32;
export const ERANGE = 34;
export const ENOTEMPTY = 39;
export const ENOTSUP = 95;
const constants = {
    F_OK, R_OK, W_OK, X_OK,
    O_RDONLY, O_WRONLY, O_RDWR, O_CREAT, O_EXCL, O_TRUNC, O_APPEND, O_NONBLOCK,
    S_IFMT, S_IFREG, S_IFDIR, S_IFLNK,
    SIGHUP, SIGINT, SIGQUIT, SIGABRT, SIGKILL, SIGUSR1, SIGUSR2, SIGPIPE,
    SIGALRM, SIGTERM, SIGCHLD,
    EPERM, ENOENT, EINTR, EIO, EBADF, EAGAIN, ENOMEM, EACCES, EEXIST,
    ENOTDIR, EISDIR, EINVAL, EMFILE, EPIPE, ERANGE, ENOTEMPTY, ENOTSUP,
};
export default constants;
"#;

// node:util/types — the type-brand predicates. Pure checks against the
// engine's own intrinsics.
const UTIL_TYPES_SHIM: &str = r#"
function ctorNameIs(value, name) {
    return typeof value === "function" &&
        value.constructor &&
        value.constructor.name === name;
}
export function isDate(value) { return value instanceof Date; }
export function isRegExp(value) { return value instanceof RegExp; }
export function isPromise(value) { return value instanceof Promise; }
export function isNativeError(value) { return value instanceof Error; }
export function isArrayBuffer(value) { return value instanceof ArrayBuffer; }
export function isAnyArrayBuffer(value) { return value instanceof ArrayBuffer; }
export function isArrayBufferView(value) { return ArrayBuffer.isView(value); }
export function isDataView(value) { return typeof DataView !== "undefined" && value instanceof DataView; }
export function isTypedArray(value) { return ArrayBuffer.isView(value) && !isDataView(value); }
export function isUint8Array(value) { return value instanceof Uint8Array; }
export function isUint8ClampedArray(value) { return typeof Uint8ClampedArray !== "undefined" && value instanceof Uint8ClampedArray; }
export function isUint16Array(value) { return value instanceof Uint16Array; }
export function isUint32Array(value) { return value instanceof Uint32Array; }
export function isInt8Array(value) { return value instanceof Int8Array; }
export function isInt16Array(value) { return value instanceof Int16Array; }
export function isInt32Array(value) { return value instanceof Int32Array; }
export function isFloat32Array(value) { return value instanceof Float32Array; }
export function isFloat64Array(value) { return value instanceof Float64Array; }
export function isBigInt64Array(value) { return typeof BigInt64Array !== "undefined" && value instanceof BigInt64Array; }
export function isBigUint64Array(value) { return typeof BigUint64Array !== "undefined" && value instanceof BigUint64Array; }
export function isMap(value) { return typeof Map !== "undefined" && value instanceof Map; }
export function isSet(value) { return typeof Set !== "undefined" && value instanceof Set; }
export function isWeakMap(value) { return typeof WeakMap !== "undefined" && value instanceof WeakMap; }
export function isWeakSet(value) { return typeof WeakSet !== "undefined" && value instanceof WeakSet; }
export function isAsyncFunction(value) { return ctorNameIs(value, "AsyncFunction"); }
export function isGeneratorFunction(value) { return ctorNameIs(value, "GeneratorFunction"); }
export function isGeneratorObject(value) {
    return value !== null && typeof value === "object" &&
        typeof value.next === "function" && typeof value.throw === "function";
}
export function isProxy() { return false; }
// Boxed primitives are identified by their internal slot, not by `instanceof`:
// the prototype and `Symbol.toStringTag` of a wrapper are both writable, so
// only invoking the matching `valueOf` distinguishes (say) a Boolean object
// wearing `String.prototype` from a real String object. `util.isDeepStrictEqual`
// depends on the distinction.
function hasBrand(valueOf, value) {
    if (value === null || typeof value !== "object") return false;
    try { valueOf.call(value); return true; } catch { return false; }
}
export function isStringObject(value) { return hasBrand(String.prototype.valueOf, value); }
export function isNumberObject(value) { return hasBrand(Number.prototype.valueOf, value); }
export function isBooleanObject(value) { return hasBrand(Boolean.prototype.valueOf, value); }
export function isSymbolObject(value) { return hasBrand(Symbol.prototype.valueOf, value); }
export function isBigIntObject(value) {
    return typeof BigInt !== "undefined" && hasBrand(BigInt.prototype.valueOf, value);
}
export function isBoxedPrimitive(value) {
    return isStringObject(value) || isNumberObject(value) || isBooleanObject(value) ||
        isSymbolObject(value) || isBigIntObject(value);
}
export function isArgumentsObject(value) {
    return Object.prototype.toString.call(value) === "[object Arguments]";
}
export function isSharedArrayBuffer() { return false; }
export function isExternal() { return false; }
export function isModuleNamespaceObject(value) {
    return Object.prototype.toString.call(value) === "[object Module]";
}
const types = {
    isDate, isRegExp, isPromise, isNativeError, isArrayBuffer, isAnyArrayBuffer,
    isArrayBufferView, isDataView, isTypedArray, isUint8Array, isUint8ClampedArray,
    isUint16Array, isUint32Array, isInt8Array, isInt16Array, isInt32Array,
    isFloat32Array, isFloat64Array, isBigInt64Array, isBigUint64Array,
    isMap, isSet, isWeakMap, isWeakSet, isAsyncFunction, isGeneratorFunction,
    isGeneratorObject, isProxy, isSymbolObject, isBigIntObject, isStringObject,
    isNumberObject, isBooleanObject, isBoxedPrimitive, isArgumentsObject,
    isSharedArrayBuffer, isExternal, isModuleNamespaceObject,
};
export default types;
"#;

// node:path/win32 — re-exports the real win32 table from the node:path shim
// (backslash separator, `;` delimiter, drive letters, UNC roots). node:path's
// own top-level surface stays posix because the chidori VFS is posix, so this
// subpath cannot re-export node:path's named exports; it lifts them off the
// `win32` object instead.
const PATH_WIN32_SHIM: &str = r#"
import { win32 as __win32 } from "node:path";
export const sep = __win32.sep;
export const delimiter = __win32.delimiter;
export const normalize = __win32.normalize;
export const isAbsolute = __win32.isAbsolute;
export const join = __win32.join;
export const resolve = __win32.resolve;
export const relative = __win32.relative;
export const toNamespacedPath = __win32.toNamespacedPath;
export const dirname = __win32.dirname;
export const basename = __win32.basename;
export const extname = __win32.extname;
export const format = __win32.format;
export const parse = __win32.parse;
export const posix = __win32.posix;
export const win32 = __win32;
export default __win32;
"#;

// node:sys — legacy alias of node:util.
const SYS_SHIM: &str = r#"
import util from "node:util";
export { inspect, promisify, inherits, format, deprecate, callbackify, types, isDeepStrictEqual, TextEncoder, TextDecoder } from "node:util";
export default util;
"#;

// node:timers — delegates to the prelude's virtual timer queue (captured and
// replayed deterministically). Handles are wrapped in Timeout/Immediate
// objects so Node idioms like `.unref()` work; ref-counting is meaningless
// here (the virtual queue always drains), so ref/unref are identity.
const TIMERS_SHIM: &str = r#"
import promises from "node:timers/promises";

class Timeout {
    constructor(id) { this._id = id; this._destroyed = false; }
    ref() { return this; }
    unref() { return this; }
    hasRef() { return true; }
    refresh() { return this; }
    close() { clearAny(this); return this; }
    [Symbol.toPrimitive]() { return this._id; }
}
class Immediate {
    constructor(id) { this._id = id; this._destroyed = false; }
    ref() { return this; }
    unref() { return this; }
    hasRef() { return true; }
    [Symbol.toPrimitive]() { return this._id; }
}
function rawId(handle) {
    if (handle === undefined || handle === null) return handle;
    if (typeof handle === "object") {
        handle._destroyed = true;
        return handle._id;
    }
    return handle;
}
function clearAny(handle) { globalThis.clearTimeout(rawId(handle)); }

export function setTimeout(callback, delay, ...args) {
    return new Timeout(globalThis.setTimeout(callback, delay, ...args));
}
export function setInterval(callback, delay, ...args) {
    return new Timeout(globalThis.setInterval(callback, delay, ...args));
}
export function setImmediate(callback, ...args) {
    return new Immediate(globalThis.setImmediate(callback, ...args));
}
export function clearTimeout(handle) { clearAny(handle); }
export function clearInterval(handle) { clearAny(handle); }
export function clearImmediate(handle) { clearAny(handle); }
// Legacy enroll/unenroll/active surface: obsolete no-ops, kept for linkage.
export function enroll() {}
export function unenroll() {}
export function active() {}
export { promises };
const timers = {
    setTimeout, setInterval, setImmediate,
    clearTimeout, clearInterval, clearImmediate,
    enroll, unenroll, active, promises,
};
export default timers;
"#;

// node:timers/promises — promise/async-iterator timer surface over the same
// virtual queue.
const TIMERS_PROMISES_SHIM: &str = r#"
function abortError() {
    const err = new Error("The operation was aborted");
    err.name = "AbortError";
    err.code = "ABORT_ERR";
    return err;
}
function wait(delay, value, options) {
    return new Promise((resolve, reject) => {
        const signal = options && options.signal;
        if (signal && signal.aborted) { reject(abortError()); return; }
        const id = globalThis.setTimeout(() => resolve(value), delay);
        if (signal && typeof signal.addEventListener === "function") {
            signal.addEventListener("abort", () => {
                globalThis.clearTimeout(id);
                reject(abortError());
            }, { once: true });
        }
    });
}
export function setTimeout(delay, value, options) {
    return wait(delay, value, options);
}
export function setImmediate(value, options) {
    return wait(0, value, options);
}
export function setInterval(delay, value, options) {
    // Async iterable that yields `value` every `delay` virtual milliseconds.
    const iterator = {
        next() {
            return wait(delay, { value, done: false }, options);
        },
        return() {
            return Promise.resolve({ value: undefined, done: true });
        },
    };
    iterator[Symbol.asyncIterator] = function () { return iterator; };
    return iterator;
}
export const scheduler = {
    wait(delay, options) { return wait(delay, undefined, options); },
    yield() { return wait(0, undefined, undefined); },
};
export default { setTimeout, setImmediate, setInterval, scheduler };
"#;

// node:async_hooks — AsyncLocalStorage with synchronous-scope propagation:
// the store is visible inside `run()`'s synchronous extent and through
// callbacks bound with AsyncResource. (Full continuation-crossing propagation
// would need engine hooks; the common middleware/logging patterns are
// synchronous and work.) Hook creation is a no-op surface, and the id
// accessors return fixed values — the runtime is single-threaded and
// deterministic, so there is exactly one execution context.
const ASYNC_HOOKS_SHIM: &str = r#"
// Every live storage, so `snapshot()` can capture the whole set of stores that
// are active at the moment of capture and re-enter them later — the runtime is
// single-threaded, so the "current async context" is just the top of each
// storage's stack.
const liveStorages = [];
function invalidFnArg(name, value) {
    const received = value === null || value === undefined
        ? String(value)
        : "type " + typeof value + " (" + (typeof value === "string" ? JSON.stringify(value) : String(value)) + ")";
    const err = new TypeError('The "' + name + '" argument must be of type function. Received ' + received);
    err.code = "ERR_INVALID_ARG_TYPE";
    // Node's coded errors stringify as `Name [CODE]: message` so a RegExp
    // validator can match on the code; keep it off enumeration like Node does.
    Object.defineProperty(err, "toString", {
        value: function () { return this.name + " [" + this.code + "]: " + this.message; },
        enumerable: false, writable: true, configurable: true,
    });
    return err;
}
class AsyncLocalStorage {
    constructor() { this._stack = []; liveStorages.push(this); }
    getStore() {
        return this._stack.length ? this._stack[this._stack.length - 1] : undefined;
    }
    run(store, callback, ...args) {
        this._stack.push(store);
        try {
            return callback(...args);
        } finally {
            this._stack.pop();
        }
    }
    exit(callback, ...args) {
        this._stack.push(undefined);
        try {
            return callback(...args);
        } finally {
            this._stack.pop();
        }
    }
    enterWith(store) { this._stack.push(store); }
    disable() { this._stack.length = 0; }
    static bind(fn) {
        if (typeof fn !== "function") throw invalidFnArg("fn", fn);
        const restore = AsyncLocalStorage.snapshot();
        return function (...args) {
            const self = this;
            return restore(function () { return fn.apply(self, args); });
        };
    }
    static snapshot() {
        // Capture the current store of every storage; the returned runner
        // re-enters exactly those stores for the duration of the callback.
        const captured = [];
        for (const storage of liveStorages) captured.push([storage, storage.getStore()]);
        return function (cb, ...args) {
            for (const entry of captured) entry[0]._stack.push(entry[1]);
            try {
                return cb(...args);
            } finally {
                for (let i = captured.length - 1; i >= 0; i--) captured[i][0]._stack.pop();
            }
        };
    }
}
class AsyncResource {
    constructor(type) { this.type = String(type === undefined ? "AsyncResource" : type); }
    runInAsyncScope(fn, thisArg, ...args) { return fn.apply(thisArg, args); }
    bind(fn, thisArg) {
        const self = this;
        return function (...args) {
            return self.runInAsyncScope(fn, thisArg === undefined ? this : thisArg, ...args);
        };
    }
    static bind(fn, type, thisArg) {
        return new AsyncResource(type).bind(fn, thisArg);
    }
    emitDestroy() { return this; }
    asyncId() { return 1; }
    triggerAsyncId() { return 0; }
}
export function executionAsyncId() { return 1; }
export function triggerAsyncId() { return 0; }
export function executionAsyncResource() { return {}; }
export function createHook() {
    return { enable() { return this; }, disable() { return this; } };
}
export { AsyncLocalStorage, AsyncResource };
export default {
    AsyncLocalStorage, AsyncResource,
    executionAsyncId, triggerAsyncId, executionAsyncResource, createHook,
};
"#;

// node:diagnostics_channel — a complete pure-JS implementation of the
// named-channel pub/sub surface.
const DIAGNOSTICS_CHANNEL_SHIM: &str = r#"
const registry = Object.create(null);

function invalidArg(message) {
    const err = new TypeError(message);
    err.code = "ERR_INVALID_ARG_TYPE";
    // Node's coded errors stringify as `Name [CODE]: message`, which is what a
    // RegExp validator passed to assert.throws matches against.
    Object.defineProperty(err, "toString", {
        value: function () { return this.name + " [" + this.code + "]: " + this.message; },
        enumerable: false, writable: true, configurable: true,
    });
    return err;
}
function describe(value) {
    if (value === null || value === undefined) return String(value);
    if (typeof value === "object") {
        const ctor = value.constructor;
        return "an instance of " + (ctor && ctor.name ? ctor.name : "Object");
    }
    if (typeof value === "string") return "type string (" + JSON.stringify(value) + ")";
    return "type " + typeof value + " (" + String(value) + ")";
}
function checkChannelName(name) {
    if (typeof name !== "string" && typeof name !== "symbol") {
        throw invalidArg(
            'The "channel" argument must be of type string or symbol. Received ' + describe(name)
        );
    }
}

// Hand the error to `process`'s 'uncaughtException' listeners on a later tick
// (Node uses process.nextTick + triggerUncaughtException). With no listener
// registered there is nothing to swallow it, so it is rethrown asynchronously.
function deliverUncaught(err) {
    queueMicrotask(function () {
        const proc = globalThis.process;
        if (proc && typeof proc.emit === "function" && proc.emit("uncaughtException", err)) return;
        throw err;
    });
}

class Channel {
    constructor(name) {
        this.name = name;
        this._subscribers = [];
    }
    get hasSubscribers() { return this._subscribers.length > 0; }
    subscribe(onMessage) {
        if (typeof onMessage !== "function") {
            throw invalidArg('The "subscription" argument must be of type function');
        }
        this._subscribers.push(onMessage);
    }
    unsubscribe(onMessage) {
        const idx = this._subscribers.indexOf(onMessage);
        if (idx === -1) return false;
        this._subscribers.splice(idx, 1);
        return true;
    }
    publish(message) {
        // A throwing subscriber must not abort the publish or reach the
        // caller: Node routes it to the uncaught-exception path instead, so
        // later subscribers still run.
        const subs = this._subscribers.slice();
        for (const fn of subs) {
            try {
                fn(message, this.name);
            } catch (err) {
                deliverUncaught(err);
            }
        }
    }
    bindStore() {}
    unbindStore() {}
    runStores(context, fn, thisArg, ...args) {
        this.publish(context);
        return fn.apply(thisArg, args);
    }
}
export function channel(name) {
    checkChannelName(name);
    return registry[name] || (registry[name] = new Channel(name));
}
export function subscribe(name, onMessage) { channel(name).subscribe(onMessage); }
export function unsubscribe(name, onMessage) { return channel(name).unsubscribe(onMessage); }
export function hasSubscribers(name) {
    checkChannelName(name);
    const ch = registry[name];
    return ch ? ch.hasSubscribers : false;
}
export function tracingChannel(nameOrChannels) {
    const prefix = typeof nameOrChannels === "string" ? nameOrChannels : "";
    const ch = (suffix) => channel("tracing:" + prefix + ":" + suffix);
    const channels = typeof nameOrChannels === "object" && nameOrChannels !== null
        ? nameOrChannels
        : { start: ch("start"), end: ch("end"), asyncStart: ch("asyncStart"), asyncEnd: ch("asyncEnd"), error: ch("error") };
    return {
        start: channels.start, end: channels.end,
        asyncStart: channels.asyncStart, asyncEnd: channels.asyncEnd, error: channels.error,
        traceSync(fn, context, thisArg, ...args) {
            context = context || {};
            channels.start.publish(context);
            try {
                const result = fn.apply(thisArg, args);
                context.result = result;
                return result;
            } catch (err) {
                context.error = err;
                channels.error.publish(context);
                throw err;
            } finally {
                channels.end.publish(context);
            }
        },
        tracePromise(fn, context, thisArg, ...args) {
            context = context || {};
            channels.start.publish(context);
            let p;
            try {
                p = Promise.resolve(fn.apply(thisArg, args));
            } catch (err) {
                context.error = err;
                channels.error.publish(context);
                channels.end.publish(context);
                throw err;
            }
            channels.end.publish(context);
            return p.then(
                (result) => { context.result = result; channels.asyncEnd.publish(context); return result; },
                (err) => { context.error = err; channels.error.publish(context); channels.asyncEnd.publish(context); throw err; }
            );
        },
        subscribe(handlers) {
            for (const key of Object.keys(handlers || {})) {
                if (channels[key]) channels[key].subscribe(handlers[key]);
            }
        },
        unsubscribe(handlers) {
            for (const key of Object.keys(handlers || {})) {
                if (channels[key]) channels[key].unsubscribe(handlers[key]);
            }
        },
    };
}
export { Channel };
export default { channel, subscribe, unsubscribe, hasSubscribers, tracingChannel, Channel };
"#;

// node:domain — deprecated in Node but still imported by older error-handling
// wrappers. run/bind/intercept route thrown errors to the domain's 'error'
// listeners.
const DOMAIN_SHIM: &str = r#"
import { EventEmitter } from "node:events";

class Domain extends EventEmitter {
    constructor() {
        super();
        this.members = [];
    }
    add(emitter) { if (this.members.indexOf(emitter) === -1) this.members.push(emitter); }
    remove(emitter) {
        const idx = this.members.indexOf(emitter);
        if (idx !== -1) this.members.splice(idx, 1);
    }
    enter() {}
    exit() {}
    run(fn, ...args) {
        try {
            return fn.apply(this, args);
        } catch (err) {
            if (this.listenerCount("error") > 0) this.emit("error", err);
            else throw err;
        }
    }
    bind(callback) {
        const self = this;
        return function (...args) {
            return self.run(() => callback.apply(this, args));
        };
    }
    intercept(callback) {
        const self = this;
        return function (err, ...args) {
            if (err) {
                if (self.listenerCount("error") > 0) { self.emit("error", err); return; }
                throw err;
            }
            return self.run(() => callback.apply(this, args));
        };
    }
    dispose() { this.removeAllListeners(); return this; }
}
export function create() { return new Domain(); }
export { Domain };
export default { create, Domain, createDomain: create };
export const createDomain = create;
"#;

// node:perf_hooks — performance.now() reads the same virtual clock the timer
// queue advances, so measurements are deterministic and replay-stable.
const PERF_HOOKS_SHIM: &str = r#"
function nowMs() {
    return typeof globalThis.__chidori_now === "number" ? globalThis.__chidori_now : Date.now();
}
const entries = [];
const marks = Object.create(null);
const performance = {
    timeOrigin: 0,
    now: nowMs,
    mark(name, options) {
        const startTime = options && typeof options.startTime === "number" ? options.startTime : nowMs();
        marks[name] = startTime;
        const entry = { name: String(name), entryType: "mark", startTime, duration: 0,
            detail: options && options.detail !== undefined ? options.detail : null };
        entries.push(entry);
        return entry;
    },
    measure(name, startOrOptions, endMark) {
        let start = 0;
        let end = nowMs();
        if (typeof startOrOptions === "string") {
            if (!(startOrOptions in marks)) throw new Error(`The "${startOrOptions}" performance mark has not been set`);
            start = marks[startOrOptions];
            if (endMark !== undefined) {
                if (!(endMark in marks)) throw new Error(`The "${endMark}" performance mark has not been set`);
                end = marks[endMark];
            }
        } else if (startOrOptions && typeof startOrOptions === "object") {
            if (typeof startOrOptions.start === "number") start = startOrOptions.start;
            else if (typeof startOrOptions.start === "string") start = marks[startOrOptions.start] || 0;
            if (typeof startOrOptions.end === "number") end = startOrOptions.end;
            else if (typeof startOrOptions.end === "string") end = marks[startOrOptions.end] || end;
            if (typeof startOrOptions.duration === "number") end = start + startOrOptions.duration;
        }
        const entry = { name: String(name), entryType: "measure", startTime: start, duration: end - start, detail: null };
        entries.push(entry);
        return entry;
    },
    getEntries() { return entries.slice(); },
    getEntriesByName(name, type) {
        return entries.filter((e) => e.name === name && (type === undefined || e.entryType === type));
    },
    getEntriesByType(type) { return entries.filter((e) => e.entryType === type); },
    clearMarks(name) {
        for (let i = entries.length - 1; i >= 0; i--) {
            if (entries[i].entryType === "mark" && (name === undefined || entries[i].name === name)) entries.splice(i, 1);
        }
        if (name === undefined) { for (const k of Object.keys(marks)) delete marks[k]; }
        else delete marks[name];
    },
    clearMeasures(name) {
        for (let i = entries.length - 1; i >= 0; i--) {
            if (entries[i].entryType === "measure" && (name === undefined || entries[i].name === name)) entries.splice(i, 1);
        }
    },
    eventLoopUtilization() { return { idle: 0, active: 0, utilization: 0 }; },
    nodeTiming: { name: "node", entryType: "node", startTime: 0, duration: 0 },
};
class PerformanceObserver {
    constructor(callback) { this._callback = callback; }
    observe() {}
    disconnect() {}
    takeRecords() { return []; }
}
PerformanceObserver.supportedEntryTypes = ["mark", "measure"];
export function monitorEventLoopDelay() {
    throw new Error("perf_hooks.monitorEventLoopDelay is not supported in the Chidori runtime (no host event loop is observable)");
}
export function createHistogram() {
    throw new Error("perf_hooks.createHistogram is not supported in the Chidori runtime");
}
export const constants = Object.freeze({
    NODE_PERFORMANCE_GC_MAJOR: 4, NODE_PERFORMANCE_GC_MINOR: 1,
    NODE_PERFORMANCE_GC_INCREMENTAL: 8, NODE_PERFORMANCE_GC_WEAKCB: 16,
});
export { performance, PerformanceObserver };
export default { performance, PerformanceObserver, constants, monitorEventLoopDelay, createHistogram };
"#;

// node:worker_threads — main-thread surface only. `isMainThread` is true and
// there is no way to spawn a second thread (single-threaded deterministic
// engine), so `Worker` throws. MessageChannel/MessagePort ARE provided — they
// are pure in-process plumbing that libraries use for structured messaging.
// postMessage passes values by reference (no structured clone) — same-realm
// delivery, documented divergence.
const WORKER_THREADS_SHIM: &str = r#"
import { EventEmitter } from "node:events";

class MessagePort extends EventEmitter {
    constructor() {
        super();
        this._other = null;
        this._closed = false;
    }
    postMessage(value) {
        const other = this._other;
        if (!other || this._closed || other._closed) return;
        queueMicrotask(() => {
            if (!other._closed) other.emit("message", value);
        });
    }
    start() {}
    close() {
        if (this._closed) return;
        this._closed = true;
        queueMicrotask(() => this.emit("close"));
    }
    ref() { return this; }
    unref() { return this; }
}
class MessageChannel {
    constructor() {
        this.port1 = new MessagePort();
        this.port2 = new MessagePort();
        this.port1._other = this.port2;
        this.port2._other = this.port1;
    }
}
class BroadcastChannel extends EventEmitter {
    constructor(name) {
        super();
        this.name = String(name);
        throw new Error("worker_threads.BroadcastChannel is not supported in the Chidori runtime (single-threaded engine)");
    }
}
class Worker {
    constructor() {
        throw new Error("worker_threads.Worker is not supported in the Chidori runtime (the engine is single-threaded and deterministic; use chidori sub-agents for parallelism)");
    }
}
export const isMainThread = true;
export const threadId = 0;
export const parentPort = null;
export const workerData = null;
export const resourceLimits = {};
export const SHARE_ENV = Symbol.for("nodejs.worker_threads.SHARE_ENV");
const environmentData = Object.create(null);
export function getEnvironmentData(key) { return environmentData[key]; }
export function setEnvironmentData(key, value) {
    if (value === undefined) delete environmentData[key];
    else environmentData[key] = value;
}
export function receiveMessageOnPort() { return undefined; }
export function markAsUntransferable() {}
export function moveMessagePortToContext() {
    throw new Error("worker_threads.moveMessagePortToContext is not supported in the Chidori runtime");
}
export { MessageChannel, MessagePort, BroadcastChannel, Worker };
export default {
    isMainThread, threadId, parentPort, workerData, resourceLimits, SHARE_ENV,
    getEnvironmentData, setEnvironmentData, receiveMessageOnPort,
    markAsUntransferable, moveMessagePortToContext,
    MessageChannel, MessagePort, BroadcastChannel, Worker,
};
"#;

// node:v8 — engine-introspection surface. The embedded engine is not V8;
// statistics report fixed zeros (deterministic), and the serialization API —
// whose byte format is V8-proprietary — throws.
const V8_SHIM: &str = r#"
function unsupported(name) {
    return function () {
        throw new Error("v8." + name + " is not supported in the Chidori runtime (the embedded engine is not V8)");
    };
}
export function getHeapStatistics() {
    return {
        total_heap_size: 0, total_heap_size_executable: 0, total_physical_size: 0,
        total_available_size: 0, used_heap_size: 0, heap_size_limit: 0,
        malloced_memory: 0, peak_malloced_memory: 0, does_zap_garbage: 0,
        number_of_native_contexts: 0, number_of_detached_contexts: 0,
        total_global_handles_size: 0, used_global_handles_size: 0, external_memory: 0,
    };
}
export function getHeapSpaceStatistics() { return []; }
export function getHeapCodeStatistics() {
    return { code_and_metadata_size: 0, bytecode_and_metadata_size: 0, external_script_source_size: 0 };
}
export function cachedDataVersionTag() { return 0; }
export function setFlagsFromString() {}
export const serialize = unsupported("serialize");
export const deserialize = unsupported("deserialize");
export const writeHeapSnapshot = unsupported("writeHeapSnapshot");
export const getHeapSnapshot = unsupported("getHeapSnapshot");
export class Serializer { constructor() { unsupported("Serializer")(); } }
export class Deserializer { constructor() { unsupported("Deserializer")(); } }
export default {
    getHeapStatistics, getHeapSpaceStatistics, getHeapCodeStatistics,
    cachedDataVersionTag, setFlagsFromString, serialize, deserialize,
    writeHeapSnapshot, getHeapSnapshot, Serializer, Deserializer,
};
"#;

// node:tty — there is no terminal attached to an agent run; isatty is
// honestly false and the stream classes (which would wrap real fds) throw.
const TTY_SHIM: &str = r#"
export function isatty() { return false; }
export class ReadStream {
    constructor() {
        throw new Error("tty.ReadStream is not supported in the Chidori runtime (no terminal is attached to an agent run)");
    }
}
export class WriteStream {
    constructor() {
        throw new Error("tty.WriteStream is not supported in the Chidori runtime (no terminal is attached to an agent run)");
    }
}
export default { isatty, ReadStream, WriteStream };
"#;

// node:net — the pure helpers (isIP/isIPv4/isIPv6) are real implementations;
// everything that would open a raw socket throws. (Networking in chidori is
// `fetch`/`node:http(s)`, which route through the captured, policy-gated
// HTTP host op.)
const NET_SHIM: &str = r#"
// Node coerces object inputs through toString but rejects non-string
// primitives (numbers, booleans, null).
function ipInput(input) {
    if (typeof input === "string") return input;
    if (input !== null && typeof input === "object") return String(input);
    return null;
}
export function isIPv4(input) {
    input = ipInput(input);
    if (input === null) return false;
    const parts = input.split(".");
    if (parts.length !== 4) return false;
    for (const part of parts) {
        if (!/^\d{1,3}$/.test(part)) return false;
        if (part.length > 1 && part[0] === "0") return false;
        if (parseInt(part, 10) > 255) return false;
    }
    return true;
}
export function isIPv6(input) {
    input = ipInput(input);
    if (input === null || input.length === 0) return false;
    let s = input;
    // Zone identifier (fe80::1%eth0): strip and validate before parsing.
    const percent = s.indexOf("%");
    if (percent !== -1) {
        const zone = s.slice(percent + 1);
        if (zone.length === 0 || !/^[0-9a-zA-Z._-]+$/.test(zone)) return false;
        s = s.slice(0, percent);
    }
    const lastColon = s.lastIndexOf(":");
    if (lastColon === -1) return false;
    if (s.indexOf(".", lastColon) !== -1) {
        // Embedded IPv4 tail (e.g. ::ffff:127.0.0.1) counts as two groups.
        if (!isIPv4(s.slice(lastColon + 1))) return false;
        s = s.slice(0, lastColon + 1) + "0:0";
    }
    const doubleColon = s.indexOf("::");
    if (doubleColon !== s.lastIndexOf("::")) return false;
    let groups;
    if (doubleColon !== -1) {
        // A stray single colon at either boundary (":1::2", "1::2:") is
        // invalid — empty segments must not be silently dropped.
        const head = s.slice(0, doubleColon);
        const tail = s.slice(doubleColon + 2);
        if (head.startsWith(":") || head.endsWith(":")) return false;
        if (tail.startsWith(":") || tail.endsWith(":")) return false;
        const headGroups = head === "" ? [] : head.split(":");
        const tailGroups = tail === "" ? [] : tail.split(":");
        if (headGroups.length + tailGroups.length > 7) return false;
        groups = headGroups.concat(tailGroups);
        if (groups.length === 0) return true;
    } else {
        groups = s.split(":");
        if (groups.length !== 8) return false;
    }
    for (const group of groups) {
        if (!/^[0-9a-fA-F]{1,4}$/.test(group)) return false;
    }
    return true;
}
export function isIP(input) {
    if (isIPv4(input)) return 4;
    if (isIPv6(input)) return 6;
    return 0;
}
function noSockets(name) {
    return function () {
        throw new Error("net." + name + " is not supported in the Chidori runtime (no raw socket capability; use fetch or node:http/node:https, which route through the captured HTTP host op)");
    };
}
export class Socket {
    constructor() { noSockets("Socket")(); }
}
export class Server {
    constructor() { noSockets("Server")(); }
}
export class BlockList {
    constructor() { noSockets("BlockList")(); }
}
export const createServer = noSockets("createServer");
export const createConnection = noSockets("createConnection");
export const connect = noSockets("connect");
export default {
    isIP, isIPv4, isIPv6, Socket, Server, BlockList,
    createServer, createConnection, connect,
};
"#;

// node:stream — a faithful-enough Readable/Writable/Duplex/Transform
// implementation over the node:events EventEmitter. Covers the surface
// packages actually use: push/read, flowing + paused modes, pipe/pipeline,
// finished, async iteration, Readable.from, and the promises API.
// Backpressure is advisory (write() reports capacity but never blocks) —
// buffers live in memory, which is where all chidori VFS data lives anyway.
const STREAM_SHIM: &str = r#"
import { EventEmitter } from "node:events";
import { Buffer } from "node:buffer";

// ES5-style constructor (not a class): the classic `Stream.call(this)` +
// `Object.setPrototypeOf` inheritance idiom must keep working — Node's own
// suite and a long tail of npm packages rely on it.
function Stream(opts) {
    EventEmitter.call(this, opts);
}
Object.setPrototypeOf(Stream.prototype, EventEmitter.prototype);
Object.setPrototypeOf(Stream, EventEmitter);
Stream.prototype.pipe = function pipe(dest, options) {
    const src = this;
    const end = !options || options.end !== false;
    function onData(chunk) { dest.write(chunk); }
    src.on("data", onData);
    src.once("end", function () {
        src.off("data", onData);
        if (end && typeof dest.end === "function") dest.end();
    });
    src.once("error", function (err) {
        src.off("data", onData);
        if (typeof dest.destroy === "function") dest.destroy(err);
    });
    dest.emit("pipe", src);
    return dest;
};

function maybeEmitEnd(stream) {
    const st = stream._readableState;
    if (!st || !st.ended || st.endEmitted || st.buffer.length > 0 || st.destroyed) return;
    st.endEmitted = true;
    stream.readable = false;
    queueMicrotask(() => {
        stream.emit("end");
        if (!stream._writableState) queueMicrotask(() => stream.emit("close"));
    });
}
function decodeForRead(st, chunk) {
    if (st.encoding && chunk !== null && typeof chunk !== "string" && chunk && typeof chunk.toString === "function") {
        return chunk.toString(st.encoding);
    }
    return chunk;
}
function flow(stream) {
    const st = stream._readableState;
    if (st.flowScheduled) return;
    st.flowScheduled = true;
    queueMicrotask(() => {
        st.flowScheduled = false;
        while (st.flowing && st.buffer.length > 0 && !st.destroyed) {
            stream.emit("data", decodeForRead(st, st.buffer.shift()));
        }
        if (st.flowing && !st.ended && !st.destroyed && !st.reading) {
            st.reading = true;
            try { stream._read(st.highWaterMark); } catch (err) { stream.destroy(err); return; }
            st.reading = false;
            if (st.buffer.length > 0) { flow(stream); return; }
        }
        maybeEmitEnd(stream);
    });
}

class Readable extends Stream {
    constructor(options) {
        super();
        const opts = options || {};
        this._readableState = {
            buffer: [],
            flowing: null,
            ended: false,
            endEmitted: false,
            destroyed: false,
            reading: false,
            flowScheduled: false,
            objectMode: !!(opts.objectMode || opts.readableObjectMode),
            encoding: opts.encoding || null,
            highWaterMark: typeof opts.highWaterMark === "number" ? opts.highWaterMark : 16384,
        };
        this.readable = true;
        if (typeof opts.read === "function") this._read = opts.read;
        if (typeof opts.destroy === "function") this._destroy = opts.destroy;
        const self = this;
        this.on("newListener", function (event) {
            if (event === "data") queueMicrotask(() => self.resume());
        });
    }
    _read() {}
    push(chunk, encoding) {
        const st = this._readableState;
        if (chunk === null) {
            st.ended = true;
            if (st.flowing) flow(this);
            else { this.emit("readable"); maybeEmitEnd(this); }
            return false;
        }
        if (!st.objectMode && typeof chunk === "string" && encoding && encoding !== "utf8" && encoding !== "utf-8") {
            chunk = Buffer.from(chunk, encoding);
        }
        st.buffer.push(chunk);
        if (st.flowing) flow(this);
        else this.emit("readable");
        return st.buffer.length < st.highWaterMark;
    }
    unshift(chunk) {
        if (chunk === null || chunk === undefined) return;
        this._readableState.buffer.unshift(chunk);
    }
    read() {
        const st = this._readableState;
        if (st.buffer.length === 0 && !st.ended && !st.destroyed && !st.reading) {
            st.reading = true;
            try { this._read(st.highWaterMark); } finally { st.reading = false; }
        }
        if (st.buffer.length === 0) {
            maybeEmitEnd(this);
            return null;
        }
        return decodeForRead(st, st.buffer.shift());
    }
    setEncoding(encoding) { this._readableState.encoding = encoding; return this; }
    pause() { this._readableState.flowing = false; return this; }
    resume() {
        const st = this._readableState;
        if (st.flowing !== true) {
            st.flowing = true;
            flow(this);
        }
        return this;
    }
    isPaused() { return this._readableState.flowing === false; }
    unpipe() { return this; }
    destroy(err) {
        const st = this._readableState;
        if (st.destroyed) return this;
        st.destroyed = true;
        this.readable = false;
        const self = this;
        const done = function (e) {
            queueMicrotask(() => {
                if (e) self.emit("error", e);
                self.emit("close");
            });
        };
        if (typeof this._destroy === "function") this._destroy(err || null, done);
        else done(err);
        return this;
    }
    get destroyed() { return this._readableState.destroyed; }
    get readableEnded() { return this._readableState.endEmitted; }
    get readableObjectMode() { return this._readableState.objectMode; }
    [Symbol.asyncIterator]() {
        const stream = this;
        const iterator = {
            next() {
                const st = stream._readableState;
                const chunk = stream.read();
                if (chunk !== null) return Promise.resolve({ value: chunk, done: false });
                if ((st.ended && st.buffer.length === 0) || st.destroyed) {
                    return Promise.resolve({ value: undefined, done: true });
                }
                return new Promise((resolve, reject) => {
                    function cleanup() {
                        stream.off("readable", onReadable);
                        stream.off("end", onEnd);
                        stream.off("error", onError);
                    }
                    function onReadable() { cleanup(); resolve(iterator.next()); }
                    function onEnd() { cleanup(); resolve({ value: undefined, done: true }); }
                    function onError(err) { cleanup(); reject(err); }
                    stream.once("readable", onReadable);
                    stream.once("end", onEnd);
                    stream.once("error", onError);
                });
            },
            return() {
                stream.destroy();
                return Promise.resolve({ value: undefined, done: true });
            },
        };
        iterator[Symbol.asyncIterator] = function () { return iterator; };
        return iterator;
    }
    static from(iterable, options) {
        const readable = new Readable(Object.assign({ objectMode: true }, options || {}));
        const feed = async function () {
            try {
                if (iterable && typeof iterable[Symbol.asyncIterator] === "function") {
                    for await (const chunk of iterable) readable.push(chunk);
                } else if (iterable && typeof iterable[Symbol.iterator] === "function") {
                    for (const chunk of iterable) readable.push(chunk);
                } else if (iterable && typeof iterable.then === "function") {
                    readable.push(await iterable);
                } else {
                    readable.push(iterable);
                }
                readable.push(null);
            } catch (err) {
                readable.destroy(err);
            }
        };
        queueMicrotask(feed);
        return readable;
    }
}

function writableHighWaterMark(opts) {
    if (typeof opts.writableHighWaterMark === "number") return opts.writableHighWaterMark;
    if (typeof opts.highWaterMark === "number") return opts.highWaterMark;
    return 16384;
}
// The buffered "length" a chunk contributes, in Node's units: one per chunk in
// object mode, bytes otherwise.
function chunkLength(chunk, encoding, objectMode) {
    if (objectMode) return 1;
    if (typeof chunk === "string") return Buffer.byteLength(chunk, encoding || "utf8");
    if (chunk && typeof chunk.length === "number") return chunk.length;
    return 1;
}
function initWritableState(stream, opts) {
    stream._writableState = {
        ended: false,
        finished: false,
        finishScheduled: false,
        destroyed: false,
        pending: 0,
        length: 0,
        needDrain: false,
        highWaterMark: writableHighWaterMark(opts),
        objectMode: !!(opts.objectMode || opts.writableObjectMode),
    };
    stream.writable = true;
    if (typeof opts.write === "function") stream._write = opts.write;
    if (typeof opts.final === "function") stream._final = opts.final;
    if (typeof opts.destroy === "function" && !stream._destroy) stream._destroy = opts.destroy;
}
function maybeFinish(stream) {
    const st = stream._writableState;
    if (!st.ended || st.finishScheduled || st.pending > 0 || st.destroyed) return;
    st.finishScheduled = true;
    const emitFinish = function () {
        st.finished = true;
        queueMicrotask(() => {
            stream.emit("finish");
            const rst = stream._readableState;
            if (!rst || rst.endEmitted) queueMicrotask(() => stream.emit("close"));
        });
    };
    if (typeof stream._final === "function") {
        stream._final(function (err) {
            if (err) { queueMicrotask(() => stream.emit("error", err)); return; }
            emitFinish();
        });
    } else {
        emitFinish();
    }
}
const writableMethods = {
    _write(chunk, encoding, callback) {
        callback(new Error("The _write() method is not implemented"));
    },
    write(chunk, encoding, callback) {
        if (typeof encoding === "function") { callback = encoding; encoding = null; }
        const st = this._writableState;
        const self = this;
        if (st.ended) {
            const err = new Error("write after end");
            if (callback) queueMicrotask(() => callback(err));
            queueMicrotask(() => self.emit("error", err));
            return false;
        }
        st.pending++;
        const len = chunkLength(chunk, encoding, st.objectMode);
        st.length += len;
        let settled = false;
        const done = function (err) {
            if (settled) return;
            settled = true;
            st.pending--;
            st.length -= len;
            if (err) queueMicrotask(() => self.emit("error", err));
            if (callback) queueMicrotask(() => callback(err || null));
            // Everything this stream had buffered has moved on: tell a writer
            // that got `false` back that it may resume.
            if (st.needDrain && st.length === 0 && !st.ended && !st.destroyed) {
                st.needDrain = false;
                queueMicrotask(() => self.emit("drain"));
            }
            maybeFinish(self);
        };
        try {
            this._write(chunk, encoding || "utf8", done);
        } catch (err) {
            done(err);
        }
        // Honest backpressure: `false` once the still-unflushed length reaches
        // the high-water mark (a transform holding its callback keeps its
        // chunk counted), and a 'drain' follows when that length reaches zero.
        const ret = st.length < st.highWaterMark;
        if (!ret) st.needDrain = true;
        return ret;
    },
    end(chunk, encoding, callback) {
        if (typeof chunk === "function") { callback = chunk; chunk = undefined; encoding = undefined; }
        else if (typeof encoding === "function") { callback = encoding; encoding = undefined; }
        if (chunk !== undefined && chunk !== null) this.write(chunk, encoding);
        const st = this._writableState;
        st.ended = true;
        this.writable = false;
        if (callback) this.once("finish", callback);
        maybeFinish(this);
        return this;
    },
    cork() {},
    uncork() {},
    setDefaultEncoding() { return this; },
};

class Writable extends Stream {
    constructor(options) {
        super();
        initWritableState(this, options || {});
        if (options && typeof options.destroy === "function") this._destroy = options.destroy;
    }
    destroy(err) {
        const st = this._writableState;
        if (st.destroyed) return this;
        st.destroyed = true;
        this.writable = false;
        const self = this;
        const done = function (e) {
            queueMicrotask(() => {
                if (e) self.emit("error", e);
                self.emit("close");
            });
        };
        if (typeof this._destroy === "function") this._destroy(err || null, done);
        else done(err);
        return this;
    }
    get destroyed() { return this._writableState.destroyed; }
    get writableEnded() { return this._writableState.ended; }
    get writableFinished() { return this._writableState.finished; }
    get writableObjectMode() { return this._writableState.objectMode; }
}
Object.assign(Writable.prototype, writableMethods);

class Duplex extends Readable {
    constructor(options) {
        super(options);
        initWritableState(this, options || {});
    }
    get writableEnded() { return this._writableState.ended; }
    get writableFinished() { return this._writableState.finished; }
}
Object.assign(Duplex.prototype, writableMethods);

class Transform extends Duplex {
    constructor(options) {
        super(options);
        const opts = options || {};
        if (typeof opts.transform === "function") this._transform = opts.transform;
        if (typeof opts.flush === "function") this._flush = opts.flush;
    }
    _transform(chunk, encoding, callback) { callback(null, chunk); }
}
Transform.prototype._write = function (chunk, encoding, callback) {
    const self = this;
    const rst = this._readableState;
    const wst = this._writableState;
    const before = rst.buffer.length;
    let called = false;
    const after = function (err, data) {
        if (called) {
            // Node surfaces a second call to a transform callback as an
            // ERR_MULTIPLE_CALLBACK error on the stream rather than silently
            // ignoring it.
            const dup = new Error("Callback called multiple times");
            dup.code = "ERR_MULTIPLE_CALLBACK";
            self.destroy(dup);
            return;
        }
        called = true;
        if (err) return callback(err);
        if (data !== undefined && data !== null) self.push(data);
        // Hold the write callback while the readable side is over its
        // high-water mark, so the writable side reports backpressure until a
        // reader drains it (this is what makes 'drain' meaningful).
        if (wst.ended || before === rst.buffer.length || rst.buffer.length < rst.highWaterMark) {
            callback(null);
        } else {
            self._transformCallback = callback;
        }
    };
    try {
        this._transform(chunk, encoding, after);
    } catch (err) {
        if (!called) { called = true; callback(err); }
    }
};
Transform.prototype._read = function () {
    // A reader took a chunk: release the write callback the transform parked.
    const callback = this._transformCallback;
    if (callback) {
        this._transformCallback = null;
        callback(null);
    }
};
Transform.prototype._final = function (callback) {
    const self = this;
    const done = function (err, data) {
        if (err) return callback(err);
        if (data !== undefined && data !== null) self.push(data);
        self.push(null);
        callback(null);
    };
    if (typeof this._flush === "function") {
        try { this._flush(done); } catch (err) { callback(err); }
    } else {
        done(null);
    }
};

class PassThrough extends Transform {}

function isWritableLike(stream) {
    return stream && typeof stream.write === "function" && stream.readable !== true;
}
function finished(stream, options, callback) {
    if (typeof options === "function") { callback = options; options = {}; }
    let done = false;
    const settle = function (err) {
        if (done) return;
        done = true;
        callback(err || null);
    };
    stream.once("error", settle);
    stream.once("close", function () { settle(null); });
    if (isWritableLike(stream)) stream.once("finish", function () { settle(null); });
    else stream.once("end", function () { settle(null); });
    return function cleanup() { done = true; };
}
function pipeline(...args) {
    const callback = typeof args[args.length - 1] === "function" &&
        typeof args[args.length - 1].pipe !== "function" &&
        typeof args[args.length - 1].write !== "function"
        ? args.pop()
        : null;
    if (args.length < 2) throw new Error("pipeline requires at least a source and a destination");
    let settled = false;
    const settle = function (err) {
        if (settled) return;
        settled = true;
        if (callback) callback(err || null);
        else if (err) queueMicrotask(() => { throw err; });
    };
    for (const stream of args) {
        stream.once("error", settle);
    }
    for (let i = 0; i < args.length - 1; i++) {
        args[i].pipe(args[i + 1]);
    }
    const last = args[args.length - 1];
    finished(last, {}, settle);
    return last;
}
const promises = {
    pipeline(...args) {
        return new Promise((resolve, reject) => {
            pipeline(...args, function (err) {
                if (err) reject(err);
                else resolve();
            });
        });
    },
    finished(stream, options) {
        return new Promise((resolve, reject) => {
            finished(stream, options || {}, function (err) {
                if (err) reject(err);
                else resolve();
            });
        });
    },
};
function isReadable(stream) {
    return !!(stream && stream.readable === true);
}
function isWritable(stream) {
    return !!(stream && stream.writable === true);
}
function addAbortSignal(signal, stream) {
    if (signal && typeof signal.addEventListener === "function") {
        signal.addEventListener("abort", function () {
            const err = new Error("The operation was aborted");
            err.name = "AbortError";
            stream.destroy(err);
        }, { once: true });
    }
    return stream;
}
Stream.Stream = Stream;
Stream.Readable = Readable;
Stream.Writable = Writable;
Stream.Duplex = Duplex;
Stream.Transform = Transform;
Stream.PassThrough = PassThrough;
Stream.pipeline = pipeline;
Stream.finished = finished;
Stream.promises = promises;
Stream.isReadable = isReadable;
Stream.isWritable = isWritable;
Stream.addAbortSignal = addAbortSignal;
export {
    Stream, Readable, Writable, Duplex, Transform, PassThrough,
    pipeline, finished, promises, isReadable, isWritable, addAbortSignal,
};
export default Stream;
"#;

// node:stream/promises — re-exports the promisified pipeline/finished built
// alongside the stream shim.
const STREAM_PROMISES_SHIM: &str = r#"
import { promises } from "node:stream";
export const pipeline = promises.pipeline;
export const finished = promises.finished;
export default promises;
"#;

// node:stream/consumers — collectors over any (async-)iterable stream.
const STREAM_CONSUMERS_SHIM: &str = r#"
import { Buffer } from "node:buffer";

async function collect(stream) {
    const chunks = [];
    for await (const chunk of stream) chunks.push(chunk);
    return chunks;
}
export async function text(stream) {
    const chunks = await collect(stream);
    let out = "";
    for (const chunk of chunks) {
        out += typeof chunk === "string" ? chunk : Buffer.from(chunk).toString("utf8");
    }
    return out;
}
export async function json(stream) {
    return JSON.parse(await text(stream));
}
export async function buffer(stream) {
    const chunks = await collect(stream);
    const buffers = [];
    for (const chunk of chunks) {
        buffers.push(typeof chunk === "string" ? Buffer.from(chunk) : Buffer.from(chunk));
    }
    return Buffer.concat(buffers);
}
export async function arrayBuffer(stream) {
    const buf = await buffer(stream);
    return buf.buffer.slice(buf.byteOffset, buf.byteOffset + buf.byteLength);
}
export async function blob(stream) {
    if (typeof Blob === "undefined") {
        throw new Error("stream/consumers.blob is not supported in the Chidori runtime (no Blob global)");
    }
    return new Blob(await collect(stream));
}
export default { text, json, buffer, arrayBuffer, blob };
"#;

// node:stream/web — WHATWG stream classes. The engine does not implement web
// streams natively; any globals that exist are re-exported, and the rest are
// fail-loud classes so `instanceof` checks still link.
const STREAM_WEB_SHIM: &str = r#"
function missing(name) {
    return class {
        constructor() {
            throw new Error("stream/web." + name + " is not available in the Chidori runtime (WHATWG streams are not implemented by the embedded engine)");
        }
    };
}
const g = globalThis;
export const ReadableStream = g.ReadableStream || missing("ReadableStream");
export const WritableStream = g.WritableStream || missing("WritableStream");
export const TransformStream = g.TransformStream || missing("TransformStream");
export const ReadableStreamDefaultReader = g.ReadableStreamDefaultReader || missing("ReadableStreamDefaultReader");
export const ReadableStreamDefaultController = g.ReadableStreamDefaultController || missing("ReadableStreamDefaultController");
export const WritableStreamDefaultWriter = g.WritableStreamDefaultWriter || missing("WritableStreamDefaultWriter");
export const WritableStreamDefaultController = g.WritableStreamDefaultController || missing("WritableStreamDefaultController");
export const TransformStreamDefaultController = g.TransformStreamDefaultController || missing("TransformStreamDefaultController");
export const ByteLengthQueuingStrategy = g.ByteLengthQueuingStrategy || missing("ByteLengthQueuingStrategy");
export const CountQueuingStrategy = g.CountQueuingStrategy || missing("CountQueuingStrategy");
export const TextEncoderStream = g.TextEncoderStream || missing("TextEncoderStream");
export const TextDecoderStream = g.TextDecoderStream || missing("TextDecoderStream");
export const CompressionStream = g.CompressionStream || missing("CompressionStream");
export const DecompressionStream = g.DecompressionStream || missing("DecompressionStream");
export default {
    ReadableStream, WritableStream, TransformStream,
    ReadableStreamDefaultReader, ReadableStreamDefaultController,
    WritableStreamDefaultWriter, WritableStreamDefaultController,
    TransformStreamDefaultController, ByteLengthQueuingStrategy,
    CountQueuingStrategy, TextEncoderStream, TextDecoderStream,
    CompressionStream, DecompressionStream,
};
"#;

// ---------------------------------------------------------------------------
// Fail-loud stubs: capabilities the runtime deliberately does not grant.
// Every module links and exposes Node's names; using them throws a clear
// error naming the missing capability, so incompatibility surfaces at first
// use — never as a silent no-op or a resolution failure deep in a package.
// ---------------------------------------------------------------------------

// node:child_process — no subprocess capability.
const CHILD_PROCESS_SHIM: &str = r#"
function unsupported(name) {
    return function () {
        throw new Error("child_process." + name + " is not supported in the Chidori runtime (agents cannot spawn subprocesses; use chidori tools for host-side work)");
    };
}
export const spawn = unsupported("spawn");
export const exec = unsupported("exec");
export const execFile = unsupported("execFile");
export const fork = unsupported("fork");
export const spawnSync = unsupported("spawnSync");
export const execSync = unsupported("execSync");
export const execFileSync = unsupported("execFileSync");
export class ChildProcess {
    constructor() { unsupported("ChildProcess")(); }
}
export default { spawn, exec, execFile, fork, spawnSync, execSync, execFileSync, ChildProcess };
"#;

// node:cluster — single-process runtime; primary-side introspection answers
// honestly, fork throws.
const CLUSTER_SHIM: &str = r#"
export const isPrimary = true;
export const isMaster = true;
export const isWorker = false;
export const worker = null;
export const workers = {};
export const settings = {};
export const SCHED_NONE = 1;
export const SCHED_RR = 2;
export const schedulingPolicy = SCHED_RR;
export function fork() {
    throw new Error("cluster.fork is not supported in the Chidori runtime (single-process deterministic engine; use chidori sub-agents for parallelism)");
}
export function setupPrimary() {}
export function setupMaster() {}
export function disconnect(callback) {
    if (typeof callback === "function") queueMicrotask(callback);
}
export default {
    isPrimary, isMaster, isWorker, worker, workers, settings,
    SCHED_NONE, SCHED_RR, schedulingPolicy,
    fork, setupPrimary, setupMaster, disconnect,
};
"#;

// node:dgram — no UDP capability.
const DGRAM_SHIM: &str = r#"
function unsupported(name) {
    return function () {
        throw new Error("dgram." + name + " is not supported in the Chidori runtime (no UDP socket capability)");
    };
}
export const createSocket = unsupported("createSocket");
export class Socket {
    constructor() { unsupported("Socket")(); }
}
export default { createSocket, Socket };
"#;

// node:dns — no resolver capability is exposed to agent code (fetch resolves
// hosts inside the captured HTTP host op). Callback APIs deliver the error
// through the callback, Node-style; promise APIs reject.
const DNS_SHIM: &str = r#"
function dnsError(name) {
    const err = new Error("dns." + name + " is not supported in the Chidori runtime (no DNS capability; fetch resolves hostnames inside the captured HTTP host op)");
    err.code = "ENOTSUP";
    err.syscall = name;
    return err;
}
function callbackFail(name, args) {
    const cb = args[args.length - 1];
    if (typeof cb === "function") {
        queueMicrotask(() => cb(dnsError(name)));
        return;
    }
    throw dnsError(name);
}
export function lookup(...args) { callbackFail("lookup", args); }
export function lookupService(...args) { callbackFail("lookupService", args); }
export function resolve(...args) { callbackFail("resolve", args); }
export function resolve4(...args) { callbackFail("resolve4", args); }
export function resolve6(...args) { callbackFail("resolve6", args); }
export function resolveAny(...args) { callbackFail("resolveAny", args); }
export function resolveCname(...args) { callbackFail("resolveCname", args); }
export function resolveCaa(...args) { callbackFail("resolveCaa", args); }
export function resolveMx(...args) { callbackFail("resolveMx", args); }
export function resolveNaptr(...args) { callbackFail("resolveNaptr", args); }
export function resolveNs(...args) { callbackFail("resolveNs", args); }
export function resolvePtr(...args) { callbackFail("resolvePtr", args); }
export function resolveSoa(...args) { callbackFail("resolveSoa", args); }
export function resolveSrv(...args) { callbackFail("resolveSrv", args); }
export function resolveTxt(...args) { callbackFail("resolveTxt", args); }
export function reverse(...args) { callbackFail("reverse", args); }
export function setServers() {}
export function getServers() { return []; }
export function setDefaultResultOrder() {}
export function getDefaultResultOrder() { return "verbatim"; }
export class Resolver {
    constructor() {
        throw new Error("dns.Resolver is not supported in the Chidori runtime (no DNS capability)");
    }
}
export const NODATA = "ENODATA";
export const FORMERR = "EFORMERR";
export const SERVFAIL = "ESERVFAIL";
export const NOTFOUND = "ENOTFOUND";
export const NOTIMP = "ENOTIMP";
export const REFUSED = "EREFUSED";
function promiseFail(name) {
    return function () {
        return new Promise((resolvePromise, rejectPromise) => rejectPromise(dnsError(name)));
    };
}
export const promises = {
    lookup: promiseFail("lookup"),
    lookupService: promiseFail("lookupService"),
    resolve: promiseFail("resolve"),
    resolve4: promiseFail("resolve4"),
    resolve6: promiseFail("resolve6"),
    resolveAny: promiseFail("resolveAny"),
    resolveCname: promiseFail("resolveCname"),
    resolveCaa: promiseFail("resolveCaa"),
    resolveMx: promiseFail("resolveMx"),
    resolveNaptr: promiseFail("resolveNaptr"),
    resolveNs: promiseFail("resolveNs"),
    resolvePtr: promiseFail("resolvePtr"),
    resolveSoa: promiseFail("resolveSoa"),
    resolveSrv: promiseFail("resolveSrv"),
    resolveTxt: promiseFail("resolveTxt"),
    reverse: promiseFail("reverse"),
    setServers() {},
    getServers() { return []; },
    setDefaultResultOrder() {},
    getDefaultResultOrder() { return "verbatim"; },
    Resolver,
};
export default {
    lookup, lookupService, resolve, resolve4, resolve6, resolveAny,
    resolveCname, resolveCaa, resolveMx, resolveNaptr, resolveNs, resolvePtr,
    resolveSoa, resolveSrv, resolveTxt, reverse,
    setServers, getServers, setDefaultResultOrder, getDefaultResultOrder,
    Resolver, promises, NODATA, FORMERR, SERVFAIL, NOTFOUND, NOTIMP, REFUSED,
};
"#;

// node:dns/promises — the promise surface of the dns stub.
const DNS_PROMISES_SHIM: &str = r#"
import { promises } from "node:dns";
export const lookup = promises.lookup;
export const lookupService = promises.lookupService;
export const resolve = promises.resolve;
export const resolve4 = promises.resolve4;
export const resolve6 = promises.resolve6;
export const resolveAny = promises.resolveAny;
export const resolveCname = promises.resolveCname;
export const resolveCaa = promises.resolveCaa;
export const resolveMx = promises.resolveMx;
export const resolveNaptr = promises.resolveNaptr;
export const resolveNs = promises.resolveNs;
export const resolvePtr = promises.resolvePtr;
export const resolveSoa = promises.resolveSoa;
export const resolveSrv = promises.resolveSrv;
export const resolveTxt = promises.resolveTxt;
export const reverse = promises.reverse;
export const setServers = promises.setServers;
export const getServers = promises.getServers;
export const setDefaultResultOrder = promises.setDefaultResultOrder;
export const getDefaultResultOrder = promises.getDefaultResultOrder;
export const Resolver = promises.Resolver;
export default promises;
"#;

// node:http2 — no HTTP/2 transport (chidori networking is the captured
// HTTP/1.1-semantics host op). Header-name constants are provided since
// packages read them without opening connections.
const HTTP2_SHIM: &str = r#"
function unsupported(name) {
    return function () {
        throw new Error("http2." + name + " is not supported in the Chidori runtime (no HTTP/2 transport; use fetch or node:http/node:https)");
    };
}
export const connect = unsupported("connect");
export const createServer = unsupported("createServer");
export const createSecureServer = unsupported("createSecureServer");
export const getPackedSettings = unsupported("getPackedSettings");
export const getUnpackedSettings = unsupported("getUnpackedSettings");
export function getDefaultSettings() {
    return {
        headerTableSize: 4096, enablePush: true, initialWindowSize: 65535,
        maxFrameSize: 16384, maxConcurrentStreams: 4294967295, maxHeaderListSize: 65535,
    };
}
export const sensitiveHeaders = Symbol.for("nodejs.http2.sensitiveHeaders");
export const constants = Object.freeze({
    HTTP2_HEADER_STATUS: ":status",
    HTTP2_HEADER_METHOD: ":method",
    HTTP2_HEADER_AUTHORITY: ":authority",
    HTTP2_HEADER_SCHEME: ":scheme",
    HTTP2_HEADER_PATH: ":path",
    HTTP2_HEADER_CONTENT_TYPE: "content-type",
    HTTP2_HEADER_CONTENT_LENGTH: "content-length",
    HTTP2_METHOD_GET: "GET",
    HTTP2_METHOD_POST: "POST",
    NGHTTP2_NO_ERROR: 0,
    NGHTTP2_CANCEL: 8,
});
export default {
    connect, createServer, createSecureServer, getDefaultSettings,
    getPackedSettings, getUnpackedSettings, sensitiveHeaders, constants,
};
"#;

// node:inspector — there is no attachable debugger; open/close are no-ops
// (Node's own inspector.open is best-effort) and Session cannot connect.
const INSPECTOR_SHIM: &str = r#"
export function open() {}
export function close() {}
export function url() { return undefined; }
export function waitForDebugger() {}
const inspectorConsole = globalThis.console;
export { inspectorConsole as console };
export class Session {
    constructor() {}
    connect() {
        throw new Error("inspector.Session.connect is not supported in the Chidori runtime (no debugger transport)");
    }
    connectToMainThread() {
        throw new Error("inspector.Session.connectToMainThread is not supported in the Chidori runtime (no debugger transport)");
    }
    disconnect() {}
    post() {
        throw new Error("inspector.Session.post is not supported in the Chidori runtime (no debugger transport)");
    }
}
export default { open, close, url, waitForDebugger, console: inspectorConsole, Session };
"#;

// node:inspector/promises — same stub surface, promise flavor.
const INSPECTOR_PROMISES_SHIM: &str = r#"
import { open, close, url, waitForDebugger, console as inspectorConsole } from "node:inspector";
export { open, close, url, waitForDebugger };
export { inspectorConsole as console };
export class Session {
    constructor() {}
    connect() {
        throw new Error("inspector.Session.connect is not supported in the Chidori runtime (no debugger transport)");
    }
    disconnect() {}
    post() {
        return Promise.reject(new Error("inspector.Session.post is not supported in the Chidori runtime (no debugger transport)"));
    }
}
export default { open, close, url, waitForDebugger, console: inspectorConsole, Session };
"#;

// node:readline — no interactive stdin is attached to an agent run. The
// cursor-motion helpers return false (their "stream is not a TTY" behavior);
// interface construction throws.
const READLINE_SHIM: &str = r#"
function noStdin(name) {
    return function () {
        throw new Error("readline." + name + " is not supported in the Chidori runtime (no interactive stdin is attached to an agent run)");
    };
}
export const createInterface = noStdin("createInterface");
export class Interface {
    constructor() { noStdin("Interface")(); }
}
export function clearLine() { return false; }
export function clearScreenDown() { return false; }
export function cursorTo() { return false; }
export function moveCursor() { return false; }
export function emitKeypressEvents() {}
export default {
    createInterface, Interface, clearLine, clearScreenDown,
    cursorTo, moveCursor, emitKeypressEvents,
};
"#;

// node:readline/promises — promise flavor of the readline stub.
const READLINE_PROMISES_SHIM: &str = r#"
function noStdin(name) {
    return function () {
        throw new Error("readline/promises." + name + " is not supported in the Chidori runtime (no interactive stdin is attached to an agent run)");
    };
}
export const createInterface = noStdin("createInterface");
export class Interface {
    constructor() { noStdin("Interface")(); }
}
export class Readline {
    constructor() { noStdin("Readline")(); }
}
export default { createInterface, Interface, Readline };
"#;

// node:repl — no interactive terminal.
const REPL_SHIM: &str = r#"
function unsupported(name) {
    return function () {
        throw new Error("repl." + name + " is not supported in the Chidori runtime (no interactive terminal)");
    };
}
export const start = unsupported("start");
export class REPLServer {
    constructor() { unsupported("REPLServer")(); }
}
export const REPL_MODE_SLOPPY = Symbol.for("repl.mode.sloppy");
export const REPL_MODE_STRICT = Symbol.for("repl.mode.strict");
export default { start, REPLServer, REPL_MODE_SLOPPY, REPL_MODE_STRICT };
"#;

// node:tls — no raw socket capability; TLS termination happens inside the
// captured HTTP host op.
const TLS_SHIM: &str = r#"
function unsupported(name) {
    return function () {
        throw new Error("tls." + name + " is not supported in the Chidori runtime (no raw socket capability; HTTPS requests go through fetch/node:https, which terminate TLS inside the captured HTTP host op)");
    };
}
export const connect = unsupported("connect");
export const createServer = unsupported("createServer");
export const createSecureContext = unsupported("createSecureContext");
export const checkServerIdentity = unsupported("checkServerIdentity");
export class TLSSocket {
    constructor() { unsupported("TLSSocket")(); }
}
export class Server {
    constructor() { unsupported("Server")(); }
}
export const rootCertificates = Object.freeze([]);
export const DEFAULT_ECDH_CURVE = "auto";
export const DEFAULT_MIN_VERSION = "TLSv1.2";
export const DEFAULT_MAX_VERSION = "TLSv1.3";
export const DEFAULT_CIPHERS = "";
export default {
    connect, createServer, createSecureContext, checkServerIdentity,
    TLSSocket, Server, rootCertificates,
    DEFAULT_ECDH_CURVE, DEFAULT_MIN_VERSION, DEFAULT_MAX_VERSION, DEFAULT_CIPHERS,
};
"#;

// node:trace_events — no trace collector.
const TRACE_EVENTS_SHIM: &str = r#"
export function createTracing() {
    throw new Error("trace_events.createTracing is not supported in the Chidori runtime (no trace collector; host-call capture is chidori's tracing surface)");
}
export function getEnabledCategories() { return undefined; }
export default { createTracing, getEnabledCategories };
"#;

// node:vm — functional, SAME-REALM. Evaluation goes through the engine's own
// `eval`/`Function` intrinsics, so contextified code runs in the one existing
// realm: same determinism prelude, same captured host ops, same capability
// policy as every other line of agent code. A `vm` "context" here is a scope
// object pushed with `with`, not a fresh global — no capability is granted
// that a plain `eval` did not already grant, which is why this is a shim and
// not a stub.
//
// The honest divergences, all consequences of there being exactly one realm:
//
//   * Contexts share the realm's intrinsics. `vm.runInNewContext('[]')
//     instanceof Array` is `true` here and `false` in Node, and there is no
//     "different Array from a different context" to test against — the
//     cross-realm-identity half of Node's `vm` is not meaningful in chidori.
//   * `this` inside contextified code is the realm's `globalThis`, not the
//     contextified sandbox.
//   * A leading `"use strict"` directive loses its directive position (the
//     source is spliced inside a `with` block, where strict mode is a syntax
//     error), so such code runs sloppy.
//   * Timeouts (`options.timeout`, `breakOnSigint`) are ignored: the runtime
//     is single-threaded and cooperatively scheduled, with no interruptible
//     evaluation.
//
// `measureMemory` (needs V8 heap statistics) and `SourceTextModule` (needs the
// module-record introspection API) have no counterpart and stay fail-loud.
const VM_SHIM: &str = r#"
// Indirect eval: `(0, eval)(src)` semantics — compiles `src` as a script in
// the realm's global scope rather than as a direct eval in this module's.
const indirectEval = eval;

// Contexts are tracked out-of-band so `isContext` cannot be spoofed and a
// sandbox never grows a visible marker property (Node uses an internal
// pointer on the object; a WeakSet is the closest JS-visible equivalent).
const contexts = new WeakSet();

// The slot the `with` head reads the sandbox out of. Namespaced so contextified
// code that enumerates `globalThis` sees nothing surprising after a run — the
// slot is restored to its prior state in the `finally` below.
const kSlot = "__chidori_vm_sandbox__";

function typeError(code, message) {
    const err = new TypeError(message);
    err.code = code;
    return err;
}

function checkCode(code, name) {
    if (typeof code !== "string") {
        throw typeError(
            "ERR_INVALID_ARG_TYPE",
            'The "' + name + '" argument must be of type string. Received ' +
            (code === null ? "null" : typeof code)
        );
    }
    return code;
}

function checkSandbox(sandbox, name) {
    if (sandbox === undefined) return {};
    if (sandbox === null || (typeof sandbox !== "object" && typeof sandbox !== "function")) {
        throw typeError(
            "ERR_INVALID_ARG_TYPE",
            'The "' + name + '" argument must be of type object. Received ' +
            (sandbox === null ? "null" : typeof sandbox)
        );
    }
    return sandbox;
}

export function isContext(sandbox) {
    if (sandbox === null || (typeof sandbox !== "object" && typeof sandbox !== "function")) {
        throw typeError(
            "ERR_INVALID_ARG_TYPE",
            'The "object" argument must be of type object. Received ' +
            (sandbox === null ? "null" : typeof sandbox)
        );
    }
    return contexts.has(sandbox);
}

export function createContext(contextObject, options) {
    const sandbox = contextObject === undefined ? {} : checkSandbox(contextObject, "contextObject");
    contexts.add(sandbox);
    return sandbox;
}

// Evaluate `code` with `sandbox`'s properties in scope.
//
// Reads and writes of properties the sandbox already has resolve through the
// `with` object record. The two remaining cases — an implicit global
// assignment (`x = 1` where `x` is not yet on the sandbox) and a hoisted
// `var`/function declaration — land on the realm's global object instead,
// because a `with` object record is never the variable environment. Both are
// reclaimed afterwards by diffing the global's own keys against a snapshot
// taken before the run and moving whatever appeared onto the sandbox. A key
// the `with` write already delivered to the sandbox wins (a `var x = 1`
// hoists `x` onto the global as `undefined` and then assigns the *sandbox*
// binding through the `with`), so the move never clobbers a live value.
function evaluateInScope(code, sandbox) {
    const hadSlot = Object.prototype.hasOwnProperty.call(globalThis, kSlot);
    const prevSlot = hadSlot ? Object.getOwnPropertyDescriptor(globalThis, kSlot) : undefined;
    const before = new Set(Object.getOwnPropertyNames(globalThis));
    Object.defineProperty(globalThis, kSlot, {
        value: sandbox, writable: true, enumerable: false, configurable: true,
    });
    try {
        return indirectEval("with (globalThis." + kSlot + ") {\n" + code + "\n}");
    } finally {
        for (const key of Object.getOwnPropertyNames(globalThis)) {
            if (key === kSlot || before.has(key)) continue;
            const desc = Object.getOwnPropertyDescriptor(globalThis, key);
            try { delete globalThis[key]; } catch (e) { /* non-configurable */ }
            if (desc !== undefined && !Object.prototype.hasOwnProperty.call(sandbox, key)) {
                try { Object.defineProperty(sandbox, key, desc); } catch (e) { /* frozen sandbox */ }
            }
        }
        if (hadSlot) Object.defineProperty(globalThis, kSlot, prevSlot);
        else delete globalThis[kSlot];
    }
}

export function runInContext(code, contextifiedObject, options) {
    checkCode(code, "code");
    const sandbox = checkSandbox(contextifiedObject, "contextifiedObject");
    if (!contexts.has(sandbox)) {
        throw typeError("ERR_INVALID_ARG_TYPE", 'The "contextifiedObject" argument must be a vm.Context');
    }
    return evaluateInScope(code, sandbox);
}

export function runInNewContext(code, contextObject, options) {
    checkCode(code, "code");
    return evaluateInScope(code, createContext(contextObject));
}

export function runInThisContext(code, options) {
    checkCode(code, "code");
    return indirectEval(code);
}

export function compileFunction(code, params, options) {
    checkCode(code, "code");
    const args = params === undefined ? [] : params;
    if (!Array.isArray(args)) {
        throw typeError("ERR_INVALID_ARG_TYPE", 'The "params" argument must be an instance of Array');
    }
    const opts = options === undefined || options === null ? {} : options;
    const names = args.map(String);
    // `parsingContext` binds the compiled body to a contextified sandbox the
    // same way `runInContext` does; the extra `with` head is invisible to the
    // returned function's signature.
    const ctx = opts.parsingContext;
    if (ctx !== undefined && ctx !== null) {
        if (!contexts.has(ctx)) {
            throw typeError("ERR_INVALID_ARG_TYPE", 'The "options.parsingContext" argument must be a vm.Context');
        }
        const inner = new Function(kSlot, ...names, "with (" + kSlot + ") {\n" + code + "\n}");
        return function (...callArgs) { return inner.call(this, ctx, ...callArgs); };
    }
    return new Function(...names, code);
}

export class Script {
    constructor(code, options) {
        this.code = checkCode(code, "code");
        const opts = options === undefined || options === null ? {} : options;
        this.filename = opts.filename === undefined ? "evalmachine.<anonymous>" : String(opts.filename);
        // Node compiles eagerly, so a syntax error surfaces from `new
        // vm.Script(...)` rather than from the first run. `new Function` is the
        // only compile-without-running primitive available here; it accepts a
        // superset of script syntax (top-level `return`), so a script that only
        // *runs* differently still constructs — the syntax errors this catches
        // are the ones a real parse would have caught too.
        new Function(this.code);
    }
    runInContext(contextifiedObject, options) {
        return runInContext(this.code, contextifiedObject, options);
    }
    runInNewContext(contextObject, options) {
        return runInNewContext(this.code, contextObject, options);
    }
    runInThisContext(options) {
        return runInThisContext(this.code, options);
    }
    createCachedData() {
        throw new Error("vm.Script.createCachedData is not supported in the Chidori runtime (there is no V8 code cache to serialize)");
    }
}

export function measureMemory() {
    throw new Error("vm.measureMemory is not supported in the Chidori runtime (no V8 heap statistics are available)");
}

export class SourceTextModule {
    constructor() {
        throw new Error("vm.SourceTextModule is not supported in the Chidori runtime (the module-record introspection API is not exposed by the engine)");
    }
}

export const constants = Object.freeze({
    USE_MAIN_CONTEXT_DEFAULT_LOADER: Symbol("vm.constants.USE_MAIN_CONTEXT_DEFAULT_LOADER"),
    DONT_CONTEXTIFY: Symbol("vm.constants.DONT_CONTEXTIFY"),
});

export default {
    Script, createContext, isContext, runInContext, runInNewContext,
    runInThisContext, compileFunction, measureMemory, SourceTextModule,
    constants,
};
"#;

// node:wasi — no WASI host.
const WASI_SHIM: &str = r#"
export class WASI {
    constructor() {
        throw new Error("wasi.WASI is not supported in the Chidori runtime (no WASI host imports are available)");
    }
}
export default { WASI };
"#;

// node:zlib — functional, backed by the `__chidori_zlib` sync native
// (flate2/miniz + pure-Rust brotli; see runtime::compress). Codecs are pure
// functions of (input, level/quality), so like node:crypto hashing they run
// inline with nothing captured, and record/replay agrees byte-for-byte. The
// streaming classes buffer their input and codec at flush — output for a
// complete stream matches the one-shot form (chidori streams are in-memory
// anyway).
const ZLIB_SHIM: &str = r#"
import { Buffer } from "node:buffer";
import { Transform } from "node:stream";

function bytesToBase64(bytes) {
    let s = "";
    for (let i = 0; i < bytes.length; i++) s += String.fromCharCode(bytes[i]);
    return btoa(s);
}
function base64ToBytes(b64) {
    const bin = atob(b64);
    const out = new Uint8Array(bin.length);
    for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
    return out;
}
// Null (rather than a throw) for anything that is not a valid codec input, so
// each entry point can raise the Node error naming its own argument.
function toBytes(data) {
    if (typeof data === "string") return new TextEncoder().encode(data);
    if (data instanceof Uint8Array) return data;
    if (ArrayBuffer.isView(data)) return new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
    if (data instanceof ArrayBuffer) return new Uint8Array(data);
    return null;
}
function concatBytes(chunks) {
    let total = 0;
    for (const c of chunks) total += c.length;
    const all = new Uint8Array(total);
    let offset = 0;
    for (const c of chunks) { all.set(c, offset); offset += c.length; }
    return all;
}

// --- Node error shapes ---------------------------------------------------
// Node's zlib is unusually strict about arguments, and callers branch on
// `err.code`, so the validation surface is reproduced faithfully: the same
// codes, the same `The "x" argument/property must be …` wording, and the same
// `Name [CODE]: message` stringification its coded errors carry.
function inspectValue(value) {
    if (typeof value === "string") return "'" + value + "'";
    if (typeof value === "bigint") return String(value) + "n";
    if (typeof value === "symbol") return value.toString();
    return String(value);
}
function receivedFor(value) {
    if (value === null || value === undefined) return " Received " + String(value);
    if (typeof value === "function") return " Received function " + (value.name || "(anonymous)");
    if (typeof value === "object") {
        const ctor = value.constructor;
        return " Received an instance of " + (ctor && ctor.name ? ctor.name : "Object");
    }
    return " Received type " + typeof value + " (" + inspectValue(value) + ")";
}
function coded(err, code) {
    err.code = code;
    Object.defineProperty(err, "toString", {
        value: function () { return this.name + " [" + this.code + "]: " + this.message; },
        enumerable: false, writable: true, configurable: true,
    });
    return err;
}
// Node picks "property" over "argument" when the name is dotted.
function invalidArgType(name, expected, value) {
    const kind = name.indexOf(".") === -1 ? "argument" : "property";
    return coded(
        new TypeError('The "' + name + '" ' + kind + " must be " + expected + "." + receivedFor(value)),
        "ERR_INVALID_ARG_TYPE"
    );
}
function outOfRange(name, range, value) {
    return coded(
        new RangeError('The value of "' + name + '" is out of range. It must be ' + range +
            ". Received " + String(value)),
        "ERR_OUT_OF_RANGE"
    );
}

const constants = Object.freeze({
    Z_NO_FLUSH: 0, Z_PARTIAL_FLUSH: 1, Z_SYNC_FLUSH: 2, Z_FULL_FLUSH: 3,
    Z_FINISH: 4, Z_BLOCK: 5,
    Z_OK: 0, Z_STREAM_END: 1, Z_NEED_DICT: 2,
    Z_NO_COMPRESSION: 0, Z_BEST_SPEED: 1, Z_BEST_COMPRESSION: 9,
    Z_DEFAULT_COMPRESSION: -1,
    Z_FILTERED: 1, Z_HUFFMAN_ONLY: 2, Z_RLE: 3, Z_FIXED: 4,
    Z_DEFAULT_STRATEGY: 0,
    Z_MIN_WINDOWBITS: 8, Z_MAX_WINDOWBITS: 15, Z_DEFAULT_WINDOWBITS: 15,
    Z_MIN_CHUNK: 64, Z_MAX_CHUNK: Infinity, Z_DEFAULT_CHUNK: 16384,
    Z_MIN_MEMLEVEL: 1, Z_MAX_MEMLEVEL: 9, Z_DEFAULT_MEMLEVEL: 8,
    Z_MIN_LEVEL: -1, Z_MAX_LEVEL: 9, Z_DEFAULT_LEVEL: -1,
    BROTLI_OPERATION_PROCESS: 0, BROTLI_OPERATION_FLUSH: 1,
    BROTLI_OPERATION_FINISH: 2,
    BROTLI_PARAM_MODE: 0, BROTLI_PARAM_QUALITY: 1, BROTLI_PARAM_LGWIN: 2,
    BROTLI_PARAM_LGBLOCK: 3, BROTLI_PARAM_SIZE_HINT: 5,
    BROTLI_MODE_GENERIC: 0, BROTLI_MODE_TEXT: 1, BROTLI_MODE_FONT: 2,
    BROTLI_MIN_QUALITY: 0, BROTLI_MAX_QUALITY: 11,
    BROTLI_DEFAULT_QUALITY: 11, BROTLI_DEFAULT_WINDOW: 22,
    BROTLI_MIN_WINDOW_BITS: 10, BROTLI_MAX_WINDOW_BITS: 24,
});

// --- Option validation ---------------------------------------------------
function checkFiniteNumber(value, name) {
    if (value === undefined) return false;
    if (typeof value === "number" && isFinite(value)) return true;
    if (typeof value !== "number") throw invalidArgType(name, "of type number", value);
    throw outOfRange(name, "a finite number", value);
}
function checkRange(value, name, lower, upper, fallback) {
    if (!checkFiniteNumber(value, name)) return fallback;
    if (value < lower || value > upper) {
        throw outOfRange(name, ">= " + lower + " and <= " + upper, value);
    }
    return value;
}
const DECOMPRESSORS = {
    inflate: true, inflateRaw: true, gunzip: true, unzip: true,
};
function isBinary(value) {
    return ArrayBuffer.isView(value) || value instanceof ArrayBuffer;
}
// The chunkSize / flush bounds every zlib and brotli stream shares.
function validateBaseOptions(opts) {
    if (!opts || typeof opts !== "object") return;
    if (opts.chunkSize !== undefined && checkFiniteNumber(opts.chunkSize, "options.chunkSize") &&
        opts.chunkSize < constants.Z_MIN_CHUNK) {
        throw outOfRange("options.chunkSize", ">= " + constants.Z_MIN_CHUNK, opts.chunkSize);
    }
    checkRange(opts.flush, "options.flush", constants.Z_NO_FLUSH, constants.Z_BLOCK, constants.Z_NO_FLUSH);
    checkRange(opts.finishFlush, "options.finishFlush", constants.Z_NO_FLUSH, constants.Z_BLOCK, constants.Z_FINISH);
}
function validateZlibOptions(op, opts) {
    validateBaseOptions(opts);
    if (!opts || typeof opts !== "object") return;
    // windowBits 0 is legal on the decompressing side — it means "read the
    // window size out of the stream header".
    const unsetWindow = opts.windowBits === undefined || opts.windowBits === null || opts.windowBits === 0;
    if (!(unsetWindow && DECOMPRESSORS[op])) {
        checkRange(opts.windowBits, "options.windowBits",
            constants.Z_MIN_WINDOWBITS, constants.Z_MAX_WINDOWBITS, constants.Z_DEFAULT_WINDOWBITS);
    }
    checkRange(opts.level, "options.level",
        constants.Z_MIN_LEVEL, constants.Z_MAX_LEVEL, constants.Z_DEFAULT_COMPRESSION);
    checkRange(opts.memLevel, "options.memLevel",
        constants.Z_MIN_MEMLEVEL, constants.Z_MAX_MEMLEVEL, constants.Z_DEFAULT_MEMLEVEL);
    checkRange(opts.strategy, "options.strategy",
        constants.Z_DEFAULT_STRATEGY, constants.Z_FIXED, constants.Z_DEFAULT_STRATEGY);
    if (opts.dictionary !== undefined && !isBinary(opts.dictionary)) {
        throw invalidArgType("options.dictionary",
            "an instance of Buffer, TypedArray, DataView, or ArrayBuffer", opts.dictionary);
    }
}
function validateBrotliOptions(opts) {
    validateBaseOptions(opts);
    if (!opts || typeof opts !== "object" || !opts.params) return;
    if (typeof opts.params !== "object") {
        throw invalidArgType("options.params", "of type object", opts.params);
    }
    for (const key of Object.keys(opts.params)) {
        const index = Number(key);
        if (!isFinite(index) || index < 0 || index > constants.BROTLI_PARAM_SIZE_HINT) {
            throw coded(new RangeError("The brotli parameter " + key + " is invalid"),
                "ERR_BROTLI_INVALID_PARAM");
        }
        const value = opts.params[key];
        if (typeof value !== "number" && typeof value !== "boolean") {
            throw invalidArgType("options.params[key]", "of type number", value);
        }
    }
}

// --- Codecs --------------------------------------------------------------
function levelOf(options) {
    if (options && typeof options === "object" && typeof options.level === "number") {
        return options.level;
    }
    return null;
}
// Brotli quality rides Node's option shape: options.params keyed by
// BROTLI_PARAM_QUALITY. Other params (lgwin, mode) are accepted and ignored —
// the codec uses its defaults for them.
function brotliQuality(options) {
    if (options && typeof options === "object" && options.params && typeof options.params === "object") {
        const quality = options.params[constants.BROTLI_PARAM_QUALITY];
        if (typeof quality === "number") return quality;
    }
    return null;
}
const BROTLI_OPS = { brotliCompress: true, brotliDecompress: true };
function runCodec(op, bytes, options) {
    const tuning = BROTLI_OPS[op] ? brotliQuality(options) : levelOf(options);
    return Buffer.from(base64ToBytes(globalThis.__chidori_zlib(op, bytesToBase64(bytes), tuning)));
}
function inputBytes(data, options, op) {
    const bytes = toBytes(data);
    if (bytes === null) {
        throw invalidArgType("buffer",
            "of type string or an instance of Buffer, TypedArray, DataView, or ArrayBuffer", data);
    }
    if (BROTLI_OPS[op]) validateBrotliOptions(options);
    else validateZlibOptions(op, options);
    return bytes;
}
// `{ info: true }` asks for the engine alongside the bytes, as Node does.
function withInfo(buffer, options, op) {
    if (options && typeof options === "object" && options.info) {
        return { buffer, engine: new CLASS_FOR_OP[op](options) };
    }
    return buffer;
}
function syncCodec(op) {
    return function (data, options) {
        return withInfo(runCodec(op, inputBytes(data, options, op), options), options, op);
    };
}
// node signature: fn(data[, options], callback) — result on a microtask.
function asyncCodec(op) {
    return function (data, options, callback) {
        if (typeof options === "function") { callback = options; options = undefined; }
        if (typeof callback !== "function") {
            throw invalidArgType("callback", "of type function", callback);
        }
        const bytes = inputBytes(data, options, op);
        queueMicrotask(() => {
            try {
                callback(null, withInfo(runCodec(op, bytes, options), options, op));
            } catch (err) {
                callback(err);
            }
        });
    };
}

// --- Streaming classes ---------------------------------------------------
// ES5-style constructors, because Node's zlib classes work with and without
// `new`. Each instance is a real Transform whose prototype is re-pointed at
// the codec class, so `zlib.Deflate() instanceof zlib.Deflate` holds and the
// full stream surface comes along. Chunks are buffered and coded at flush:
// output for a complete stream is identical to the one-shot form.
function codecHandlers(op, options) {
    const chunks = [];
    return {
        transform(chunk, encoding, callback) {
            const bytes = toBytes(chunk);
            if (bytes === null) {
                return callback(invalidArgType("chunk",
                    "of type string or an instance of Buffer, TypedArray, DataView, or ArrayBuffer", chunk));
            }
            chunks.push(bytes);
            callback(null);
        },
        flush(callback) {
            try {
                callback(null, runCodec(op, concatBytes(chunks), options));
            } catch (err) {
                callback(err);
            }
        },
    };
}
function defineCodecClass(name, op) {
    const Ctor = function (options) {
        if (!(this instanceof Ctor)) return new Ctor(options);
        if (BROTLI_OPS[op]) validateBrotliOptions(options);
        else validateZlibOptions(op, options);
        const stream = new Transform(Object.assign({}, options, codecHandlers(op, options)));
        Object.setPrototypeOf(stream, Ctor.prototype);
        stream._codecOp = op;
        stream._codecOptions = options;
        stream.bytesWritten = 0;
        return stream;
    };
    Ctor.prototype = Object.create(Transform.prototype);
    Object.defineProperty(Ctor.prototype, "constructor", {
        value: Ctor, enumerable: false, writable: true, configurable: true,
    });
    Object.defineProperty(Ctor, "name", { value: name, configurable: true });
    return Ctor;
}

const Deflate = defineCodecClass("Deflate", "deflate");
const DeflateRaw = defineCodecClass("DeflateRaw", "deflateRaw");
const Gzip = defineCodecClass("Gzip", "gzip");
const Inflate = defineCodecClass("Inflate", "inflate");
const InflateRaw = defineCodecClass("InflateRaw", "inflateRaw");
const Gunzip = defineCodecClass("Gunzip", "gunzip");
const Unzip = defineCodecClass("Unzip", "unzip");
const BrotliCompress = defineCodecClass("BrotliCompress", "brotliCompress");
const BrotliDecompress = defineCodecClass("BrotliDecompress", "brotliDecompress");
const CLASS_FOR_OP = {
    deflate: Deflate, deflateRaw: DeflateRaw, gzip: Gzip,
    inflate: Inflate, inflateRaw: InflateRaw, gunzip: Gunzip, unzip: Unzip,
    brotliCompress: BrotliCompress, brotliDecompress: BrotliDecompress,
};

// Node's private-but-widely-used synchronous entry point: code one buffer end
// to end and hand back the result without touching the stream's event surface.
function _processChunk(chunk, flushFlag, callback) {
    const op = this._codecOp;
    const bytes = toBytes(chunk);
    if (bytes === null) {
        throw invalidArgType("chunk",
            "of type string or an instance of Buffer, TypedArray, DataView, or ArrayBuffer", chunk);
    }
    if (typeof callback === "function") {
        let result;
        try { result = runCodec(op, bytes, this._codecOptions); } catch (err) { return callback(err); }
        callback(null, result);
        return undefined;
    }
    return runCodec(op, bytes, this._codecOptions);
}
// Only the compressing zlib classes take params(); the validation is the
// point (Node throws before touching the handle).
function params(level, strategy, callback) {
    checkRange(level, "level", constants.Z_MIN_LEVEL, constants.Z_MAX_LEVEL, constants.Z_DEFAULT_COMPRESSION);
    checkRange(strategy, "strategy", constants.Z_DEFAULT_STRATEGY, constants.Z_FIXED, constants.Z_DEFAULT_STRATEGY);
    if (typeof level === "number") this._codecOptions = Object.assign({}, this._codecOptions, { level });
    if (typeof callback === "function") queueMicrotask(callback);
    return this;
}
for (const Ctor of [Deflate, DeflateRaw, Gzip]) {
    Ctor.prototype.params = params;
}
for (const Ctor of Object.keys(CLASS_FOR_OP).map((op) => CLASS_FOR_OP[op])) {
    Ctor.prototype._processChunk = _processChunk;
    Ctor.prototype.reset = function reset() { return this; };
    Ctor.prototype.close = function close(callback) {
        if (typeof callback === "function") queueMicrotask(callback);
        return this;
    };
}

export const deflateSync = syncCodec("deflate");
export const deflateRawSync = syncCodec("deflateRaw");
export const gzipSync = syncCodec("gzip");
export const inflateSync = syncCodec("inflate");
export const inflateRawSync = syncCodec("inflateRaw");
export const gunzipSync = syncCodec("gunzip");
export const unzipSync = syncCodec("unzip");
export const brotliCompressSync = syncCodec("brotliCompress");
export const brotliDecompressSync = syncCodec("brotliDecompress");
export const deflate = asyncCodec("deflate");
export const deflateRaw = asyncCodec("deflateRaw");
export const gzip = asyncCodec("gzip");
export const inflate = asyncCodec("inflate");
export const inflateRaw = asyncCodec("inflateRaw");
export const gunzip = asyncCodec("gunzip");
export const unzip = asyncCodec("unzip");
export const brotliCompress = asyncCodec("brotliCompress");
export const brotliDecompress = asyncCodec("brotliDecompress");
export function createDeflate(options) { return new Deflate(options); }
export function createDeflateRaw(options) { return new DeflateRaw(options); }
export function createGzip(options) { return new Gzip(options); }
export function createInflate(options) { return new Inflate(options); }
export function createInflateRaw(options) { return new InflateRaw(options); }
export function createGunzip(options) { return new Gunzip(options); }
export function createUnzip(options) { return new Unzip(options); }
export function createBrotliCompress(options) { return new BrotliCompress(options); }
export function createBrotliDecompress(options) { return new BrotliDecompress(options); }
export {
    constants,
    Deflate, DeflateRaw, Gzip, Inflate, InflateRaw, Gunzip, Unzip,
    BrotliCompress, BrotliDecompress,
};
export default {
    deflate, deflateSync, deflateRaw, deflateRawSync,
    inflate, inflateSync, inflateRaw, inflateRawSync,
    gzip, gzipSync, gunzip, gunzipSync, unzip, unzipSync,
    brotliCompress, brotliCompressSync, brotliDecompress, brotliDecompressSync,
    createDeflate, createDeflateRaw, createInflate, createInflateRaw,
    createGzip, createGunzip, createUnzip,
    createBrotliCompress, createBrotliDecompress,
    Deflate, DeflateRaw, Gzip, Inflate, InflateRaw, Gunzip, Unzip,
    BrotliCompress, BrotliDecompress,
    constants,
};
"#;

// node:module — resolver introspection. `builtinModules` reflects the actual
// allowlist (spliced in from `NODE_BUILTIN_ALLOWLIST` so there is one source
// of truth), minus the `node:`-prefix-only names, which Node excludes from
// `builtinModules` and rejects from bare `isBuiltin` lookups; createRequire
// links but the returned require throws, matching the loader's leaf-only
// CommonJS stance.
static MODULE_SHIM: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    use crate::runtime::typescript::transpile::{
        NODE_BUILTIN_ALLOWLIST, NODE_PREFIX_ONLY_BUILTINS,
    };
    let bare: Vec<&str> = NODE_BUILTIN_ALLOWLIST
        .iter()
        .copied()
        .filter(|name| !NODE_PREFIX_ONLY_BUILTINS.contains(name))
        .collect();
    let list = serde_json::to_string(&bare).expect("builtin allowlist serializes");
    let prefix_only =
        serde_json::to_string(NODE_PREFIX_ONLY_BUILTINS).expect("prefix-only list serializes");
    format!(
        r#"
const builtinModules = Object.freeze({list});
const prefixOnlyBuiltins = Object.freeze({prefix_only});
export {{ builtinModules }};
export function isBuiltin(specifier) {{
    const spec = String(specifier);
    if (spec.startsWith("node:")) {{
        const name = spec.slice(5);
        return builtinModules.indexOf(name) !== -1 || prefixOnlyBuiltins.indexOf(name) !== -1;
    }}
    return builtinModules.indexOf(spec) !== -1;
}}
export function createRequire(filename) {{
    function require(specifier) {{
        throw new Error(
            "Cannot require('" + specifier + "'): chidori loads modules as ESM and does not emulate CommonJS require(). Use import instead."
        );
    }}
    require.resolve = function (specifier) {{
        throw new Error("require.resolve('" + specifier + "') is not supported in the Chidori runtime");
    }};
    require.cache = Object.create(null);
    require.main = undefined;
    return require;
}}
export function syncBuiltinESMExports() {{}}
export function register() {{
    throw new Error("module.register is not supported in the Chidori runtime (no customization-hook loader threads)");
}}
export class Module {{}}
Module.builtinModules = builtinModules;
Module.isBuiltin = isBuiltin;
Module.createRequire = createRequire;
Module.syncBuiltinESMExports = syncBuiltinESMExports;
export default Module;
"#
    )
});

// node:test shim. Node's runner registers tests, schedules them, and reports
// through a TAP/stream reporter; chidori has no test-process lifecycle to hang
// that on, so tests run *eagerly* at the call site and a failure propagates to
// the caller — synchronously for a synchronous body, as a rejected promise for
// an asynchronous one. That keeps `node:test` usable as the grouping wrapper
// Node's own core tests use it as (a failing assertion still fails the run)
// without pretending to schedule or report.
const TEST_SHIM: &str = r#"
import assert from "node:assert";

const ASSERT_METHODS = [
    "ok", "fail", "equal", "notEqual", "strictEqual", "notStrictEqual",
    "deepEqual", "notDeepEqual", "deepStrictEqual", "notDeepStrictEqual",
    "throws", "doesNotThrow", "rejects", "doesNotReject", "match",
    "doesNotMatch", "ifError",
];

// Hook frames pushed by `describe`; `beforeEach`/`afterEach` declared in a
// suite body apply to the tests declared after them in that body.
const suiteStack = [];

function unsupported(name) {
    return function () {
        throw new Error(
            "node:test " + name + " is not supported in the Chidori runtime " +
            "(tests run eagerly at the call site; there is no scheduler to hook)"
        );
    };
}

function parseTestArgs(args) {
    let name;
    let options;
    let fn;
    let index = 0;
    const first = args[0];
    if (typeof first === "string") { name = first; index = 1; }
    else if (typeof first === "function") { fn = first; index = 1; }
    else if (first !== null && typeof first === "object") { options = first; index = 1; }
    if (fn === undefined && index < args.length) {
        const second = args[index];
        if (typeof second === "function") { fn = second; index += 1; }
        else if (second !== null && typeof second === "object") { options = second; index += 1; }
    }
    if (fn === undefined && typeof args[index] === "function") fn = args[index];
    if (name === undefined) name = (fn && fn.name) || "<anonymous>";
    return { name, options: options || {}, fn };
}

function makeContext(name, options) {
    const context = {
        name,
        fullName: name,
        filePath: undefined,
        signal: undefined,
        assert: {},
        diagnostic() {},
        runOnly() {},
        skip() { context.__skipped = true; },
        todo() { context.__todo = true; },
        plan(count) { context.__plan = count; },
        before(fn) { return runHook(fn, context); },
        after(fn) { return runHook(fn, context); },
        beforeEach() {},
        afterEach() {},
        test(...args) { context.__assertions += 1; return runTest(args); },
        get mock() { return unsupported("t.mock")(); },
    };
    context.__assertions = 0;
    context.__plan = options.plan;
    for (const key of ASSERT_METHODS) {
        context.assert[key] = function (...args) {
            context.__assertions += 1;
            return assert[key](...args);
        };
    }
    context.assert.snapshot = unsupported("t.assert.snapshot");
    context.it = context.test;
    return context;
}

function runHook(fn, context) {
    if (typeof fn !== "function") return undefined;
    return fn(context);
}

function settled(value) {
    // A thenable rather than a plain Promise so an unawaited failure still
    // surfaces: nothing here is deferred, so there is nothing to lose.
    return { then(onFulfilled) { return Promise.resolve().then(() => onFulfilled && onFulfilled(value)); },
             catch() { return this; },
             finally(onFinally) { if (onFinally) onFinally(); return this; } };
}

function verifyPlan(context) {
    if (context.__plan === undefined) return;
    if (context.__assertions !== context.__plan) {
        throw new Error(
            "plan: expected " + context.__plan + " assertion(s), got " + context.__assertions
        );
    }
}

function runTest(args) {
    const parsed = parseTestArgs(args);
    const options = parsed.options;
    if (parsed.fn === undefined || options.skip || options.todo) return settled(undefined);

    const context = makeContext(parsed.name, options);
    const frame = suiteStack[suiteStack.length - 1];
    if (frame) for (const hook of frame.beforeEach) runHook(hook, context);

    const finish = () => {
        if (frame) for (const hook of frame.afterEach) runHook(hook, context);
    };

    let result;
    try {
        result = parsed.fn.length > 1
            ? new Promise((resolve, reject) => {
                const done = (err) => (err ? reject(err) : resolve());
                Promise.resolve(parsed.fn(context, done)).catch(reject);
            })
            : parsed.fn(context);
    } catch (err) {
        finish();
        throw err;
    }

    if (result !== null && typeof result === "object" && typeof result.then === "function") {
        return Promise.resolve(result).then(
            (value) => { finish(); verifyPlan(context); return value; },
            (err) => { finish(); throw err; }
        );
    }
    finish();
    verifyPlan(context);
    return settled(result);
}

function test(...args) { return runTest(args); }

function describe(...args) {
    const parsed = parseTestArgs(args);
    if (parsed.fn === undefined || parsed.options.skip || parsed.options.todo) return settled(undefined);
    const frame = { beforeEach: [], afterEach: [], before: [], after: [] };
    suiteStack.push(frame);
    try {
        const result = parsed.fn.call(undefined);
        if (result !== null && typeof result === "object" && typeof result.then === "function") {
            return Promise.resolve(result).then(
                (value) => { suiteStack.pop(); return value; },
                (err) => { suiteStack.pop(); throw err; }
            );
        }
    } catch (err) {
        suiteStack.pop();
        throw err;
    }
    suiteStack.pop();
    return settled(undefined);
}

function before(fn) { return runHook(fn, undefined); }
function after(fn) { return runHook(fn, undefined); }
function beforeEach(fn) {
    const frame = suiteStack[suiteStack.length - 1];
    if (frame) frame.beforeEach.push(fn);
}
function afterEach(fn) {
    const frame = suiteStack[suiteStack.length - 1];
    if (frame) frame.afterEach.push(fn);
}

const it = test;
const suite = describe;
function skip() { return settled(undefined); }
function todo() { return settled(undefined); }
function only(...args) { return runTest(args); }

const mock = {
    fn: unsupported("mock.fn"),
    method: unsupported("mock.method"),
    getter: unsupported("mock.getter"),
    setter: unsupported("mock.setter"),
    module: unsupported("mock.module"),
    timers: { enable: unsupported("mock.timers.enable"), reset() {} },
    reset() {},
    restoreAll() {},
};
const run = unsupported("run");
const snapshot = {
    setResolveSnapshotPath: unsupported("snapshot.setResolveSnapshotPath"),
    setDefaultSnapshotSerializers: unsupported("snapshot.setDefaultSnapshotSerializers"),
};

test.test = test;
test.it = it;
test.describe = describe;
test.suite = suite;
test.before = before;
test.after = after;
test.beforeEach = beforeEach;
test.afterEach = afterEach;
test.skip = skip;
test.todo = todo;
test.only = only;
test.mock = mock;
test.run = run;
test.snapshot = snapshot;
test.assert = { register: unsupported("assert.register") };

export { test, it, describe, suite, before, after, beforeEach, afterEach, skip, todo, only, mock, run, snapshot };
export default test;
"#;

/// Shim source for the compat suite; consulted by `builtins::shim_source`
/// after its own table.
pub fn compat_shim_source(name: &str) -> Option<&'static str> {
    match name {
        "querystring" => Some(QUERYSTRING_SHIM),
        "test" => Some(TEST_SHIM),
        "string_decoder" => Some(STRING_DECODER_SHIM),
        "punycode" => Some(PUNYCODE_SHIM),
        "console" => Some(CONSOLE_SHIM),
        "constants" => Some(CONSTANTS_SHIM),
        "util/types" => Some(UTIL_TYPES_SHIM),
        "path/win32" => Some(PATH_WIN32_SHIM),
        "sys" => Some(SYS_SHIM),
        "timers" => Some(TIMERS_SHIM),
        "timers/promises" => Some(TIMERS_PROMISES_SHIM),
        "async_hooks" => Some(ASYNC_HOOKS_SHIM),
        "diagnostics_channel" => Some(DIAGNOSTICS_CHANNEL_SHIM),
        "domain" => Some(DOMAIN_SHIM),
        "perf_hooks" => Some(PERF_HOOKS_SHIM),
        "worker_threads" => Some(WORKER_THREADS_SHIM),
        "v8" => Some(V8_SHIM),
        "tty" => Some(TTY_SHIM),
        "net" => Some(NET_SHIM),
        "stream" => Some(STREAM_SHIM),
        "stream/promises" => Some(STREAM_PROMISES_SHIM),
        "stream/consumers" => Some(STREAM_CONSUMERS_SHIM),
        "stream/web" => Some(STREAM_WEB_SHIM),
        "module" => Some(MODULE_SHIM.as_str()),
        "child_process" => Some(CHILD_PROCESS_SHIM),
        "cluster" => Some(CLUSTER_SHIM),
        "dgram" => Some(DGRAM_SHIM),
        "dns" => Some(DNS_SHIM),
        "dns/promises" => Some(DNS_PROMISES_SHIM),
        "http2" => Some(HTTP2_SHIM),
        "inspector" => Some(INSPECTOR_SHIM),
        "inspector/promises" => Some(INSPECTOR_PROMISES_SHIM),
        "readline" => Some(READLINE_SHIM),
        "readline/promises" => Some(READLINE_PROMISES_SHIM),
        "repl" => Some(REPL_SHIM),
        "tls" => Some(TLS_SHIM),
        "trace_events" => Some(TRACE_EVENTS_SHIM),
        "vm" => Some(VM_SHIM),
        "wasi" => Some(WASI_SHIM),
        "zlib" => Some(ZLIB_SHIM),
        _ => None,
    }
}
