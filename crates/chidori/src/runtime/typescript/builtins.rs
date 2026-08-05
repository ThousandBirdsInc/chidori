//! Shim sources for `node:` builtins.
//!
//! When the resolver under the `Node` import policy encounters `node:process`,
//! `node:buffer`, etc., it returns a synthetic resolved path of the form
//! `<workspace>/__node_builtins__/<name>.js`. The snapshot bundler routes
//! module-source reads through `source_for()` so those synthetic paths hand
//! back the bodies below instead of hitting the filesystem.
//!
//! Shims are intentionally minimal: they expose the surface that real-world
//! packages tend to touch (`process.env`, `Buffer.from`, `util.inspect`) and
//! delegate to the globals the chidori prelude already installs where
//! possible. Anything beyond that throws a clear error so missing surface
//! shows up at first use, not as a silent miscompare.

use std::path::Path;

/// Allowlisted builtin names. Kept in sync with `NODE_BUILTIN_ALLOWLIST` in
/// `transpile.rs`. The first block is served from this file; the rest come
/// from `builtins_compat.rs`, which completes coverage of the Node builtin
/// module suite.
#[allow(dead_code)] // Reference copy of the allowlist; transpile.rs owns the enforced one.
pub const BUILTIN_NAMES: &[&str] = &[
    "process",
    "buffer",
    "util",
    "fs",
    "fs/promises",
    "crypto",
    "http",
    "https",
    "path",
    "path/posix",
    "events",
    "url",
    "assert",
    "assert/strict",
    "os",
    // Served from builtins_compat.rs (functional implementations).
    "async_hooks",
    "console",
    "constants",
    "diagnostics_channel",
    "domain",
    "module",
    "net",
    "path/win32",
    "perf_hooks",
    "punycode",
    "querystring",
    "stream",
    "stream/consumers",
    "stream/promises",
    "stream/web",
    "string_decoder",
    "sys",
    "test",
    "timers",
    "timers/promises",
    "tty",
    "util/types",
    "v8",
    "worker_threads",
    // Served from builtins_compat.rs (fail-loud capability stubs).
    "child_process",
    "cluster",
    "dgram",
    "dns",
    "dns/promises",
    "http2",
    "inspector",
    "inspector/promises",
    "readline",
    "readline/promises",
    "repl",
    "tls",
    "trace_events",
    "vm",
    "wasi",
    "zlib",
];

const PROCESS_SHIM: &str = r#"
// node:process shim. The chidori prelude already installs `globalThis.process`
// with an `env` populated from CHIDORI_AGENT_ENV; we re-export it here so
// `import process from "node:process"` and `import { env } from "node:process"`
// both work without diverging from the global.
const process = globalThis.process;
const env = process.env;
const argv = process.argv || [];
const platform = process.platform || "chidori";
const version = process.version || "v0.0.0-chidori";
const versions = process.versions || Object.freeze({ node: "0.0.0-chidori" });
const nextTick = process.nextTick
    ? process.nextTick.bind(process)
    : function (cb, ...args) { Promise.resolve().then(() => cb(...args)); };
const cwd = process.cwd ? process.cwd.bind(process) : function () { return "/"; };
const hrtime = process.hrtime;
const pid = process.pid;
const title = process.title;
const stdout = process.stdout;
const stderr = process.stderr;
export { process as default, env, argv, platform, version, versions, nextTick, cwd, hrtime, pid, title, stdout, stderr };
"#;

const BUFFER_SHIM: &str = r#"
// node:buffer shim. A Uint8Array subclass covering the construction,
// comparison, encoding, and byte-manipulation surface packages actually
// touch. Encodings: utf8, hex, base64, base64url, latin1/binary, ascii,
// utf16le/ucs2.
function normEnc(encoding) {
    if (encoding === undefined || encoding === null) return "utf8";
    const e = String(encoding).toLowerCase();
    if (e === "utf-8") return "utf8";
    if (e === "binary") return "latin1";
    if (e === "ucs2" || e === "ucs-2" || e === "utf-16le") return "utf16le";
    return e;
}
const ENCODINGS = ["utf8", "hex", "base64", "base64url", "latin1", "ascii", "utf16le"];
// Node renders the offending value into ERR_INVALID_ARG_TYPE /
// ERR_OUT_OF_RANGE messages (lib/internal/errors.js `invalidArgTypeHelper`);
// packages assert on those strings, so mirror the shapes exactly.
function inspectValue(input) {
    const t = typeof input;
    if (t === "string") return "'" + input + "'";
    if (t === "bigint") return String(input) + "n";
    if (t === "symbol") return input.toString();
    return String(input);
}
function receivedHelper(input) {
    if (input === null) return " Received null";
    if (input === undefined) return " Received undefined";
    const t = typeof input;
    // Node's `determineSpecificType` interpolates `value.name` unconditionally,
    // so an anonymous callable really does render as "function " with a
    // trailing space — not "(anonymous)". Vendored tests compare the message
    // verbatim.
    if (t === "function") return " Received function " + input.name;
    if (t === "object") {
        const ctor = input.constructor;
        if (ctor && typeof ctor.name === "string" && ctor.name.length !== 0) {
            return " Received an instance of " + ctor.name;
        }
        return " Received [Object: null prototype] {}";
    }
    let inspected = inspectValue(input);
    if (inspected.length > 25) inspected = inspected.slice(0, 25) + "...";
    return " Received type " + t + " (" + inspected + ")";
}
function invalidArgType(name, expected, actual) {
    // Node leaves already-qualified names ("first argument") unquoted.
    const subject = name.endsWith(" argument")
        ? "The " + name
        : 'The "' + name + '" ' + (name.indexOf(".") !== -1 ? "property" : "argument");
    const err = new TypeError(subject + " must be " + expected + "." + receivedHelper(actual));
    err.code = "ERR_INVALID_ARG_TYPE";
    return err;
}
function outOfRange(name, range, actual) {
    const err = new RangeError(
        'The value of "' + name + '" is out of range. It must be ' + range + ". Received " + inspectValue(actual)
    );
    err.code = "ERR_OUT_OF_RANGE";
    return err;
}
function rangeText(min, max) {
    if (min !== undefined && max !== undefined) return ">= " + min + " && <= " + max;
    if (min !== undefined) return ">= " + min;
    return "<= " + max;
}
// Node's `validateOffset` (a number in [min, max]); NaN counts as out of range.
function validateOffset(value, name, min, max) {
    if (typeof value !== "number") throw invalidArgType(name, "of type number", value);
    if (Number.isNaN(value) || value < min || value > max) {
        throw outOfRange(name, rangeText(min, max), value);
    }
    return value;
}
// Node's `validateInteger` (an integer in [min, max]).
function validateInteger(value, name, min, max) {
    if (typeof value !== "number") throw invalidArgType(name, "of type number", value);
    if (!Number.isInteger(value)) throw outOfRange(name, "an integer", value);
    if ((min !== undefined && value < min) || (max !== undefined && value > max)) {
        throw outOfRange(name, rangeText(min, max), value);
    }
    return value;
}
function compareBytes(a, aStart, aEnd, b, bStart, bEnd) {
    const aLen = aEnd - aStart;
    const bLen = bEnd - bStart;
    const len = Math.min(aLen, bLen);
    for (let i = 0; i < len; i++) {
        const x = a[aStart + i];
        const y = b[bStart + i];
        if (x !== y) return x < y ? -1 : 1;
    }
    if (aLen !== bLen) return aLen < bLen ? -1 : 1;
    return 0;
}
const BUF_OR_U8 = "an instance of Buffer or Uint8Array";
// UTF-8 decoding, done here rather than through TextDecoder: the engine's
// strings are UTF-8, so `String.fromCharCode` cannot join a surrogate pair
// (each half becomes U+FFFD) and everything outside the BMP comes back
// mangled. `String.fromCodePoint` builds those characters directly. The
// error handling is the WHATWG one — a maximal invalid subsequence collapses
// to a single U+FFFD and decoding resumes at the byte that ended it — which
// is what Buffer.toString and string_decoder are specified against.
function decodeUtf8(bytes) {
    const len = bytes.length;
    let out = "";
    let i = 0;
    while (i < len) {
        const b0 = bytes[i];
        if (b0 < 0x80) {
            out += String.fromCharCode(b0);
            i++;
            continue;
        }
        let need;
        let cp;
        let lower = 0x80;
        let upper = 0xbf;
        if (b0 >= 0xc2 && b0 <= 0xdf) {
            need = 1;
            cp = b0 & 0x1f;
        } else if (b0 >= 0xe0 && b0 <= 0xef) {
            need = 2;
            cp = b0 & 0x0f;
            if (b0 === 0xe0) lower = 0xa0;       // no overlong encodings
            else if (b0 === 0xed) upper = 0x9f;  // no surrogate code points
        } else if (b0 >= 0xf0 && b0 <= 0xf4) {
            need = 3;
            cp = b0 & 0x07;
            if (b0 === 0xf0) lower = 0x90;
            else if (b0 === 0xf4) upper = 0x8f;  // no code points above U+10FFFF
        } else {
            // Continuation byte with no lead, or a byte UTF-8 never uses.
            out += "�";
            i++;
            continue;
        }
        let j = i + 1;
        let complete = true;
        for (let k = 0; k < need; k++) {
            const b = bytes[j];
            const lo = k === 0 ? lower : 0x80;
            const hi = k === 0 ? upper : 0xbf;
            if (j >= len || b < lo || b > hi) {
                complete = false;
                break;
            }
            cp = (cp << 6) | (b & 0x3f);
            j++;
        }
        // `j` stops on the offending byte, which is re-examined from scratch.
        out += complete ? String.fromCodePoint(cp) : "�";
        i = j;
    }
    return out;
}
function decodeString(input, enc) {
    if (enc === "base64" || enc === "base64url") {
        let b64 = input.replace(/-/g, "+").replace(/_/g, "/").replace(/[^A-Za-z0-9+/=]/g, "");
        const bin = atob(b64);
        const out = new Uint8Array(bin.length);
        for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
        return out;
    }
    if (enc === "hex") {
        const n = input.length >>> 1;
        const out = new Uint8Array(n);
        for (let i = 0; i < n; i++) {
            const byte = parseInt(input.substr(i * 2, 2), 16);
            if (Number.isNaN(byte)) return out.subarray(0, i);
            out[i] = byte;
        }
        return out;
    }
    if (enc === "latin1" || enc === "ascii") {
        const out = new Uint8Array(input.length);
        for (let i = 0; i < input.length; i++) out[i] = input.charCodeAt(i) & (enc === "ascii" ? 0x7f : 0xff);
        return out;
    }
    if (enc === "utf16le") {
        const out = new Uint8Array(input.length * 2);
        for (let i = 0; i < input.length; i++) {
            const c = input.charCodeAt(i);
            out[i * 2] = c & 0xff;
            out[i * 2 + 1] = c >> 8;
        }
        return out;
    }
    return new TextEncoder().encode(input);
}
class Buffer extends Uint8Array {
    static from(input, encodingOrOffset, length) {
        if (typeof input === "string") {
            return wrap(decodeString(input, normEnc(encodingOrOffset)));
        }
        if (input instanceof ArrayBuffer) {
            // Shares the ArrayBuffer's memory, like Node.
            return new Buffer(input, encodingOrOffset || 0, length);
        }
        if (ArrayBuffer.isView(input)) {
            // Copies, like Node (share requires the ArrayBuffer form).
            return wrap(new Uint8Array(input.buffer, input.byteOffset, input.byteLength).slice());
        }
        if (Array.isArray(input)) return wrap(Uint8Array.from(input));
        if (input !== null && typeof input === "object") {
            // Boxed primitives / objects with a primitive value. Node consults
            // valueOf() first and only accepts a string or object result.
            if (typeof input.valueOf === "function") {
                const value = input.valueOf();
                if (
                    value !== null && value !== undefined && value !== input &&
                    (typeof value === "string" || typeof value === "object")
                ) {
                    return Buffer.from(value, encodingOrOffset, length);
                }
            }
            // JSON round trip shape.
            if (input.type === "Buffer" && Array.isArray(input.data)) {
                return wrap(Uint8Array.from(input.data));
            }
            // Array-likes.
            if (typeof input.length === "number") {
                return wrap(Uint8Array.from(input));
            }
            const primitive = input[Symbol.toPrimitive];
            if (typeof primitive === "function") {
                const value = primitive.call(input, "string");
                // A non-string primitive is *not* accepted, it falls through
                // to the ERR_INVALID_ARG_TYPE below (naming the original input).
                if (typeof value === "string") {
                    return wrap(decodeString(value, normEnc(encodingOrOffset)));
                }
            }
        }
        throw invalidArgType(
            "first argument",
            "of type string or an instance of Buffer, ArrayBuffer, or Array or an Array-like Object",
            input
        );
    }
    // Copies the *bytes* of a TypedArray, independent of its element width.
    static copyBytesFrom(view, offset, length) {
        if (!ArrayBuffer.isView(view) || typeof view.BYTES_PER_ELEMENT !== "number") {
            throw invalidArgType("view", "an instance of TypedArray", view);
        }
        const viewLength = view.length;
        if (viewLength === 0) return Buffer.alloc(0);
        if (offset !== undefined || length !== undefined) {
            if (offset === undefined) {
                offset = 0;
            } else {
                validateInteger(offset, "offset", 0);
                if (offset >= viewLength) return Buffer.alloc(0);
            }
            let end;
            if (length === undefined) {
                end = viewLength;
            } else {
                validateInteger(length, "length", 0);
                end = offset + length;
            }
            view = view.slice(offset, end);
        }
        return wrap(new Uint8Array(view.buffer, view.byteOffset, view.byteLength).slice());
    }
    static alloc(size, fill, encoding) {
        const buf = new Buffer(new ArrayBuffer(size >>> 0));
        if (fill !== undefined && fill !== 0) buf.fill(fill, 0, buf.length, encoding);
        return buf;
    }
    static allocUnsafe(size) { return new Buffer(new ArrayBuffer(size >>> 0)); }
    static allocUnsafeSlow(size) { return new Buffer(new ArrayBuffer(size >>> 0)); }
    static of(...items) { return wrap(Uint8Array.from(items)); }
    static isBuffer(value) { return value instanceof Buffer; }
    static isEncoding(encoding) {
        if (typeof encoding !== "string" || encoding.length === 0) return false;
        return ENCODINGS.indexOf(normEnc(encoding)) !== -1;
    }
    static byteLength(input, encoding) {
        if (typeof input === "string") return decodeString(input, normEnc(encoding)).length;
        if (ArrayBuffer.isView(input)) return input.byteLength;
        if (input instanceof ArrayBuffer) return input.byteLength;
        throw invalidArgType(
            "string",
            "of type string or an instance of Buffer or ArrayBuffer",
            input
        );
    }
    static compare(buf1, buf2) {
        if (!(buf1 instanceof Uint8Array)) throw invalidArgType("buf1", BUF_OR_U8, buf1);
        if (!(buf2 instanceof Uint8Array)) throw invalidArgType("buf2", BUF_OR_U8, buf2);
        return compareBytes(buf1, 0, buf1.length, buf2, 0, buf2.length);
    }
    static concat(list, totalLength) {
        if (!Array.isArray(list)) throw invalidArgType("list", "an instance of Array", list);
        // Node returns an empty buffer for an empty list, whatever totalLength says.
        if (list.length === 0) return Buffer.alloc(0);
        if (totalLength === undefined) {
            totalLength = 0;
            for (let i = 0; i < list.length; i++) {
                const buf = list[i];
                if (!(buf instanceof Uint8Array)) {
                    throw invalidArgType("list[" + i + "]", BUF_OR_U8, buf);
                }
                totalLength += buf.length;
            }
        } else {
            validateOffset(totalLength, "totalLength", 0, kMaxLength);
        }
        const out = Buffer.allocUnsafe(totalLength);
        let pos = 0;
        for (let i = 0; i < list.length; i++) {
            const buf = list[i];
            if (!(buf instanceof Uint8Array)) {
                throw invalidArgType("list[" + i + "]", BUF_OR_U8, buf);
            }
            if (pos + buf.length > totalLength) {
                out.set(buf.subarray(0, totalLength - pos), pos);
                pos = totalLength;
                break;
            }
            out.set(buf, pos);
            pos += buf.length;
        }
        // Anything the sources did not cover stays zero-filled.
        if (pos < totalLength) Uint8Array.prototype.fill.call(out, 0, pos, totalLength);
        return out;
    }
    equals(other) {
        if (!(other instanceof Uint8Array)) throw invalidArgType("otherBuffer", BUF_OR_U8, other);
        return compareBytes(this, 0, this.length, other, 0, other.length) === 0;
    }
    compare(target, targetStart, targetEnd, sourceStart, sourceEnd) {
        if (!(target instanceof Uint8Array)) throw invalidArgType("target", BUF_OR_U8, target);
        if (
            targetStart === undefined && targetEnd === undefined &&
            sourceStart === undefined && sourceEnd === undefined
        ) {
            return compareBytes(this, 0, this.length, target, 0, target.length);
        }
        if (targetStart === undefined) targetStart = 0;
        else validateOffset(targetStart, "targetStart", 0, kMaxLength);
        if (targetEnd === undefined) targetEnd = target.length;
        else validateOffset(targetEnd, "targetEnd", 0, target.length);
        if (sourceStart === undefined) sourceStart = 0;
        else validateOffset(sourceStart, "sourceStart", 0, kMaxLength);
        if (sourceEnd === undefined) sourceEnd = this.length;
        else validateOffset(sourceEnd, "sourceEnd", 0, this.length);
        if (sourceStart >= sourceEnd) return targetStart >= targetEnd ? 0 : -1;
        if (targetStart >= targetEnd) return 1;
        return compareBytes(
            this, sourceStart, Math.min(sourceEnd, this.length),
            target, targetStart, Math.min(targetEnd, target.length)
        );
    }
    copy(target, targetStart, sourceStart, sourceEnd) {
        targetStart = targetStart === undefined ? 0 : targetStart;
        sourceStart = sourceStart === undefined ? 0 : sourceStart;
        sourceEnd = sourceEnd === undefined ? this.length : sourceEnd;
        const chunk = this.subarray(sourceStart, sourceEnd);
        const room = target.length - targetStart;
        const sliced = chunk.length > room ? chunk.subarray(0, room) : chunk;
        target.set(sliced, targetStart);
        return sliced.length;
    }
    fill(value, start, end, encoding) {
        start = start === undefined ? 0 : start;
        end = end === undefined ? this.length : end;
        if (typeof value === "string") {
            const bytes = decodeString(value, normEnc(encoding));
            if (bytes.length === 0) return this;
            for (let i = start; i < end; i++) this[i] = bytes[(i - start) % bytes.length];
            return this;
        }
        if (ArrayBuffer.isView(value)) {
            const bytes = value;
            if (bytes.length === 0) return this;
            for (let i = start; i < end; i++) this[i] = bytes[(i - start) % bytes.length];
            return this;
        }
        Uint8Array.prototype.fill.call(this, value, start, end);
        return this;
    }
    write(string, offset, length, encoding) {
        if (typeof offset === "string") { encoding = offset; offset = 0; length = undefined; }
        else if (typeof length === "string") { encoding = length; length = undefined; }
        offset = offset === undefined ? 0 : offset;
        const bytes = decodeString(String(string), normEnc(encoding));
        const room = this.length - offset;
        const n = Math.min(bytes.length, length === undefined ? room : Math.min(length, room));
        this.set(bytes.subarray(0, n), offset);
        return n;
    }
    subarray(start, end) {
        const view = Uint8Array.prototype.subarray.call(this, start, end);
        return new Buffer(view.buffer, view.byteOffset, view.byteLength);
    }
    slice(start, end) { return this.subarray(start, end); }
    indexOf(value, byteOffset, encoding) {
        if (typeof byteOffset === "string") { encoding = byteOffset; byteOffset = 0; }
        byteOffset = byteOffset === undefined ? 0 : byteOffset;
        if (typeof value === "number") {
            return Uint8Array.prototype.indexOf.call(this, value & 0xff, byteOffset);
        }
        const needle = typeof value === "string" ? decodeString(value, normEnc(encoding)) : value;
        if (needle.length === 0) return byteOffset <= this.length ? byteOffset : this.length;
        outer: for (let i = byteOffset; i + needle.length <= this.length; i++) {
            for (let j = 0; j < needle.length; j++) {
                if (this[i + j] !== needle[j]) continue outer;
            }
            return i;
        }
        return -1;
    }
    includes(value, byteOffset, encoding) {
        return this.indexOf(value, byteOffset, encoding) !== -1;
    }
    toString(encoding, start, end) {
        const enc = normEnc(encoding);
        const view = start !== undefined || end !== undefined ? this.subarray(start, end) : this;
        if (enc === "base64" || enc === "base64url") {
            let s = "";
            for (let i = 0; i < view.length; i++) s += String.fromCharCode(view[i]);
            const b64 = btoa(s);
            return enc === "base64url"
                ? b64.replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "")
                : b64;
        }
        if (enc === "hex") {
            let h = "";
            for (let i = 0; i < view.length; i++) h += view[i].toString(16).padStart(2, "0");
            return h;
        }
        if (enc === "latin1" || enc === "ascii") {
            let s = "";
            for (let i = 0; i < view.length; i++) s += String.fromCharCode(view[i] & (enc === "ascii" ? 0x7f : 0xff));
            return s;
        }
        if (enc === "utf16le") {
            let s = "";
            for (let i = 0; i + 1 < view.length; i += 2) {
                const c = view[i] | (view[i + 1] << 8);
                // Join surrogate pairs into one code point: the engine's
                // strings are UTF-8, so appending the halves separately loses
                // the character (see decodeUtf8 above).
                if (c >= 0xd800 && c <= 0xdbff && i + 3 < view.length) {
                    const c2 = view[i + 2] | (view[i + 3] << 8);
                    if (c2 >= 0xdc00 && c2 <= 0xdfff) {
                        s += String.fromCodePoint(0x10000 + ((c - 0xd800) << 10) + (c2 - 0xdc00));
                        i += 2;
                        continue;
                    }
                }
                s += String.fromCharCode(c);
            }
            return s;
        }
        return decodeUtf8(view);
    }
    toJSON() {
        return { type: "Buffer", data: Array.from(this) };
    }
}
function wrap(bytes) {
    return new Buffer(bytes.buffer, bytes.byteOffset, bytes.byteLength);
}
const INSPECT_MAX_BYTES = 50;
const kMaxLength = 2147483647;
const constants = Object.freeze({ MAX_LENGTH: kMaxLength, MAX_STRING_LENGTH: 536870888 });
function atobExport(data) { return atob(data); }
function btoaExport(data) { return btoa(data); }
export { Buffer, INSPECT_MAX_BYTES, kMaxLength, constants, atobExport as atob, btoaExport as btoa };
export default { Buffer, INSPECT_MAX_BYTES, kMaxLength, constants, atob: atobExport, btoa: btoaExport };
"#;

const UTIL_SHIM: &str = r#"
// node:util shim. `inspect` is a real (if abbreviated) reimplementation of
// Node's algorithm — quoted strings inside containers, class/null-prototype
// prefixes, depth limiting, circular marking and the `util.inspect.custom`
// hook — because `format`, `console` and node:events' unhandled-'error'
// message all render through it and Node's output shape is observable.
// `format` follows Node's specifier rules (%s/%d/%i/%f/%j/%o/%O/%c, `%%`
// escaping, and inspection of the leftover arguments). The type predicates
// live in node:util/types and are re-exported as `types`.
import types from "node:util/types";

const customInspectSymbol = Symbol.for("nodejs.util.inspect.custom");
const kCustomPromisifiedSymbol = Symbol.for("nodejs.util.promisify.custom");
const kCustomPromisifyArgsSymbol = Symbol.for("nodejs.util.promisify.customArgs");

function invalidArgType(name, expected, actual) {
    const received = actual === null
        ? "null"
        : typeof actual === "object"
            ? "an instance of " + ((actual.constructor && actual.constructor.name) || "Object")
            : "type " + typeof actual + " (" + String(actual) + ")";
    const err = new TypeError(
        'The "' + name + '" argument must be of type ' + expected + ". Received " + received
    );
    err.code = "ERR_INVALID_ARG_TYPE";
    return err;
}

// ---------------------------------------------------------------- inspect

const kIdentifierKey = /^[A-Za-z_$][A-Za-z0-9_$]*$/;
const kEscapes = { "\b": "\\b", "\t": "\\t", "\n": "\\n", "\f": "\\f", "\r": "\\r" };

// Node prefers single quotes, falling back to double then backtick so the
// contents never need escaping when a cheaper quote is available.
function strEscape(str) {
    let quote = "'";
    if (str.indexOf("'") !== -1) {
        if (str.indexOf('"') === -1) quote = '"';
        else if (str.indexOf("`") === -1) quote = "`";
    }
    let out = "";
    for (let i = 0; i < str.length; i++) {
        const ch = str[i];
        if (ch === quote || ch === "\\") { out += "\\" + ch; continue; }
        const mapped = kEscapes[ch];
        if (mapped !== undefined) { out += mapped; continue; }
        const code = str.charCodeAt(i);
        if (code < 0x20 || code === 0x7f) {
            out += "\\x" + (code < 16 ? "0" : "") + code.toString(16);
            continue;
        }
        out += ch;
    }
    return quote + out + quote;
}

function formatNumber(number) {
    return Object.is(number, -0) ? "-0" : String(number);
}
function formatPrimitive(value) {
    switch (typeof value) {
        case "string": return strEscape(value);
        case "number": return formatNumber(value);
        case "bigint": return String(value) + "n";
        case "symbol": return value.toString();
        case "undefined": return "undefined";
        default: return String(value);
    }
}
function formatFunctionBase(value, ctorName) {
    const type = ctorName === "GeneratorFunction" || ctorName === "AsyncGeneratorFunction"
        ? "GeneratorFunction"
        : "Function";
    const isClass = /^\s*class[\s{]/.test(Function.prototype.toString.call(value));
    if (isClass) return value.name ? "[class " + value.name + "]" : "[class (anonymous)]";
    return value.name ? "[" + type + ": " + value.name + "]" : "[" + type + " (anonymous)]";
}

// Walks the prototype chain for the nearest own `constructor` with a name;
// `null` means the value has (or reached) a null prototype.
function getCtorName(value) {
    let obj = value;
    while (obj !== null && obj !== undefined) {
        let descriptor;
        try { descriptor = Object.getOwnPropertyDescriptor(obj, "constructor"); } catch { return null; }
        if (descriptor !== undefined && typeof descriptor.value === "function" &&
            descriptor.value.name !== "") {
            return descriptor.value.name;
        }
        obj = Object.getPrototypeOf(obj);
    }
    return null;
}

function isBelowBreakLength(ctx, output, start) {
    let total = output.length + start;
    if (total + output.length > ctx.breakLength) return false;
    for (let i = 0; i < output.length; i++) {
        total += output[i].length;
        if (total > ctx.breakLength) return false;
    }
    return true;
}
function reduceToSingleString(ctx, output, base, braces) {
    const start = output.length + ctx.indentationLvl + braces[0].length + base.length + 10;
    if (isBelowBreakLength(ctx, output, start)) {
        const joined = output.join(", ");
        if (joined.indexOf("\n") === -1) {
            return (base ? base + " " : "") +
                (output.length === 0 ? braces[0] + braces[1] : braces[0] + " " + joined + " " + braces[1]);
        }
    }
    const indent = "\n" + " ".repeat(ctx.indentationLvl);
    return (base ? base + " " : "") + braces[0] + indent + "  " +
        output.join("," + indent + "  ") + indent + braces[1];
}

function formatKey(key) {
    if (typeof key === "symbol") return "[" + key.toString() + "]";
    return kIdentifierKey.test(key) ? key : strEscape(key);
}
function formatProperty(ctx, value, recurseTimes, key) {
    let descriptor;
    try { descriptor = Object.getOwnPropertyDescriptor(value, key); } catch { descriptor = undefined; }
    let str;
    if (descriptor !== undefined && descriptor.get !== undefined) {
        str = descriptor.set !== undefined ? "[Getter/Setter]" : "[Getter]";
    } else if (descriptor !== undefined && descriptor.set !== undefined) {
        str = "[Setter]";
    } else {
        ctx.indentationLvl += 2;
        try { str = formatValue(ctx, value[key], recurseTimes + 1); }
        finally { ctx.indentationLvl -= 2; }
    }
    return formatKey(key) + ": " + str;
}
function isIndexKey(key) {
    const n = Number(key);
    return Number.isInteger(n) && n >= 0 && String(n) === key;
}
function extraKeys(value, skipIndices) {
    const keys = [];
    for (const key of Object.keys(value)) {
        if (skipIndices && isIndexKey(key)) continue;
        keys.push(key);
    }
    const symbols = Object.getOwnPropertySymbols(value);
    for (const sym of symbols) {
        if (Object.prototype.propertyIsEnumerable.call(value, sym)) keys.push(sym);
    }
    return keys;
}

function formatValue(ctx, value, recurseTimes) {
    if (typeof value !== "object" && typeof value !== "function") return formatPrimitive(value);
    if (value === null) return "null";

    // `util.inspect.custom` wins over every built-in rendering, exactly like
    // Node — including when it throws, which callers are expected to handle.
    let custom;
    try { custom = value[customInspectSymbol]; } catch { custom = undefined; }
    if (typeof custom === "function" && custom !== inspect) {
        const depth = ctx.depth === Infinity ? null : ctx.depth - recurseTimes;
        const ret = custom.call(value, depth, { depth, colors: false, showHidden: ctx.showHidden }, inspect);
        if (ret !== value) return typeof ret === "string" ? ret : formatValue(ctx, ret, recurseTimes);
    }

    if (ctx.seen.indexOf(value) !== -1) return "[Circular *1]";

    ctx.seen.push(value);
    try {
        return formatRaw(ctx, value, recurseTimes);
    } finally {
        ctx.seen.pop();
    }
}

function formatRaw(ctx, value, recurseTimes) {
    const ctorName = getCtorName(value);
    let tag = "";
    try {
        const raw = value[Symbol.toStringTag];
        if (typeof raw === "string" && raw !== ctorName) tag = raw;
    } catch { tag = ""; }

    let base = "";
    let braces;
    let output;
    let keys;

    if (Array.isArray(value)) {
        if (recurseTimes > ctx.depth) return ctorName === "Array" ? "[Array]" : "[" + ctorName + "]";
        const prefix = ctorName === "Array"
            ? ""
            : (ctorName === null ? "[Array: null prototype] " : ctorName + "(" + value.length + ") ");
        braces = [prefix + "[", "]"];
        keys = extraKeys(value, true);
        output = [];
        ctx.indentationLvl += 2;
        try {
            for (let i = 0; i < value.length && i < ctx.maxArrayLength; i++) {
                output.push(Object.prototype.hasOwnProperty.call(value, i)
                    ? formatValue(ctx, value[i], recurseTimes + 1)
                    : "<1 empty item>");
            }
            if (value.length > ctx.maxArrayLength) {
                output.push("... " + (value.length - ctx.maxArrayLength) + " more items");
            }
        } finally { ctx.indentationLvl -= 2; }
    } else if (ArrayBuffer.isView(value) && !types.isDataView(value)) {
        if (recurseTimes > ctx.depth) return "[" + (ctorName || "TypedArray") + "]";
        braces = [(ctorName || "TypedArray") + "(" + value.length + ") [", "]"];
        keys = extraKeys(value, true);
        output = [];
        for (let i = 0; i < value.length && i < ctx.maxArrayLength; i++) {
            output.push(formatPrimitive(value[i]));
        }
        if (value.length > ctx.maxArrayLength) {
            output.push("... " + (value.length - ctx.maxArrayLength) + " more items");
        }
    } else if (types.isMap(value)) {
        if (recurseTimes > ctx.depth) return "[Map]";
        braces = [(ctorName || "Map") + "(" + value.size + ") {", "}"];
        keys = extraKeys(value, false);
        output = [];
        ctx.indentationLvl += 2;
        try {
            for (const entry of value) {
                output.push(formatValue(ctx, entry[0], recurseTimes + 1) + " => " +
                    formatValue(ctx, entry[1], recurseTimes + 1));
            }
        } finally { ctx.indentationLvl -= 2; }
    } else if (types.isSet(value)) {
        if (recurseTimes > ctx.depth) return "[Set]";
        braces = [(ctorName || "Set") + "(" + value.size + ") {", "}"];
        keys = extraKeys(value, false);
        output = [];
        ctx.indentationLvl += 2;
        try {
            for (const entry of value) output.push(formatValue(ctx, entry, recurseTimes + 1));
        } finally { ctx.indentationLvl -= 2; }
    } else if (types.isDate(value)) {
        const time = Date.prototype.getTime.call(value);
        base = Number.isNaN(time) ? "Invalid Date" : Date.prototype.toISOString.call(value);
        braces = ["{", "}"];
        keys = extraKeys(value, false);
        output = [];
    } else if (types.isRegExp(value)) {
        base = RegExp.prototype.toString.call(value);
        braces = ["{", "}"];
        keys = extraKeys(value, false);
        output = [];
    } else if (value instanceof Error) {
        base = typeof value.stack === "string" && value.stack.length !== 0
            ? value.stack
            : "[" + (value.name || "Error") + (value.message ? ": " + value.message : "") + "]";
        braces = ["{", "}"];
        keys = extraKeys(value, false);
        output = [];
    } else if (typeof value === "function") {
        base = formatFunctionBase(value, ctorName);
        braces = ["{", "}"];
        keys = extraKeys(value, false);
        output = [];
    } else if (types.isBoxedPrimitive(value)) {
        base = "[" + (types.isStringObject(value) ? "String" :
            types.isNumberObject(value) ? "Number" :
            types.isBooleanObject(value) ? "Boolean" :
            types.isBigIntObject(value) ? "BigInt" : "Symbol") +
            ": " + formatPrimitive(boxedValueOf(value)) + "]";
        braces = ["{", "}"];
        keys = extraKeys(value, types.isStringObject(value));
        output = [];
    } else if (types.isPromise(value)) {
        base = "Promise";
        braces = ["{", "}"];
        keys = extraKeys(value, false);
        output = ["<pending>"];
    } else {
        if (recurseTimes > ctx.depth) {
            return ctorName === "Object" || ctorName === null ? "[Object]" : "[" + ctorName + "]";
        }
        const prefix = ctorName === "Object"
            ? (tag ? "Object [" + tag + "] " : "")
            : ctorName === null
                ? "[Object: null prototype] "
                : ctorName + (tag ? " [" + tag + "] " : " ");
        braces = [prefix + "{", "}"];
        keys = extraKeys(value, false);
        output = [];
    }

    for (const key of keys) output.push(formatProperty(ctx, value, recurseTimes, key));
    if (output.length === 0 && base !== "") return base;
    return reduceToSingleString(ctx, output, base, braces);
}

function boxedValueOf(value) {
    if (types.isStringObject(value)) return String.prototype.valueOf.call(value);
    if (types.isNumberObject(value)) return Number.prototype.valueOf.call(value);
    if (types.isBooleanObject(value)) return Boolean.prototype.valueOf.call(value);
    if (types.isBigIntObject(value)) return BigInt.prototype.valueOf.call(value);
    return Symbol.prototype.valueOf.call(value);
}

// `showHidden` is accepted and forwarded to `util.inspect.custom` hooks, but
// non-enumerable properties are not rendered — enumerating internal slots the
// way Node's `%o` does needs engine support this runtime does not have. The
// same goes for `colors`, `numericSeparator` and `sorted`: they round-trip
// through `defaultOptions` for callers that read them back, and are otherwise
// inert.
function inspect(value, opts) {
    const ctx = {
        depth: inspect.defaultOptions.depth,
        showHidden: inspect.defaultOptions.showHidden,
        breakLength: inspect.defaultOptions.breakLength,
        maxArrayLength: inspect.defaultOptions.maxArrayLength,
        seen: [],
        indentationLvl: 0,
    };
    if (typeof opts === "boolean") {
        ctx.showHidden = opts;
        if (typeof arguments[2] === "number") ctx.depth = arguments[2];
    } else if (opts !== null && typeof opts === "object") {
        if (opts.depth !== undefined) ctx.depth = opts.depth === null ? Infinity : opts.depth;
        if (opts.showHidden !== undefined) ctx.showHidden = opts.showHidden;
        if (opts.breakLength !== undefined) ctx.breakLength = opts.breakLength;
        if (opts.maxArrayLength !== undefined) {
            ctx.maxArrayLength = opts.maxArrayLength === null ? Infinity : opts.maxArrayLength;
        }
    }
    return formatValue(ctx, value, 0);
}
inspect.custom = customInspectSymbol;
inspect.defaultOptions = {
    depth: 2,
    showHidden: false,
    colors: false,
    breakLength: 128,
    maxArrayLength: 100,
    compact: 3,
    numericSeparator: false,
    sorted: false,
    getters: false,
};

// ----------------------------------------------------------------- format

let circularErrorMessage;
function tryStringify(arg) {
    try {
        return JSON.stringify(arg);
    } catch (err) {
        if (circularErrorMessage === undefined) {
            circularErrorMessage = null;
            try {
                const probe = {};
                probe.probe = probe;
                JSON.stringify(probe);
            } catch (circularError) {
                circularErrorMessage = circularError.message;
            }
        }
        if (err instanceof TypeError && err.message === circularErrorMessage) return "[Circular]";
        throw err;
    }
}

const kBuiltinPrototypes = [
    Object.prototype, Array.prototype, Function.prototype, Error.prototype,
    Date.prototype, RegExp.prototype, Number.prototype, String.prototype,
    Boolean.prototype, Symbol.prototype,
];
// `%s` renders an object through its own `toString`/`Symbol.toPrimitive` when
// it has one, and through `inspect` otherwise (Node's rule — it is what makes
// `%s` on a plain object print `{ a: [Array] }` rather than `[object Object]`).
function hasCustomToString(value) {
    try {
        if (typeof value[Symbol.toPrimitive] === "function") return true;
        let obj = value;
        while (obj !== null) {
            const descriptor = Object.getOwnPropertyDescriptor(obj, "toString");
            if (descriptor !== undefined) {
                return typeof descriptor.value === "function" && kBuiltinPrototypes.indexOf(obj) === -1;
            }
            obj = Object.getPrototypeOf(obj);
        }
    } catch { return false; }
    return false;
}

function formatWithOptionsInternal(inspectOptions, args) {
    const first = args[0];
    let a = 0;
    let str = "";
    let join = "";

    if (typeof first === "string") {
        if (args.length === 1) return first;
        let tempStr;
        let lastPos = 0;
        for (let i = 0; i < first.length - 1; i++) {
            if (first.charCodeAt(i) !== 37) continue; // '%'
            const nextChar = first.charCodeAt(++i);
            if (a + 1 !== args.length) {
                switch (nextChar) {
                    case 115: { // 's'
                        const tempArg = args[++a];
                        if (typeof tempArg === "number") tempStr = formatNumber(tempArg);
                        else if (typeof tempArg === "bigint") tempStr = String(tempArg) + "n";
                        else if (typeof tempArg !== "object" || tempArg === null || hasCustomToString(tempArg)) {
                            tempStr = String(tempArg);
                        } else {
                            tempStr = inspect(tempArg, { ...inspectOptions, depth: 0 });
                        }
                        break;
                    }
                    case 106: // 'j'
                        tempStr = tryStringify(args[++a]);
                        break;
                    case 100: { // 'd'
                        const tempNum = args[++a];
                        if (typeof tempNum === "bigint") tempStr = String(tempNum) + "n";
                        else if (typeof tempNum === "symbol") tempStr = "NaN";
                        else tempStr = formatNumber(Number(tempNum));
                        break;
                    }
                    case 79: // 'O'
                        tempStr = inspect(args[++a], inspectOptions);
                        break;
                    case 111: // 'o'
                        tempStr = inspect(args[++a], { ...inspectOptions, showHidden: true, depth: 4 });
                        break;
                    case 105: { // 'i'
                        const tempInteger = args[++a];
                        if (typeof tempInteger === "bigint") tempStr = String(tempInteger) + "n";
                        else if (typeof tempInteger === "symbol") tempStr = "NaN";
                        else tempStr = formatNumber(parseInt(tempInteger, 10));
                        break;
                    }
                    case 102: { // 'f'
                        const tempFloat = args[++a];
                        if (typeof tempFloat === "symbol") tempStr = "NaN";
                        else tempStr = formatNumber(parseFloat(tempFloat));
                        break;
                    }
                    case 99: // 'c'
                        a += 1;
                        tempStr = "";
                        break;
                    case 37: // '%'
                        str += first.slice(lastPos, i);
                        lastPos = i + 1;
                        continue;
                    default:
                        continue;
                }
                if (lastPos !== i - 1) str += first.slice(lastPos, i - 1);
                str += tempStr;
                lastPos = i + 1;
            } else if (nextChar === 37) {
                str += first.slice(lastPos, i);
                lastPos = i + 1;
            }
        }
        if (lastPos !== 0) {
            a++;
            join = " ";
            if (lastPos < first.length) str += first.slice(lastPos);
        }
    }

    while (a < args.length) {
        const value = args[a];
        str += join;
        str += typeof value !== "string" ? inspect(value, inspectOptions) : value;
        join = " ";
        a++;
    }
    return str;
}

function format(...args) {
    return formatWithOptionsInternal({}, args);
}
function formatWithOptions(inspectOptions, ...args) {
    if (inspectOptions === null || typeof inspectOptions !== "object") {
        throw invalidArgType("inspectOptions", "object", inspectOptions);
    }
    return formatWithOptionsInternal(inspectOptions, args);
}

// --------------------------------------------------------------- promisify

function promisify(original) {
    if (typeof original !== "function") throw invalidArgType("original", "function", original);

    if (original[kCustomPromisifiedSymbol]) {
        const fn = original[kCustomPromisifiedSymbol];
        if (typeof fn !== "function") {
            throw invalidArgType("util.promisify.custom", "function", fn);
        }
        Object.defineProperty(fn, kCustomPromisifiedSymbol, {
            value: fn, enumerable: false, writable: false, configurable: true,
        });
        return fn;
    }

    // `customPromisifyArgs` lets a multi-value callback resolve to an object.
    const argumentNames = original[kCustomPromisifyArgsSymbol];

    function fn(...args) {
        return new Promise((resolve, reject) => {
            args.push((err, ...values) => {
                if (err) { reject(err); return; }
                if (argumentNames !== undefined && values.length > 1) {
                    const obj = {};
                    for (let i = 0; i < argumentNames.length; i++) obj[argumentNames[i]] = values[i];
                    resolve(obj);
                } else {
                    resolve(values[0]);
                }
            });
            original.apply(this, args);
        });
    }
    Object.setPrototypeOf(fn, Object.getPrototypeOf(original));
    Object.defineProperty(fn, kCustomPromisifiedSymbol, {
        value: fn, enumerable: false, writable: false, configurable: true,
    });
    try { Object.defineProperties(fn, Object.getOwnPropertyDescriptors(original)); } catch {}
    return fn;
}
promisify.custom = kCustomPromisifiedSymbol;

function callbackify(fn) {
    if (typeof fn !== "function") throw invalidArgType("original", "function", fn);
    return function (...args) {
        const cb = args.pop();
        Promise.resolve(fn.apply(this, args)).then(
            (value) => queueMicrotask(() => cb(null, value)),
            (err) => queueMicrotask(() => cb(err || new Error("rejected with falsy value")))
        );
    };
}
// Node de-duplicates by `code` when one is supplied, so two wrappers sharing a
// code warn once between them.
const emittedDeprecations = Object.create(null);
function deprecate(fn, message, code) {
    if (code !== undefined && emittedDeprecations[code] === undefined) {
        emittedDeprecations[code] = false;
    }
    let warned = false;
    function deprecated(...args) {
        const already = code !== undefined ? emittedDeprecations[code] : warned;
        if (!already) {
            warned = true;
            if (code !== undefined) emittedDeprecations[code] = true;
            const warning = new Error(message);
            warning.name = "DeprecationWarning";
            if (code !== undefined) warning.code = code;
            if (globalThis.process && typeof globalThis.process.emitWarning === "function") {
                globalThis.process.emitWarning(warning);
            } else if (globalThis.console && typeof globalThis.console.warn === "function") {
                globalThis.console.warn("DeprecationWarning: " + message);
            }
        }
        return fn.apply(this, args);
    }
    return deprecated;
}
function inherits(ctor, superCtor) {
    ctor.super_ = superCtor;
    Object.setPrototypeOf(ctor.prototype, superCtor.prototype);
}

// -------------------------------------------------------- isDeepStrictEqual

function ownEnumerableKeys(value) {
    const keys = Object.keys(value);
    for (const sym of Object.getOwnPropertySymbols(value)) {
        if (Object.prototype.propertyIsEnumerable.call(value, sym)) keys.push(sym);
    }
    return keys;
}
// Boxed primitives compare by their *internal* value, and the brand checks in
// node:util/types are slot-based, so a swapped prototype or a forged
// Symbol.toStringTag cannot make a Boolean object look like a String one.
function isEqualBoxedPrimitive(val1, val2) {
    if (types.isNumberObject(val1)) {
        return types.isNumberObject(val2) &&
            Object.is(Number.prototype.valueOf.call(val1), Number.prototype.valueOf.call(val2));
    }
    if (types.isStringObject(val1)) {
        return types.isStringObject(val2) &&
            String.prototype.valueOf.call(val1) === String.prototype.valueOf.call(val2);
    }
    if (types.isBooleanObject(val1)) {
        return types.isBooleanObject(val2) &&
            Boolean.prototype.valueOf.call(val1) === Boolean.prototype.valueOf.call(val2);
    }
    if (types.isBigIntObject(val1)) {
        return types.isBigIntObject(val2) &&
            BigInt.prototype.valueOf.call(val1) === BigInt.prototype.valueOf.call(val2);
    }
    return types.isSymbolObject(val2) &&
        Symbol.prototype.valueOf.call(val1) === Symbol.prototype.valueOf.call(val2);
}
function byteEqual(a, b) {
    for (let i = 0; i < a.length; i++) if (a[i] !== b[i]) return false;
    return true;
}
// The snapshot policy cannot rely on Set/WeakSet being present, so cycles are
// tracked with a plain array of visited pairs (the node:assert shim does the
// same).
function innerDeepEqual(val1, val2, memos) {
    if (val1 === val2) return val1 !== 0 || Object.is(val1, val2);
    if (typeof val1 !== "object" || val1 === null) {
        return typeof val1 === "number" && Number.isNaN(val1) &&
            typeof val2 === "number" && Number.isNaN(val2);
    }
    if (typeof val2 !== "object" || val2 === null) return false;
    if (Object.getPrototypeOf(val1) !== Object.getPrototypeOf(val2)) return false;
    if (Object.prototype.toString.call(val1) !== Object.prototype.toString.call(val2)) return false;

    if (Array.isArray(val1)) {
        if (!Array.isArray(val2) || val1.length !== val2.length) return false;
    } else if (types.isDate(val1)) {
        if (Date.prototype.getTime.call(val1) !== Date.prototype.getTime.call(val2)) return false;
    } else if (types.isRegExp(val1)) {
        if (val1.source !== val2.source || val1.flags !== val2.flags ||
            val1.lastIndex !== val2.lastIndex) return false;
    } else if (val1 instanceof Error) {
        if (val1.message !== val2.message || val1.name !== val2.name) return false;
    } else if (ArrayBuffer.isView(val1)) {
        if (!ArrayBuffer.isView(val2) || val1.byteLength !== val2.byteLength) return false;
        if (!byteEqual(new Uint8Array(val1.buffer, val1.byteOffset, val1.byteLength),
                       new Uint8Array(val2.buffer, val2.byteOffset, val2.byteLength))) return false;
    } else if (types.isArrayBuffer(val1)) {
        if (val1.byteLength !== val2.byteLength) return false;
        if (!byteEqual(new Uint8Array(val1), new Uint8Array(val2))) return false;
    } else if (types.isSet(val1)) {
        if (val1.size !== val2.size) return false;
        if (!setEquiv(val1, val2, memos)) return false;
    } else if (types.isMap(val1)) {
        if (val1.size !== val2.size) return false;
        if (!mapEquiv(val1, val2, memos)) return false;
    } else if (types.isBoxedPrimitive(val1)) {
        if (!isEqualBoxedPrimitive(val1, val2)) return false;
    }

    for (const pair of memos) {
        if (pair[0] === val1 && pair[1] === val2) return true;
    }
    memos.push([val1, val2]);
    try {
        const keys1 = ownEnumerableKeys(val1);
        const keys2 = ownEnumerableKeys(val2);
        if (keys1.length !== keys2.length) return false;
        for (const key of keys1) {
            if (!Object.prototype.hasOwnProperty.call(val2, key)) return false;
            if (!innerDeepEqual(val1[key], val2[key], memos)) return false;
        }
        return true;
    } finally {
        memos.pop();
    }
}
function setEquiv(a, b, memos) {
    const remaining = [];
    for (const item of b) remaining.push(item);
    for (const item of a) {
        let matched = -1;
        for (let i = 0; i < remaining.length; i++) {
            if (innerDeepEqual(item, remaining[i], memos)) { matched = i; break; }
        }
        if (matched === -1) return false;
        remaining.splice(matched, 1);
    }
    return true;
}
function mapEquiv(a, b, memos) {
    const remaining = [];
    for (const entry of b) remaining.push(entry);
    for (const entry of a) {
        let matched = -1;
        for (let i = 0; i < remaining.length; i++) {
            if (innerDeepEqual(entry[0], remaining[i][0], memos) &&
                innerDeepEqual(entry[1], remaining[i][1], memos)) { matched = i; break; }
        }
        if (matched === -1) return false;
        remaining.splice(matched, 1);
    }
    return true;
}
function isDeepStrictEqual(a, b) {
    return innerDeepEqual(a, b, []);
}

// Legacy predicates, still imported by older packages.
const isArray = Array.isArray;
function isBoolean(v) { return typeof v === "boolean"; }
function isNull(v) { return v === null; }
function isNullOrUndefined(v) { return v === null || v === undefined; }
function isNumber(v) { return typeof v === "number"; }
function isString(v) { return typeof v === "string"; }
function isSymbol(v) { return typeof v === "symbol"; }
function isUndefined(v) { return v === undefined; }
function isObject(v) { return v !== null && typeof v === "object"; }
function isFunction(v) { return typeof v === "function"; }
function isPrimitive(v) { return v === null || (typeof v !== "object" && typeof v !== "function"); }
function isRegExp(v) { return v instanceof RegExp; }
function isDate(v) { return v instanceof Date; }
function isError(v) { return v instanceof Error; }
function isBuffer(v) { return v instanceof Uint8Array; }
const TextEncoder = globalThis.TextEncoder;
const TextDecoder = globalThis.TextDecoder;
function debuglog() { return function () {}; }
export {
    inspect, format, formatWithOptions, promisify, callbackify, deprecate, inherits, types,
    isDeepStrictEqual, debuglog, TextEncoder, TextDecoder,
    isArray, isBoolean, isNull, isNullOrUndefined, isNumber, isString,
    isSymbol, isUndefined, isObject, isFunction, isPrimitive, isRegExp,
    isDate, isError, isBuffer,
};
export default {
    inspect, format, formatWithOptions, promisify, callbackify, deprecate, inherits, types,
    isDeepStrictEqual, debuglog, TextEncoder, TextDecoder,
    isArray, isBoolean, isNull, isNullOrUndefined, isNumber, isString,
    isSymbol, isUndefined, isObject, isFunction, isPrimitive, isRegExp,
    isDate, isError, isBuffer,
};
"#;

// node:fs shim backed by the captured, snapshot-resident virtual filesystem.
// All byte payloads cross the host boundary base64-encoded (the `__chidori_fs_*`
// natives) so binary content survives intact. Reads/writes never touch the host
// disk — see docs/captured-effects-vfs-crypto-timers.md. Only the surface that
// real packages tend to touch is implemented; everything else is simply absent
// so missing surface shows up as a clear "not a function" at first use.
const FS_SHIM: &str = r#"
import { Buffer } from "node:buffer";

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
// Normalize the (string | { encoding }) options form to an encoding or null.
function optEncoding(options) {
    if (typeof options === "string") return options;
    if (options && typeof options === "object") return options.encoding ?? null;
    return null;
}
function toBase64(data, encoding) {
    if (typeof data === "string") {
        if (encoding === "base64") return data;
        if (encoding === "hex") {
            const bytes = new Uint8Array(data.length / 2);
            for (let i = 0; i < bytes.length; i++) bytes[i] = parseInt(data.substr(i * 2, 2), 16);
            return bytesToBase64(bytes);
        }
        return bytesToBase64(new TextEncoder().encode(data));
    }
    if (ArrayBuffer.isView(data)) {
        return bytesToBase64(new Uint8Array(data.buffer, data.byteOffset, data.byteLength));
    }
    if (data instanceof ArrayBuffer) return bytesToBase64(new Uint8Array(data));
    throw new TypeError("fs: unsupported data type for write");
}
function decodeRead(b64, encoding) {
    if (!encoding) return Buffer.from(base64ToBytes(b64));
    if (encoding === "base64") return b64;
    const bytes = base64ToBytes(b64);
    if (encoding === "hex") {
        let h = "";
        for (let i = 0; i < bytes.length; i++) h += bytes[i].toString(16).padStart(2, "0");
        return h;
    }
    return new TextDecoder().decode(bytes);
}
function makeStats(raw) {
    return {
        size: raw.size,
        mtimeSeq: raw.mtimeSeq,
        isFile() { return raw.isFile; },
        isDirectory() { return raw.isDirectory; },
        isSymbolicLink() { return false; },
    };
}

export function readFileSync(path, options) {
    return decodeRead(globalThis.__chidori_fs_read(String(path)), optEncoding(options));
}
export function writeFileSync(path, data, options) {
    globalThis.__chidori_fs_write(String(path), toBase64(data, optEncoding(options)));
}
export function appendFileSync(path, data, options) {
    globalThis.__chidori_fs_append(String(path), toBase64(data, optEncoding(options)));
}
export function existsSync(path) { return globalThis.__chidori_fs_exists(String(path)); }
export function readdirSync(path) { return globalThis.__chidori_fs_readdir(String(path)); }
export function mkdirSync(path, options) {
    const recursive = !!(options && typeof options === "object" && options.recursive);
    globalThis.__chidori_fs_mkdir(String(path), recursive);
}
export function rmSync(path, options) {
    const o = options || {};
    globalThis.__chidori_fs_rm(String(path), !!o.recursive, !!o.force);
}
export function rmdirSync(path, options) {
    const o = options || {};
    globalThis.__chidori_fs_rm(String(path), !!o.recursive, false);
}
export function unlinkSync(path) { globalThis.__chidori_fs_rm(String(path), false, false); }
export function renameSync(from, to) { globalThis.__chidori_fs_rename(String(from), String(to)); }
export function statSync(path) { return makeStats(globalThis.__chidori_fs_stat(String(path))); }
export const lstatSync = statSync;
export function realpathSync(path) { return String(path); }

export const promises = {
    readFile: async (p, o) => readFileSync(p, o),
    writeFile: async (p, d, o) => writeFileSync(p, d, o),
    appendFile: async (p, d, o) => appendFileSync(p, d, o),
    readdir: async (p) => readdirSync(p),
    mkdir: async (p, o) => mkdirSync(p, o),
    rm: async (p, o) => rmSync(p, o),
    rmdir: async (p, o) => rmdirSync(p, o),
    unlink: async (p) => unlinkSync(p),
    rename: async (a, b) => renameSync(a, b),
    stat: async (p) => statSync(p),
    lstat: async (p) => statSync(p),
    realpath: async (p) => realpathSync(p),
};

const fs = {
    readFileSync, writeFileSync, appendFileSync, existsSync, readdirSync, mkdirSync,
    rmSync, rmdirSync, unlinkSync, renameSync, statSync, lstatSync, realpathSync, promises,
};
export default fs;
"#;

// node:crypto shim. Hashing/HMAC are deterministic and run inline (flagged
// CryptoHash); randomness is captured and replayed (flagged CryptoRandom). See
// docs/captured-effects-vfs-crypto-timers.md.
const CRYPTO_SHIM: &str = r#"
import { Buffer } from "node:buffer";

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
function toBytes(data, encoding) {
    if (typeof data === "string") {
        if (encoding === "base64") return base64ToBytes(data);
        if (encoding === "hex") {
            const out = new Uint8Array(data.length / 2);
            for (let i = 0; i < out.length; i++) out[i] = parseInt(data.substr(i * 2, 2), 16);
            return out;
        }
        return new TextEncoder().encode(data);
    }
    if (ArrayBuffer.isView(data)) return new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
    if (data instanceof ArrayBuffer) return new Uint8Array(data);
    throw new TypeError("crypto: unsupported data type");
}
function encodeDigest(b64, encoding) {
    if (!encoding || encoding === "buffer") return Buffer.from(base64ToBytes(b64));
    if (encoding === "base64") return b64;
    const bytes = base64ToBytes(b64);
    if (encoding === "hex") {
        let h = "";
        for (let i = 0; i < bytes.length; i++) h += bytes[i].toString(16).padStart(2, "0");
        return h;
    }
    return new TextDecoder().decode(bytes);
}
function concat(chunks) {
    let total = 0;
    for (const c of chunks) total += c.length;
    const all = new Uint8Array(total);
    let o = 0;
    for (const c of chunks) { all.set(c, o); o += c.length; }
    return all;
}

export function randomBytes(size) {
    return Buffer.from(base64ToBytes(globalThis.__chidori_crypto_random(size >>> 0)));
}
export function randomFillSync(buf, offset, size) {
    const view = new Uint8Array(buf.buffer, buf.byteOffset, buf.byteLength);
    offset = offset || 0;
    size = size === undefined ? view.length - offset : size;
    const bytes = base64ToBytes(globalThis.__chidori_crypto_random(size));
    view.set(bytes.subarray(0, size), offset);
    return buf;
}
export function randomUUID() {
    const b = base64ToBytes(globalThis.__chidori_crypto_random(16));
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    const h = [];
    for (let i = 0; i < 16; i++) h.push(b[i].toString(16).padStart(2, "0"));
    return `${h[0]}${h[1]}${h[2]}${h[3]}-${h[4]}${h[5]}-${h[6]}${h[7]}-${h[8]}${h[9]}-${h[10]}${h[11]}${h[12]}${h[13]}${h[14]}${h[15]}`;
}
export function randomInt(a, b) {
    let min, max;
    if (b === undefined) { min = 0; max = a; } else { min = a; max = b; }
    const range = max - min;
    if (range <= 0) throw new RangeError("randomInt: max must be greater than min");
    const bytes = base64ToBytes(globalThis.__chidori_crypto_random(6));
    let v = 0;
    for (let i = 0; i < bytes.length; i++) v = v * 256 + bytes[i];
    return min + (v % range);
}
export function createHash(algorithm) {
    const chunks = [];
    return {
        update(data, encoding) { chunks.push(toBytes(data, encoding)); return this; },
        digest(encoding) {
            const b64 = globalThis.__chidori_crypto_hash(algorithm, bytesToBase64(concat(chunks)));
            return encodeDigest(b64, encoding);
        },
    };
}
export function createHmac(algorithm, key) {
    const keyBytes = toBytes(key);
    const chunks = [];
    return {
        update(data, encoding) { chunks.push(toBytes(data, encoding)); return this; },
        digest(encoding) {
            const b64 = globalThis.__chidori_crypto_hmac(
                algorithm,
                bytesToBase64(keyBytes),
                bytesToBase64(concat(chunks))
            );
            return encodeDigest(b64, encoding);
        },
    };
}
export const webcrypto = globalThis.crypto;
export const subtle = globalThis.crypto ? globalThis.crypto.subtle : undefined;
export function getRandomValues(typedArray) { return globalThis.crypto.getRandomValues(typedArray); }

const crypto = {
    randomBytes, randomFillSync, randomUUID, randomInt, createHash, createHmac,
    webcrypto, subtle, getRandomValues,
};
export default crypto;
"#;

// node:fs/promises re-exports the promise API from the fs shim so
// `import { readFile } from "node:fs/promises"` resolves without diverging.
const FS_PROMISES_SHIM: &str = r#"
import { promises } from "node:fs";
export const readFile = promises.readFile;
export const writeFile = promises.writeFile;
export const appendFile = promises.appendFile;
export const readdir = promises.readdir;
export const mkdir = promises.mkdir;
export const rm = promises.rm;
export const rmdir = promises.rmdir;
export const unlink = promises.unlink;
export const rename = promises.rename;
export const stat = promises.stat;
export const lstat = promises.lstat;
export const realpath = promises.realpath;
export default promises;
"#;

// node:http client shim. Only the *client* surface is provided (request/get);
// there are no listening sockets. Every request is performed by the captured
// `__chidori_http` host op — the same networking capture `globalThis.fetch`
// uses — so a `node:http` request is subject to the security policy and the
// approval-pause path exactly like `fetch`: the network call happens
// synchronously inside `ClientRequest.end()`, so an AskBefore policy throws the
// pause sentinel from there and the engine pauses the run. Response events
// (`response`/`data`/`end`) are emitted after the blocking call resolves, on a
// microtask, so listeners registered inside the response callback still fire.
// `createHttpModule` is exported so `node:https` can reuse this implementation
// with an `https:` default protocol.
//
// Composed at first use rather than written as one literal: the shim's
// error handler must recognize the pause sentinel by its marker text (the
// sentinel reaches JS as a plain thrown string — see `runtime::errors`), and
// splicing `PAUSE_MARKER` in keeps `errors.rs` the single home of that
// marker's spelling.
static HTTP_SHIM: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    [
        HTTP_SHIM_HEAD,
        crate::runtime::errors::PAUSE_MARKER,
        HTTP_SHIM_TAIL,
    ]
    .concat()
});

const HTTP_SHIM_HEAD: &str = r#"
class EventEmitter {
    constructor() { this._ev = {}; }
    on(type, cb) { (this._ev[type] = this._ev[type] || []).push(cb); return this; }
    addListener(type, cb) { return this.on(type, cb); }
    once(type, cb) {
        const self = this;
        function wrapper() { self.off(type, wrapper); return cb.apply(this, arguments); }
        return this.on(type, wrapper);
    }
    off(type, cb) {
        if (this._ev[type]) this._ev[type] = this._ev[type].filter((f) => f !== cb);
        return this;
    }
    removeListener(type, cb) { return this.off(type, cb); }
    emit(type) {
        const args = Array.prototype.slice.call(arguments, 1);
        const list = this._ev[type] ? this._ev[type].slice() : [];
        // Node throws when an 'error' event has no listener. Preserving that
        // keeps policy denials / transport failures fail-closed: an agent that
        // ignores errors still sees the run fail rather than silently continue.
        if (list.length === 0 && type === "error") {
            const err = args[0];
            throw err instanceof Error ? err : new Error("Unhandled 'error' event: " + String(err));
        }
        for (const f of list) f.apply(this, args);
        return list.length > 0;
    }
}

class IncomingMessage extends EventEmitter {
    constructor(res) {
        super();
        this.statusCode = res ? res.status : 0;
        this.statusMessage = "";
        this.headers = (res && res.headers) || {};
        this.complete = false;
    }
}

function normalizeBody(body) {
    if (body === undefined || body === null) return undefined;
    if (typeof body === "string") return body;
    return JSON.stringify(body);
}

function createHttpModule(defaultProtocol) {
    class ClientRequest extends EventEmitter {
        constructor(url, options, cb) {
            super();
            this._url = url;
            this._method = String((options && options.method) || "GET").toUpperCase();
            this._headers = {};
            const hdrs = (options && options.headers) || {};
            for (const k of Object.keys(hdrs)) this._headers[String(k).toLowerCase()] = String(hdrs[k]);
            this._chunks = [];
            this._ended = false;
            if (typeof cb === "function") this.on("response", cb);
        }
        setHeader(name, value) { this._headers[String(name).toLowerCase()] = String(value); return this; }
        getHeader(name) { return this._headers[String(name).toLowerCase()]; }
        removeHeader(name) { delete this._headers[String(name).toLowerCase()]; }
        write(chunk) { if (chunk !== undefined && chunk !== null) this._chunks.push(chunk); return true; }
        end(chunk) {
            if (this._ended) return this;
            this._ended = true;
            if (chunk !== undefined && chunk !== null) this._chunks.push(chunk);
            const body = this._chunks.length ? this._chunks.join("") : undefined;
            const options = { method: this._method, headers: this._headers };
            const normalized = normalizeBody(body);
            if (normalized !== undefined) options.body = normalized;
            let res;
            try {
                // Synchronous, policy-gated host call. An AskBefore policy throws
                // the pause sentinel here; we let it propagate so the run pauses.
                res = globalThis.__chidori_http(this._url, options);
            } catch (err) {
                // Surface transport-style failures through the 'error' event, the
                // node convention — but never swallow the pause sentinel, which
                // must keep unwinding to the engine.
                if (err && typeof err.message === "string" && err.message.indexOf(""#;

const HTTP_SHIM_TAIL: &str = r#"") !== -1) {
                    throw err;
                }
                const self = this;
                queueMicrotask(() => self.emit("error", err instanceof Error ? err : new Error(String(err))));
                return this;
            }
            if (res && res.status === 0 && res.error) {
                const self = this;
                queueMicrotask(() => self.emit("error", new Error(res.error)));
                return this;
            }
            const incoming = new IncomingMessage(res);
            this.emit("response", incoming);
            queueMicrotask(() => {
                const b = res ? res.body : null;
                if (b !== undefined && b !== null) {
                    incoming.emit("data", typeof b === "string" ? b : JSON.stringify(b));
                }
                incoming.complete = true;
                incoming.emit("end");
            });
            return this;
        }
        abort() { return this; }
        destroy() { return this; }
        setTimeout() { return this; }
    }

    function buildUrl(input, options) {
        if (typeof input === "string") return input;
        // URL instance.
        if (input && typeof input.href === "string") return input.href;
        // Options object (node style): { protocol, host/hostname, port, path }.
        const opts = input || {};
        const protocol = opts.protocol || defaultProtocol;
        const host = opts.hostname || opts.host || "localhost";
        const port = opts.port ? ":" + opts.port : "";
        const path = opts.path || "/";
        return protocol + "//" + host + port + path;
    }

    // node signatures: request(url[, options][, cb]) | request(options[, cb]).
    function request(input, options, cb) {
        if (typeof options === "function") { cb = options; options = undefined; }
        let opts;
        if (typeof input === "string" || (input && typeof input.href === "string")) {
            opts = options || {};
        } else {
            opts = input || {};
        }
        const url = buildUrl(input, opts);
        return new ClientRequest(url, opts, cb);
    }

    function get(input, options, cb) {
        const req = request(input, options, cb);
        req.end();
        return req;
    }

    function unsupportedServer() {
        throw new Error("node:http server APIs are not supported in the Chidori runtime");
    }

    return {
        request,
        get,
        ClientRequest,
        IncomingMessage,
        createServer: unsupportedServer,
        Server: unsupportedServer,
        Agent: class Agent {},
        globalAgent: {},
        METHODS: ["GET", "POST", "PUT", "DELETE", "PATCH", "HEAD", "OPTIONS"],
        STATUS_CODES: {},
    };
}

const __httpModule = createHttpModule("http:");
export const request = __httpModule.request;
export const get = __httpModule.get;
export const ClientRequest = __httpModule.ClientRequest;
export { IncomingMessage };
export const createServer = __httpModule.createServer;
export const Server = __httpModule.Server;
export const Agent = __httpModule.Agent;
export const globalAgent = __httpModule.globalAgent;
export const METHODS = __httpModule.METHODS;
export const STATUS_CODES = __httpModule.STATUS_CODES;
export { createHttpModule };
export default __httpModule;
"#;

// node:https client shim. Reuses the node:http implementation with an `https:`
// default protocol so policy + pause behavior is identical.
const HTTPS_SHIM: &str = r#"
import { createHttpModule } from "node:http";
const __httpsModule = createHttpModule("https:");
export const request = __httpsModule.request;
export const get = __httpsModule.get;
export const ClientRequest = __httpsModule.ClientRequest;
export const IncomingMessage = __httpsModule.IncomingMessage;
export const createServer = __httpsModule.createServer;
export const Server = __httpsModule.Server;
export const Agent = __httpsModule.Agent;
export const globalAgent = __httpsModule.globalAgent;
export const METHODS = __httpsModule.METHODS;
export const STATUS_CODES = __httpsModule.STATUS_CODES;
export default __httpsModule;
"#;

// node:path shim. A faithful port of Node's `lib/path.js` — both flavours,
// implemented as pure logic: `posix` (forward slash, `:` delimiter) and
// `win32` (backslash *and* forward slash separators, drive letters `C:`,
// drive-relative paths `C:foo`, UNC roots `\\server\share`, device paths
// `\\?\…`, `;` delimiter, case-insensitive comparison in `relative`).
//
// The chidori VFS is posix, so the module's *default* shape stays posix —
// top-level `sep`/`join`/… are the posix table — but `path.win32` is now the
// real win32 module rather than an alias of posix, so code that formats
// Windows paths (and Node's own path test suite, which exercises both
// tables) behaves the way Node does. Both objects carry the `posix`/`win32`
// self-references Node exposes.
//
// Argument validation mirrors Node's: a non-string path throws a TypeError
// carrying `code = "ERR_INVALID_ARG_TYPE"` and Node's message shape, so
// `assert.throws({ code })` checks hold.
const PATH_SHIM: &str = r#"
const CHAR_UPPERCASE_A = 65;
const CHAR_LOWERCASE_A = 97;
const CHAR_UPPERCASE_Z = 90;
const CHAR_LOWERCASE_Z = 122;
const CHAR_DOT = 46;
const CHAR_FORWARD_SLASH = 47;
const CHAR_BACKWARD_SLASH = 92;
const CHAR_COLON = 58;
const CHAR_QUESTION_MARK = 63;

// Node's `determineSpecificType` (lib/internal/errors.js), reduced to the
// cases a path argument can be: it drives the `Received …` tail of the
// ERR_INVALID_ARG_TYPE message.
function determineSpecificType(value) {
    if (value === null || value === undefined) return String(value);
    if (typeof value === "function") {
        return value.name ? `function ${value.name}` : "function";
    }
    if (typeof value === "object") {
        const ctor = value.constructor;
        if (ctor && typeof ctor.name === "string" && ctor.name) {
            return `an instance of ${ctor.name}`;
        }
        return "[Object: null prototype] {}";
    }
    let inspected;
    if (typeof value === "string") inspected = `'${value}'`;
    else if (typeof value === "bigint") inspected = `${value}n`;
    else if (typeof value === "symbol") inspected = value.toString();
    else inspected = String(value);
    if (inspected.length > 8) inspected = `${inspected.slice(0, 8)}...`;
    return `type ${typeof value} (${inspected})`;
}

function invalidArgType(name, expected, actual) {
    const err = new TypeError(
        `The "${name}" argument must be of type ${expected}. Received ${determineSpecificType(actual)}`
    );
    err.code = "ERR_INVALID_ARG_TYPE";
    return err;
}

function validateString(value, name) {
    if (typeof value !== "string") throw invalidArgType(name, "string", value);
}

function validateObject(value, name) {
    if (value === null || Array.isArray(value) || typeof value !== "object") {
        throw invalidArgType(name, "object", value);
    }
}

function isPathSeparator(code) {
    return code === CHAR_FORWARD_SLASH || code === CHAR_BACKWARD_SLASH;
}

function isPosixPathSeparator(code) {
    return code === CHAR_FORWARD_SLASH;
}

function isWindowsDeviceRoot(code) {
    return (code >= CHAR_UPPERCASE_A && code <= CHAR_UPPERCASE_Z) ||
           (code >= CHAR_LOWERCASE_A && code <= CHAR_LOWERCASE_Z);
}

// Resolve "." and ".." segments in `path`. `allowAboveRoot` keeps leading
// ".." segments (relative paths); `separator` is the flavour's separator and
// `isSep` its separator predicate.
function normalizeString(path, allowAboveRoot, separator, isSep) {
    let res = "";
    let lastSegmentLength = 0;
    let lastSlash = -1;
    let dots = 0;
    let code = 0;
    for (let i = 0; i <= path.length; ++i) {
        if (i < path.length) code = path.charCodeAt(i);
        else if (isSep(code)) break;
        else code = CHAR_FORWARD_SLASH;

        if (isSep(code)) {
            if (lastSlash === i - 1 || dots === 1) {
                // NOOP
            } else if (dots === 2) {
                if (res.length < 2 || lastSegmentLength !== 2 ||
                    res.charCodeAt(res.length - 1) !== CHAR_DOT ||
                    res.charCodeAt(res.length - 2) !== CHAR_DOT) {
                    if (res.length > 2) {
                        const lastSlashIndex = res.lastIndexOf(separator);
                        if (lastSlashIndex === -1) {
                            res = "";
                            lastSegmentLength = 0;
                        } else {
                            res = res.slice(0, lastSlashIndex);
                            lastSegmentLength = res.length - 1 - res.lastIndexOf(separator);
                        }
                        lastSlash = i;
                        dots = 0;
                        continue;
                    } else if (res.length !== 0) {
                        res = "";
                        lastSegmentLength = 0;
                        lastSlash = i;
                        dots = 0;
                        continue;
                    }
                }
                if (allowAboveRoot) {
                    res += res.length > 0 ? `${separator}..` : "..";
                    lastSegmentLength = 2;
                }
            } else {
                if (res.length > 0) res += `${separator}${path.slice(lastSlash + 1, i)}`;
                else res = path.slice(lastSlash + 1, i);
                lastSegmentLength = i - lastSlash - 1;
            }
            lastSlash = i;
            dots = 0;
        } else if (code === CHAR_DOT && dots !== -1) {
            ++dots;
        } else {
            dots = -1;
        }
    }
    return res;
}

function formatExt(ext) {
    return ext ? `${ext[0] === "." ? "" : "."}${ext}` : "";
}

function formatWith(sep, pathObject) {
    validateObject(pathObject, "pathObject");
    const dir = pathObject.dir || pathObject.root;
    const base = pathObject.base || `${pathObject.name || ""}${formatExt(pathObject.ext)}`;
    if (!dir) return base;
    return dir === pathObject.root ? `${dir}${base}` : `${dir}${sep}${base}`;
}

const win32 = {
    resolve(...args) {
        let resolvedDevice = "";
        let resolvedTail = "";
        let resolvedAbsolute = false;

        for (let i = args.length - 1; i >= -1; i--) {
            let path;
            if (i >= 0) {
                path = args[i];
                validateString(path, `paths[${i}]`);
                if (path.length === 0) continue;
            } else if (resolvedDevice.length === 0) {
                path = process.cwd();
            } else {
                // Windows keeps a per-drive cwd in `=C:`-style env vars; fall
                // back to the process cwd, and to the drive root when that cwd
                // belongs to another drive.
                path = (process.env && process.env[`=${resolvedDevice}`]) || process.cwd();
                if (path === undefined ||
                    (path.slice(0, 2).toLowerCase() !== resolvedDevice.toLowerCase() &&
                     path.charCodeAt(2) === CHAR_BACKWARD_SLASH)) {
                    path = `${resolvedDevice}\\`;
                }
            }

            const len = path.length;
            let rootEnd = 0;
            let device = "";
            let isAbsolute = false;
            const code = path.charCodeAt(0);

            if (len === 1) {
                if (isPathSeparator(code)) {
                    rootEnd = 1;
                    isAbsolute = true;
                }
            } else if (isPathSeparator(code)) {
                // Possible UNC root; a leading separator is absolute either way.
                isAbsolute = true;
                if (isPathSeparator(path.charCodeAt(1))) {
                    let j = 2;
                    let last = j;
                    while (j < len && !isPathSeparator(path.charCodeAt(j))) j++;
                    if (j < len && j !== last) {
                        const firstPart = path.slice(last, j);
                        last = j;
                        while (j < len && isPathSeparator(path.charCodeAt(j))) j++;
                        if (j < len && j !== last) {
                            last = j;
                            while (j < len && !isPathSeparator(path.charCodeAt(j))) j++;
                            if (j === len || j !== last) {
                                device = `\\\\${firstPart}\\${path.slice(last, j)}`;
                                rootEnd = j;
                            }
                        }
                    }
                } else {
                    rootEnd = 1;
                }
            } else if (isWindowsDeviceRoot(code) && path.charCodeAt(1) === CHAR_COLON) {
                device = path.slice(0, 2);
                rootEnd = 2;
                if (len > 2 && isPathSeparator(path.charCodeAt(2))) {
                    isAbsolute = true;
                    rootEnd = 3;
                }
            }

            if (device.length > 0) {
                if (resolvedDevice.length > 0) {
                    // Another device — this segment cannot contribute.
                    if (device.toLowerCase() !== resolvedDevice.toLowerCase()) continue;
                } else {
                    resolvedDevice = device;
                }
            }

            if (resolvedAbsolute) {
                if (resolvedDevice.length > 0) break;
            } else {
                resolvedTail = `${path.slice(rootEnd)}\\${resolvedTail}`;
                resolvedAbsolute = isAbsolute;
                if (isAbsolute && resolvedDevice.length > 0) break;
            }
        }

        resolvedTail = normalizeString(resolvedTail, !resolvedAbsolute, "\\", isPathSeparator);

        return resolvedAbsolute
            ? `${resolvedDevice}\\${resolvedTail}`
            : `${resolvedDevice}${resolvedTail}` || ".";
    },

    normalize(path) {
        validateString(path, "path");
        const len = path.length;
        if (len === 0) return ".";
        let rootEnd = 0;
        let device;
        let isAbsolute = false;
        const code = path.charCodeAt(0);

        if (len === 1) {
            return isPosixPathSeparator(code) ? "\\" : path;
        }
        if (isPathSeparator(code)) {
            isAbsolute = true;
            if (isPathSeparator(path.charCodeAt(1))) {
                let j = 2;
                let last = j;
                while (j < len && !isPathSeparator(path.charCodeAt(j))) j++;
                if (j < len && j !== last) {
                    const firstPart = path.slice(last, j);
                    last = j;
                    while (j < len && isPathSeparator(path.charCodeAt(j))) j++;
                    if (j < len && j !== last) {
                        last = j;
                        while (j < len && !isPathSeparator(path.charCodeAt(j))) j++;
                        if (j === len) {
                            // A bare UNC root — nothing left to normalize.
                            return `\\\\${firstPart}\\${path.slice(last)}\\`;
                        }
                        if (j !== last) {
                            device = `\\\\${firstPart}\\${path.slice(last, j)}`;
                            rootEnd = j;
                        }
                    }
                }
            } else {
                rootEnd = 1;
            }
        } else if (isWindowsDeviceRoot(code) && path.charCodeAt(1) === CHAR_COLON) {
            device = path.slice(0, 2);
            rootEnd = 2;
            if (len > 2 && isPathSeparator(path.charCodeAt(2))) {
                isAbsolute = true;
                rootEnd = 3;
            }
        }

        let tail = rootEnd < len
            ? normalizeString(path.slice(rootEnd), !isAbsolute, "\\", isPathSeparator)
            : "";
        if (tail.length === 0 && !isAbsolute) tail = ".";
        if (tail.length > 0 && isPathSeparator(path.charCodeAt(len - 1))) tail += "\\";
        if (device === undefined) return isAbsolute ? `\\${tail}` : tail;
        return isAbsolute ? `${device}\\${tail}` : `${device}${tail}`;
    },

    isAbsolute(path) {
        validateString(path, "path");
        const len = path.length;
        if (len === 0) return false;
        const code = path.charCodeAt(0);
        return isPathSeparator(code) ||
            // A drive letter alone (`C:`) is drive-*relative*, not absolute.
            (len > 2 && isWindowsDeviceRoot(code) && path.charCodeAt(1) === CHAR_COLON &&
             isPathSeparator(path.charCodeAt(2)));
    },

    join(...args) {
        if (args.length === 0) return ".";
        let joined;
        let firstPart;
        for (let i = 0; i < args.length; ++i) {
            const arg = args[i];
            validateString(arg, "path");
            if (arg.length > 0) {
                if (joined === undefined) joined = firstPart = arg;
                else joined += `\\${arg}`;
            }
        }
        if (joined === undefined) return ".";

        // Collapse a leading run of separators unless the first non-empty
        // argument clearly names a UNC path (exactly two separators followed
        // by a non-separator), which normalize() would otherwise invent.
        let needsReplace = true;
        let slashCount = 0;
        if (isPathSeparator(firstPart.charCodeAt(0))) {
            ++slashCount;
            const firstLen = firstPart.length;
            if (firstLen > 1 && isPathSeparator(firstPart.charCodeAt(1))) {
                ++slashCount;
                if (firstLen > 2) {
                    if (isPathSeparator(firstPart.charCodeAt(2))) ++slashCount;
                    else needsReplace = false;
                }
            }
        }
        if (needsReplace) {
            while (slashCount < joined.length && isPathSeparator(joined.charCodeAt(slashCount))) {
                slashCount++;
            }
            if (slashCount >= 2) joined = `\\${joined.slice(slashCount)}`;
        }

        return win32.normalize(joined);
    },

    relative(from, to) {
        validateString(from, "from");
        validateString(to, "to");
        if (from === to) return "";

        const fromOrig = win32.resolve(from);
        const toOrig = win32.resolve(to);
        if (fromOrig === toOrig) return "";

        // Windows path comparison is case-insensitive; the *output* still
        // comes from the original-cased resolved paths.
        from = fromOrig.toLowerCase();
        to = toOrig.toLowerCase();
        if (from === to) return "";

        // Lowercasing can change a string's length (`İ` -> `i` + combining
        // dot), which would desynchronize indices into the original-cased
        // strings; fall back to a segment-wise comparison when it does.
        if (fromOrig.length !== from.length || toOrig.length !== to.length) {
            const fromSplit = fromOrig.split("\\");
            const toSplit = toOrig.split("\\");
            if (fromSplit[fromSplit.length - 1] === "") fromSplit.pop();
            if (toSplit[toSplit.length - 1] === "") toSplit.pop();

            const fromCount = fromSplit.length;
            const toCount = toSplit.length;
            const shared = fromCount < toCount ? fromCount : toCount;

            let k;
            for (k = 0; k < shared; k++) {
                if (fromSplit[k].toLowerCase() !== toSplit[k].toLowerCase()) break;
            }

            if (k === 0) return toOrig;
            if (k === shared) {
                if (toCount > shared) return toSplit.slice(k).join("\\");
                if (fromCount > shared) return "..\\".repeat(fromCount - 1 - k) + "..";
                return "";
            }
            return "..\\".repeat(fromCount - k) + toSplit.slice(k).join("\\");
        }

        let fromStart = 0;
        while (fromStart < from.length && from.charCodeAt(fromStart) === CHAR_BACKWARD_SLASH) {
            fromStart++;
        }
        let fromEnd = from.length;
        while (fromEnd - 1 > fromStart && from.charCodeAt(fromEnd - 1) === CHAR_BACKWARD_SLASH) {
            fromEnd--;
        }
        const fromLen = fromEnd - fromStart;

        let toStart = 0;
        while (toStart < to.length && to.charCodeAt(toStart) === CHAR_BACKWARD_SLASH) {
            toStart++;
        }
        let toEnd = to.length;
        while (toEnd - 1 > toStart && to.charCodeAt(toEnd - 1) === CHAR_BACKWARD_SLASH) {
            toEnd--;
        }
        const toLen = toEnd - toStart;

        const length = fromLen < toLen ? fromLen : toLen;
        let lastCommonSep = -1;
        let i = 0;
        for (; i < length; i++) {
            const fromCode = from.charCodeAt(fromStart + i);
            if (fromCode !== to.charCodeAt(toStart + i)) break;
            else if (fromCode === CHAR_BACKWARD_SLASH) lastCommonSep = i;
        }

        if (i !== length) {
            // Different roots (drives / UNC shares) — `to` stands alone.
            if (lastCommonSep === -1) return toOrig;
        } else {
            if (toLen > length) {
                if (to.charCodeAt(toStart + i) === CHAR_BACKWARD_SLASH) {
                    return toOrig.slice(toStart + i + 1);
                }
                if (i === 2) return toOrig.slice(toStart + i);
            }
            if (fromLen > length) {
                if (from.charCodeAt(fromStart + i) === CHAR_BACKWARD_SLASH) lastCommonSep = i;
                else if (i === 2) lastCommonSep = 3;
            }
            if (lastCommonSep === -1) lastCommonSep = 0;
        }

        let out = "";
        for (i = fromStart + lastCommonSep + 1; i <= fromEnd; ++i) {
            if (i === fromEnd || from.charCodeAt(i) === CHAR_BACKWARD_SLASH) {
                out += out.length === 0 ? ".." : "\\..";
            }
        }

        toStart += lastCommonSep;
        if (out.length > 0) return `${out}${toOrig.slice(toStart, toEnd)}`;
        if (toOrig.charCodeAt(toStart) === CHAR_BACKWARD_SLASH) ++toStart;
        return toOrig.slice(toStart, toEnd);
    },

    toNamespacedPath(path) {
        if (typeof path !== "string" || path.length === 0) return path;
        const resolvedPath = win32.resolve(path);
        if (resolvedPath.length <= 2) return path;
        if (resolvedPath.charCodeAt(0) === CHAR_BACKWARD_SLASH) {
            if (resolvedPath.charCodeAt(1) === CHAR_BACKWARD_SLASH) {
                const code = resolvedPath.charCodeAt(2);
                if (code !== CHAR_QUESTION_MARK && code !== CHAR_DOT) {
                    return `\\\\?\\UNC\\${resolvedPath.slice(2)}`;
                }
            }
        } else if (isWindowsDeviceRoot(resolvedPath.charCodeAt(0)) &&
                   resolvedPath.charCodeAt(1) === CHAR_COLON &&
                   resolvedPath.charCodeAt(2) === CHAR_BACKWARD_SLASH) {
            return `\\\\?\\${resolvedPath}`;
        }
        return resolvedPath;
    },

    dirname(path) {
        validateString(path, "path");
        const len = path.length;
        if (len === 0) return ".";
        let rootEnd = -1;
        let offset = 0;
        const code = path.charCodeAt(0);

        if (len === 1) return isPathSeparator(code) ? path : ".";

        if (isPathSeparator(code)) {
            rootEnd = offset = 1;
            if (isPathSeparator(path.charCodeAt(1))) {
                let j = 2;
                let last = j;
                while (j < len && !isPathSeparator(path.charCodeAt(j))) j++;
                if (j < len && j !== last) {
                    last = j;
                    while (j < len && isPathSeparator(path.charCodeAt(j))) j++;
                    if (j < len && j !== last) {
                        last = j;
                        while (j < len && !isPathSeparator(path.charCodeAt(j))) j++;
                        // A bare UNC root is its own dirname.
                        if (j === len) return path;
                        if (j !== last) rootEnd = offset = j + 1;
                    }
                }
            }
        } else if (isWindowsDeviceRoot(code) && path.charCodeAt(1) === CHAR_COLON) {
            rootEnd = len > 2 && isPathSeparator(path.charCodeAt(2)) ? 3 : 2;
            offset = rootEnd;
        }

        let end = -1;
        let matchedSlash = true;
        for (let i = len - 1; i >= offset; --i) {
            if (isPathSeparator(path.charCodeAt(i))) {
                if (!matchedSlash) { end = i; break; }
            } else {
                matchedSlash = false;
            }
        }

        if (end === -1) {
            if (rootEnd === -1) return ".";
            end = rootEnd;
        }
        return path.slice(0, end);
    },

    basename(path, suffix) {
        if (suffix !== undefined) validateString(suffix, "suffix");
        validateString(path, "path");
        let start = 0;
        let end = -1;
        let matchedSlash = true;

        // Skip a drive prefix so `C:\` isn't read as a trailing separator.
        if (path.length >= 2 && isWindowsDeviceRoot(path.charCodeAt(0)) &&
            path.charCodeAt(1) === CHAR_COLON) {
            start = 2;
        }

        if (suffix !== undefined && suffix.length > 0 && suffix.length <= path.length) {
            if (suffix === path) return "";
            let extIdx = suffix.length - 1;
            let firstNonSlashEnd = -1;
            for (let i = path.length - 1; i >= start; --i) {
                const code = path.charCodeAt(i);
                if (isPathSeparator(code)) {
                    if (!matchedSlash) { start = i + 1; break; }
                } else {
                    if (firstNonSlashEnd === -1) {
                        matchedSlash = false;
                        firstNonSlashEnd = i + 1;
                    }
                    if (extIdx >= 0) {
                        if (code === suffix.charCodeAt(extIdx)) {
                            if (--extIdx === -1) end = i;
                        } else {
                            extIdx = -1;
                            end = firstNonSlashEnd;
                        }
                    }
                }
            }
            if (start === end) end = firstNonSlashEnd;
            else if (end === -1) end = path.length;
            return path.slice(start, end);
        }
        for (let i = path.length - 1; i >= start; --i) {
            if (isPathSeparator(path.charCodeAt(i))) {
                if (!matchedSlash) { start = i + 1; break; }
            } else if (end === -1) {
                matchedSlash = false;
                end = i + 1;
            }
        }
        if (end === -1) return "";
        return path.slice(start, end);
    },

    extname(path) {
        validateString(path, "path");
        let start = 0;
        let startDot = -1;
        let startPart = 0;
        let end = -1;
        let matchedSlash = true;
        let preDotState = 0;

        if (path.length >= 2 && path.charCodeAt(1) === CHAR_COLON &&
            isWindowsDeviceRoot(path.charCodeAt(0))) {
            start = startPart = 2;
        }

        for (let i = path.length - 1; i >= start; --i) {
            const code = path.charCodeAt(i);
            if (isPathSeparator(code)) {
                if (!matchedSlash) { startPart = i + 1; break; }
                continue;
            }
            if (end === -1) { matchedSlash = false; end = i + 1; }
            if (code === CHAR_DOT) {
                if (startDot === -1) startDot = i;
                else if (preDotState !== 1) preDotState = 1;
            } else if (startDot !== -1) {
                preDotState = -1;
            }
        }

        if (startDot === -1 || end === -1 || preDotState === 0 ||
            (preDotState === 1 && startDot === end - 1 && startDot === startPart + 1)) {
            return "";
        }
        return path.slice(startDot, end);
    },

    format(pathObject) {
        return formatWith("\\", pathObject);
    },

    parse(path) {
        validateString(path, "path");
        const ret = { root: "", dir: "", base: "", ext: "", name: "" };
        if (path.length === 0) return ret;

        const len = path.length;
        let rootEnd = 0;
        let code = path.charCodeAt(0);

        if (len === 1) {
            if (isPathSeparator(code)) {
                ret.root = ret.dir = path;
                return ret;
            }
            ret.base = ret.name = path;
            return ret;
        }
        if (isPathSeparator(code)) {
            rootEnd = 1;
            if (isPathSeparator(path.charCodeAt(1))) {
                let j = 2;
                let last = j;
                while (j < len && !isPathSeparator(path.charCodeAt(j))) j++;
                if (j < len && j !== last) {
                    last = j;
                    while (j < len && isPathSeparator(path.charCodeAt(j))) j++;
                    if (j < len && j !== last) {
                        last = j;
                        while (j < len && !isPathSeparator(path.charCodeAt(j))) j++;
                        if (j === len) rootEnd = j;
                        else if (j !== last) rootEnd = j + 1;
                    }
                }
            }
        } else if (isWindowsDeviceRoot(code) && path.charCodeAt(1) === CHAR_COLON) {
            if (len <= 2) {
                ret.root = ret.dir = path;
                return ret;
            }
            rootEnd = 2;
            if (isPathSeparator(path.charCodeAt(2))) {
                if (len === 3) {
                    ret.root = ret.dir = path;
                    return ret;
                }
                rootEnd = 3;
            }
        }
        if (rootEnd > 0) ret.root = path.slice(0, rootEnd);

        let startDot = -1;
        let startPart = rootEnd;
        let end = -1;
        let matchedSlash = true;
        let i = path.length - 1;
        let preDotState = 0;

        for (; i >= rootEnd; --i) {
            code = path.charCodeAt(i);
            if (isPathSeparator(code)) {
                if (!matchedSlash) { startPart = i + 1; break; }
                continue;
            }
            if (end === -1) { matchedSlash = false; end = i + 1; }
            if (code === CHAR_DOT) {
                if (startDot === -1) startDot = i;
                else if (preDotState !== 1) preDotState = 1;
            } else if (startDot !== -1) {
                preDotState = -1;
            }
        }

        if (end !== -1) {
            if (startDot === -1 || preDotState === 0 ||
                (preDotState === 1 && startDot === end - 1 && startDot === startPart + 1)) {
                ret.base = ret.name = path.slice(startPart, end);
            } else {
                ret.name = path.slice(startPart, startDot);
                ret.base = path.slice(startPart, end);
                ret.ext = path.slice(startDot, end);
            }
        }

        // `C:\abc` keeps the trailing separator in `dir`; `C:\abc\def` drops it.
        if (startPart > 0 && startPart !== rootEnd) ret.dir = path.slice(0, startPart - 1);
        else ret.dir = ret.root;

        return ret;
    },

    sep: "\\",
    delimiter: ";",
    win32: null,
    posix: null,
};

const posix = {
    resolve(...args) {
        let resolvedPath = "";
        let resolvedAbsolute = false;

        for (let i = args.length - 1; i >= 0 && !resolvedAbsolute; i--) {
            const path = args[i];
            validateString(path, `paths[${i}]`);
            if (path.length === 0) continue;
            resolvedPath = `${path}/${resolvedPath}`;
            resolvedAbsolute = path.charCodeAt(0) === CHAR_FORWARD_SLASH;
        }

        if (!resolvedAbsolute) {
            const cwd = process.cwd();
            resolvedPath = `${cwd}/${resolvedPath}`;
            resolvedAbsolute = cwd.charCodeAt(0) === CHAR_FORWARD_SLASH;
        }

        const normalized = normalizeString(resolvedPath, !resolvedAbsolute, "/", isPosixPathSeparator);
        if (resolvedAbsolute) return `/${normalized}`;
        return normalized.length > 0 ? normalized : ".";
    },

    normalize(path) {
        validateString(path, "path");
        if (path.length === 0) return ".";
        const isAbsolute = path.charCodeAt(0) === CHAR_FORWARD_SLASH;
        const trailingSeparator = path.charCodeAt(path.length - 1) === CHAR_FORWARD_SLASH;

        path = normalizeString(path, !isAbsolute, "/", isPosixPathSeparator);

        if (path.length === 0) {
            if (isAbsolute) return "/";
            return trailingSeparator ? "./" : ".";
        }
        if (trailingSeparator) path += "/";
        return isAbsolute ? `/${path}` : path;
    },

    isAbsolute(path) {
        validateString(path, "path");
        return path.length > 0 && path.charCodeAt(0) === CHAR_FORWARD_SLASH;
    },

    join(...args) {
        if (args.length === 0) return ".";
        let joined;
        for (let i = 0; i < args.length; ++i) {
            const arg = args[i];
            validateString(arg, "path");
            if (arg.length > 0) {
                if (joined === undefined) joined = arg;
                else joined += `/${arg}`;
            }
        }
        if (joined === undefined) return ".";
        return posix.normalize(joined);
    },

    relative(from, to) {
        validateString(from, "from");
        validateString(to, "to");
        if (from === to) return "";

        from = posix.resolve(from);
        to = posix.resolve(to);
        if (from === to) return "";

        const fromStart = 1;
        const fromEnd = from.length;
        const fromLen = fromEnd - fromStart;
        const toStart = 1;
        const toLen = to.length - toStart;

        const length = fromLen < toLen ? fromLen : toLen;
        let lastCommonSep = -1;
        let i = 0;
        for (; i < length; i++) {
            const fromCode = from.charCodeAt(fromStart + i);
            if (fromCode !== to.charCodeAt(toStart + i)) break;
            else if (fromCode === CHAR_FORWARD_SLASH) lastCommonSep = i;
        }
        if (i === length) {
            if (toLen > length) {
                if (to.charCodeAt(toStart + i) === CHAR_FORWARD_SLASH) {
                    // `from` is the exact parent of `to`.
                    return to.slice(toStart + i + 1);
                }
                if (i === 0) return to.slice(toStart + i);
            } else if (fromLen > length) {
                if (from.charCodeAt(fromStart + i) === CHAR_FORWARD_SLASH) lastCommonSep = i;
                else if (i === 0) lastCommonSep = 0;
            }
        }

        let out = "";
        for (i = fromStart + lastCommonSep + 1; i <= fromEnd; ++i) {
            if (i === fromEnd || from.charCodeAt(i) === CHAR_FORWARD_SLASH) {
                out += out.length === 0 ? ".." : "/..";
            }
        }

        return `${out}${to.slice(toStart + lastCommonSep)}`;
    },

    toNamespacedPath(path) {
        // No-op on posix.
        return path;
    },

    dirname(path) {
        validateString(path, "path");
        if (path.length === 0) return ".";
        const hasRoot = path.charCodeAt(0) === CHAR_FORWARD_SLASH;
        let end = -1;
        let matchedSlash = true;
        for (let i = path.length - 1; i >= 1; --i) {
            if (path.charCodeAt(i) === CHAR_FORWARD_SLASH) {
                if (!matchedSlash) { end = i; break; }
            } else {
                matchedSlash = false;
            }
        }
        if (end === -1) return hasRoot ? "/" : ".";
        if (hasRoot && end === 1) return "//";
        return path.slice(0, end);
    },

    basename(path, suffix) {
        if (suffix !== undefined) validateString(suffix, "ext");
        validateString(path, "path");
        let start = 0;
        let end = -1;
        let matchedSlash = true;

        if (suffix !== undefined && suffix.length > 0 && suffix.length <= path.length) {
            if (suffix === path) return "";
            let extIdx = suffix.length - 1;
            let firstNonSlashEnd = -1;
            for (let i = path.length - 1; i >= 0; --i) {
                const code = path.charCodeAt(i);
                if (code === CHAR_FORWARD_SLASH) {
                    if (!matchedSlash) { start = i + 1; break; }
                } else {
                    if (firstNonSlashEnd === -1) {
                        matchedSlash = false;
                        firstNonSlashEnd = i + 1;
                    }
                    if (extIdx >= 0) {
                        if (code === suffix.charCodeAt(extIdx)) {
                            if (--extIdx === -1) end = i;
                        } else {
                            extIdx = -1;
                            end = firstNonSlashEnd;
                        }
                    }
                }
            }
            if (start === end) end = firstNonSlashEnd;
            else if (end === -1) end = path.length;
            return path.slice(start, end);
        }
        for (let i = path.length - 1; i >= 0; --i) {
            if (path.charCodeAt(i) === CHAR_FORWARD_SLASH) {
                if (!matchedSlash) { start = i + 1; break; }
            } else if (end === -1) {
                matchedSlash = false;
                end = i + 1;
            }
        }
        if (end === -1) return "";
        return path.slice(start, end);
    },

    extname(path) {
        validateString(path, "path");
        let startDot = -1;
        let startPart = 0;
        let end = -1;
        let matchedSlash = true;
        let preDotState = 0;
        for (let i = path.length - 1; i >= 0; --i) {
            const code = path.charCodeAt(i);
            if (code === CHAR_FORWARD_SLASH) {
                if (!matchedSlash) { startPart = i + 1; break; }
                continue;
            }
            if (end === -1) { matchedSlash = false; end = i + 1; }
            if (code === CHAR_DOT) {
                if (startDot === -1) startDot = i;
                else if (preDotState !== 1) preDotState = 1;
            } else if (startDot !== -1) {
                preDotState = -1;
            }
        }

        if (startDot === -1 || end === -1 || preDotState === 0 ||
            (preDotState === 1 && startDot === end - 1 && startDot === startPart + 1)) {
            return "";
        }
        return path.slice(startDot, end);
    },

    format(pathObject) {
        return formatWith("/", pathObject);
    },

    parse(path) {
        validateString(path, "path");
        const ret = { root: "", dir: "", base: "", ext: "", name: "" };
        if (path.length === 0) return ret;
        const isAbsolute = path.charCodeAt(0) === CHAR_FORWARD_SLASH;
        let start;
        if (isAbsolute) {
            ret.root = "/";
            start = 1;
        } else {
            start = 0;
        }
        let startDot = -1;
        let startPart = 0;
        let end = -1;
        let matchedSlash = true;
        let i = path.length - 1;
        let preDotState = 0;

        for (; i >= start; --i) {
            const code = path.charCodeAt(i);
            if (code === CHAR_FORWARD_SLASH) {
                if (!matchedSlash) { startPart = i + 1; break; }
                continue;
            }
            if (end === -1) { matchedSlash = false; end = i + 1; }
            if (code === CHAR_DOT) {
                if (startDot === -1) startDot = i;
                else if (preDotState !== 1) preDotState = 1;
            } else if (startDot !== -1) {
                preDotState = -1;
            }
        }

        if (end !== -1) {
            const baseStart = startPart === 0 && isAbsolute ? 1 : startPart;
            if (startDot === -1 || preDotState === 0 ||
                (preDotState === 1 && startDot === end - 1 && startDot === startPart + 1)) {
                ret.base = ret.name = path.slice(baseStart, end);
            } else {
                ret.name = path.slice(baseStart, startDot);
                ret.base = path.slice(baseStart, end);
                ret.ext = path.slice(startDot, end);
            }
        }

        if (startPart > 0) ret.dir = path.slice(0, startPart - 1);
        else if (isAbsolute) ret.dir = "/";

        return ret;
    },

    sep: "/",
    delimiter: ":",
    win32: null,
    posix: null,
};

// Both flavours expose both tables (and themselves), the way Node does.
win32.win32 = win32;
win32.posix = posix;
posix.win32 = win32;
posix.posix = posix;

// Legacy internal alias, docs-only deprecated in Node as DEP0080.
win32._makeLong = win32.toNamespacedPath;
posix._makeLong = posix.toNamespacedPath;

// The chidori VFS is posix, so the module's own surface is the posix table.
const sep = posix.sep;
const delimiter = posix.delimiter;
const normalize = posix.normalize;
const isAbsolute = posix.isAbsolute;
const join = posix.join;
const resolve = posix.resolve;
const relative = posix.relative;
const toNamespacedPath = posix.toNamespacedPath;
const dirname = posix.dirname;
const basename = posix.basename;
const extname = posix.extname;
const format = posix.format;
const parse = posix.parse;

export {
    sep, delimiter, normalize, isAbsolute, join, resolve, relative,
    toNamespacedPath, dirname, basename, extname, format, parse, posix, win32,
};
export default posix;
"#;

// node:path/posix re-exports node:path (which is already posix-style). Named
// re-exports are spelled out because the bundler does not support `export *`.
const PATH_POSIX_SHIM: &str = r#"
import path from "node:path";
export { sep, delimiter, normalize, isAbsolute, join, resolve, relative, toNamespacedPath, dirname, basename, extname, format, parse, posix, win32 } from "node:path";
export default path;
"#;

// node:events shim. Matches Node's shape, not just its surface: EventEmitter
// is an ES5-style constructor *function* with lazy `_events` initialization,
// because the classic inheritance idiom — `EventEmitter.call(this)` /
// `Stream.call(this)` + `Object.setPrototypeOf` — is everywhere in older npm
// packages (and throughout Node's own test suite) and a `class` constructor
// cannot be `.call()`ed. Validation errors carry Node's error codes
// (ERR_INVALID_ARG_TYPE / ERR_OUT_OF_RANGE / ERR_UNHANDLED_ERROR) so
// `assert.throws({ code })` checks hold.
const EVENTS_SHIM: &str = r#"
import { inspect } from "node:util";

// Node keeps `_events[type]` as the listener *itself* when there is exactly
// one, and only promotes to an array on the second registration. Packages and
// Node's own test suite observe that shape directly (`ee._events.foo === fn`),
// so the storage mirrors it rather than always using arrays.
const kCapture = Symbol("kCapture");
const kShapeMode = Symbol("shapeMode");
let defaultMaxListeners = 10;

function argTypeSuffix(actual) {
    if (actual === null) return "null";
    if (typeof actual === "object") {
        const ctor = actual.constructor;
        return "an instance of " + (ctor && ctor.name ? ctor.name : "Object");
    }
    if (typeof actual === "string") return "type string ('" + actual + "')";
    return "type " + typeof actual + " (" + String(actual) + ")";
}
function invalidArgType(name, expected, actual) {
    const err = new TypeError(
        'The "' + name + '" argument must be of type ' + expected +
        ". Received " + argTypeSuffix(actual)
    );
    err.code = "ERR_INVALID_ARG_TYPE";
    return err;
}
function outOfRange(name, range, actual) {
    const err = new RangeError(
        'The value of "' + name + '" is out of range. It must be ' + range +
        ". Received " + String(actual)
    );
    err.code = "ERR_OUT_OF_RANGE";
    return err;
}
// Node's internal validateNumber(value, name, min): a non-number is a type
// error, NaN and out-of-band values are range errors.
function validateNumber(value, name, min) {
    if (typeof value !== "number") throw invalidArgType(name, "number", value);
    if (value < min || Number.isNaN(value)) throw outOfRange(name, ">= " + min, value);
}
function checkListener(listener) {
    if (typeof listener !== "function") {
        throw invalidArgType("listener", "function", listener);
    }
}
function ownKeys(target) {
    if (typeof Reflect !== "undefined" && typeof Reflect.ownKeys === "function") {
        return Reflect.ownKeys(target);
    }
    return Object.getOwnPropertyNames(target).concat(Object.getOwnPropertySymbols(target));
}
function arrayClone(arr) {
    const copy = new Array(arr.length);
    for (let i = 0; i < arr.length; i++) copy[i] = arr[i];
    return copy;
}
function spliceOne(list, index) {
    for (; index + 1 < list.length; index++) list[index] = list[index + 1];
    list.pop();
}

function EventEmitter(opts) {
    EventEmitter.init.call(this, opts);
}
EventEmitter.EventEmitter = EventEmitter;
EventEmitter.errorMonitor = Symbol("events.errorMonitor");
EventEmitter.captureRejectionSymbol = Symbol.for("nodejs.rejection");
Object.defineProperty(EventEmitter, "defaultMaxListeners", {
    enumerable: true,
    configurable: true,
    get() { return defaultMaxListeners; },
    set(arg) {
        validateNumber(arg, "defaultMaxListeners", 0);
        defaultMaxListeners = arg;
    },
});
Object.defineProperty(EventEmitter, "captureRejections", {
    enumerable: true,
    configurable: true,
    get() { return EventEmitter.prototype[kCapture]; },
    set(value) {
        if (typeof value !== "boolean") {
            throw invalidArgType("options.captureRejections", "boolean", value);
        }
        EventEmitter.prototype[kCapture] = value;
    },
});
EventEmitter.prototype._events = undefined;
EventEmitter.prototype._eventsCount = 0;
EventEmitter.prototype._maxListeners = undefined;
EventEmitter.prototype[kCapture] = false;

// `_events` is only reset when it is genuinely this instance's own — the
// prototype comparison is what makes `MyEE.prototype = new EventEmitter()`
// (an old but still common inheritance idiom) give each instance its own
// listener map instead of sharing the prototype's.
EventEmitter.init = function init(opts) {
    const proto = Object.getPrototypeOf(this);
    if (this._events === undefined ||
        (proto !== null && this._events === proto._events)) {
        this._events = Object.create(null);
        this._eventsCount = 0;
        this[kShapeMode] = false;
    } else {
        this[kShapeMode] = true;
    }
    this._maxListeners = this._maxListeners || undefined;
    if (opts && opts.captureRejections) {
        if (typeof opts.captureRejections !== "boolean") {
            throw invalidArgType("options.captureRejections", "boolean", opts.captureRejections);
        }
        this[kCapture] = Boolean(opts.captureRejections);
    } else {
        this[kCapture] = EventEmitter.prototype[kCapture];
    }
};

// `events.setMaxListeners(n[, ...emitters])` — the module-level form.
EventEmitter.setMaxListeners = function setMaxListeners(n, ...eventTargets) {
    if (n === undefined) n = defaultMaxListeners;
    validateNumber(n, "setMaxListeners", 0);
    if (eventTargets.length === 0) {
        defaultMaxListeners = n;
        return;
    }
    for (const target of eventTargets) {
        if (target && typeof target.setMaxListeners === "function") {
            target.setMaxListeners(n);
        } else {
            throw invalidArgType("eventTargets", "EventEmitter or EventTarget", target);
        }
    }
};

EventEmitter.prototype.setMaxListeners = function setMaxListeners(n) {
    validateNumber(n, "setMaxListeners", 0);
    this._maxListeners = n;
    return this;
};
function _getMaxListeners(that) {
    return that._maxListeners === undefined ? defaultMaxListeners : that._maxListeners;
}
EventEmitter.prototype.getMaxListeners = function getMaxListeners() {
    return _getMaxListeners(this);
};

function addCatch(that, promise, type, args) {
    if (!that[kCapture]) return;
    try {
        const then = promise.then;
        if (typeof then === "function") {
            then.call(promise, undefined, function (err) {
                emitUnhandledRejectionOrErr(that, err, type, args);
            });
        }
    } catch (err) {
        that.emit("error", err);
    }
}
function emitUnhandledRejectionOrErr(that, err, type, args) {
    if (typeof that[EventEmitter.captureRejectionSymbol] === "function") {
        that[EventEmitter.captureRejectionSymbol](err, type, ...args);
    } else {
        that.emit("error", err);
    }
}

function _addListener(target, type, listener, prepend) {
    checkListener(listener);
    let events = target._events;
    let existing;
    if (events === undefined) {
        events = target._events = Object.create(null);
        target._eventsCount = 0;
    } else {
        // 'newListener' is emitted *before* the listener is stored, so a
        // handler observing `listeners(type)` sees the pre-insertion state
        // (and a handler that registers its own listener lands ahead of it).
        if (events.newListener !== undefined) {
            target.emit("newListener", type, listener.listener || listener);
            // A newListener handler may have replaced the whole map.
            events = target._events;
        }
        existing = events[type];
    }

    if (existing === undefined) {
        events[type] = listener;
        ++target._eventsCount;
    } else {
        if (typeof existing === "function") {
            existing = events[type] = prepend ? [listener, existing] : [existing, listener];
        } else if (prepend) {
            existing.unshift(listener);
        } else {
            existing.push(listener);
        }
        const m = _getMaxListeners(target);
        if (m > 0 && existing.length > m && !existing.warned) {
            existing.warned = true;
            const w = new Error(
                "Possible EventEmitter memory leak detected. " + existing.length + " " +
                String(type) + " listeners added. Use emitter.setMaxListeners() to increase limit"
            );
            w.name = "MaxListenersExceededWarning";
            w.emitter = target;
            w.type = type;
            w.count = existing.length;
            if (globalThis.process && typeof globalThis.process.emitWarning === "function") {
                globalThis.process.emitWarning(w);
            }
        }
    }
    return target;
}

EventEmitter.prototype.addListener = function addListener(type, listener) {
    return _addListener(this, type, listener, false);
};
// Node aliases the two; `assert.strictEqual(ee.addListener, ee.on)` holds.
EventEmitter.prototype.on = EventEmitter.prototype.addListener;
EventEmitter.prototype.prependListener = function prependListener(type, listener) {
    return _addListener(this, type, listener, true);
};

function onceWrapper() {
    if (!this.fired) {
        this.target.removeListener(this.type, this.wrapFn);
        this.fired = true;
        return this.listener.apply(this.target, arguments);
    }
}
function _onceWrap(target, type, listener) {
    const state = { fired: false, wrapFn: undefined, target, type, listener };
    const wrapped = onceWrapper.bind(state);
    wrapped.listener = listener;
    state.wrapFn = wrapped;
    return wrapped;
}
EventEmitter.prototype.once = function once(type, listener) {
    checkListener(listener);
    this.on(type, _onceWrap(this, type, listener));
    return this;
};
EventEmitter.prototype.prependOnceListener = function prependOnceListener(type, listener) {
    checkListener(listener);
    this.prependListener(type, _onceWrap(this, type, listener));
    return this;
};

EventEmitter.prototype.removeListener = function removeListener(type, listener) {
    checkListener(listener);
    const events = this._events;
    if (events === undefined) return this;
    const list = events[type];
    if (list === undefined) return this;

    if (list === listener || list.listener === listener) {
        this._eventsCount -= 1;
        if (this[kShapeMode]) {
            events[type] = undefined;
        } else if (this._eventsCount === 0) {
            this._events = Object.create(null);
        } else {
            delete events[type];
            if (events.removeListener) {
                this.emit("removeListener", type, list.listener || listener);
            }
        }
    } else if (typeof list !== "function") {
        let position = -1;
        for (let i = list.length - 1; i >= 0; i--) {
            if (list[i] === listener || list[i].listener === listener) {
                position = i;
                break;
            }
        }
        if (position < 0) return this;
        if (position === 0) list.shift();
        else spliceOne(list, position);
        if (list.length === 1) events[type] = list[0];
        if (events.removeListener !== undefined) {
            this.emit("removeListener", type, listener);
        }
    }
    return this;
};
EventEmitter.prototype.off = EventEmitter.prototype.removeListener;

EventEmitter.prototype.removeAllListeners = function removeAllListeners(type) {
    const events = this._events;
    if (events === undefined) return this;

    // Nobody is listening for 'removeListener', so the whole map can go.
    if (events.removeListener === undefined) {
        if (arguments.length === 0) {
            this._events = Object.create(null);
            this._eventsCount = 0;
        } else if (events[type] !== undefined) {
            if (--this._eventsCount === 0) this._events = Object.create(null);
            else delete events[type];
        }
        this[kShapeMode] = false;
        return this;
    }

    if (arguments.length === 0) {
        for (const key of ownKeys(events)) {
            if (key === "removeListener") continue;
            this.removeAllListeners(key);
        }
        this.removeAllListeners("removeListener");
        this._events = Object.create(null);
        this._eventsCount = 0;
        this[kShapeMode] = false;
        return this;
    }

    const listeners = events[type];
    if (typeof listeners === "function") {
        this.removeListener(type, listeners);
    } else if (listeners !== undefined) {
        // LIFO order, one removeListener emission per listener.
        for (let i = listeners.length - 1; i >= 0; i--) {
            this.removeListener(type, listeners[i]);
        }
    }
    return this;
};

EventEmitter.prototype.emit = function emit(type, ...args) {
    let doError = type === "error";
    const events = this._events;
    if (events !== undefined) {
        if (doError && events[EventEmitter.errorMonitor] !== undefined) {
            this.emit(EventEmitter.errorMonitor, ...args);
        }
        doError = doError && events.error === undefined;
    } else if (!doError) {
        return false;
    }

    if (doError) {
        const er = args.length > 0 ? args[0] : undefined;
        if (er instanceof Error) throw er;
        // Node stringifies the context with `inspect` (so a string argument
        // shows up quoted) and falls back to plain coercion when a custom
        // inspect implementation throws.
        let stringifiedEr;
        try { stringifiedEr = inspect(er); } catch { stringifiedEr = er; }
        const err = new Error(
            stringifiedEr === undefined ? "Unhandled error." : "Unhandled error. (" + stringifiedEr + ")"
        );
        err.code = "ERR_UNHANDLED_ERROR";
        err.context = er;
        throw err;
    }

    const handler = events[type];
    if (handler === undefined) return false;

    if (typeof handler === "function") {
        const result = handler.apply(this, args);
        if (result !== undefined && result !== null) addCatch(this, result, type, args);
    } else {
        // Emission runs against a snapshot: a listener removed mid-emit still
        // runs for the in-flight emit, exactly like Node.
        const listeners = arrayClone(handler);
        for (let i = 0; i < listeners.length; ++i) {
            const result = listeners[i].apply(this, args);
            if (result !== undefined && result !== null) addCatch(this, result, type, args);
        }
    }
    return true;
};

function _listeners(target, type, unwrap) {
    const events = target._events;
    if (events === undefined) return [];
    const evlistener = events[type];
    if (evlistener === undefined) return [];
    if (typeof evlistener === "function") {
        return unwrap ? [evlistener.listener || evlistener] : [evlistener];
    }
    const ret = arrayClone(evlistener);
    if (unwrap) {
        for (let i = 0; i < ret.length; ++i) {
            const orig = ret[i].listener;
            if (typeof orig === "function") ret[i] = orig;
        }
    }
    return ret;
}
EventEmitter.prototype.listeners = function listeners(type) {
    return _listeners(this, type, true);
};
EventEmitter.prototype.rawListeners = function rawListeners(type) {
    return _listeners(this, type, false);
};
EventEmitter.prototype.listenerCount = function listenerCount(type, listener) {
    const events = this._events;
    if (events !== undefined) {
        const evlistener = events[type];
        if (typeof evlistener === "function") {
            if (listener !== undefined && listener !== null) {
                return listener === evlistener || listener === evlistener.listener ? 1 : 0;
            }
            return 1;
        } else if (evlistener !== undefined) {
            if (listener !== undefined && listener !== null) {
                let matching = 0;
                for (let i = 0; i < evlistener.length; i++) {
                    if (evlistener[i] === listener || evlistener[i].listener === listener) matching++;
                }
                return matching;
            }
            return evlistener.length;
        }
    }
    return 0;
};
EventEmitter.prototype.eventNames = function eventNames() {
    return this._eventsCount > 0 ? ownKeys(this._events) : [];
};
// Legacy static form, still exercised by Node's suite and old packages.
EventEmitter.listenerCount = function (emitter, type) {
    return emitter.listenerCount(type);
};

function once(emitter, name) {
    return new Promise((resolve, reject) => {
        function onEvent(...args) { cleanup(); resolve(args); }
        function onError(err) { cleanup(); reject(err); }
        function cleanup() { emitter.off(name, onEvent); emitter.off("error", onError); }
        emitter.once(name, onEvent);
        if (name !== "error") emitter.once("error", onError);
    });
}
function getEventListeners(emitter, name) {
    return emitter.rawListeners(name);
}
function getMaxListeners(emitter) {
    return emitter.getMaxListeners();
}
const setMaxListeners = EventEmitter.setMaxListeners;
const errorMonitor = EventEmitter.errorMonitor;
const captureRejectionSymbol = EventEmitter.captureRejectionSymbol;

export { EventEmitter, once, getEventListeners, getMaxListeners, setMaxListeners, errorMonitor, captureRejectionSymbol };
export default EventEmitter;
"#;

// node:url shim. The chidori engine does not install WHATWG `URL`/
// `URLSearchParams` globals, so this provides a conformant subset implemented in
// pure JS: parsing, the standard component accessors, searchParams manipulation,
// `toString`, and relative-base resolution via `new URL(input, base)`.
//
// The legacy half (`Url`, `parse`, `format`, `resolve`, `resolveObject`) is a
// port of Node's own `lib/url.js` rather than an approximation: it is still
// what `url.parse()` returns, and its quirks (backslash rewriting, the
// auth-vs-host disambiguation, "unsafe" protocols, RFC 3986 relative merging)
// are load-bearing for the packages that use it. `pathToFileURL` /
// `fileURLToPath` mirror `internal/url.js`, including its Node error codes.
// (Uses `r##` delimiters because the body contains `"#`.)
const URL_SHIM: &str = r##"
import { toASCII, toUnicode } from "node:punycode";
import { parse as qsParse, stringify as qsStringify } from "node:querystring";

const SPECIAL_PORTS = { "http:": "80", "https:": "443", "ws:": "80", "wss:": "443", "ftp:": "21" };

// Errors carry Node's codes: callers (and Node's own test suite) branch on
// `err.code`, not on the message.
function describeValue(value) {
    if (value === null) return "null";
    if (typeof value === "object") {
        const ctor = value.constructor;
        return "an instance of " + ((ctor && ctor.name) || "Object");
    }
    return "type " + typeof value;
}
function codedTypeError(code, message) {
    const err = new TypeError(message);
    err.code = code;
    return err;
}
function invalidArgType(name, expected, actual) {
    const kinds = Array.isArray(expected) ? expected.join(" or ") : expected;
    return codedTypeError("ERR_INVALID_ARG_TYPE",
        `The "${name}" argument must be of type ${kinds}. Received ${describeValue(actual)}`);
}
function invalidArgValue(name, value, reason) {
    return codedTypeError("ERR_INVALID_ARG_VALUE",
        `The argument '${name}' ${reason}. Received ${String(value)}`);
}
function invalidUrl(input) {
    const err = codedTypeError("ERR_INVALID_URL", "Invalid URL");
    err.input = String(input);
    return err;
}
function invalidUrlScheme(expected) {
    return codedTypeError("ERR_INVALID_URL_SCHEME", `The URL must be of scheme ${expected}`);
}
function invalidFileUrlHost(platform) {
    return codedTypeError("ERR_INVALID_FILE_URL_HOST",
        `File URL host must be "localhost" or empty on ${platform}`);
}
function invalidFileUrlPath(reason) {
    return codedTypeError("ERR_INVALID_FILE_URL_PATH", `File URL path ${reason}`);
}
function validateString(value, name) {
    if (typeof value !== "string") throw invalidArgType(name, "string", value);
}

// ---------------------------------------------------------------------------
// Percent-encoding. `percentEncode` walks code points (surrogate pairs
// included) and rewrites everything its `safe` table rejects as UTF-8 %XX
// triplets. It is the primitive behind pathname escaping, `pathToFileURL`, and
// legacy auth escaping — each of which differs only in its table.
// ---------------------------------------------------------------------------
const hexTable = [];
for (let i = 0; i < 256; i++) {
    hexTable.push("%" + (i < 16 ? "0" : "") + i.toString(16).toUpperCase());
}

function safeTable(chars) {
    const table = [];
    for (let i = 0; i < 128; i++) table.push(false);
    for (let i = 0; i < chars.length; i++) table[chars.charCodeAt(i)] = true;
    return table;
}
const ALNUM = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
// WHATWG "path percent-encode set", complemented. `%` is safe so an already
// escaped pathname survives a round trip unchanged.
const PATH_SAFE = safeTable(ALNUM + "!$%&'()*+,-./:;=@[\\]^_|~");
// What `pathToFileURL` leaves unescaped: unreserved ASCII plus sub-delimiters
// (`%` again excluded from escaping — `encodePathChars` handled it already).
const FILE_PATH_SAFE = safeTable(ALNUM + "!$%&'()*+,-./:;=@_");
// RFC 3986 userinfo: unreserved + sub-delimiters + ":". Used by legacy format().
const AUTH_SAFE = safeTable(ALNUM + "!$&'()*+,-.:;=_~");

function percentEncode(str, safe) {
    const parts = [];
    let lastPos = 0;
    for (let i = 0; i < str.length; i++) {
        const c = str.charCodeAt(i);
        if (c < 0x80) {
            if (safe[c]) continue;
            if (lastPos < i) parts.push(str.slice(lastPos, i));
            parts.push(hexTable[c]);
            lastPos = i + 1;
            continue;
        }
        if (lastPos < i) parts.push(str.slice(lastPos, i));
        let cp = c;
        if (c >= 0xd800 && c < 0xdc00 && i + 1 < str.length) {
            const low = str.charCodeAt(i + 1);
            if (low >= 0xdc00 && low < 0xe000) {
                cp = 0x10000 + (((c & 0x3ff) << 10) | (low & 0x3ff));
                i += 1;
            }
        }
        if (cp < 0x800) {
            parts.push(hexTable[0xc0 | (cp >> 6)], hexTable[0x80 | (cp & 0x3f)]);
        } else if (cp < 0x10000) {
            parts.push(hexTable[0xe0 | (cp >> 12)], hexTable[0x80 | ((cp >> 6) & 0x3f)],
                hexTable[0x80 | (cp & 0x3f)]);
        } else {
            parts.push(hexTable[0xf0 | (cp >> 18)], hexTable[0x80 | ((cp >> 12) & 0x3f)],
                hexTable[0x80 | ((cp >> 6) & 0x3f)], hexTable[0x80 | (cp & 0x3f)]);
        }
        lastPos = i + 1;
    }
    if (lastPos === 0) return str;
    if (lastPos < str.length) parts.push(str.slice(lastPos));
    return parts.join("");
}

// ---------------------------------------------------------------------------
// WHATWG URL / URLSearchParams
// ---------------------------------------------------------------------------
class URLSearchParams {
    constructor(init) {
        this._list = [];
        if (init === undefined || init === null || init === "") return;
        if (typeof init === "string") {
            this._parse(init);
        } else if (init instanceof URLSearchParams) {
            this._list = init._list.map((p) => [p[0], p[1]]);
        } else if (Array.isArray(init)) {
            for (const pair of init) this._list.push([String(pair[0]), String(pair[1])]);
        } else if (typeof init === "object") {
            for (const k of Object.keys(init)) this._list.push([k, String(init[k])]);
        }
    }
    _parse(query) {
        if (query.charCodeAt(0) === 63) query = query.slice(1);
        if (query === "") return;
        for (const part of query.split("&")) {
            if (part === "") continue;
            const eq = part.indexOf("=");
            let name, value;
            if (eq === -1) { name = part; value = ""; }
            else { name = part.slice(0, eq); value = part.slice(eq + 1); }
            this._list.push([decode(name), decode(value)]);
        }
    }
    append(name, value) { this._list.push([String(name), String(value)]); }
    delete(name) { name = String(name); this._list = this._list.filter((p) => p[0] !== name); }
    get(name) { name = String(name); for (const p of this._list) if (p[0] === name) return p[1]; return null; }
    getAll(name) { name = String(name); return this._list.filter((p) => p[0] === name).map((p) => p[1]); }
    has(name) { name = String(name); return this._list.some((p) => p[0] === name); }
    set(name, value) {
        name = String(name); value = String(value);
        let found = false;
        const out = [];
        for (const p of this._list) {
            if (p[0] === name) {
                if (!found) { out.push([name, value]); found = true; }
            } else { out.push(p); }
        }
        if (!found) out.push([name, value]);
        this._list = out;
    }
    sort() { this._list.sort((a, b) => (a[0] < b[0] ? -1 : a[0] > b[0] ? 1 : 0)); }
    forEach(cb, thisArg) { for (const p of this._list) cb.call(thisArg, p[1], p[0], this); }
    keys() { return this._list.map((p) => p[0])[Symbol.iterator](); }
    values() { return this._list.map((p) => p[1])[Symbol.iterator](); }
    entries() { return this._list.map((p) => [p[0], p[1]])[Symbol.iterator](); }
    [Symbol.iterator]() { return this.entries(); }
    get size() { return this._list.length; }
    toString() {
        return this._list.map((p) => encode(p[0]) + "=" + encode(p[1])).join("&");
    }
}

function encode(s) {
    return encodeURIComponent(s).replace(/%20/g, "+").replace(/[!'()~*]/g, (c) =>
        "%" + c.charCodeAt(0).toString(16).toUpperCase());
}
function decode(s) {
    try { return decodeURIComponent(String(s).replace(/\+/g, " ")); } catch { return s; }
}

// Parse an absolute URL string into components. Returns null on failure.
function parseAbsolute(input) {
    const m = /^([a-zA-Z][a-zA-Z0-9+.-]*:)(\/\/)?([^/?#]*)([^?#]*)(\?[^#]*)?(#.*)?$/.exec(input);
    if (!m) return null;
    const protocol = m[1].toLowerCase();
    const hasAuthority = m[2] === "//";
    let username = "", password = "", hostname = "", port = "";
    if (hasAuthority) {
        let authority = m[3];
        const at = authority.lastIndexOf("@");
        if (at !== -1) {
            const cred = authority.slice(0, at);
            authority = authority.slice(at + 1);
            const colon = cred.indexOf(":");
            if (colon === -1) username = cred;
            else { username = cred.slice(0, colon); password = cred.slice(colon + 1); }
        }
        const pcolon = authority.lastIndexOf(":");
        if (pcolon !== -1 && authority.indexOf("]", pcolon) === -1 &&
            /^[0-9]*$/.test(authority.slice(pcolon + 1))) {
            hostname = authority.slice(0, pcolon);
            port = authority.slice(pcolon + 1);
        } else {
            hostname = authority;
        }
        hostname = hostname.toLowerCase();
    }
    let pathname = m[4] || "";
    if (hasAuthority && pathname === "") pathname = "/";
    if (SPECIAL_PORTS[protocol] === port) port = "";
    return {
        protocol, username, password, hostname, port, slashes: hasAuthority,
        host: port ? hostname + ":" + port : hostname,
        pathname: percentEncode(pathname, PATH_SAFE),
        search: m[5] || "",
        hash: m[6] || "",
    };
}

function isAbsoluteUrl(input) {
    return /^[a-zA-Z][a-zA-Z0-9+.-]*:/.test(input);
}

class URL {
    constructor(input, base) {
        input = String(input);
        let comps;
        if (isAbsoluteUrl(input)) {
            comps = parseAbsolute(input);
        } else if (base !== undefined && base !== null) {
            const baseUrl = base instanceof URL ? base : new URL(String(base));
            comps = resolveRelative(baseUrl, input);
        }
        if (!comps) throw invalidUrl(input);
        this._protocol = comps.protocol;
        this._username = comps.username;
        this._password = comps.password;
        this._hostname = comps.hostname;
        this._port = comps.port;
        this._slashes = comps.slashes;
        this._pathname = comps.pathname;
        this._search = comps.search;
        this._hash = comps.hash;
        this._searchParams = new URLSearchParams(this._search);
    }
    get protocol() { return this._protocol; }
    set protocol(v) { v = String(v); this._protocol = v.endsWith(":") ? v.toLowerCase() : v.toLowerCase() + ":"; }
    get username() { return this._username; }
    set username(v) { this._username = String(v); }
    get password() { return this._password; }
    set password(v) { this._password = String(v); }
    get hostname() { return this._hostname; }
    set hostname(v) { this._hostname = String(v).toLowerCase(); this._slashes = true; }
    get port() { return this._port; }
    set port(v) { this._port = v === "" ? "" : String(parseInt(v, 10)); }
    get host() {
        if (this._port) return this._hostname + ":" + this._port;
        return this._hostname;
    }
    set host(v) {
        v = String(v);
        const colon = v.lastIndexOf(":");
        if (colon !== -1) { this._hostname = v.slice(0, colon); this._port = v.slice(colon + 1); }
        else { this._hostname = v; this._port = ""; }
        this._slashes = true;
    }
    get origin() {
        if (!this._hostname) return "null";
        return this._protocol + "//" + this.host;
    }
    get pathname() { return this._pathname; }
    set pathname(v) {
        v = String(v);
        if (v && v.charCodeAt(0) !== 47) v = "/" + v;
        this._pathname = percentEncode(v, PATH_SAFE);
    }
    get search() { return this._searchParams.toString() ? "?" + this._searchParams.toString() : ""; }
    set search(v) {
        v = String(v);
        if (v && v.charCodeAt(0) === 63) v = v.slice(1);
        this._search = v ? "?" + v : "";
        this._searchParams = new URLSearchParams(v);
    }
    get searchParams() { return this._searchParams; }
    get hash() { return this._hash; }
    set hash(v) {
        v = String(v);
        if (v === "") { this._hash = ""; return; }
        this._hash = v.charCodeAt(0) === 35 ? v : "#" + v;
    }
    get href() { return this.toString(); }
    set href(v) {
        const next = new URL(String(v));
        this._protocol = next._protocol; this._username = next._username;
        this._password = next._password; this._hostname = next._hostname;
        this._port = next._port; this._slashes = next._slashes;
        this._pathname = next._pathname; this._search = next._search;
        this._hash = next._hash; this._searchParams = next._searchParams;
    }
    toString() {
        let out = this._protocol;
        if (this._slashes || this._hostname) {
            out += "//";
            if (this._username) {
                out += this._username;
                if (this._password) out += ":" + this._password;
                out += "@";
            }
            out += this.host;
        }
        out += this._pathname;
        out += this.search;
        out += this._hash;
        return out;
    }
    toJSON() { return this.toString(); }
}

// Resolve a relative reference against a base URL (a small subset of the WHATWG
// algorithm: absolute paths, relative paths, query-only, and fragment-only).
function resolveRelative(base, ref) {
    const out = {
        protocol: base._protocol, username: base._username, password: base._password,
        hostname: base._hostname, port: base._port, host: base.host, slashes: base._slashes,
        pathname: base._pathname, search: base._search, hash: base._hash,
    };
    if (ref === "") return out;
    if (ref.charCodeAt(0) === 35) { out.hash = ref; return out; }
    if (ref.charCodeAt(0) === 63) { out.search = ref; out.hash = ""; return out; }
    if (ref.startsWith("//")) {
        const parsed = parseAbsolute(base._protocol + ref);
        return parsed || out;
    }
    out.search = ""; out.hash = "";
    let path;
    if (ref.charCodeAt(0) === 47) {
        path = ref;
    } else {
        const baseDir = base._pathname.slice(0, base._pathname.lastIndexOf("/") + 1) || "/";
        path = baseDir + ref;
    }
    const segments = [];
    for (const seg of path.split("/")) {
        if (seg === "..") segments.pop();
        else if (seg !== ".") segments.push(seg);
    }
    out.pathname = segments.join("/") || "/";
    if (out.pathname.charCodeAt(0) !== 47) out.pathname = "/" + out.pathname;
    const qi = out.pathname.indexOf("?");
    if (qi !== -1) { out.search = out.pathname.slice(qi); out.pathname = out.pathname.slice(0, qi); }
    const hi = out.pathname.indexOf("#");
    if (hi !== -1) { out.hash = out.pathname.slice(hi); out.pathname = out.pathname.slice(0, hi); }
    out.pathname = percentEncode(out.pathname, PATH_SAFE);
    return out;
}

// ---------------------------------------------------------------------------
// Legacy `url.Url` / parse / format / resolve / resolveObject.
//
// A port of Node's `lib/url.js`. This API predates the WHATWG parser, is still
// what `url.parse()` returns, and has enough quirks — backslash rewriting,
// auth-vs-host disambiguation, "unsafe" protocols, RFC 3986 relative merging —
// that approximating it is not an option for the packages that depend on it.
// ---------------------------------------------------------------------------
const protocolPattern = /^[a-z0-9.+-]+:/i;
const portPattern = /:[0-9]*$/;
const hostPattern = /^\/\/[^@/]+@[^@/]+/;
// A path-only URL: `/foo` or `//foo`, optionally with a query.
const simplePathPattern = /^(\/\/?(?!\/)[^?\s]*)(\?[^\s]*)?$/;
const hostnameMaxLen = 255;
const forbiddenHostChars = /[\0\t\n\r #%\/:<>?@[\\\]^|]/;
// An IPv6 literal legitimately carries `[`, `]` and `:`.
const forbiddenHostCharsIpv6 = /[\0\t\n\r #%\/<>?@\\^|]/;
// Tabs and newlines are dropped from the authority rather than terminating it,
// so that `http://a\tb.com/` cannot be used to spoof a host.
const tabOrNewline = /[\t\n\r]/g;
// Protocols that keep "unsafe"/"unwise" characters verbatim.
const unsafeProtocol = { javascript: true, "javascript:": true };
// Protocols that never carry a hostname.
const hostlessProtocol = { javascript: true, "javascript:": true };
// Protocols that always carry a `//`.
const slashedProtocol = {
    http: true, "http:": true, https: true, "https:": true,
    ftp: true, "ftp:": true, gopher: true, "gopher:": true,
    file: true, "file:": true, ws: true, "ws:": true, wss: true, "wss:": true,
};

function Url() {
    this.protocol = null;
    this.slashes = null;
    this.auth = null;
    this.host = null;
    this.port = null;
    this.hostname = null;
    this.hash = null;
    this.search = null;
    this.query = null;
    this.pathname = null;
    this.path = null;
    this.href = null;
}

function isIpv6Hostname(hostname) {
    return hostname.charCodeAt(0) === 91 && hostname.charCodeAt(hostname.length - 1) === 93;
}

// The delimiters and "unwise" characters of RFC 2396 that legacy parse()
// always escapes in the post-host remainder, plus the single quote (in case of
// an XSS attack), even where encodeURIComponent would leave them alone.
const escapedCodes = [];
for (let i = 0; i < 126; i++) escapedCodes.push("");
for (const code of [9, 10, 13, 32, 34, 39, 60, 62, 92, 94, 96, 123, 124, 125]) {
    escapedCodes[code] = hexTable[code];
}

function autoEscapeStr(rest) {
    let escaped = "";
    let lastEscapedPos = 0;
    for (let i = 0; i < rest.length; i++) {
        const escapedChar = escapedCodes[rest.charCodeAt(i)];
        if (escapedChar) {
            if (i > lastEscapedPos) escaped += rest.slice(lastEscapedPos, i);
            escaped += escapedChar;
            lastEscapedPos = i + 1;
        }
    }
    if (lastEscapedPos === 0) return rest;
    if (lastEscapedPos < rest.length) escaped += rest.slice(lastEscapedPos);
    return escaped;
}

// Trim the hostname at the first character RFC 2396 forbids and push the
// remainder back onto the path — what browsers do with `http://x.y.com;a/b`.
function getHostname(self, rest, hostname) {
    for (let i = 0; i < hostname.length; i++) {
        const code = hostname.charCodeAt(i);
        if (code === 47 || code === 92 || code === 35 || code === 63 || code === 58) {
            self.hostname = hostname.slice(0, i);
            return `/${hostname.slice(i)}${rest}`;
        }
    }
    return rest;
}

Url.prototype.parse = function parse(url, parseQueryString, slashesDenoteHost) {
    validateString(url, "url");

    // Chrome/IE/Opera backslash handling: a backslash before the query string
    // becomes a forward slash. Leading and trailing whitespace is trimmed.
    let hasHash = false;
    let hasAt = false;
    let start = -1;
    let end = -1;
    let rest = "";
    let lastPos = 0;
    let inWs = false;
    let split = false;
    for (let i = 0; i < url.length; i++) {
        const code = url.charCodeAt(i);
        const isWs = code < 33 || code === 160 || code === 65279;
        if (start === -1) {
            if (isWs) continue;
            lastPos = start = i;
        } else if (inWs) {
            if (!isWs) { end = -1; inWs = false; }
        } else if (isWs) {
            end = i;
            inWs = true;
        }
        if (!split) {
            if (code === 64) {
                hasAt = true;
            } else if (code === 35) {
                hasHash = true;
                split = true;
            } else if (code === 63) {
                split = true;
            } else if (code === 92) {
                if (i - lastPos > 0) rest += url.slice(lastPos, i);
                rest += "/";
                lastPos = i + 1;
            }
        } else if (!hasHash && code === 35) {
            hasHash = true;
        }
    }

    if (start !== -1) {
        if (lastPos === start) {
            // No backslash was converted: take the trimmed input as-is.
            if (end === -1) rest = start === 0 ? url : url.slice(start);
            else rest = url.slice(start, end);
        } else if (end === -1 && lastPos < url.length) {
            rest += url.slice(lastPos);
        } else if (end !== -1 && lastPos < end) {
            rest += url.slice(lastPos, end);
        }
    }

    if (!slashesDenoteHost && !hasHash && !hasAt) {
        const simplePath = simplePathPattern.exec(rest);
        if (simplePath) {
            this.path = rest;
            this.href = rest;
            this.pathname = simplePath[1];
            if (simplePath[2]) {
                this.search = simplePath[2];
                this.query = parseQueryString ? qsParse(this.search.slice(1)) : this.search.slice(1);
            } else if (parseQueryString) {
                this.search = null;
                this.query = Object.create(null);
            }
            return this;
        }
    }

    let proto = protocolPattern.exec(rest);
    let lowerProto;
    if (proto) {
        proto = proto[0];
        lowerProto = proto.toLowerCase();
        this.protocol = lowerProto;
        rest = rest.slice(proto.length);
    }

    // `user@server` always denotes a hostname, and `//foo/bar` resolves to
    // host=foo the way a browser resolves relative URLs.
    let slashes;
    if (slashesDenoteHost || proto || hostPattern.test(rest)) {
        slashes = rest.charCodeAt(0) === 47 && rest.charCodeAt(1) === 47;
        if (slashes && !(proto && hostlessProtocol[lowerProto])) {
            rest = rest.slice(2);
            this.slashes = true;
        }
    }

    if (!hostlessProtocol[lowerProto] && (slashes || (proto && !slashedProtocol[proto]))) {
        // The first `/`, `?`, `;` or `#` ends the host — but everything left of
        // the LAST `@` is auth, so `http://a@b@c/` is user `a@b` host `c`,
        // while `http://a@b?@c` is user `a` host `b`. URLs are obnoxious.
        let hostEnd = -1;
        let atSign = -1;
        let nonHost = -1;
        for (let i = 0; i < rest.length; i++) {
            const code = rest.charCodeAt(i);
            if (code === 32 || code === 34 || code === 37 || code === 39 || code === 59 ||
                code === 60 || code === 62 || code === 92 || code === 94 || code === 96 ||
                code === 123 || code === 124 || code === 125) {
                // Never valid in a hostname per RFC 2396.
                if (nonHost === -1) nonHost = i;
            } else if (code === 35 || code === 47 || code === 63) {
                if (nonHost === -1) nonHost = i;
                hostEnd = i;
            } else if (code === 64) {
                atSign = i;
                nonHost = -1;
            }
            if (hostEnd !== -1) break;
        }
        start = 0;
        if (atSign !== -1) {
            this.auth = decodeURIComponent(rest.slice(0, atSign).replace(tabOrNewline, ""));
            start = atSign + 1;
        }
        if (nonHost === -1) {
            this.host = rest.slice(start).replace(tabOrNewline, "");
            rest = "";
        } else {
            this.host = rest.slice(start, nonHost).replace(tabOrNewline, "");
            rest = rest.slice(nonHost);
        }

        this.parseHost();

        // A hostname was indicated, so it must be present even when empty.
        if (typeof this.hostname !== "string") this.hostname = "";
        const hostname = this.hostname;
        const ipv6Hostname = isIpv6Hostname(hostname);
        if (!ipv6Hostname) rest = getHostname(this, rest, hostname);

        if (this.hostname.length > hostnameMaxLen) this.hostname = "";
        else this.hostname = this.hostname.toLowerCase();

        if (this.hostname !== "") {
            if (ipv6Hostname) {
                if (forbiddenHostCharsIpv6.test(this.hostname)) throw invalidUrl(url);
            } else {
                // IDNA: punycode the labels that need it. A hostname that only
                // becomes invalid *after* conversion is a spoofing vector, so
                // it is a hard error rather than something to paper over.
                this.hostname = toASCII(this.hostname);
                if (this.hostname === "" || forbiddenHostChars.test(this.hostname)) {
                    throw invalidUrl(url);
                }
            }
        }

        this.host = (this.hostname || "") + (this.port ? ":" + this.port : "");

        // `host` keeps the brackets of an IPv6 literal; `hostname` drops them.
        if (ipv6Hostname) {
            this.hostname = this.hostname.slice(1, -1);
            if (rest[0] !== "/") rest = "/" + rest;
        }
    }

    if (!unsafeProtocol[lowerProto]) rest = autoEscapeStr(rest);

    let questionIdx = -1;
    let hashIdx = -1;
    for (let i = 0; i < rest.length; i++) {
        const code = rest.charCodeAt(i);
        if (code === 35) {
            this.hash = rest.slice(i);
            hashIdx = i;
            break;
        } else if (code === 63 && questionIdx === -1) {
            questionIdx = i;
        }
    }

    if (questionIdx !== -1) {
        if (hashIdx === -1) {
            this.search = rest.slice(questionIdx);
            this.query = rest.slice(questionIdx + 1);
        } else {
            this.search = rest.slice(questionIdx, hashIdx);
            this.query = rest.slice(questionIdx + 1, hashIdx);
        }
        if (parseQueryString) this.query = qsParse(this.query);
    } else if (parseQueryString) {
        this.search = null;
        this.query = Object.create(null);
    }

    const useQuestionIdx = questionIdx !== -1 && (hashIdx === -1 || questionIdx < hashIdx);
    const firstIdx = useQuestionIdx ? questionIdx : hashIdx;
    if (firstIdx === -1) {
        if (rest.length > 0) this.pathname = rest;
    } else if (firstIdx > 0) {
        this.pathname = rest.slice(0, firstIdx);
    }
    if (slashedProtocol[lowerProto] && this.hostname && !this.pathname) {
        this.pathname = "/";
    }

    // To support http.request.
    if (this.pathname || this.search) {
        this.path = (this.pathname || "") + (this.search || "");
    }

    this.href = this.format();
    return this;
};

Url.prototype.parseHost = function parseHost() {
    let host = this.host;
    let port = portPattern.exec(host);
    if (port) {
        port = port[0];
        if (port !== ":") this.port = port.slice(1);
        host = host.slice(0, host.length - port.length);
    }
    if (host) this.hostname = host;
};

Url.prototype.format = function format() {
    let auth = this.auth || "";
    if (auth) auth = percentEncode(auth, AUTH_SAFE) + "@";

    let protocol = this.protocol || "";
    let pathname = this.pathname || "";
    let hash = this.hash || "";
    let host = "";
    let query = "";

    if (this.host) {
        host = auth + this.host;
    } else if (this.hostname) {
        host = auth + (this.hostname.includes(":") && !isIpv6Hostname(this.hostname)
            ? "[" + this.hostname + "]"
            : this.hostname);
        if (this.port) host += ":" + this.port;
    }

    if (this.query !== null && typeof this.query === "object") query = qsStringify(this.query);

    let search = this.search || (query && "?" + query) || "";

    if (protocol && protocol.charCodeAt(protocol.length - 1) !== 58) protocol += ":";

    let newPathname = "";
    let lastPos = 0;
    for (let i = 0; i < pathname.length; i++) {
        const code = pathname.charCodeAt(i);
        if (code === 35 || code === 63) {
            if (i - lastPos > 0) newPathname += pathname.slice(lastPos, i);
            newPathname += code === 35 ? "%23" : "%3F";
            lastPos = i + 1;
        }
    }
    if (lastPos > 0) {
        pathname = lastPos !== pathname.length ? newPathname + pathname.slice(lastPos) : newPathname;
    }

    // Only slashed protocols get the `//` — not mailto:, xmpp:, … unless they
    // had one to begin with.
    if (this.slashes || slashedProtocol[protocol]) {
        if (this.slashes || host) {
            if (pathname && pathname.charCodeAt(0) !== 47) pathname = "/" + pathname;
            host = "//" + host;
        } else if (protocol === "file:") {
            host = "//";
        }
    }

    search = search.replace(/#/g, "%23");

    if (hash && hash.charCodeAt(0) !== 35) hash = "#" + hash;
    if (search && search.charCodeAt(0) !== 63) search = "?" + search;

    return protocol + host + pathname + search + hash;
};

Url.prototype.resolve = function resolve(relative) {
    return this.resolveObject(urlParse(relative, false, true)).format();
};

Url.prototype.resolveObject = function resolveObject(relative) {
    if (typeof relative === "string") {
        const rel = new Url();
        rel.parse(relative, false, true);
        relative = rel;
    }

    const result = new Url();
    for (const tkey of Object.keys(this)) result[tkey] = this[tkey];

    // The hash is always overridden — even `href=""` clears it.
    result.hash = relative.hash;

    if (relative.href === "") {
        result.href = result.format();
        return result;
    }

    // Hrefs like `//foo/bar` always cut back to the protocol.
    if (relative.slashes && !relative.protocol) {
        for (const rkey of Object.keys(relative)) {
            if (rkey !== "protocol") result[rkey] = relative[rkey];
        }
        if (slashedProtocol[result.protocol] && result.hostname && !result.pathname) {
            result.path = result.pathname = "/";
        }
        result.href = result.format();
        return result;
    }

    if (relative.protocol && relative.protocol !== result.protocol) {
        // Switching to an unknown protocol replaces everything. Switching to a
        // known one keeps a host unless the reference brings its own — and
        // file:, being hostless, drops it.
        if (!slashedProtocol[relative.protocol]) {
            for (const k of Object.keys(relative)) result[k] = relative[k];
            result.href = result.format();
            return result;
        }

        result.protocol = relative.protocol;
        if (!relative.host && !/^file:?$/.test(relative.protocol) &&
            !hostlessProtocol[relative.protocol]) {
            const relPath = (relative.pathname || "").split("/");
            while (relPath.length && !(relative.host = relPath.shift())) { /* first non-empty */ }
            if (!relative.host) relative.host = "";
            if (!relative.hostname) relative.hostname = "";
            if (relPath[0] !== "") relPath.unshift("");
            if (relPath.length < 2) relPath.unshift("");
            result.pathname = relPath.join("/");
        } else {
            result.pathname = relative.pathname;
        }

        result.search = relative.search;
        result.query = relative.query;
        result.host = relative.host || "";
        result.auth = relative.auth;
        result.hostname = relative.hostname || relative.host;
        result.port = relative.port;
        if (result.pathname || result.search) {
            result.path = (result.pathname || "") + (result.search || "");
        }
        result.slashes = result.slashes || relative.slashes;
        result.href = result.format();
        return result;
    }

    const isSourceAbs = result.pathname && result.pathname.charAt(0) === "/";
    const isRelAbs = relative.host || (relative.pathname && relative.pathname.charAt(0) === "/");
    let mustEndAbs = isRelAbs || isSourceAbs || (result.host && relative.pathname);
    const removeAllDots = mustEndAbs;
    let srcPath = (result.pathname && result.pathname.split("/")) || [];
    const relPath = (relative.pathname && relative.pathname.split("/")) || [];
    const noLeadingSlashes = result.protocol && !slashedProtocol[result.protocol];

    // In a non-slashed URL `../..` may crawl all the way up into the hostname,
    // so the first path part gets moved into the host field further down.
    if (noLeadingSlashes) {
        result.hostname = "";
        result.port = null;
        if (result.host) {
            if (srcPath[0] === "") srcPath[0] = result.host;
            else srcPath.unshift(result.host);
        }
        result.host = "";
        if (relative.protocol) {
            relative.hostname = null;
            relative.port = null;
            result.auth = null;
            if (relative.host) {
                if (relPath[0] === "") relPath[0] = relative.host;
                else relPath.unshift(relative.host);
            }
            relative.host = null;
        }
        mustEndAbs = mustEndAbs && (relPath[0] === "" || srcPath[0] === "");
    }

    if (isRelAbs) {
        if (relative.host || relative.host === "") {
            if (result.host !== relative.host) result.auth = null;
            result.host = relative.host;
            result.port = relative.port;
        }
        if (relative.hostname || relative.hostname === "") {
            if (result.hostname !== relative.hostname) result.auth = null;
            result.hostname = relative.hostname;
        }
        result.search = relative.search;
        result.query = relative.query;
        srcPath = relPath;
    } else if (relPath.length) {
        // Relative: throw away the base's last segment, take the new path.
        if (!srcPath) srcPath = [];
        srcPath.pop();
        srcPath = srcPath.concat(relPath);
        result.search = relative.search;
        result.query = relative.query;
    } else if (relative.search !== null && relative.search !== undefined) {
        // Query-only reference (`href='?foo'`). Last, because it simplifies
        // the booleans above.
        if (noLeadingSlashes) {
            result.hostname = result.host = srcPath.shift();
            // The auth can end up stuck in the host, which happens for e.g.
            // resolveObject('mailto:local1@domain1', 'local2@domain2').
            const authInHost = result.host && result.host.indexOf("@") > 0 && result.host.split("@");
            if (authInHost) {
                result.auth = authInHost.shift();
                result.host = result.hostname = authInHost.shift();
            }
        }
        result.search = relative.search;
        result.query = relative.query;
        if (result.pathname !== null || result.search !== null) {
            result.path = (result.pathname || "") + (result.search || "");
        }
        result.href = result.format();
        return result;
    }

    if (!srcPath.length) {
        // No path at all; everything else was handled above.
        result.pathname = null;
        result.path = result.search ? "/" + result.search : null;
        result.href = result.format();
        return result;
    }

    // A URL ending in `.` or `..` gets a trailing slash; anything else
    // non-slashy must NOT get one.
    let last = srcPath[srcPath.length - 1];
    const hasTrailingSlash =
        ((result.host || relative.host || srcPath.length > 1) && (last === "." || last === "..")) ||
        last === "";

    // Strip single dots and resolve double dots against the parent; `up`
    // counts the ones that tried to climb above the root.
    let up = 0;
    for (let i = srcPath.length - 1; i >= 0; i--) {
        last = srcPath[i];
        if (last === ".") {
            srcPath.splice(i, 1);
        } else if (last === "..") {
            srcPath.splice(i, 1);
            up++;
        } else if (up) {
            srcPath.splice(i, 1);
            up--;
        }
    }

    // If the path is allowed above the root, restore the leading `..`s.
    if (!mustEndAbs && !removeAllDots) {
        while (up--) srcPath.unshift("..");
    }

    if (mustEndAbs && srcPath[0] !== "" && (!srcPath[0] || srcPath[0].charAt(0) !== "/")) {
        srcPath.unshift("");
    }

    if (hasTrailingSlash && srcPath.join("/").slice(-1) !== "/") srcPath.push("");

    const isAbsolute = srcPath[0] === "" || (srcPath[0] && srcPath[0].charAt(0) === "/");

    // Put the host back.
    if (noLeadingSlashes) {
        result.hostname = result.host = isAbsolute ? "" : srcPath.length ? srcPath.shift() : "";
        const authInHost = result.host && result.host.indexOf("@") > 0
            ? result.host.split("@")
            : false;
        if (authInHost) {
            result.auth = authInHost.shift();
            result.host = result.hostname = authInHost.shift();
        }
    }

    mustEndAbs = mustEndAbs || (result.host && srcPath.length);

    if (mustEndAbs && !isAbsolute) srcPath.unshift("");

    if (!srcPath.length) {
        result.pathname = null;
        result.path = null;
    } else {
        result.pathname = srcPath.join("/");
    }

    if (result.pathname !== null || result.search !== null) {
        result.path = (result.pathname || "") + (result.search || "");
    }
    result.auth = relative.auth || result.auth;
    result.slashes = result.slashes || relative.slashes;
    result.href = result.format();
    return result;
};

function urlParse(url, parseQueryString, slashesDenoteHost) {
    if (url instanceof Url) return url;
    const urlObject = new Url();
    urlObject.parse(url, parseQueryString, slashesDenoteHost);
    return urlObject;
}

function urlFormat(urlObject, options) {
    // A string is round-tripped through the parser, which cleans up wonky URLs.
    if (typeof urlObject === "string") {
        urlObject = urlParse(urlObject);
    } else if (typeof urlObject !== "object" || urlObject === null) {
        throw invalidArgType("urlObject", ["Object", "string"], urlObject);
    } else if (urlObject instanceof URL) {
        return urlObject.toString();
    } else if (!(urlObject instanceof Url)) {
        return Url.prototype.format.call(urlObject);
    }
    return urlObject.format(options);
}

function urlResolve(source, relative) {
    return urlParse(source, false, true).resolve(relative);
}

function urlResolveObject(source, relative) {
    if (!source) return relative;
    return urlParse(source, false, true).resolveObject(relative);
}

// ---------------------------------------------------------------------------
// file: URL <-> path conversion. The runtime is posix-only, so the Windows
// branches are reachable only when a caller passes `{ windows: true }`.
// ---------------------------------------------------------------------------
const IS_WINDOWS = false;

function wantsWindows(options) {
    if (options === undefined || options === null) return IS_WINDOWS;
    return options.windows === undefined ? IS_WINDOWS : !!options.windows;
}

// Escape what has to survive the URL parser verbatim. `%` goes first so the
// escapes introduced below are not escaped a second time.
function encodePathChars(filepath, windows) {
    if (filepath.indexOf("%") !== -1) filepath = filepath.replace(/%/g, "%25");
    // A backslash is a valid character in a posix path, not a separator.
    if (!windows && filepath.indexOf("\\") !== -1) filepath = filepath.replace(/\\/g, "%5C");
    if (filepath.indexOf("\n") !== -1) filepath = filepath.replace(/\n/g, "%0A");
    if (filepath.indexOf("\r") !== -1) filepath = filepath.replace(/\r/g, "%0D");
    if (filepath.indexOf("\t") !== -1) filepath = filepath.replace(/\t/g, "%09");
    return filepath;
}

// `path.posix.resolve` anchored at `/`: the runtime has no real cwd, and a
// file URL only needs the absolute, dot-segment-free form.
function posixResolve(filepath) {
    const parts = (filepath.charCodeAt(0) === 47 ? filepath : "/" + filepath).split("/");
    const out = [];
    for (const part of parts) {
        if (part === "" || part === ".") continue;
        if (part === "..") { out.pop(); continue; }
        out.push(part);
    }
    return "/" + out.join("/");
}

// `path.win32.resolve` for the shapes a file URL can carry: a drive-letter
// path or a rooted one. Anything else is anchored at `C:\`.
function win32Resolve(filepath) {
    let normalized = filepath.replace(/\//g, "\\");
    let prefix = "C:\\";
    const drive = /^([a-zA-Z]:)\\?/.exec(normalized);
    if (drive) {
        prefix = drive[1] + "\\";
        normalized = normalized.slice(drive[0].length);
    } else if (normalized.charCodeAt(0) === 92) {
        prefix = "\\";
        normalized = normalized.slice(1);
    }
    const out = [];
    for (const part of normalized.split("\\")) {
        if (part === "" || part === ".") continue;
        if (part === "..") { out.pop(); continue; }
        out.push(part);
    }
    return prefix + out.join("\\");
}

function pathToFileURL(filepath, options) {
    validateString(filepath, "path");
    const windows = wantsWindows(options);

    if (windows && filepath.startsWith("\\\\")) {
        // UNC (`\\server\share\resource`) and extended UNC (`\\?\UNC\…`). The
        // `\\?\` prefix of a *local* extended path is not a server name, and
        // falls out as an empty host because `?` is not a valid domain.
        const isExtendedUNC = filepath.startsWith("\\\\?\\UNC\\");
        const prefixLength = isExtendedUNC ? 8 : 2;
        const hostnameEndIndex = filepath.indexOf("\\", prefixLength);
        if (hostnameEndIndex === -1) {
            throw invalidArgValue("path", filepath, "must be a complete UNC resource path");
        }
        if (hostnameEndIndex === 2) {
            throw invalidArgValue("path", filepath, "must not have an empty UNC servername");
        }
        const hostname = domainToASCII(filepath.slice(prefixLength, hostnameEndIndex));
        const tail = filepath.slice(hostnameEndIndex).replace(/\\/g, "/");
        return new URL("file://" + hostname +
            percentEncode(encodePathChars(tail, true), FILE_PATH_SAFE));
    }

    let resolved = windows ? win32Resolve(filepath) : posixResolve(filepath);
    // resolve() strips the trailing separator, so put it back.
    const lastCode = filepath.charCodeAt(filepath.length - 1);
    const sep = windows ? "\\" : "/";
    if ((lastCode === 47 || (windows && lastCode === 92)) && resolved.slice(-1) !== sep) {
        resolved += "/";
    }
    resolved = encodePathChars(resolved, windows);
    if (windows) resolved = "/" + resolved.replace(/\\/g, "/");
    return new URL("file://" + percentEncode(resolved, FILE_PATH_SAFE));
}

// `%2F` (and, on Windows, `%5C`) would silently become a path separator once
// decoded, so a file URL carrying one is rejected outright.
function rejectEncodedSeparators(pathname, windows) {
    for (let n = 0; n < pathname.length; n++) {
        if (pathname[n] !== "%") continue;
        const third = pathname.charCodeAt(n + 2) | 0x20;
        if (pathname[n + 1] === "2" && third === 102) {
            throw invalidFileUrlPath("must not include encoded / characters");
        }
        if (windows && pathname[n + 1] === "5" && third === 99) {
            throw invalidFileUrlPath("must not include encoded \\ characters");
        }
    }
}

function fileURLToPath(path, options) {
    const windows = wantsWindows(options);
    let url = path;
    if (typeof path === "string") url = new URL(path);
    else if (!(path instanceof URL)) throw invalidArgType("path", ["string", "URL"], path);
    if (url.protocol !== "file:") throw invalidUrlScheme("file");

    const pathname = url.pathname;
    if (!windows) {
        if (url.hostname !== "") throw invalidFileUrlHost("chidori");
        rejectEncodedSeparators(pathname, false);
        return decodeURIComponent(pathname);
    }
    rejectEncodedSeparators(pathname, true);
    const decoded = decodeURIComponent(pathname.replace(/\//g, "\\"));
    if (url.hostname !== "") return `\\\\${domainToUnicode(url.hostname)}${decoded}`;
    const letter = decoded.charCodeAt(1) | 0x20;
    if (letter < 97 || letter > 122 || decoded.charAt(2) !== ":") {
        throw invalidFileUrlPath("must be absolute");
    }
    return decoded.slice(1);
}

function domainToASCII(domain) {
    if (domain === undefined) throw invalidArgType("domain", "string", domain);
    const value = String(domain);
    // A domain containing a forbidden host code point has no ASCII form.
    if (value === "" || forbiddenHostChars.test(value)) return "";
    try { return toASCII(value); } catch { return ""; }
}
function domainToUnicode(domain) {
    if (domain === undefined) throw invalidArgType("domain", "string", domain);
    try { return toUnicode(String(domain)); } catch { return ""; }
}

// Translate a WHATWG URL into the option bag `http.request` expects.
function urlToHttpOptions(url) {
    const options = {
        protocol: url.protocol,
        hostname: url.hostname && url.hostname.charCodeAt(0) === 91
            ? url.hostname.slice(1, -1)
            : url.hostname,
        hash: url.hash,
        search: url.search,
        pathname: url.pathname,
        path: `${url.pathname || ""}${url.search || ""}`,
        href: url.href,
    };
    if (url.port !== "") options.port = Number(url.port);
    if (url.username || url.password) {
        options.auth = `${decodeURIComponent(url.username)}:${decodeURIComponent(url.password)}`;
    }
    return options;
}

const url = {
    Url, parse: urlParse, format: urlFormat, resolve: urlResolve,
    resolveObject: urlResolveObject, URL, URLSearchParams,
    domainToASCII, domainToUnicode, fileURLToPath, pathToFileURL, urlToHttpOptions,
};

export {
    Url, urlParse as parse, urlFormat as format, urlResolve as resolve,
    urlResolveObject as resolveObject, URL, URLSearchParams,
    domainToASCII, domainToUnicode, fileURLToPath, pathToFileURL, urlToHttpOptions,
};
export default url;
"##;

// node:assert shim, modelled on Node's `lib/assert.js`: vendored Node tests
// match on error *shape*, so `AssertionError` carries `code`/`operator`/
// `generatedMessage`, argument validation throws Node's `ERR_*` codes, and the
// generated messages reuse Node's wording. Two deliberate simplifications:
// `ok()` cannot quote the failing expression (the engine exposes no
// source-position API for a call site) so it falls back to Node's
// `<actual> == true` form, and the value diff is a common prefix/suffix
// listing rather than Node's line-by-line LCS diff.
const ASSERT_SHIM: &str = r#"
const kNoException = Symbol("assert.noException");
const kMaxShortStringLength = 12;
const kMaxLongStringLength = 512;
const kCustomInspect = Symbol.for("nodejs.util.inspect.custom");

// Node's `addEllipsis`: clip a long/multi-line `actual`/`expected` before it is
// echoed back by `util.inspect`. The `slice(kMaxLongStringLength)` (rather than
// `slice(0, …)`) is Node's own — vendored tests assert on the resulting tail.
function addEllipsis(string) {
    const lines = string.split("\n", 11);
    if (lines.length > 10) {
        lines.length = 10;
        return lines.join("\n") + "\n...";
    } else if (string.length > kMaxLongStringLength) {
        return string.slice(kMaxLongStringLength) + "...";
    }
    return string;
}

const kReadableOperator = {
    deepStrictEqual: "Expected values to be strictly deep-equal:",
    strictEqual: "Expected values to be strictly equal:",
    strictEqualObject: 'Expected "actual" to be reference-equal to "expected":',
    deepEqual: "Expected values to be loosely deep-equal:",
    notDeepStrictEqual: 'Expected "actual" not to be strictly deep-equal to:',
    notStrictEqual: 'Expected "actual" to be strictly unequal to:',
    notStrictEqualObject: 'Expected "actual" not to be reference-equal to "expected":',
    notDeepEqual: 'Expected "actual" not to be loosely deep-equal to:',
    notDeepEqualUnequal: "Expected values not to be loosely deep-equal:",
    notIdentical: "Values have same structure but are not reference-equal:",
};

function isIdentifierKey(key) { return /^[A-Za-z_$][A-Za-z0-9_$]*$/.test(key); }

function isErrorValue(value) {
    return value instanceof Error || Object.prototype.toString.call(value) === "[object Error]";
}

function ctorNameOf(value) {
    const proto = Object.getPrototypeOf(value);
    if (proto === null) return null;
    const ctor = proto.constructor;
    return ctor && typeof ctor.name === "string" ? ctor.name : "";
}

function inspectString(str) {
    let quote = "'";
    if (str.indexOf("'") !== -1) {
        if (str.indexOf('"') === -1) quote = '"';
        else if (str.indexOf("`") === -1) quote = "`";
    }
    let out = quote;
    for (let i = 0; i < str.length; i++) {
        const ch = str[i];
        const code = str.charCodeAt(i);
        if (ch === quote || ch === "\\") out += "\\" + ch;
        else if (ch === "\n") out += "\\n";
        else if (ch === "\r") out += "\\r";
        else if (ch === "\t") out += "\\t";
        else if (code < 0x20 || code === 0x7f) out += "\\x" + code.toString(16).padStart(2, "0");
        else out += ch;
    }
    return out + quote;
}

// Node breaks a long string that spans lines into one quoted chunk per line,
// joined with ` +`, so a multi-line `actual` reads as source rather than as one
// unreadable `\n`-riddled blob. The thresholds are Node's: only when the string
// is longer than 16 characters AND longer than what is left of the default
// `breakLength` (80 as of Node v22) budget at the current indentation.
//
// Applied on the non-compact (`inspectValue`) path only — the path `assert`
// builds its diffs on, and the one vendored tests count lines of.
function splitKeepingNewlines(str) {
    const out = [];
    let start = 0;
    for (let i = 0; i < str.length; i++) {
        if (str[i] === "\n") {
            out.push(str.slice(start, i + 1));
            start = i + 1;
        }
    }
    if (start < str.length) out.push(str.slice(start));
    return out;
}
function inspectStringAt(str, indentationLvl) {
    if (str.length > 16 && str.length > 80 - indentationLvl - 4) {
        const lines = splitKeepingNewlines(str);
        if (lines.length > 1) {
            return lines.map(inspectString).join(" +\n" + " ".repeat(indentationLvl + 2));
        }
    }
    return inspectString(str);
}

// Objects that turned out to be circularly referenced during the render in
// flight, mapped to their `*n` index. Module-scoped rather than threaded
// through every recursive call because the index has to be visible to both
// ends of the back-edge; `inspect`/`inspectValue` reset it.
let circularRefs = new Map();

// `compact` mirrors Node's default `util.inspect` (single-line, insertion
// order); the multi-line, key-sorted form is what `assert` uses to build its
// diffs (Node's `inspectValue`, i.e. `{ compact: false, sorted: true }`).
function inspectAny(value, compact, depth, seen) {
    const kind = typeof value;
    if (kind === "string") return compact ? inspectString(value) : inspectStringAt(value, depth * 2);
    if (kind === "symbol") return String(value);
    if (kind === "bigint") return String(value) + "n";
    if (kind === "function") {
        return value.name ? "[Function: " + value.name + "]" : "[Function (anonymous)]";
    }
    if (value === null) return "null";
    if (kind !== "object") return String(value);
    // Node numbers each circularly-referenced object and marks BOTH ends: the
    // back-edge renders as `[Circular *n]` and the object it points at gains a
    // `<ref *n>` prefix. The index is assigned when the back-edge is found,
    // which is always while the target is still being rendered — so the prefix
    // can be applied on the way back out (see the tail of this function).
    if (seen.indexOf(value) !== -1) {
        let idx = circularRefs.get(value);
        if (idx === undefined) {
            idx = circularRefs.size + 1;
            circularRefs.set(value, idx);
        }
        return "[Circular *" + idx + "]";
    }
    if (value instanceof RegExp) return String(value);
    if (value instanceof Date) {
        return Number.isNaN(value.getTime()) ? "Invalid Date" : value.toISOString();
    }
    if (isErrorValue(value)) {
        const name = value.name === undefined ? "Error" : String(value.name);
        const message = value.message === undefined ? "" : String(value.message);
        return "[" + (message ? name + ": " + message : name) + "]";
    }
    if (depth > 4) return Array.isArray(value) ? "[Array]" : "[Object]";

    const next = seen.concat([value]);
    const entries = [];
    let open = "{";
    let close = "}";
    let prefix = "";
    if (Array.isArray(value)) {
        open = "[";
        close = "]";
        for (let i = 0; i < value.length; i++) {
            entries.push(inspectAny(value[i], compact, depth + 1, next));
        }
        for (const key of Object.keys(value)) {
            if (!/^\d+$/.test(key)) {
                entries.push(formatKey(key) + ": " + inspectAny(value[key], compact, depth + 1, next));
            }
        }
    } else if (value instanceof Map) {
        prefix = "Map(" + value.size + ") ";
        for (const entry of value) {
            entries.push(
                inspectAny(entry[0], compact, depth + 1, next) + " => " +
                inspectAny(entry[1], compact, depth + 1, next)
            );
        }
    } else if (value instanceof Set) {
        prefix = "Set(" + value.size + ") ";
        for (const entry of value) entries.push(inspectAny(entry, compact, depth + 1, next));
    } else if (ArrayBuffer.isView(value) && typeof value.length === "number") {
        prefix = ctorNameOf(value) + "(" + value.length + ") ";
        open = "[";
        close = "]";
        for (let i = 0; i < value.length; i++) entries.push(String(value[i]));
    } else {
        const name = ctorNameOf(value);
        if (name === null) prefix = "[Object: null prototype] ";
        else if (name && name !== "Object") prefix = name + " ";
        const keys = Object.keys(value);
        if (!compact) keys.sort();
        for (const key of keys) {
            entries.push(formatKey(key) + ": " + inspectAny(value[key], compact, depth + 1, next));
        }
        for (const sym of Object.getOwnPropertySymbols(value)) {
            if (Object.prototype.propertyIsEnumerable.call(value, sym)) {
                entries.push("[" + String(sym) + "]: " + inspectAny(value[sym], compact, depth + 1, next));
            }
        }
    }
    const refIdx = circularRefs.get(value);
    if (refIdx !== undefined) prefix = "<ref *" + refIdx + "> " + prefix;
    if (entries.length === 0) return prefix + open + close;
    if (compact) return prefix + open + " " + entries.join(", ") + " " + close;
    const indent = "  ".repeat(depth + 1);
    return prefix + open + "\n" + indent + entries.join(",\n" + indent) + "\n" + "  ".repeat(depth) + close;
}

function formatKey(key) { return isIdentifierKey(key) ? key : inspectString(key); }
// Reset per top-level render: `<ref *n>` numbering restarts for every value.
function inspect(value) {
    circularRefs = new Map();
    return inspectAny(value, true, 0, []);
}
function inspectValue(value) {
    circularRefs = new Map();
    return inspectAny(value, false, 0, []);
}
// Node renders the *thrown* value at `depth: -1` when naming it in a message.
function inspectShallow(value) {
    if (value !== null && typeof value === "object") return Array.isArray(value) ? "[Array]" : "[Object]";
    return inspect(value);
}

function determineSpecificType(value) {
    if (value === null || value === undefined) return String(value);
    if (typeof value === "function" && value.name) return "function " + value.name;
    if (typeof value === "object") {
        const name = ctorNameOf(value);
        return name ? "an instance of " + name : inspect(value);
    }
    let inspected = inspect(value);
    if (inspected.length > 28) inspected = inspected.slice(0, 25) + "...";
    return "type " + typeof value + " (" + inspected + ")";
}

function codedTypeError(code, message) {
    const err = new TypeError(message);
    err.code = code;
    err.stack = "TypeError [" + code + "]: " + message;
    return err;
}
function invalidArgType(name, expectation, value) {
    return codedTypeError(
        "ERR_INVALID_ARG_TYPE",
        'The "' + name + '" argument must be ' + expectation + ". Received " + determineSpecificType(value)
    );
}
function invalidReturnValue(name, value) {
    return codedTypeError(
        "ERR_INVALID_RETURN_VALUE",
        'Expected instance of Promise to be returned from the "' + name +
        '" function but got ' + determineSpecificType(value) + "."
    );
}
function invalidArgValue(name, value, reason) {
    return codedTypeError(
        "ERR_INVALID_ARG_VALUE",
        "The argument '" + name + "' " + reason + ". Received " + inspect(value)
    );
}
function ambiguousArgument(name, details) {
    return codedTypeError("ERR_AMBIGUOUS_ARGUMENT", 'The "' + name + '" argument is ambiguous. ' + details);
}
function missingArgs() {
    return codedTypeError("ERR_MISSING_ARGS", 'The "actual" and "expected" arguments must be specified');
}

// ---------------------------------------------------------------------------
// Node's diff renderer (lib/internal/assert/myers_diff.js +
// lib/internal/assert/assertion_error.js, v22.12.0), ported verbatim modulo
// the ANSI colour escapes — chidori never writes to a TTY, so every
// `colors.*` primordial is the empty string here and the diff is the
// monochrome form vendored tests compare against.
//
// This has to be the *real* Myers diff: the vendored suite asserts on the
// exact interleaving of `+`/`-`/context lines, on the `... Skipped lines`
// collapse of runs longer than five, and on the trailing-comma equivalence
// that lets an object's last property line match across the two sides.
// ---------------------------------------------------------------------------

const kNopLinesToCollapse = 5;

function areLinesEqual(actual, expected, checkCommaDisparity) {
    if (actual === expected) return true;
    if (checkCommaDisparity) return actual + "," === expected || actual === expected + ",";
    return false;
}

function myersDiff(actual, expected, checkCommaDisparity) {
    const actualLength = actual.length;
    const expectedLength = expected.length;
    const max = actualLength + expectedLength;
    const v = new Array(2 * max + 1).fill(0);
    const trace = [];

    for (let diffLevel = 0; diffLevel <= max; diffLevel++) {
        trace.push(v.slice());
        for (let diagonalIndex = -diffLevel; diagonalIndex <= diffLevel; diagonalIndex += 2) {
            let x;
            if (diagonalIndex === -diffLevel ||
                (diagonalIndex !== diffLevel && v[diagonalIndex - 1 + max] < v[diagonalIndex + 1 + max])) {
                x = v[diagonalIndex + 1 + max];
            } else {
                x = v[diagonalIndex - 1 + max] + 1;
            }
            let y = x - diagonalIndex;
            while (x < actualLength && y < expectedLength &&
                   areLinesEqual(actual[x], expected[y], checkCommaDisparity)) {
                x++;
                y++;
            }
            v[diagonalIndex + max] = x;
            if (x >= actualLength && y >= expectedLength) {
                return myersBacktrack(trace, actual, expected, checkCommaDisparity);
            }
        }
    }
    return [];
}

function myersBacktrack(trace, actual, expected, checkCommaDisparity) {
    const actualLength = actual.length;
    const expectedLength = expected.length;
    const max = actualLength + expectedLength;
    let x = actualLength;
    let y = expectedLength;
    const result = [];

    for (let diffLevel = trace.length - 1; diffLevel >= 0; diffLevel--) {
        const v = trace[diffLevel];
        const diagonalIndex = x - y;
        let prevDiagonalIndex;
        if (diagonalIndex === -diffLevel ||
            (diagonalIndex !== diffLevel && v[diagonalIndex - 1 + max] < v[diagonalIndex + 1 + max])) {
            prevDiagonalIndex = diagonalIndex + 1;
        } else {
            prevDiagonalIndex = diagonalIndex - 1;
        }
        const prevX = v[prevDiagonalIndex + max];
        const prevY = prevX - prevDiagonalIndex;

        while (x > prevX && y > prevY) {
            const value = !checkCommaDisparity || actual[x - 1].endsWith(",")
                ? actual[x - 1]
                : expected[y - 1];
            result.push({ type: "nop", value });
            x--;
            y--;
        }
        if (diffLevel > 0) {
            if (x > prevX) {
                result.push({ type: "insert", value: actual[x - 1] });
                x--;
            } else {
                result.push({ type: "delete", value: expected[y - 1] });
                y--;
            }
        }
    }
    return result;
}

function printMyersDiff(diff) {
    let message = "";
    let skipped = false;
    let nopCount = 0;

    for (let diffIdx = diff.length - 1; diffIdx >= 0; diffIdx--) {
        const { type, value } = diff[diffIdx];
        const previousType = diffIdx < diff.length - 1 ? diff[diffIdx + 1].type : null;
        const typeChanged = previousType && type !== previousType;

        if (typeChanged && previousType === "nop") {
            // Avoid grouping if only one line would have been grouped otherwise.
            if (nopCount === kNopLinesToCollapse + 1) {
                message += "  " + diff[diffIdx + 1].value + "\n";
            } else if (nopCount === kNopLinesToCollapse + 2) {
                message += "  " + diff[diffIdx + 2].value + "\n";
                message += "  " + diff[diffIdx + 1].value + "\n";
            }
            if (nopCount >= kNopLinesToCollapse + 3) {
                message += "...\n";
                message += "  " + diff[diffIdx + 1].value + "\n";
                skipped = true;
            }
            nopCount = 0;
        }

        if (type === "insert") {
            message += "+ " + value + "\n";
        } else if (type === "delete") {
            message += "- " + value + "\n";
        } else {
            if (nopCount < kNopLinesToCollapse) message += "  " + value + "\n";
            nopCount++;
        }
    }
    return { message: "\n" + message.replace(/\s+$/, ""), skipped };
}

function checkOperator(actual, expected, operator) {
    if (operator === "strictEqual" &&
        ((typeof actual === "object" && actual !== null &&
          typeof expected === "object" && expected !== null) ||
         (typeof actual === "function" && typeof expected === "function"))) {
        return "strictEqualObject";
    }
    return operator;
}

// The indicator column is measured against an 80-column terminal: chidori has
// no TTY, which is exactly Node's non-TTY fallback width.
function getStackedDiff(actual, expected) {
    let message = "\n+ " + actual + "\n- " + expected;
    const stringsLen = actual.length + expected.length;
    if (stringsLen <= 80) {
        let indicatorIdx = -1;
        for (let i = 0; i < actual.length; i++) {
            if (actual[i] !== expected[i]) {
                // The first two characters are skipped: with the quotes, a
                // difference that early is already obvious.
                if (i >= 3) indicatorIdx = i;
                break;
            }
        }
        if (indicatorIdx !== -1) message += "\n" + " ".repeat(indicatorIdx + 2) + "^";
    }
    return { message };
}

function getSimpleDiff(originalActual, actual, originalExpected, expected) {
    let stringsLen = actual.length + expected.length;
    // Accounting for the quotes wrapping strings.
    if (typeof originalActual === "string") stringsLen -= 2;
    if (typeof originalExpected === "string") stringsLen -= 2;
    if (stringsLen <= kMaxShortStringLength && (originalActual !== 0 || originalExpected !== 0)) {
        return { message: actual + " !== " + expected, header: "" };
    }
    return getStackedDiff(actual, expected);
}

function isSimpleDiff(actual, inspectedActual, expected, inspectedExpected) {
    if (inspectedActual.length > 1 || inspectedExpected.length > 1) return false;
    return typeof actual !== "object" || actual === null ||
        typeof expected !== "object" || expected === null;
}

function createErrDiff(actual, expected, operator, customMessage) {
    operator = checkOperator(actual, expected, operator);

    let skipped = false;
    let message = "";
    const inspectedActual = inspectValue(actual);
    const inspectedExpected = inspectValue(expected);
    const splitActual = inspectedActual.split("\n");
    const splitExpected = inspectedExpected.split("\n");
    const showSimpleDiff = isSimpleDiff(actual, splitActual, expected, splitExpected);
    let header = "+ actual - expected";

    if (showSimpleDiff) {
        const simpleDiff = getSimpleDiff(actual, splitActual[0], expected, splitExpected[0]);
        message = simpleDiff.message;
        if (simpleDiff.header !== undefined) header = simpleDiff.header;
        if (simpleDiff.skipped) skipped = true;
    } else if (inspectedActual === inspectedExpected) {
        // Structurally identical, different references.
        operator = "notIdentical";
        if (splitActual.length > 50) {
            message = splitActual.slice(0, 50).join("\n") + "\n...}";
            skipped = true;
        } else {
            message = splitActual.join("\n");
        }
        header = "";
    } else {
        const checkCommaDisparity = actual !== null && actual !== undefined && typeof actual === "object";
        const printed = printMyersDiff(myersDiff(splitActual, splitExpected, checkCommaDisparity));
        message = printed.message;
        if (printed.skipped) skipped = true;
    }

    const headerMessage = (customMessage || kReadableOperator[operator]) + "\n" + header;
    return headerMessage + (skipped ? "\n... Skipped lines" : "") + "\n" + message + "\n";
}

function generateMessage(operator, actual, expected) {
    if (operator === "deepStrictEqual" || operator === "strictEqual") {
        return createErrDiff(actual, expected, operator);
    }
    if (operator === "notDeepStrictEqual" || operator === "notStrictEqual") {
        let base = kReadableOperator[operator];
        if (operator === "notStrictEqual" &&
            ((typeof actual === "object" && actual !== null) || typeof actual === "function")) {
            base = kReadableOperator.notStrictEqualObject;
        }
        const lines = inspectValue(actual).split("\n");
        if (lines.length > 50) {
            lines[46] = "...";
            while (lines.length > 47) lines.pop();
        }
        if (lines.length === 1) return base + (lines[0].length > 5 ? "\n\n" : " ") + lines[0];
        return base + "\n\n" + lines.join("\n") + "\n";
    }
    let res = inspectValue(actual);
    let other = inspectValue(expected);
    const known = kReadableOperator[operator];
    if (operator === "notDeepEqual" && res === other) {
        res = known + "\n\n" + res;
        if (res.length > 1024) res = res.slice(0, 1021) + "...";
        return res;
    }
    if (res.length > kMaxLongStringLength) res = res.slice(0, 509) + "...";
    if (other.length > kMaxLongStringLength) other = other.slice(0, 509) + "...";
    if (operator === "deepEqual") {
        res = known + "\n\n" + res + "\n\nshould loosely deep-equal\n\n";
    } else {
        const unequal = kReadableOperator[operator + "Unequal"];
        if (unequal) res = unequal + "\n\n" + res + "\n\nshould not loosely deep-equal\n\n";
        else other = " " + operator + " " + other;
    }
    return res + other;
}

class AssertionError extends Error {
    constructor(options) {
        if (options === null || typeof options !== "object") {
            throw invalidArgType("options", "of type object", options);
        }
        const operator = options.operator;
        const actual = options.actual;
        const expected = options.expected;
        const message = options.message;
        let text;
        if (message !== undefined && message !== null) {
            // Since v22 a *custom* message on the two equality operators still
            // renders the diff — it only replaces the "Expected values to be…"
            // headline.
            if (operator === "deepStrictEqual" || operator === "strictEqual") {
                text = createErrDiff(actual, expected, operator, String(message));
            } else {
                text = String(message);
            }
        } else {
            text = generateMessage(operator, actual, expected);
        }
        super(text);
        // Node computes this as `!message`, so an empty-string message still
        // counts as generated.
        const generatedMessage = !message;
        this.name = "AssertionError";
        this.code = "ERR_ASSERTION";
        this.generatedMessage = generatedMessage;
        this.actual = actual;
        this.expected = expected;
        this.operator = operator;
        // The engine builds `stack` once, at construction, from the message and
        // the *base* Error name; restate it so tests that match on `stack`
        // (`/Failed/`, `!stack.includes('at Function.throws')`) see Node's text.
        this.stack = "AssertionError [ERR_ASSERTION]: " + text;
        // …then hand the cut point to the engine so the assert internals below
        // `stackStartFn` never make it into the trace, exactly as Node does
        // (`ErrorCaptureStackTrace(this, stackStartFn || stackStartFunction)`).
        // Frames outside the assertion — the caller's own — still show up.
        const stackStartFn = options.stackStartFn || options.stackStartFunction;
        if (typeof Error.captureStackTrace === "function") {
            Error.captureStackTrace(this, stackStartFn);
            this.stack = "AssertionError [ERR_ASSERTION]: " + text;
        }
    }

    toString() {
        return this.name + " [" + this.code + "]: " + this.message;
    }

    // Node renders an AssertionError with `actual`/`expected` clipped: the
    // message already contains a combined view of both, so repeating either in
    // full would drown it. `depth: 0` keeps nested values shallow for the same
    // reason.
    [kCustomInspect](depth, ctx, inspectFn) {
        if (typeof inspectFn !== "function") return this.stack;
        const tmpActual = this.actual;
        const tmpExpected = this.expected;
        if (typeof this.actual === "string") this.actual = addEllipsis(this.actual);
        if (typeof this.expected === "string") this.expected = addEllipsis(this.expected);
        // Node passes `customInspect: false` to the nested call; shadowing the
        // hook with an own non-callable is the portable equivalent (and is
        // reverted before returning, so it never becomes observable).
        Object.defineProperty(this, kCustomInspect, {
            value: undefined, writable: true, enumerable: false, configurable: true,
        });
        try {
            return inspectFn(this, { depth: 0 });
        } finally {
            delete this[kCustomInspect];
            this.actual = tmpActual;
            this.expected = tmpExpected;
        }
    }
}

// Node's async assertions keep their own frame in the stack — only the
// synchronous internals are elided by `stackStartFn` — and vendored tests
// match on it (`assert.match(err.stack, /rejects/)`).
function markAsyncFrame(err, name) {
    if (err instanceof AssertionError && typeof err.stack === "string" &&
        err.stack.indexOf("at async " + name) === -1) {
        err.stack += "\n    at async " + name + " (node:internal/assert)";
    }
    return err;
}

function innerFail(options) {
    if (options.message instanceof Error) throw options.message;
    throw new AssertionError(options);
}

function ownEnumerableKeys(value) {
    const keys = Object.keys(value);
    for (const sym of Object.getOwnPropertySymbols(value)) {
        if (Object.prototype.propertyIsEnumerable.call(value, sym)) keys.push(sym);
    }
    return keys;
}

function bytesEqual(a, b) {
    if (a.byteLength !== b.byteLength) return false;
    const va = new Uint8Array(a.buffer || a, a.buffer ? a.byteOffset : 0, a.byteLength);
    const vb = new Uint8Array(b.buffer || b, b.buffer ? b.byteOffset : 0, b.byteLength);
    for (let i = 0; i < va.length; i++) if (va[i] !== vb[i]) return false;
    return true;
}

// `seenA`/`seenB` are parallel plain arrays rather than a Map: the snapshot
// policy keeps recursion state cheap to clone across the comparison.
function isDeepEqual(a, b, strict, seenA, seenB) {
    if (strict ? Object.is(a, b) : a === b) return true;

    const aObj = a !== null && typeof a === "object";
    const bObj = b !== null && typeof b === "object";
    if (!aObj || !bObj) {
        if (strict) return false;
        if (typeof a === "number" && typeof b === "number") {
            return Number.isNaN(a) && Number.isNaN(b);
        }
        return a == b;
    }

    if (strict && Object.getPrototypeOf(a) !== Object.getPrototypeOf(b)) return false;

    const tag = Object.prototype.toString.call(a);
    if (tag !== Object.prototype.toString.call(b)) return false;

    if (tag === "[object Date]") return Object.is(a.getTime(), b.getTime());
    if (tag === "[object RegExp]") return a.source === b.source && a.flags === b.flags;
    if (tag === "[object Number]" || tag === "[object String]" || tag === "[object Boolean]") {
        if (!Object.is(a.valueOf(), b.valueOf())) return false;
    }
    if (tag === "[object Symbol]" || tag === "[object BigInt]") {
        if (a.valueOf() !== b.valueOf()) return false;
    }
    if (isErrorValue(a) && (a.name !== b.name || a.message !== b.message)) return false;
    if (ArrayBuffer.isView(a)) return bytesEqual(a, b);
    if (tag === "[object ArrayBuffer]") return bytesEqual(a, b);
    if (Array.isArray(a) && a.length !== b.length) return false;

    seenA = seenA || [];
    seenB = seenB || [];
    const seenIndex = seenA.indexOf(a);
    if (seenIndex !== -1) return seenB[seenIndex] === b;
    const nextA = seenA.concat([a]);
    const nextB = seenB.concat([b]);

    if (a instanceof Map) {
        if (a.size !== b.size) return false;
        for (const entry of a) {
            if (b.has(entry[0])) {
                if (!isDeepEqual(entry[1], b.get(entry[0]), strict, nextA, nextB)) return false;
                continue;
            }
            let found = false;
            for (const other of b) {
                if (isDeepEqual(entry[0], other[0], strict, nextA, nextB) &&
                    isDeepEqual(entry[1], other[1], strict, nextA, nextB)) { found = true; break; }
            }
            if (!found) return false;
        }
    } else if (a instanceof Set) {
        if (a.size !== b.size) return false;
        for (const entry of a) {
            if (b.has(entry)) continue;
            let found = false;
            for (const other of b) {
                if (isDeepEqual(entry, other, strict, nextA, nextB)) { found = true; break; }
            }
            if (!found) return false;
        }
    }

    const ka = ownEnumerableKeys(a);
    const kb = ownEnumerableKeys(b);
    if (ka.length !== kb.length) return false;
    for (const key of ka) {
        if (!Object.prototype.propertyIsEnumerable.call(b, key)) return false;
        if (!isDeepEqual(a[key], b[key], strict, nextA, nextB)) return false;
    }
    return true;
}

function innerOk(argLen, value, message) {
    if (value) return;
    let generatedMessage = false;
    if (argLen === 0) {
        generatedMessage = true;
        message = "No value argument passed to `assert.ok()`";
    } else if (message instanceof Error) {
        throw message;
    } else if (message === undefined || message === null) {
        generatedMessage = true;
        message = undefined;
    }
    const err = new AssertionError({ actual: value, expected: true, message, operator: "==" });
    err.generatedMessage = generatedMessage;
    throw err;
}

function ok(...args) { innerOk(args.length, args[0], args[1]); }

function fail(actual, expected, message, operator) {
    const argsLen = arguments.length;
    let internalMessage;
    if (argsLen === 0) {
        internalMessage = "Failed";
    } else if (argsLen === 1) {
        message = actual;
        actual = undefined;
    } else if (argsLen === 2) {
        operator = "!=";
    }
    if (message instanceof Error) throw message;
    const err = new AssertionError({
        actual,
        expected,
        operator: operator === undefined ? "fail" : operator,
        message,
    });
    if (internalMessage !== undefined) {
        err.message = internalMessage;
        err.generatedMessage = true;
        err.stack = "AssertionError [ERR_ASSERTION]: " + internalMessage;
    }
    throw err;
}

function equal(actual, expected, message) {
    if (arguments.length < 2) throw missingArgs();
    if (actual != expected && (!Number.isNaN(actual) || !Number.isNaN(expected))) {
        innerFail({ actual, expected, message, operator: "==", stackStartFn: equal });
    }
}
function notEqual(actual, expected, message) {
    if (arguments.length < 2) throw missingArgs();
    if (actual == expected || (Number.isNaN(actual) && Number.isNaN(expected))) {
        innerFail({ actual, expected, message, operator: "!=", stackStartFn: notEqual });
    }
}
function strictEqual(actual, expected, message) {
    if (arguments.length < 2) throw missingArgs();
    if (!Object.is(actual, expected)) {
        innerFail({ actual, expected, message, operator: "strictEqual", stackStartFn: strictEqual });
    }
}
function notStrictEqual(actual, expected, message) {
    if (arguments.length < 2) throw missingArgs();
    if (Object.is(actual, expected)) {
        innerFail({ actual, expected, message, operator: "notStrictEqual", stackStartFn: notStrictEqual });
    }
}
function deepEqual(actual, expected, message) {
    if (arguments.length < 2) throw missingArgs();
    if (!isDeepEqual(actual, expected, false)) {
        innerFail({ actual, expected, message, operator: "deepEqual", stackStartFn: deepEqual });
    }
}
function notDeepEqual(actual, expected, message) {
    if (arguments.length < 2) throw missingArgs();
    if (isDeepEqual(actual, expected, false)) {
        innerFail({ actual, expected, message, operator: "notDeepEqual", stackStartFn: notDeepEqual });
    }
}
function deepStrictEqual(actual, expected, message) {
    if (arguments.length < 2) throw missingArgs();
    if (!isDeepEqual(actual, expected, true)) {
        innerFail({ actual, expected, message, operator: "deepStrictEqual", stackStartFn: deepStrictEqual });
    }
}
function notDeepStrictEqual(actual, expected, message) {
    if (arguments.length < 2) throw missingArgs();
    if (isDeepEqual(actual, expected, true)) {
        innerFail({ actual, expected, message, operator: "notDeepStrictEqual", stackStartFn: notDeepStrictEqual });
    }
}

class Comparison {
    constructor(obj, keys, actual) {
        for (const key of keys) {
            if (!(key in obj)) continue;
            if (actual !== undefined && typeof actual[key] === "string" &&
                obj[key] instanceof RegExp && obj[key].test(actual[key])) {
                this[key] = actual[key];
            } else {
                this[key] = obj[key];
            }
        }
    }
}

function compareExceptionKey(actual, expected, key, message, keys, operator, stackStartFn) {
    if (key in actual && isDeepEqual(actual[key], expected[key], true)) return;
    if (!message) {
        const err = new AssertionError({
            actual: new Comparison(actual, keys),
            expected: new Comparison(expected, keys, actual),
            operator: "deepStrictEqual",
            stackStartFn,
        });
        err.actual = actual;
        err.expected = expected;
        err.operator = operator;
        throw err;
    }
    innerFail({ actual, expected, message, operator, stackStartFn });
}

function expectedException(actual, expected, message, operator, stackStartFn) {
    let generatedMessage = false;
    let throwError = false;

    if (typeof expected !== "function") {
        if (expected instanceof RegExp) {
            const str = String(actual);
            if (expected.test(str)) return true;
            if (!message) {
                generatedMessage = true;
                message = "The input did not match the regular expression " + String(expected) +
                    ". Input:\n\n" + inspect(str) + "\n";
            }
            throwError = true;
        } else if (typeof actual !== "object" || actual === null) {
            const err = new AssertionError({ actual, expected, message, operator: "deepStrictEqual", stackStartFn });
            err.operator = operator;
            throw err;
        } else {
            const keys = Object.keys(expected);
            if (expected instanceof Error) keys.push("name", "message");
            else if (keys.length === 0) throw invalidArgValue("error", expected, "may not be an empty object");
            for (const key of keys) {
                if (typeof actual[key] === "string" && expected[key] instanceof RegExp &&
                    expected[key].test(actual[key])) continue;
                compareExceptionKey(actual, expected, key, message, keys, operator, stackStartFn);
            }
            return true;
        }
    } else if (expected.prototype !== undefined && actual instanceof expected) {
        return true;
    } else if (Object.prototype.isPrototypeOf.call(Error, expected)) {
        if (!message) {
            generatedMessage = true;
            message = 'The error is expected to be an instance of "' + expected.name + '". Received ';
            if (isErrorValue(actual)) {
                const name = (actual.constructor && actual.constructor.name) || actual.name;
                if (expected.name === name) {
                    message += "an error with identical name but a different prototype.";
                } else {
                    message += '"' + name + '"';
                }
                message += "\n\nError message:\n\n" + actual.message;
            } else {
                message += '"' + inspectShallow(actual) + '"';
            }
        }
        throwError = true;
    } else {
        const res = expected.call({}, actual);
        if (res !== true) {
            if (!message) {
                generatedMessage = true;
                message = (expected.name ? 'The "' + expected.name + '" validation function' : "The validation function") +
                    ' is expected to return "true". Received ' + inspect(res);
                if (isErrorValue(actual)) message += "\n\nCaught error:\n\n" + String(actual);
            }
            throwError = true;
        }
    }

    if (throwError) {
        const err = new AssertionError({ actual, expected, message, operator, stackStartFn });
        err.generatedMessage = generatedMessage;
        throw err;
    }
    return true;
}

function expectsError(operator, actual, args, stackStartFn) {
    let error = args[0];
    let message = args[1];
    if (typeof error === "string") {
        if (args.length === 2) {
            throw invalidArgType("error", "of type function or an instance of Error, RegExp, or Object", error);
        }
        if (typeof actual === "object" && actual !== null) {
            if (actual.message === error) {
                throw ambiguousArgument("error/message",
                    'The error message "' + actual.message + '" is identical to the message.');
            }
        } else if (actual === error) {
            throw ambiguousArgument("error/message",
                'The error "' + actual + '" is identical to the message.');
        }
        message = error;
        error = undefined;
    } else if (error !== null && error !== undefined &&
               typeof error !== "object" && typeof error !== "function") {
        throw invalidArgType("error", "of type function or an instance of Error, RegExp, or Object", error);
    }

    if (actual === kNoException) {
        let details = "";
        if (error && error.name) details += " (" + error.name + ")";
        details += message ? ": " + message : ".";
        innerFail({
            actual: undefined,
            expected: error,
            operator,
            stackStartFn,
            message: "Missing expected " + (operator === "rejects" ? "rejection" : "exception") + details,
        });
    }
    if (!error) return;
    expectedException(actual, error, message, operator, stackStartFn);
}

// `doesNotThrow`/`doesNotReject` need a *predicate*, not the throwing matcher:
// a non-matching error is re-thrown untouched, a matching one becomes an
// AssertionError. Only functions and regular expressions are legal here.
function matchesUnwanted(actual, expected) {
    if (expected instanceof RegExp) return expected.test(String(actual));
    if (typeof expected !== "function") {
        throw invalidArgType("expected", "of type function or an instance of RegExp", expected);
    }
    if (expected.prototype !== undefined && actual instanceof expected) return true;
    if (Object.prototype.isPrototypeOf.call(Error, expected)) return false;
    return expected.call({}, actual) === true;
}

function expectsNoError(operator, actual, args, stackStartFn) {
    if (actual === kNoException) return;
    let error = args[0];
    let message = args[1];
    if (typeof error === "string") {
        message = error;
        error = undefined;
    }
    if (!error || matchesUnwanted(actual, error)) {
        innerFail({
            actual,
            expected: error,
            operator,
            stackStartFn,
            message: "Got unwanted " + (operator === "doesNotReject" ? "rejection" : "exception") +
                (message ? ": " + message : ".") +
                '\nActual message: "' + (actual && actual.message) + '"',
        });
    }
    throw actual;
}

function getActual(fn) {
    if (typeof fn !== "function") throw invalidArgType("fn", "of type function", fn);
    try { fn(); } catch (err) { return err; }
    return kNoException;
}

function isPromiseLike(value) {
    return value !== null && typeof value === "object" &&
        typeof value.then === "function" && typeof value.catch === "function";
}

async function waitForActual(promiseFn) {
    let result;
    if (typeof promiseFn === "function") {
        result = promiseFn();
        if (!isPromiseLike(result)) throw invalidReturnValue("promiseFn", result);
    } else if (isPromiseLike(promiseFn)) {
        result = promiseFn;
    } else {
        throw invalidArgType("promiseFn", "of type function or an instance of Promise", promiseFn);
    }
    try { await result; } catch (err) { return err; }
    return kNoException;
}

function throws(fn, ...args) { expectsError("throws", getActual(fn), args, throws); }
function doesNotThrow(fn, ...args) { expectsNoError("doesNotThrow", getActual(fn), args, doesNotThrow); }

async function rejects(promiseFn, ...args) {
    try {
        expectsError("rejects", await waitForActual(promiseFn), args, rejects);
    } catch (err) {
        throw markAsyncFrame(err, "rejects");
    }
}

async function doesNotReject(promiseFn, ...args) {
    try {
        expectsNoError("doesNotReject", await waitForActual(promiseFn), args, doesNotReject);
    } catch (err) {
        throw markAsyncFrame(err, "doesNotReject");
    }
}

function internalMatch(string, regexp, message, shouldMatch, operator) {
    if (!(regexp instanceof RegExp)) throw invalidArgType("regexp", "an instance of RegExp", regexp);
    const matched = typeof string === "string" && regexp.test(string);
    if (typeof string === "string" && matched === shouldMatch) return;
    if (message instanceof Error) throw message;
    const generatedMessage = !message;
    if (!message) {
        if (typeof string !== "string") {
            message = 'The "string" argument must be of type string. Received type ' +
                typeof string + " (" + inspect(string) + ")";
        } else if (shouldMatch) {
            message = "The input did not match the regular expression " + String(regexp) +
                ". Input:\n\n" + inspect(string) + "\n";
        } else {
            message = "The input was expected to not match the regular expression " + String(regexp) +
                ". Input:\n\n" + inspect(string) + "\n";
        }
    }
    const err = new AssertionError({ actual: string, expected: regexp, message, operator });
    err.generatedMessage = generatedMessage;
    throw err;
}

function match(string, regexp, message) { internalMatch(string, regexp, message, true, "match"); }
function doesNotMatch(string, regexp, message) { internalMatch(string, regexp, message, false, "doesNotMatch"); }

function ifError(value) {
    if (value === null || value === undefined) return;
    const message = "ifError got unwanted exception: " +
        (isErrorValue(value) ? (value.message || String(value)) : inspect(value));
    const err = new AssertionError({ actual: value, expected: null, operator: "ifError", message });
    err.generatedMessage = true;
    throw err;
}

function assert(...args) { innerOk(args.length, args[0], args[1]); }
assert.ok = ok;
assert.fail = fail;
assert.equal = equal;
assert.notEqual = notEqual;
assert.strictEqual = strictEqual;
assert.notStrictEqual = notStrictEqual;
assert.deepEqual = deepEqual;
assert.notDeepEqual = notDeepEqual;
assert.deepStrictEqual = deepStrictEqual;
assert.notDeepStrictEqual = notDeepStrictEqual;
assert.throws = throws;
assert.doesNotThrow = doesNotThrow;
assert.rejects = rejects;
assert.doesNotReject = doesNotReject;
assert.match = match;
assert.doesNotMatch = doesNotMatch;
assert.ifError = ifError;
assert.AssertionError = AssertionError;

// `assert.strict` is a distinct namespace whose loose aliases point at the
// strict implementations (Node exposes exactly the same key set on both).
function strict(...args) { innerOk(args.length, args[0], args[1]); }
Object.assign(strict, assert);
strict.equal = strictEqual;
strict.notEqual = notStrictEqual;
strict.deepEqual = deepStrictEqual;
strict.notDeepEqual = notDeepStrictEqual;
strict.strict = strict;
assert.strict = strict;

export { ok, fail, equal, notEqual, strictEqual, notStrictEqual, deepEqual, notDeepEqual, deepStrictEqual, notDeepStrictEqual, throws, doesNotThrow, rejects, doesNotReject, match, doesNotMatch, ifError, AssertionError, strict };
export default assert;
"#;

// node:assert/strict exposes `assert.strict` as its default and binds the
// loose names to the strict implementations. Named re-exports are spelled out
// because the bundler does not support `export *`.
const ASSERT_STRICT_SHIM: &str = r#"
import assert from "node:assert";
import { ok, fail, strictEqual, notStrictEqual, deepStrictEqual, notDeepStrictEqual, throws, doesNotThrow, rejects, doesNotReject, match, doesNotMatch, ifError, AssertionError } from "node:assert";

const strict = assert.strict;
const equal = strictEqual;
const notEqual = notStrictEqual;
const deepEqual = deepStrictEqual;
const notDeepEqual = notDeepStrictEqual;

export { ok, fail, equal, notEqual, strictEqual, notStrictEqual, deepEqual, notDeepEqual, deepStrictEqual, notDeepStrictEqual, throws, doesNotThrow, rejects, doesNotReject, match, doesNotMatch, ifError, AssertionError, strict };
export default strict;
"#;

// node:os shim. The host's real OS details are nondeterministic, so — exactly
// like the `process` shim's fixed `platform`/`versions` — every value here is a
// FIXED virtualized constant. Nothing reads the real machine, so two runs (and
// record/replay) agree byte-for-byte.
const OS_SHIM: &str = r#"
const EOL = "\n";
function platform() { return "chidori"; }
function type() { return "Chidori"; }
function arch() { return "wasm32"; }
function release() { return "0.0.0-chidori"; }
function version() { return "Chidori Deterministic Runtime"; }
function hostname() { return "chidori"; }
function homedir() { return "/"; }
function tmpdir() { return "/tmp"; }
function endianness() { return "LE"; }
function cpus() { return []; }
function networkInterfaces() { return {}; }
function userInfo() { return { username: "chidori", uid: -1, gid: -1, shell: null, homedir: "/" }; }
// Fixed values: real memory/load/uptime would leak host state into the run.
function totalmem() { return 0; }
function freemem() { return 0; }
function uptime() { return 0; }
function loadavg() { return [0, 0, 0]; }
function availableParallelism() { return 1; }
const constants = Object.freeze({ signals: {}, errno: {}, priority: {} });

const os = {
    EOL, platform, type, arch, release, version, hostname, homedir, tmpdir,
    endianness, cpus, networkInterfaces, userInfo, totalmem, freemem, uptime,
    loadavg, availableParallelism, constants,
};
export { EOL, platform, type, arch, release, version, hostname, homedir, tmpdir, endianness, cpus, networkInterfaces, userInfo, totalmem, freemem, uptime, loadavg, availableParallelism, constants };
export default os;
"#;

/// Return the synthetic builtin source for a resolved path that lives under
/// `__node_builtins__/`, or `None` if the path doesn't match. The bundler
/// uses this to short-circuit a filesystem read for builtin shim paths.
pub fn source_for(path: &Path) -> Option<&'static str> {
    shim_source(&builtin_name_from_path(path)?)
}

/// Return the synthetic builtin source for a `node:` builtin *name* (e.g.
/// `"crypto"` or `"fs/promises"`), or `None` if the name isn't an allowlisted
/// builtin. The module loader serves `node:` specifiers straight from this by
/// name; [`source_for`] is the by-path wrapper for the synthetic
/// `__node_builtins__/` resolved paths.
pub fn shim_source(name: &str) -> Option<&'static str> {
    match name {
        "process" => Some(PROCESS_SHIM),
        "buffer" => Some(BUFFER_SHIM),
        "util" => Some(UTIL_SHIM),
        "fs" => Some(FS_SHIM),
        "fs/promises" => Some(FS_PROMISES_SHIM),
        "crypto" => Some(CRYPTO_SHIM),
        "http" => Some(HTTP_SHIM.as_str()),
        "https" => Some(HTTPS_SHIM),
        "path" => Some(PATH_SHIM),
        "path/posix" => Some(PATH_POSIX_SHIM),
        "events" => Some(EVENTS_SHIM),
        "url" => Some(URL_SHIM),
        "assert" => Some(ASSERT_SHIM),
        "assert/strict" => Some(ASSERT_STRICT_SHIM),
        "os" => Some(OS_SHIM),
        // Everything else in the Node builtin suite lives in builtins_compat.
        other => crate::runtime::typescript::builtins_compat::compat_shim_source(other),
    }
}

/// Return the builtin name (e.g. `"process"` or `"fs/promises"`) if `path`
/// points under the synthetic builtin directory. Matches paths regardless of
/// their workspace prefix so callers don't need to know the resolver's root.
/// Multi-segment names (`fs/promises`) are reconstructed from everything after
/// the `__node_builtins__` component, with the `.js` suffix stripped.
pub fn builtin_name_from_path(path: &Path) -> Option<String> {
    let mut segments: Vec<String> = Vec::new();
    let mut found_root = false;
    for component in path.components() {
        let part = component.as_os_str().to_str()?;
        if found_root {
            segments.push(part.to_string());
        } else if part == "__node_builtins__" {
            found_root = true;
        }
    }
    if !found_root || segments.is_empty() {
        return None;
    }
    let joined = segments.join("/");
    let name = joined.strip_suffix(".js")?;
    Some(name.to_string())
}

// ---------------------------------------------------------------------------
// Vendored packages: self-contained UMD bundles served as synthetic ES modules.
//
// npm `react` / `react-dom` are CommonJS (internal `require`), which the
// ESM-only engine can't link. The official UMD builds are self-contained, so we
// wrap them in an ESM shim that runs the bundle (which populates `globalThis`)
// and re-exports — making `import React from 'react'` and
// `import { renderToStaticMarkup } from 'react-dom/server'` resolve and link.
// This is analogous to the `node:` builtin shims above.
// ---------------------------------------------------------------------------

const REACT_UMD: &str = include_str!("../vendor/react/react.js");
const REACT_DOM_SERVER_UMD: &str = include_str!("../vendor/react/react-dom-server.js");

/// True for bare specifiers served from the vendored-package registry.
pub fn is_vendored_package(specifier: &str) -> bool {
    matches!(
        specifier,
        "react" | "react-dom" | "react-dom/server" | "react-dom/server.browser"
    )
}

/// Resolve a vendored bare specifier to `(module_key, esm_source)`, or `None`.
/// The key is stable so the module graph evaluates each bundle exactly once.
pub fn vendored_module(specifier: &str) -> Option<(String, String)> {
    match specifier {
        "react" | "react-dom" => Some((
            "vendor:react".to_string(),
            format!(
                "globalThis.self = globalThis; globalThis.global = globalThis;\n\
                 {REACT_UMD}\n\
                 const __R = globalThis.React;\n\
                 export default __R;\n\
                 export const createElement = __R.createElement,\n\
                   cloneElement = __R.cloneElement, createContext = __R.createContext,\n\
                   Fragment = __R.Fragment, Children = __R.Children,\n\
                   Component = __R.Component, PureComponent = __R.PureComponent,\n\
                   memo = __R.memo, forwardRef = __R.forwardRef,\n\
                   isValidElement = __R.isValidElement, version = __R.version,\n\
                   useState = __R.useState, useEffect = __R.useEffect,\n\
                   useLayoutEffect = __R.useLayoutEffect, useMemo = __R.useMemo,\n\
                   useRef = __R.useRef, useCallback = __R.useCallback,\n\
                   useContext = __R.useContext, useReducer = __R.useReducer,\n\
                   useId = __R.useId;\n"
            ),
        )),
        "react-dom/server" | "react-dom/server.browser" => Some((
            "vendor:react-dom/server".to_string(),
            format!(
                "import 'react';\n\
                 {REACT_DOM_SERVER_UMD}\n\
                 const __S = globalThis.ReactDOMServer;\n\
                 export default __S;\n\
                 export const renderToString = __S.renderToString,\n\
                   renderToStaticMarkup = __S.renderToStaticMarkup;\n"
            ),
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn matches_builtin_path() {
        let path = PathBuf::from("/some/workspace/__node_builtins__/process.js");
        assert_eq!(builtin_name_from_path(&path).as_deref(), Some("process"));
        assert!(source_for(&path).unwrap().contains("globalThis.process"));
    }

    #[test]
    fn matches_nested_builtin_path() {
        let path = PathBuf::from("/ws/__node_builtins__/fs/promises.js");
        assert_eq!(
            builtin_name_from_path(&path).as_deref(),
            Some("fs/promises")
        );
        assert!(source_for(&path).unwrap().contains("from \"node:fs\""));
    }

    #[test]
    fn fs_shim_is_registered() {
        let path = PathBuf::from("/ws/__node_builtins__/fs.js");
        assert_eq!(builtin_name_from_path(&path).as_deref(), Some("fs"));
        assert!(source_for(&path).unwrap().contains("__chidori_fs_read"));
    }

    #[test]
    fn http_and_https_shims_are_registered_and_route_through_chidori_http() {
        let http = PathBuf::from("/ws/__node_builtins__/http.js");
        assert_eq!(builtin_name_from_path(&http).as_deref(), Some("http"));
        // The shim must perform requests via the captured networking host op
        // (the same one `globalThis.fetch` uses), never a public `chidori.http`.
        assert!(source_for(&http)
            .unwrap()
            .contains("globalThis.__chidori_http"));
        assert!(!source_for(&http)
            .unwrap()
            .contains("globalThis.chidori.http"));

        let https = PathBuf::from("/ws/__node_builtins__/https.js");
        assert_eq!(builtin_name_from_path(&https).as_deref(), Some("https"));
        // node:https reuses node:http's implementation.
        assert!(source_for(&https).unwrap().contains("from \"node:http\""));

        // Both names are in the import allowlist so the resolver accepts them.
        assert!(BUILTIN_NAMES.contains(&"http"));
        assert!(BUILTIN_NAMES.contains(&"https"));
    }

    #[test]
    fn path_events_url_assert_os_shims_are_registered() {
        for name in [
            "path",
            "path/posix",
            "events",
            "url",
            "assert",
            "assert/strict",
            "os",
        ] {
            assert!(
                shim_source(name).is_some(),
                "shim_source missing for node:{name}"
            );
            assert!(
                BUILTIN_NAMES.contains(&name),
                "BUILTIN_NAMES missing node:{name}"
            );
        }
        // Allowlist (transpile.rs) and BUILTIN_NAMES (here) must stay in sync,
        // and every allowlisted name must have a shim source.
        for name in crate::runtime::typescript::transpile::NODE_BUILTIN_ALLOWLIST {
            assert!(
                BUILTIN_NAMES.contains(name),
                "allowlist/BUILTIN_NAMES mismatch: {name}"
            );
            assert!(shim_source(name).is_some(), "no shim source for {name}");
        }
        // node:path exposes a self-aliasing posix object; node:os virtualizes
        // platform like node:process does.
        assert!(shim_source("path").unwrap().contains("posix.posix = posix"));
        assert!(shim_source("os").unwrap().contains("\"chidori\""));
        assert!(shim_source("url")
            .unwrap()
            .contains("class URLSearchParams"));
        // ES5-style constructor: `EventEmitter.call(this)` must keep working.
        assert!(shim_source("events")
            .unwrap()
            .contains("function EventEmitter"));
    }

    #[test]
    fn non_builtin_path_is_none() {
        let path = PathBuf::from("/some/workspace/src/index.ts");
        assert_eq!(builtin_name_from_path(&path), None);
        assert_eq!(source_for(&path), None);
    }

    #[test]
    fn compat_suite_shims_are_registered() {
        // Functional implementations route through the expected machinery…
        assert!(shim_source("stream").unwrap().contains("class Readable"));
        assert!(shim_source("stream/promises")
            .unwrap()
            .contains("from \"node:stream\""));
        assert!(shim_source("querystring").unwrap().contains("function parse"));
        assert!(shim_source("string_decoder")
            .unwrap()
            .contains("function StringDecoder"));
        assert!(shim_source("punycode").unwrap().contains("xn--"));
        assert!(shim_source("timers/promises")
            .unwrap()
            .contains("globalThis.setTimeout"));
        assert!(shim_source("async_hooks")
            .unwrap()
            .contains("class AsyncLocalStorage"));
        assert!(shim_source("module").unwrap().contains("builtinModules"));
        // …the module shim's builtin list is spliced from the live allowlist…
        assert!(shim_source("module").unwrap().contains("\"stream\""));
        // …zlib (deflate/gzip AND brotli families) routes through the
        // compression native…
        assert!(shim_source("zlib").unwrap().contains("__chidori_zlib"));
        assert!(shim_source("zlib").unwrap().contains("brotliCompress"));
        assert!(shim_source("zlib")
            .unwrap()
            .contains("BROTLI_PARAM_QUALITY"));
        // …and capability stubs fail loud, not silent.
        // (node:vm is *not* in this list: it is a functional same-realm shim
        // built on the engine's own `eval`, checked by
        // `run_agent_node_vm_same_realm_contexts`. Only the two surfaces with
        // no counterpart — `measureMemory` and `SourceTextModule` — fail loud.)
        assert!(shim_source("vm").unwrap().contains("with (globalThis."));
        assert!(shim_source("vm")
            .unwrap()
            .contains("vm.SourceTextModule is not supported in the Chidori runtime"));
        for name in ["child_process", "tls", "wasi", "dgram"] {
            assert!(
                shim_source(name)
                    .unwrap()
                    .contains("not supported in the Chidori runtime"),
                "stub for {name} must throw a clear unsupported error"
            );
        }
        // The by-path lookup works for compat shims too (snapshot bundler).
        let path = PathBuf::from("/ws/__node_builtins__/stream/promises.js");
        assert_eq!(
            builtin_name_from_path(&path).as_deref(),
            Some("stream/promises")
        );
        assert!(source_for(&path).is_some());
    }
}
