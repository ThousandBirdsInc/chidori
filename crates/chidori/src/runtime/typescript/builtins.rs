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
            // JSON round trip shape.
            if (input.type === "Buffer" && Array.isArray(input.data)) {
                return wrap(Uint8Array.from(input.data));
            }
            // Array-likes.
            if (typeof input.length === "number") {
                return wrap(Uint8Array.from(input));
            }
            // Boxed primitives / objects with a primitive value.
            if (typeof input.valueOf === "function") {
                const value = input.valueOf();
                if (value !== input && (typeof value === "string" || typeof value === "object")) {
                    return Buffer.from(value, encodingOrOffset, length);
                }
            }
            const primitive = input[Symbol.toPrimitive];
            if (typeof primitive === "function") {
                return Buffer.from(primitive.call(input, "string"), encodingOrOffset, length);
            }
        }
        const err = new TypeError(
            "The first argument must be of type string or an instance of Buffer, ArrayBuffer, or Array or an Array-like Object. Received " + typeof input
        );
        err.code = "ERR_INVALID_ARG_TYPE";
        throw err;
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
        const err = new TypeError("Buffer.byteLength expects a string, Buffer, or ArrayBuffer");
        err.code = "ERR_INVALID_ARG_TYPE";
        throw err;
    }
    static compare(a, b) {
        const len = Math.min(a.length, b.length);
        for (let i = 0; i < len; i++) {
            if (a[i] !== b[i]) return a[i] < b[i] ? -1 : 1;
        }
        if (a.length !== b.length) return a.length < b.length ? -1 : 1;
        return 0;
    }
    static concat(list, totalLength) {
        if (!Array.isArray(list)) {
            const err = new TypeError('The "list" argument must be an instance of Array');
            err.code = "ERR_INVALID_ARG_TYPE";
            throw err;
        }
        let total = 0;
        for (const b of list) total += b.length;
        if (totalLength !== undefined) total = totalLength >>> 0;
        const out = Buffer.allocUnsafe(total);
        let offset = 0;
        for (const b of list) {
            if (offset >= total) break;
            const chunk = b.length > total - offset ? b.subarray(0, total - offset) : b;
            out.set(chunk, offset);
            offset += chunk.length;
        }
        return out;
    }
    equals(other) { return Buffer.compare(this, other) === 0; }
    compare(other) { return Buffer.compare(this, other); }
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
            for (let i = 0; i + 1 < view.length; i += 2) s += String.fromCharCode(view[i] | (view[i + 1] << 8));
            return s;
        }
        return new TextDecoder().decode(view);
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
const kMaxShortLength = 14;

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

// `compact` mirrors Node's default `util.inspect` (single-line, insertion
// order); the multi-line, key-sorted form is what `assert` uses to build its
// diffs (Node's `inspectValue`, i.e. `{ compact: false, sorted: true }`).
function inspectAny(value, compact, depth, seen) {
    const kind = typeof value;
    if (kind === "string") return inspectString(value);
    if (kind === "symbol") return String(value);
    if (kind === "bigint") return String(value) + "n";
    if (kind === "function") {
        return value.name ? "[Function: " + value.name + "]" : "[Function (anonymous)]";
    }
    if (value === null) return "null";
    if (kind !== "object") return String(value);
    if (seen.indexOf(value) !== -1) return "[Circular *1]";
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
    if (entries.length === 0) return prefix + open + close;
    if (compact) return prefix + open + " " + entries.join(", ") + " " + close;
    const indent = "  ".repeat(depth + 1);
    return prefix + open + "\n" + indent + entries.join(",\n" + indent) + "\n" + "  ".repeat(depth) + close;
}

function formatKey(key) { return isIdentifierKey(key) ? key : inspectString(key); }
function inspect(value) { return inspectAny(value, true, 0, []); }
function inspectValue(value) { return inspectAny(value, false, 0, []); }
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

function diffLines(actualLines, expectedLines, header, indicator) {
    let prefix = 0;
    while (prefix < actualLines.length && prefix < expectedLines.length &&
           actualLines[prefix] === expectedLines[prefix]) prefix++;
    let suffix = 0;
    while (suffix < actualLines.length - prefix && suffix < expectedLines.length - prefix &&
           actualLines[actualLines.length - 1 - suffix] === expectedLines[expectedLines.length - 1 - suffix]) suffix++;

    const out = [];
    let skipped = false;
    if (prefix > 7) {
        for (let i = 0; i < 5; i++) out.push("  " + actualLines[i]);
        out.push("...");
        out.push("  " + actualLines[prefix - 1]);
        skipped = true;
    } else {
        for (let i = 0; i < prefix; i++) out.push("  " + actualLines[i]);
    }
    for (let i = prefix; i < actualLines.length - suffix; i++) out.push("+ " + actualLines[i]);
    for (let i = prefix; i < expectedLines.length - suffix; i++) out.push("- " + expectedLines[i]);
    for (let i = actualLines.length - suffix; i < actualLines.length; i++) out.push("  " + actualLines[i]);
    return header + "\n+ actual - expected" + (skipped ? "\n... Skipped lines" : "") +
        "\n\n" + out.join("\n") + indicator + "\n";
}

function createErrDiff(actual, expected, operator) {
    const actualLines = inspectValue(actual).split("\n");
    const expectedLines = inspectValue(expected).split("\n");
    if (operator === "strictEqual" &&
        typeof actual === "object" && actual !== null &&
        typeof expected === "object" && expected !== null) {
        operator = "strictEqualObject";
    }
    if (actualLines.join("\n") === expectedLines.join("\n")) {
        return kReadableOperator.notIdentical + "\n\n" + actualLines.join("\n") + "\n";
    }
    let indicator = "";
    if (actualLines.length === 1 && expectedLines.length === 1) {
        const inputLength = actualLines[0].length + expectedLines[0].length;
        if (inputLength <= kMaxShortLength) {
            if ((typeof actual !== "object" || actual === null) &&
                (typeof expected !== "object" || expected === null) &&
                (actual !== 0 || expected !== 0)) {
                return kReadableOperator[operator] + "\n\n" + actualLines[0] + " !== " + expectedLines[0] + "\n";
            }
        } else if (operator !== "strictEqualObject" && inputLength <= 80) {
            let i = 0;
            while (i < actualLines[0].length && actualLines[0][i] === expectedLines[0][i]) i++;
            if (i > 2) indicator = "\n  " + " ".repeat(i) + "^";
        }
    }
    return diffLines(actualLines, expectedLines, kReadableOperator[operator], indicator);
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
    const res = inspectValue(actual);
    const other = inspectValue(expected);
    const known = kReadableOperator[operator];
    if (operator === "notDeepEqual" && res === other) return known + "\n\n" + res;
    if (operator === "deepEqual") return known + "\n\n" + res + "\n\nshould loosely deep-equal\n\n" + other;
    const unequal = kReadableOperator[operator + "Unequal"];
    if (unequal) return unequal + "\n\n" + res + "\n\nshould not loosely deep-equal\n\n" + other;
    return res + " " + operator + " " + other;
}

class AssertionError extends Error {
    constructor(options) {
        if (options === null || typeof options !== "object") {
            throw invalidArgType("options", "of type object", options);
        }
        const operator = options.operator;
        const actual = options.actual;
        const expected = options.expected;
        let generatedMessage = false;
        let text;
        if (options.message === undefined || options.message === null) {
            generatedMessage = true;
            text = generateMessage(operator, actual, expected);
        } else {
            text = String(options.message);
        }
        super(text);
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
        innerFail({ actual, expected, message, operator: "==" });
    }
}
function notEqual(actual, expected, message) {
    if (arguments.length < 2) throw missingArgs();
    if (actual == expected || (Number.isNaN(actual) && Number.isNaN(expected))) {
        innerFail({ actual, expected, message, operator: "!=" });
    }
}
function strictEqual(actual, expected, message) {
    if (arguments.length < 2) throw missingArgs();
    if (!Object.is(actual, expected)) {
        innerFail({ actual, expected, message, operator: "strictEqual" });
    }
}
function notStrictEqual(actual, expected, message) {
    if (arguments.length < 2) throw missingArgs();
    if (Object.is(actual, expected)) {
        innerFail({ actual, expected, message, operator: "notStrictEqual" });
    }
}
function deepEqual(actual, expected, message) {
    if (arguments.length < 2) throw missingArgs();
    if (!isDeepEqual(actual, expected, false)) {
        innerFail({ actual, expected, message, operator: "deepEqual" });
    }
}
function notDeepEqual(actual, expected, message) {
    if (arguments.length < 2) throw missingArgs();
    if (isDeepEqual(actual, expected, false)) {
        innerFail({ actual, expected, message, operator: "notDeepEqual" });
    }
}
function deepStrictEqual(actual, expected, message) {
    if (arguments.length < 2) throw missingArgs();
    if (!isDeepEqual(actual, expected, true)) {
        innerFail({ actual, expected, message, operator: "deepStrictEqual" });
    }
}
function notDeepStrictEqual(actual, expected, message) {
    if (arguments.length < 2) throw missingArgs();
    if (isDeepEqual(actual, expected, true)) {
        innerFail({ actual, expected, message, operator: "notDeepStrictEqual" });
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

function compareExceptionKey(actual, expected, key, message, keys, operator) {
    if (key in actual && isDeepEqual(actual[key], expected[key], true)) return;
    if (!message) {
        const err = new AssertionError({
            actual: new Comparison(actual, keys),
            expected: new Comparison(expected, keys, actual),
            operator: "deepStrictEqual",
        });
        err.actual = actual;
        err.expected = expected;
        err.operator = operator;
        throw err;
    }
    innerFail({ actual, expected, message, operator });
}

function expectedException(actual, expected, message, operator) {
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
            const err = new AssertionError({ actual, expected, message, operator: "deepStrictEqual" });
            err.operator = operator;
            throw err;
        } else {
            const keys = Object.keys(expected);
            if (expected instanceof Error) keys.push("name", "message");
            else if (keys.length === 0) throw invalidArgValue("error", expected, "may not be an empty object");
            for (const key of keys) {
                if (typeof actual[key] === "string" && expected[key] instanceof RegExp &&
                    expected[key].test(actual[key])) continue;
                compareExceptionKey(actual, expected, key, message, keys, operator);
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
        const err = new AssertionError({ actual, expected, message, operator });
        err.generatedMessage = generatedMessage;
        throw err;
    }
    return true;
}

function expectsError(operator, actual, args) {
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
            message: "Missing expected " + (operator === "rejects" ? "rejection" : "exception") + details,
        });
    }
    if (!error) return;
    expectedException(actual, error, message, operator);
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

function expectsNoError(operator, actual, args) {
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

function throws(fn, ...args) { expectsError("throws", getActual(fn), args); }
function doesNotThrow(fn, ...args) { expectsNoError("doesNotThrow", getActual(fn), args); }

async function rejects(promiseFn, ...args) {
    try {
        expectsError("rejects", await waitForActual(promiseFn), args);
    } catch (err) {
        throw markAsyncFrame(err, "rejects");
    }
}

async function doesNotReject(promiseFn, ...args) {
    try {
        expectsNoError("doesNotReject", await waitForActual(promiseFn), args);
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
            .contains("class StringDecoder"));
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
