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
    if (t === "function") return " Received function " + (input.name || "(anonymous)");
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
// node:util shim. `inspect` delegates to JSON.stringify with a fallback for
// circular structures; `promisify` supports the custom-symbol override;
// `format` covers the printf-ish specifiers packages actually use. The type
// predicates live in node:util/types and are re-exported as `types`.
import types from "node:util/types";

function inspect(value) {
    if (typeof value === "string") return value;
    try { return JSON.stringify(value); } catch { return String(value); }
}
function format(f, ...args) {
    if (typeof f !== "string") {
        const all = [f, ...args];
        const parts = [];
        for (const a of all) parts.push(inspect(a));
        return parts.join(" ");
    }
    let i = 0;
    let out = f.replace(/%[sdijfoOc%]/g, (spec) => {
        if (spec === "%%") return "%";
        if (i >= args.length) return spec;
        const a = args[i++];
        switch (spec) {
            case "%s": return typeof a === "string" ? a : inspect(a);
            case "%d": return String(Number(a));
            case "%i": return String(parseInt(a, 10));
            case "%f": return String(parseFloat(a));
            case "%j":
                try { return JSON.stringify(a); } catch { return "[Circular]"; }
            case "%o":
            case "%O": return inspect(a);
            case "%c": return "";
            default: return spec;
        }
    });
    for (; i < args.length; i++) out += " " + inspect(args[i]);
    return out;
}
function promisify(fn) {
    if (typeof fn !== "function") throw new TypeError("promisify expects a function");
    const custom = fn[promisify.custom];
    if (typeof custom === "function") return custom;
    return function (...args) {
        return new Promise((resolve, reject) => {
            try {
                fn.call(this, ...args, (err, value) => err ? reject(err) : resolve(value));
            } catch (e) { reject(e); }
        });
    };
}
promisify.custom = Symbol.for("nodejs.util.promisify.custom");
function callbackify(fn) {
    if (typeof fn !== "function") throw new TypeError("callbackify expects a function");
    return function (...args) {
        const cb = args.pop();
        Promise.resolve(fn.apply(this, args)).then(
            (value) => queueMicrotask(() => cb(null, value)),
            (err) => queueMicrotask(() => cb(err || new Error("rejected with falsy value")))
        );
    };
}
function deprecate(fn, message) {
    let warned = false;
    return function (...args) {
        if (!warned) {
            warned = true;
            if (globalThis.console && typeof globalThis.console.warn === "function") {
                globalThis.console.warn("DeprecationWarning: " + message);
            }
        }
        return fn.apply(this, args);
    };
}
function inherits(ctor, superCtor) {
    ctor.super_ = superCtor;
    Object.setPrototypeOf(ctor.prototype, superCtor.prototype);
}
function isDeepStrictEqual(a, b) {
    if (Object.is(a, b)) return true;
    if (a === null || b === null || typeof a !== "object" || typeof b !== "object") {
        return false;
    }
    if (a instanceof Date || b instanceof Date) {
        return a instanceof Date && b instanceof Date && a.getTime() === b.getTime();
    }
    if (a instanceof RegExp || b instanceof RegExp) {
        return a instanceof RegExp && b instanceof RegExp && a.source === b.source && a.flags === b.flags;
    }
    if (Array.isArray(a) !== Array.isArray(b)) return false;
    const ka = Object.keys(a);
    const kb = Object.keys(b);
    if (ka.length !== kb.length) return false;
    for (const k of ka) {
        if (!Object.prototype.hasOwnProperty.call(b, k)) return false;
        if (!isDeepStrictEqual(a[k], b[k])) return false;
    }
    return true;
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
    inspect, format, promisify, callbackify, deprecate, inherits, types,
    isDeepStrictEqual, debuglog, TextEncoder, TextDecoder,
    isArray, isBoolean, isNull, isNullOrUndefined, isNumber, isString,
    isSymbol, isUndefined, isObject, isFunction, isPrimitive, isRegExp,
    isDate, isError, isBuffer,
};
export default {
    inspect, format, promisify, callbackify, deprecate, inherits, types,
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

// node:path shim. Pure logic, posix-style only (the chidori VFS is posix). The
// implementation mirrors Node's `path.posix`, and `path.posix` is exported as a
// self-alias so `import { posix } from "node:path"` hands back the same module.
const PATH_SHIM: &str = r#"
const sep = "/";
const delimiter = ":";

function assertString(value, name) {
    if (typeof value !== "string") {
        throw new TypeError(`Path "${name}" must be a string. Received ${typeof value}`);
    }
}

// Normalize an array of path segments, resolving "." and "..". `allowAboveRoot`
// keeps leading ".." segments for relative paths.
function normalizeArray(parts, allowAboveRoot) {
    const res = [];
    for (const p of parts) {
        if (p === "" || p === ".") continue;
        if (p === "..") {
            if (res.length && res[res.length - 1] !== "..") res.pop();
            else if (allowAboveRoot) res.push("..");
        } else {
            res.push(p);
        }
    }
    return res;
}

function normalize(path) {
    assertString(path, "path");
    if (path.length === 0) return ".";
    const isAbsolute = path.charCodeAt(0) === 47;
    const trailingSep = path.charCodeAt(path.length - 1) === 47;
    let normalized = normalizeArray(path.split("/"), !isAbsolute).join("/");
    if (!normalized && !isAbsolute) normalized = ".";
    if (normalized && trailingSep) normalized += "/";
    return (isAbsolute ? "/" : "") + normalized;
}

function isAbsolute(path) {
    assertString(path, "path");
    return path.length > 0 && path.charCodeAt(0) === 47;
}

function join(...parts) {
    if (parts.length === 0) return ".";
    let joined;
    for (const part of parts) {
        assertString(part, "path");
        if (part.length > 0) {
            joined = joined === undefined ? part : joined + "/" + part;
        }
    }
    if (joined === undefined) return ".";
    return normalize(joined);
}

function resolve(...parts) {
    let resolved = "";
    let isAbsoluteAcc = false;
    for (let i = parts.length - 1; i >= -1 && !isAbsoluteAcc; i--) {
        const path = i >= 0 ? parts[i] : "/";
        assertString(path, "path");
        if (path.length === 0) continue;
        resolved = path + "/" + resolved;
        isAbsoluteAcc = path.charCodeAt(0) === 47;
    }
    const normalized = normalizeArray(resolved.split("/"), !isAbsoluteAcc).join("/");
    if (isAbsoluteAcc) return "/" + normalized;
    return normalized.length > 0 ? normalized : ".";
}

function dirname(path) {
    assertString(path, "path");
    if (path.length === 0) return ".";
    const hasRoot = path.charCodeAt(0) === 47;
    let end = -1;
    let matchedSlash = true;
    for (let i = path.length - 1; i >= 1; i--) {
        if (path.charCodeAt(i) === 47) {
            if (!matchedSlash) { end = i; break; }
        } else {
            matchedSlash = false;
        }
    }
    if (end === -1) return hasRoot ? "/" : ".";
    if (hasRoot && end === 1) return "//";
    return path.slice(0, end);
}

function basename(path, ext) {
    assertString(path, "path");
    if (ext !== undefined) assertString(ext, "ext");
    let start = 0;
    let end = -1;
    let matchedSlash = true;
    for (let i = path.length - 1; i >= 0; i--) {
        if (path.charCodeAt(i) === 47) {
            if (!matchedSlash) { start = i + 1; break; }
        } else {
            if (end === -1) { matchedSlash = false; end = i + 1; }
        }
    }
    if (end === -1) return "";
    let base = path.slice(start, end);
    if (ext && base.endsWith(ext) && base !== ext) {
        base = base.slice(0, base.length - ext.length);
    }
    return base;
}

function extname(path) {
    assertString(path, "path");
    let startDot = -1;
    let startPart = 0;
    let end = -1;
    let matchedSlash = true;
    let preDotState = 0;
    for (let i = path.length - 1; i >= 0; i--) {
        const code = path.charCodeAt(i);
        if (code === 47) {
            if (!matchedSlash) { startPart = i + 1; break; }
            continue;
        }
        if (end === -1) { matchedSlash = false; end = i + 1; }
        if (code === 46) {
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
}

function relative(from, to) {
    assertString(from, "from");
    assertString(to, "to");
    if (from === to) return "";
    from = resolve(from);
    to = resolve(to);
    if (from === to) return "";
    const fromParts = from.split("/").filter((p) => p.length);
    const toParts = to.split("/").filter((p) => p.length);
    let i = 0;
    while (i < fromParts.length && i < toParts.length && fromParts[i] === toParts[i]) i++;
    const up = [];
    for (let j = i; j < fromParts.length; j++) up.push("..");
    return up.concat(toParts.slice(i)).join("/");
}

function parse(path) {
    assertString(path, "path");
    const root = isAbsolute(path) ? "/" : "";
    const dir = dirname(path);
    const base = basename(path);
    const ext = extname(path);
    const name = ext ? base.slice(0, base.length - ext.length) : base;
    return { root, dir: dir === "." && root === "" ? "" : dir, base, ext, name };
}

function format(obj) {
    if (obj === null || typeof obj !== "object") {
        throw new TypeError("Parameter 'pathObject' must be an object");
    }
    const dir = obj.dir || obj.root || "";
    const base = obj.base || ((obj.name || "") + (obj.ext || ""));
    if (!dir) return base;
    if (dir === obj.root) return dir + base;
    return dir + "/" + base;
}

const posix = {
    sep, delimiter, normalize, isAbsolute, join, resolve, dirname, basename,
    extname, relative, parse, format,
};
posix.posix = posix;
posix.win32 = posix;

export { sep, delimiter, normalize, isAbsolute, join, resolve, dirname, basename, extname, relative, parse, format, posix };
export const win32 = posix;
export default posix;
"#;

// node:path/posix re-exports node:path (which is already posix-style). Named
// re-exports are spelled out because the bundler does not support `export *`.
const PATH_POSIX_SHIM: &str = r#"
import path from "node:path";
export { sep, delimiter, normalize, isAbsolute, join, resolve, dirname, basename, extname, relative, parse, format, posix, win32 } from "node:path";
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
function invalidArg(message) {
    const err = new TypeError(message);
    err.code = "ERR_INVALID_ARG_TYPE";
    return err;
}
function checkListener(listener) {
    if (typeof listener !== "function") {
        throw invalidArg('The "listener" argument must be of type function');
    }
}
function ensureEvents(target) {
    if (target._events === undefined) {
        target._events = Object.create(null);
    }
    return target._events;
}

function EventEmitter(opts) {
    EventEmitter.init.call(this, opts);
}
EventEmitter.init = function init() {
    if (this._events === undefined) {
        this._events = Object.create(null);
    }
    if (this._maxListeners === undefined) {
        this._maxListeners = undefined;
    }
};
EventEmitter.prototype.setMaxListeners = function setMaxListeners(n) {
    if (typeof n !== "number" || n < 0 || Number.isNaN(n)) {
        const err = new RangeError(
            'The value of "n" is out of range. It must be a non-negative number. Received ' + n
        );
        err.code = "ERR_OUT_OF_RANGE";
        throw err;
    }
    this._maxListeners = n;
    return this;
};
EventEmitter.prototype.getMaxListeners = function getMaxListeners() {
    return this._maxListeners === undefined ? EventEmitter.defaultMaxListeners : this._maxListeners;
};
EventEmitter.prototype.addListener = function addListener(type, listener) {
    return this.on(type, listener);
};
EventEmitter.prototype.on = function on(type, listener) {
    checkListener(listener);
    const events = ensureEvents(this);
    if (events.newListener !== undefined) {
        this.emit("newListener", type, listener.listener || listener);
    }
    (events[type] = events[type] || []).push(listener);
    return this;
};
EventEmitter.prototype.prependListener = function prependListener(type, listener) {
    checkListener(listener);
    const events = ensureEvents(this);
    if (events.newListener !== undefined) {
        this.emit("newListener", type, listener.listener || listener);
    }
    (events[type] = events[type] || []).unshift(listener);
    return this;
};
function onceWrap(target, type, listener) {
    let fired = false;
    function wrapper(...args) {
        if (fired) return;
        fired = true;
        target.removeListener(type, wrapper);
        return listener.apply(target, args);
    }
    wrapper.listener = listener;
    return wrapper;
}
EventEmitter.prototype.once = function once(type, listener) {
    checkListener(listener);
    return this.on(type, onceWrap(this, type, listener));
};
EventEmitter.prototype.prependOnceListener = function prependOnceListener(type, listener) {
    checkListener(listener);
    return this.prependListener(type, onceWrap(this, type, listener));
};
EventEmitter.prototype.off = function off(type, listener) {
    return this.removeListener(type, listener);
};
EventEmitter.prototype.removeListener = function removeListener(type, listener) {
    checkListener(listener);
    const events = ensureEvents(this);
    const list = events[type];
    if (!list) return this;
    for (let i = list.length - 1; i >= 0; i--) {
        if (list[i] === listener || list[i].listener === listener) {
            const removed = list[i].listener || list[i];
            list.splice(i, 1);
            if (list.length === 0) delete events[type];
            if (events.removeListener !== undefined) {
                this.emit("removeListener", type, removed);
            }
            break;
        }
    }
    return this;
};
EventEmitter.prototype.removeAllListeners = function removeAllListeners(type) {
    const events = ensureEvents(this);
    if (arguments.length === 0) {
        // Emit 'removeListener' for each listener (Node does), unless there
        // is no such listener registered.
        if (events.removeListener !== undefined) {
            for (const key of Object.keys(events)) {
                if (key === "removeListener") continue;
                this.removeAllListeners(key);
            }
            this.removeAllListeners("removeListener");
        }
        this._events = Object.create(null);
        return this;
    }
    const list = events[type];
    if (list) {
        if (events.removeListener !== undefined) {
            for (let i = list.length - 1; i >= 0; i--) {
                const removed = list[i].listener || list[i];
                list.splice(i, 1);
                this.emit("removeListener", type, removed);
            }
            delete events[type];
        } else {
            delete events[type];
        }
    }
    return this;
};
EventEmitter.prototype.emit = function emit(type, ...args) {
    const events = ensureEvents(this);
    if (type === "error" && events[EventEmitter.errorMonitor] !== undefined) {
        for (const fn of events[EventEmitter.errorMonitor].slice()) fn.apply(this, args);
    }
    const list = events[type] ? events[type].slice() : [];
    if (list.length === 0) {
        if (type === "error") {
            const err = args[0];
            if (err instanceof Error) throw err;
            const wrapped = new Error(
                "Unhandled error." + (err !== undefined ? " (" + String(err) + ")" : "")
            );
            wrapped.code = "ERR_UNHANDLED_ERROR";
            wrapped.context = err;
            throw wrapped;
        }
        return false;
    }
    for (const fn of list) fn.apply(this, args);
    return true;
};
EventEmitter.prototype.listeners = function listeners(type) {
    const events = ensureEvents(this);
    return (events[type] || []).map((l) => l.listener || l);
};
EventEmitter.prototype.rawListeners = function rawListeners(type) {
    const events = ensureEvents(this);
    return (events[type] || []).slice();
};
EventEmitter.prototype.listenerCount = function listenerCount(type, listener) {
    const events = ensureEvents(this);
    const list = events[type];
    if (!list) return 0;
    if (listener === undefined) return list.length;
    let count = 0;
    for (const l of list) {
        if (l === listener || l.listener === listener) count++;
    }
    return count;
};
EventEmitter.prototype.eventNames = function eventNames() {
    const events = ensureEvents(this);
    return Object.keys(events).concat(Object.getOwnPropertySymbols(events));
};
EventEmitter.defaultMaxListeners = 10;
EventEmitter.EventEmitter = EventEmitter;
EventEmitter.errorMonitor = Symbol("events.errorMonitor");
EventEmitter.captureRejectionSymbol = Symbol.for("nodejs.rejection");
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
function setMaxListeners(n, ...emitters) {
    if (emitters.length === 0) {
        EventEmitter.defaultMaxListeners = n;
        return;
    }
    for (const emitter of emitters) emitter.setMaxListeners(n);
}
const errorMonitor = EventEmitter.errorMonitor;
const captureRejectionSymbol = EventEmitter.captureRejectionSymbol;

export { EventEmitter, once, getEventListeners, getMaxListeners, setMaxListeners, errorMonitor, captureRejectionSymbol };
export default EventEmitter;
"#;

// node:url shim. The chidori engine does not install WHATWG `URL`/
// `URLSearchParams` globals, so this provides a conformant subset implemented in
// pure JS: parsing, the standard component accessors, searchParams manipulation,
// `toString`, and relative-base resolution via `new URL(input, base)`. The
// legacy `url.parse`/`url.format` helpers are also provided since some packages
// still reach for them. (Uses `r##` delimiters because the body contains `"#`.)
const URL_SHIM: &str = r##"
const SPECIAL_PORTS = { "http:": "80", "https:": "443", "ws:": "80", "wss:": "443", "ftp:": "21" };

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
    let username = "", password = "", host = "", hostname = "", port = "";
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
        host = authority;
        const pcolon = authority.lastIndexOf(":");
        if (pcolon !== -1 && /^[0-9]*$/.test(authority.slice(pcolon + 1))) {
            hostname = authority.slice(0, pcolon);
            port = authority.slice(pcolon + 1);
        } else {
            hostname = authority;
        }
    }
    let pathname = m[4] || "";
    if (hasAuthority && pathname === "") pathname = "/";
    if (SPECIAL_PORTS[protocol] === port) port = "";
    return {
        protocol, username, password, hostname, port,
        host: port ? hostname + ":" + port : hostname,
        pathname,
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
        if (!comps) throw new TypeError(`Invalid URL: ${input}`);
        this._protocol = comps.protocol;
        this._username = comps.username;
        this._password = comps.password;
        this._hostname = comps.hostname;
        this._port = comps.port;
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
    set hostname(v) { this._hostname = String(v); }
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
    }
    get origin() {
        if (!this._hostname) return "null";
        return this._protocol + "//" + this.host;
    }
    get pathname() { return this._pathname; }
    set pathname(v) {
        v = String(v);
        if (v && v.charCodeAt(0) !== 47) v = "/" + v;
        this._pathname = v;
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
        this._port = next._port; this._pathname = next._pathname;
        this._search = next._search; this._hash = next._hash;
        this._searchParams = next._searchParams;
    }
    toString() {
        let out = this._protocol;
        if (this._hostname || this._protocol === "http:" || this._protocol === "https:") {
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
        hostname: base._hostname, port: base._port, host: base.host,
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
    return out;
}

// Legacy url.parse / url.format (Node's older API). Minimal but enough for
// packages that haven't migrated to WHATWG URL.
function parse(urlStr) {
    if (isAbsoluteUrl(urlStr)) {
        const c = parseAbsolute(urlStr);
        if (c) {
            return {
                protocol: c.protocol, slashes: true, auth: null,
                host: c.host, port: c.port || null, hostname: c.hostname,
                hash: c.hash || null, search: c.search || null,
                query: c.search ? c.search.slice(1) : null,
                pathname: c.pathname || null,
                path: (c.pathname || "") + (c.search || ""),
                href: urlStr,
            };
        }
    }
    let hash = null, search = null, pathname = urlStr;
    const hi = pathname.indexOf("#");
    if (hi !== -1) { hash = pathname.slice(hi); pathname = pathname.slice(0, hi); }
    const qi = pathname.indexOf("?");
    if (qi !== -1) { search = pathname.slice(qi); pathname = pathname.slice(0, qi); }
    return {
        protocol: null, slashes: null, auth: null, host: null, port: null,
        hostname: null, hash, search, query: search ? search.slice(1) : null,
        pathname: pathname || null, path: (pathname || "") + (search || ""), href: urlStr,
    };
}

function format(obj) {
    if (obj instanceof URL) return obj.toString();
    if (typeof obj === "string") return obj;
    let out = "";
    if (obj.protocol) out += obj.protocol.endsWith(":") ? obj.protocol : obj.protocol + ":";
    if (obj.slashes || obj.host || obj.hostname) out += "//";
    if (obj.auth) out += obj.auth + "@";
    if (obj.host) out += obj.host;
    else if (obj.hostname) { out += obj.hostname; if (obj.port) out += ":" + obj.port; }
    if (obj.pathname) out += obj.pathname;
    if (obj.search) out += obj.search.charCodeAt(0) === 63 ? obj.search : "?" + obj.search;
    else if (obj.query && typeof obj.query === "string") out += "?" + obj.query;
    if (obj.hash) out += obj.hash.charCodeAt(0) === 35 ? obj.hash : "#" + obj.hash;
    return out;
}

function fileURLToPath(url) {
    const u = url instanceof URL ? url : new URL(String(url));
    if (u.protocol !== "file:") throw new TypeError("The URL must be of scheme file");
    return decodeURIComponent(u.pathname);
}
function pathToFileURL(path) {
    return new URL("file://" + (String(path).charCodeAt(0) === 47 ? "" : "/") + encodeURI(String(path)));
}

export { URL, URLSearchParams, parse, format, fileURLToPath, pathToFileURL };
export default { URL, URLSearchParams, parse, format, fileURLToPath, pathToFileURL };
"##;

// node:assert shim. The strict-mode variants are the defaults here (equal uses
// ===), matching modern Node guidance; `assert/strict` re-exports the same
// surface. Deep equality is a structural recursive compare adequate for plain
// JSON-ish data, arrays, dates, and regexps.
const ASSERT_SHIM: &str = r#"
class AssertionError extends Error {
    constructor(options) {
        const opts = options || {};
        super(opts.message || "Assertion failed");
        this.name = "AssertionError";
        this.code = "ERR_ASSERTION";
        this.actual = opts.actual;
        this.expected = opts.expected;
        this.operator = opts.operator;
    }
}

function fail(actual, expected, message, operator) {
    if (arguments.length === 1) { throw new AssertionError({ message: actual }); }
    if (message instanceof Error) throw message;
    throw new AssertionError({ message, actual, expected, operator: operator || "fail" });
}

function ok(value, message) {
    if (!value) {
        throw new AssertionError({ message: message || `The expression evaluated to a falsy value:`, actual: value, expected: true, operator: "==" });
    }
}

function deepEqualImpl(a, b, strict, seen) {
    if (strict ? a === b : a == b) return true;
    if (a === null || b === null || typeof a !== "object" || typeof b !== "object") {
        if (a instanceof Date && b instanceof Date) return a.getTime() === b.getTime();
        return !strict && a == b;
    }
    if (a instanceof Date || b instanceof Date) {
        return a instanceof Date && b instanceof Date && a.getTime() === b.getTime();
    }
    if (a instanceof RegExp || b instanceof RegExp) {
        return a instanceof RegExp && b instanceof RegExp && a.source === b.source && a.flags === b.flags;
    }
    // Plain-array cycle guard: the snapshot policy disables Set/WeakSet, so we
    // track visited objects in an array we carry through the recursion.
    seen = seen || [];
    if (seen.indexOf(a) !== -1) return true;
    seen.push(a);
    if (Array.isArray(a) !== Array.isArray(b)) return false;
    const ka = Object.keys(a);
    const kb = Object.keys(b);
    if (ka.length !== kb.length) return false;
    for (const k of ka) {
        if (!Object.prototype.hasOwnProperty.call(b, k)) return false;
        if (!deepEqualImpl(a[k], b[k], strict, seen)) return false;
    }
    return true;
}

function equal(actual, expected, message) {
    if (actual != expected) {
        throw new AssertionError({ message, actual, expected, operator: "==" });
    }
}
function notEqual(actual, expected, message) {
    if (actual == expected) {
        throw new AssertionError({ message, actual, expected, operator: "!=" });
    }
}
function strictEqual(actual, expected, message) {
    if (!Object.is(actual, expected)) {
        throw new AssertionError({ message, actual, expected, operator: "strictEqual" });
    }
}
function notStrictEqual(actual, expected, message) {
    if (Object.is(actual, expected)) {
        throw new AssertionError({ message, actual, expected, operator: "notStrictEqual" });
    }
}
function deepEqual(actual, expected, message) {
    if (!deepEqualImpl(actual, expected, false)) {
        throw new AssertionError({ message, actual, expected, operator: "deepEqual" });
    }
}
function notDeepEqual(actual, expected, message) {
    if (deepEqualImpl(actual, expected, false)) {
        throw new AssertionError({ message, actual, expected, operator: "notDeepEqual" });
    }
}
function deepStrictEqual(actual, expected, message) {
    if (!deepEqualImpl(actual, expected, true)) {
        throw new AssertionError({ message, actual, expected, operator: "deepStrictEqual" });
    }
}
function notDeepStrictEqual(actual, expected, message) {
    if (deepEqualImpl(actual, expected, true)) {
        throw new AssertionError({ message, actual, expected, operator: "notDeepStrictEqual" });
    }
}

function matchError(err, expected) {
    if (expected === undefined) return true;
    if (typeof expected === "function") {
        if (expected === Error || Object.prototype.isPrototypeOf.call(Error, expected) || expected.prototype instanceof Error) {
            return err instanceof expected;
        }
        return expected(err) === true;
    }
    if (expected instanceof RegExp) return expected.test(String(err && err.message !== undefined ? err.message : err));
    if (expected instanceof Error) return err && err.message === expected.message;
    if (typeof expected === "object") {
        for (const k of Object.keys(expected)) {
            if (!err || err[k] !== expected[k]) return false;
        }
        return true;
    }
    return false;
}

function throws(fn, expected, message) {
    let threw = false;
    let caught;
    try { fn(); } catch (e) { threw = true; caught = e; }
    if (!threw) {
        throw new AssertionError({ message: message || "Missing expected exception.", operator: "throws" });
    }
    if (!matchError(caught, expected)) {
        if (caught instanceof Error && !(expected instanceof RegExp || typeof expected === "function")) throw caught;
        throw new AssertionError({ message: message || "Got unwanted exception.", actual: caught, expected, operator: "throws" });
    }
}

function doesNotThrow(fn, expected, message) {
    try { fn(); } catch (e) {
        if (matchError(e, expected)) {
            throw new AssertionError({ message: message || "Got unwanted exception.", actual: e, operator: "doesNotThrow" });
        }
        throw e;
    }
}

async function rejects(promiseOrFn, expected, message) {
    let caught;
    let threw = false;
    try {
        const p = typeof promiseOrFn === "function" ? promiseOrFn() : promiseOrFn;
        await p;
    } catch (e) { threw = true; caught = e; }
    if (!threw) {
        throw new AssertionError({ message: message || "Missing expected rejection.", operator: "rejects" });
    }
    if (!matchError(caught, expected)) {
        throw new AssertionError({ message: message || "Got unwanted rejection.", actual: caught, expected, operator: "rejects" });
    }
}

async function doesNotReject(promiseOrFn, expected, message) {
    try {
        const p = typeof promiseOrFn === "function" ? promiseOrFn() : promiseOrFn;
        await p;
    } catch (e) {
        if (matchError(e, expected)) {
            throw new AssertionError({ message: message || "Got unwanted rejection.", actual: e, operator: "doesNotReject" });
        }
        throw e;
    }
}

function match(value, regexp, message) {
    if (!(regexp instanceof RegExp)) throw new TypeError("regexp must be a RegExp");
    if (!regexp.test(value)) {
        throw new AssertionError({ message, actual: value, expected: regexp, operator: "match" });
    }
}
function doesNotMatch(value, regexp, message) {
    if (!(regexp instanceof RegExp)) throw new TypeError("regexp must be a RegExp");
    if (regexp.test(value)) {
        throw new AssertionError({ message, actual: value, expected: regexp, operator: "doesNotMatch" });
    }
}

function assert(value, message) { ok(value, message); }
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
assert.AssertionError = AssertionError;
assert.strict = assert;

export { ok, fail, equal, notEqual, strictEqual, notStrictEqual, deepEqual, notDeepEqual, deepStrictEqual, notDeepStrictEqual, throws, doesNotThrow, rejects, doesNotReject, match, doesNotMatch, AssertionError };
export default assert;
"#;

// node:assert/strict re-exports node:assert (whose default is already strict)
// and exposes the same default. Named re-exports are spelled out because the
// bundler does not support `export *`.
const ASSERT_STRICT_SHIM: &str = r#"
import assert from "node:assert";
export { ok, fail, equal, notEqual, strictEqual, notStrictEqual, deepEqual, notDeepEqual, deepStrictEqual, notDeepStrictEqual, throws, doesNotThrow, rejects, doesNotReject, match, doesNotMatch, AssertionError } from "node:assert";
export default assert;
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
        for name in ["child_process", "vm", "tls", "wasi", "dgram"] {
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
