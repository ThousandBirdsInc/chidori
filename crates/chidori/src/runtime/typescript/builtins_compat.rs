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
function qsEscape(str) {
    return encodeURIComponent(String(str));
}
function qsUnescape(str) {
    try { return decodeURIComponent(String(str)); } catch { return String(str); }
}
function stringifyPrimitive(v) {
    if (typeof v === "string") return v;
    if (typeof v === "number" && isFinite(v)) return String(v);
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
    const decode = options && typeof options.decodeURIComponent === "function"
        ? options.decodeURIComponent
        : qsUnescape;
    const parts = qs.split(sep);
    const limit = maxKeys > 0 ? Math.min(parts.length, maxKeys) : parts.length;
    for (let i = 0; i < limit; i++) {
        const part = parts[i];
        if (part.length === 0) continue;
        const idx = part.indexOf(eq);
        let key, value;
        if (idx === -1) {
            key = decode(part.replace(/\+/g, " "));
            value = "";
        } else {
            key = decode(part.slice(0, idx).replace(/\+/g, " "));
            value = decode(part.slice(idx + eq.length).replace(/\+/g, " "));
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
    escape: qsEscape, unescape: qsUnescape,
};
export { parse, stringify, parse as decode, stringify as encode, qsEscape as escape, qsUnescape as unescape };
export default querystring;
"#;

// node:string_decoder — a StringDecoder that buffers incomplete multi-byte
// sequences across write() calls, matching Node's streaming semantics for the
// encodings Buffer supports here.
const STRING_DECODER_SHIM: &str = r#"
import { Buffer } from "node:buffer";

function normEnc(encoding) {
    if (!encoding) return "utf8";
    const e = String(encoding).toLowerCase();
    if (e === "utf-8") return "utf8";
    if (e === "utf-16le" || e === "ucs2" || e === "ucs-2") return "utf16le";
    if (e === "binary") return "latin1";
    return e;
}
function toBytes(buf) {
    if (buf instanceof Uint8Array) return buf;
    if (typeof buf === "string") return new TextEncoder().encode(buf);
    if (ArrayBuffer.isView(buf)) return new Uint8Array(buf.buffer, buf.byteOffset, buf.byteLength);
    if (buf instanceof ArrayBuffer) return new Uint8Array(buf);
    throw new TypeError("StringDecoder.write expects a Buffer or typed array");
}
// How many trailing bytes belong to an unfinished UTF-8 sequence (0 if the
// buffer ends on a complete boundary).
function utf8IncompleteTail(bytes) {
    const len = bytes.length;
    for (let back = 1; back <= 3 && back <= len; back++) {
        const b = bytes[len - back];
        if ((b & 0x80) === 0) return 0;
        if ((b & 0xc0) === 0xc0) {
            const need = (b & 0xe0) === 0xc0 ? 2 : (b & 0xf0) === 0xe0 ? 3 : 4;
            return back < need ? back : 0;
        }
        // Continuation byte: keep scanning backwards for the lead.
    }
    return 0;
}
function bytesToBase64(bytes) {
    let s = "";
    for (let i = 0; i < bytes.length; i++) s += String.fromCharCode(bytes[i]);
    return btoa(s);
}

class StringDecoder {
    constructor(encoding) {
        this.encoding = normEnc(encoding);
        this._carry = null;
    }
    write(buf) {
        let bytes = toBytes(buf);
        if (this._carry && this._carry.length) {
            const merged = new Uint8Array(this._carry.length + bytes.length);
            merged.set(this._carry, 0);
            merged.set(bytes, this._carry.length);
            bytes = merged;
            this._carry = null;
        }
        let keep = 0;
        if (this.encoding === "utf8") keep = utf8IncompleteTail(bytes);
        else if (this.encoding === "utf16le") keep = bytes.length % 2;
        else if (this.encoding === "base64" || this.encoding === "base64url") keep = bytes.length % 3;
        const complete = keep === 0 ? bytes : bytes.subarray(0, bytes.length - keep);
        if (keep !== 0) this._carry = bytes.slice(bytes.length - keep);
        return this._decode(complete);
    }
    _decode(bytes) {
        if (bytes.length === 0) return "";
        if (this.encoding === "utf8") return new TextDecoder().decode(bytes);
        if (this.encoding === "utf16le") {
            let out = "";
            for (let i = 0; i + 1 < bytes.length; i += 2) {
                out += String.fromCharCode(bytes[i] | (bytes[i + 1] << 8));
            }
            return out;
        }
        if (this.encoding === "base64") return bytesToBase64(bytes);
        if (this.encoding === "base64url") {
            return bytesToBase64(bytes).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
        }
        if (this.encoding === "hex") {
            let out = "";
            for (let i = 0; i < bytes.length; i++) out += bytes[i].toString(16).padStart(2, "0");
            return out;
        }
        // latin1 / ascii.
        let out = "";
        for (let i = 0; i < bytes.length; i++) out += String.fromCharCode(bytes[i] & 0xff);
        return out;
    }
    end(buf) {
        let out = buf === undefined || buf === null ? "" : this.write(buf);
        if (this._carry && this._carry.length) {
            const rest = this._carry;
            this._carry = null;
            if (this.encoding === "utf8") out += "�";
            else out += this._decode(rest);
        }
        return out;
    }
}
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

function error(type) {
    throw new RangeError("punycode: " + type);
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
    let out = "";
    for (const cp of codePoints) {
        if (cp > 0xffff) {
            const u = cp - 0x10000;
            out += String.fromCharCode(0xd800 + (u >> 10), 0xdc00 + (u & 0x3ff));
        } else {
            out += String.fromCharCode(cp);
        }
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
            if (digit >= base || digit > Math.floor((maxInt - i) / w)) error("overflow");
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
    return mapDomain(input, function (label) {
        return /[^\x00-\x7e]/.test(label) ? "xn--" + encode(label.toLowerCase()) : label;
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
export function isSymbolObject() { return false; }
export function isStringObject(value) { return typeof value === "object" && value !== null && value instanceof String; }
export function isNumberObject(value) { return typeof value === "object" && value !== null && value instanceof Number; }
export function isBooleanObject(value) { return typeof value === "object" && value !== null && value instanceof Boolean; }
export function isBoxedPrimitive(value) {
    return isStringObject(value) || isNumberObject(value) || isBooleanObject(value);
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
    isGeneratorObject, isProxy, isSymbolObject, isStringObject, isNumberObject,
    isBooleanObject, isBoxedPrimitive, isArgumentsObject, isSharedArrayBuffer,
    isExternal, isModuleNamespaceObject,
};
export default types;
"#;

// node:path/win32 — the chidori VFS is posix-only, so `path.win32` has always
// aliased the posix implementation (see the node:path shim); the subpath
// module re-exports the same surface.
const PATH_WIN32_SHIM: &str = r#"
import path from "node:path";
export { sep, delimiter, normalize, isAbsolute, join, resolve, dirname, basename, extname, relative, parse, format, posix, win32 } from "node:path";
export default path;
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
class AsyncLocalStorage {
    constructor() { this._stack = []; }
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
    static bind(fn) { return fn; }
    static snapshot() { return function (cb, ...args) { return cb(...args); }; }
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

class Channel {
    constructor(name) {
        this.name = name;
        this._subscribers = [];
    }
    get hasSubscribers() { return this._subscribers.length > 0; }
    subscribe(onMessage) {
        if (typeof onMessage !== "function") throw new TypeError("subscriber must be a function");
        this._subscribers.push(onMessage);
    }
    unsubscribe(onMessage) {
        const idx = this._subscribers.indexOf(onMessage);
        if (idx === -1) return false;
        this._subscribers.splice(idx, 1);
        return true;
    }
    publish(message) {
        const subs = this._subscribers.slice();
        for (const fn of subs) fn(message, this.name);
    }
    bindStore() {}
    unbindStore() {}
    runStores(context, fn, thisArg, ...args) {
        this.publish(context);
        return fn.apply(thisArg, args);
    }
}
export function channel(name) {
    return registry[name] || (registry[name] = new Channel(name));
}
export function subscribe(name, onMessage) { channel(name).subscribe(onMessage); }
export function unsubscribe(name, onMessage) { return channel(name).unsubscribe(onMessage); }
export function hasSubscribers(name) {
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
export function isIPv4(input) {
    if (typeof input !== "string") return false;
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
    if (typeof input !== "string" || input.length === 0) return false;
    let s = input;
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
        const head = s.slice(0, doubleColon).split(":").filter((g) => g.length > 0);
        const tail = s.slice(doubleColon + 2).split(":").filter((g) => g.length > 0);
        if (head.length + tail.length > 7) return false;
        groups = head.concat(tail);
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

class Stream extends EventEmitter {
    pipe(dest, options) {
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
    }
}

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

function initWritableState(stream, opts) {
    stream._writableState = {
        ended: false,
        finished: false,
        finishScheduled: false,
        destroyed: false,
        pending: 0,
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
        let settled = false;
        const done = function (err) {
            if (settled) return;
            settled = true;
            st.pending--;
            if (err) queueMicrotask(() => self.emit("error", err));
            if (callback) queueMicrotask(() => callback(err || null));
            maybeFinish(self);
        };
        try {
            this._write(chunk, encoding || "utf8", done);
        } catch (err) {
            done(err);
        }
        return true;
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
    try {
        this._transform(chunk, encoding, function (err, data) {
            if (err) return callback(err);
            if (data !== undefined && data !== null) self.push(data);
            callback(null);
        });
    } catch (err) {
        callback(err);
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

// node:vm — arbitrary secondary evaluation contexts would bypass the
// determinism prelude and capability policy, so the whole surface throws.
const VM_SHIM: &str = r#"
function unsupported(name) {
    return function () {
        throw new Error("vm." + name + " is not supported in the Chidori runtime (secondary evaluation contexts would bypass the determinism prelude and capability policy)");
    };
}
export class Script {
    constructor() { unsupported("Script")(); }
}
export const createContext = unsupported("createContext");
export const isContext = unsupported("isContext");
export const runInContext = unsupported("runInContext");
export const runInNewContext = unsupported("runInNewContext");
export const runInThisContext = unsupported("runInThisContext");
export const compileFunction = unsupported("compileFunction");
export const measureMemory = unsupported("measureMemory");
export class SourceTextModule {
    constructor() { unsupported("SourceTextModule")(); }
}
export default {
    Script, createContext, isContext, runInContext, runInNewContext,
    runInThisContext, compileFunction, measureMemory, SourceTextModule,
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
// (flate2/miniz in Rust; see runtime::compress). Codecs are pure functions of
// (input, level), so like node:crypto hashing they run inline with nothing
// captured, and record/replay agrees byte-for-byte. The streaming classes
// buffer their input and codec at flush — output for a complete stream
// matches the one-shot form (chidori streams are in-memory anyway). Brotli is
// the one codec family not provided (no Brotli codec in the runtime); those
// entry points stay fail-loud.
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
function toBytes(data) {
    if (typeof data === "string") return new TextEncoder().encode(data);
    if (data instanceof Uint8Array) return data;
    if (ArrayBuffer.isView(data)) return new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
    if (data instanceof ArrayBuffer) return new Uint8Array(data);
    throw new TypeError("zlib: unsupported input type (expected string, Buffer, typed array, or ArrayBuffer)");
}
function levelOf(options) {
    if (options && typeof options === "object" && typeof options.level === "number") {
        return options.level;
    }
    return null;
}
function codec(op, data, options) {
    const b64 = globalThis.__chidori_zlib(op, bytesToBase64(toBytes(data)), levelOf(options));
    return Buffer.from(base64ToBytes(b64));
}
// node signature: fn(data[, options], callback) — result on a microtask.
function asyncify(op) {
    return function (data, options, callback) {
        if (typeof options === "function") { callback = options; options = undefined; }
        if (typeof callback !== "function") throw new TypeError("zlib: callback must be a function");
        queueMicrotask(() => {
            try {
                callback(null, codec(op, data, options));
            } catch (err) {
                callback(err);
            }
        });
    };
}
// Streaming form: buffer chunks, codec the concatenation at flush. Complete-
// stream output is identical to the one-shot functions.
function codecStream(op, options) {
    const chunks = [];
    return new Transform({
        transform(chunk, encoding, callback) {
            try { chunks.push(toBytes(chunk)); } catch (err) { return callback(err); }
            callback(null);
        },
        flush(callback) {
            try {
                let total = 0;
                for (const c of chunks) total += c.length;
                const all = new Uint8Array(total);
                let offset = 0;
                for (const c of chunks) { all.set(c, offset); offset += c.length; }
                callback(null, codec(op, all, options));
            } catch (err) {
                callback(err);
            }
        },
    });
}
export function deflateSync(data, options) { return codec("deflate", data, options); }
export function deflateRawSync(data, options) { return codec("deflateRaw", data, options); }
export function gzipSync(data, options) { return codec("gzip", data, options); }
export function inflateSync(data, options) { return codec("inflate", data, options); }
export function inflateRawSync(data, options) { return codec("inflateRaw", data, options); }
export function gunzipSync(data, options) { return codec("gunzip", data, options); }
export function unzipSync(data, options) { return codec("unzip", data, options); }
export const deflate = asyncify("deflate");
export const deflateRaw = asyncify("deflateRaw");
export const gzip = asyncify("gzip");
export const inflate = asyncify("inflate");
export const inflateRaw = asyncify("inflateRaw");
export const gunzip = asyncify("gunzip");
export const unzip = asyncify("unzip");
export function createDeflate(options) { return codecStream("deflate", options); }
export function createDeflateRaw(options) { return codecStream("deflateRaw", options); }
export function createGzip(options) { return codecStream("gzip", options); }
export function createInflate(options) { return codecStream("inflate", options); }
export function createInflateRaw(options) { return codecStream("inflateRaw", options); }
export function createGunzip(options) { return codecStream("gunzip", options); }
export function createUnzip(options) { return codecStream("unzip", options); }
function noBrotli(name) {
    return function () {
        throw new Error("zlib." + name + " is not supported in the Chidori runtime (no Brotli codec; use the gzip/deflate family)");
    };
}
export const brotliCompress = noBrotli("brotliCompress");
export const brotliCompressSync = noBrotli("brotliCompressSync");
export const brotliDecompress = noBrotli("brotliDecompress");
export const brotliDecompressSync = noBrotli("brotliDecompressSync");
export const createBrotliCompress = noBrotli("createBrotliCompress");
export const createBrotliDecompress = noBrotli("createBrotliDecompress");
export const constants = Object.freeze({
    Z_NO_FLUSH: 0, Z_PARTIAL_FLUSH: 1, Z_SYNC_FLUSH: 2, Z_FULL_FLUSH: 3,
    Z_FINISH: 4, Z_BLOCK: 5,
    Z_OK: 0, Z_STREAM_END: 1, Z_NEED_DICT: 2,
    Z_NO_COMPRESSION: 0, Z_BEST_SPEED: 1, Z_BEST_COMPRESSION: 9,
    Z_DEFAULT_COMPRESSION: -1,
    Z_DEFAULT_STRATEGY: 0,
    BROTLI_MIN_QUALITY: 0, BROTLI_MAX_QUALITY: 11,
});
export default {
    deflate, deflateSync, deflateRaw, deflateRawSync,
    inflate, inflateSync, inflateRaw, inflateRawSync,
    gzip, gzipSync, gunzip, gunzipSync, unzip, unzipSync,
    brotliCompress, brotliCompressSync, brotliDecompress, brotliDecompressSync,
    createDeflate, createDeflateRaw, createInflate, createInflateRaw,
    createGzip, createGunzip, createUnzip,
    createBrotliCompress, createBrotliDecompress,
    constants,
};
"#;

// node:module — resolver introspection. `builtinModules` reflects the actual
// allowlist (spliced in from `NODE_BUILTIN_ALLOWLIST` so there is one source
// of truth); createRequire links but the returned require throws, matching
// the loader's leaf-only CommonJS stance.
static MODULE_SHIM: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    let names = crate::runtime::typescript::transpile::NODE_BUILTIN_ALLOWLIST;
    let list = serde_json::to_string(names).expect("builtin allowlist serializes");
    format!(
        r#"
const builtinModules = Object.freeze({list});
export {{ builtinModules }};
export function isBuiltin(specifier) {{
    const name = String(specifier).startsWith("node:")
        ? String(specifier).slice(5)
        : String(specifier);
    return builtinModules.indexOf(name) !== -1;
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

/// Shim source for the compat suite; consulted by `builtins::shim_source`
/// after its own table.
pub fn compat_shim_source(name: &str) -> Option<&'static str> {
    match name {
        "querystring" => Some(QUERYSTRING_SHIM),
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
