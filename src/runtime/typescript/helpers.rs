//! Shared, engine-agnostic JavaScript helper sources.
//!
//! These polyfills and SDK-sugar scripts are plain, deterministic JS installed
//! into the runtime before agent code runs. They were previously colocated with
//! the (removed) QuickJS snapshot runtime; the pure-Rust engine
//! (`rust_engine`) and tool-metadata evaluation (`tools`) install them now.

/// Reads the `CHIDORI_AGENT_ENV` JSON blob the host sets and returns it as a
/// JSON object literal (or `{}`), for `process.env` — never the raw OS env.
pub(crate) fn chidori_agent_env_json() -> String {
    match std::env::var("CHIDORI_AGENT_ENV") {
        Ok(raw) if !raw.trim().is_empty() => {
            match serde_json::from_str::<serde_json::Value>(&raw) {
                Ok(value) if value.is_object() => value.to_string(),
                _ => "{}".to_string(),
            }
        }
        _ => "{}".to_string(),
    }
}

pub(crate) const CHIDORI_JS_HELPERS_SCRIPT: &str = r#"
(() => {
    globalThis.chidori = globalThis.chidori || {};

    globalThis.chidori.tryCall = async function tryCall(fn) {
        try {
            return { ok: true, value: await fn() };
        } catch (err) {
            return {
                ok: false,
                error: String(err && err.message ? err.message : err),
            };
        }
    };

    globalThis.chidori.retry = async function retry(fn, options) {
        const attempts = Math.max(1, Number(options && options.attempts) || 3);
        let lastErr;
        for (let i = 0; i < attempts; i += 1) {
            try {
                return await fn();
            } catch (err) {
                lastErr = err;
            }
        }
        throw lastErr;
    };

    globalThis.chidori.parallel = async function parallel(tasks, options) {
        if (!Array.isArray(tasks)) {
            throw new Error("chidori.parallel expects an array of task functions");
        }
        for (const [index, task] of tasks.entries()) {
            if (typeof task !== "function") {
                throw new Error(`chidori.parallel task ${index} must be a function`);
            }
        }
        const concurrency = Math.max(
            1,
            Math.min(
                tasks.length || 1,
                Number(options && options.concurrency) || tasks.length || 1,
            ),
        );
        const results = new Array(tasks.length);
        let next = 0;

        async function worker() {
            while (next < tasks.length) {
                const index = next;
                next += 1;
                results[index] = await tasks[index]();
            }
        }

        await Promise.all(Array.from({ length: concurrency }, () => worker()));
        return results;
    };

    globalThis.__chidori_install_memory_helpers = function installMemoryHelpers() {
        const current = globalThis.chidori && globalThis.chidori.memory;
        if (typeof current !== "function") {
            return null;
        }
        const memoryCall = current.__chidori_call || current;
        function memory(...args) {
            return memoryCall.call(globalThis.chidori, ...args);
        }
        memory.__chidori_call = memoryCall;
        memory.set = memory.set || function set(key, value, options) {
            return memory("set", key, value, options);
        };
        memory.get = memory.get || function get(key, options) {
            return memory("get", key, null, options);
        };
        memory.delete = memory.delete || function deleteKey(key, options) {
            return memory("delete", key, null, options);
        };
        memory.clear = memory.clear || function clear(options) {
            return memory("clear", null, null, options);
        };
        globalThis.chidori.memory = memory;
        return null;
    };
    globalThis.__chidori_install_memory_helpers();

    // chidori.context(): an immutable, turn-structured prompt builder. Each
    // builder call allocates ONE new node pointing at its parent, so contexts
    // share their prefix structurally — `base.user("a")` and `base.user("b")`
    // share every segment of `base`. Only `.prompt()` / `.respond()` cross the
    // host boundary (as the durable prompt effect, carrying the flattened
    // chain); building is pure in-VM work.
    (() => {
        const nativePrompt = globalThis.chidori.prompt;
        const nativeDigest = globalThis.chidori.__contextDigest;
        if (typeof nativePrompt !== "function") {
            return;
        }
        function flatten(ctx) {
            const out = [];
            let node = ctx;
            while (node) {
                if (node.__segment) out.push(node.__segment);
                node = node.__parent;
            }
            out.reverse();
            return out;
        }
        function deepFreeze(value) {
            if (value && typeof value === "object" && !Object.isFrozen(value)) {
                Object.freeze(value);
                for (const key of Object.keys(value)) deepFreeze(value[key]);
            }
            return value;
        }
        async function send(ctx, options, mode) {
            const opts = Object.assign({}, options || {}, {
                __context: flatten(ctx),
                __mode: mode,
            });
            const out = await nativePrompt.call(globalThis.chidori, "", opts);
            let extended = ctx;
            for (const message of out.messages || []) {
                extended = append(extended, { kind: "message", message });
            }
            return { out, extended };
        }
        const proto = {
            system(text) {
                return append(this, { kind: "system", text: String(text) });
            },
            tools(names) {
                return append(this, {
                    kind: "tools",
                    names: (names || []).map(String),
                });
            },
            doc(label, text) {
                return append(this, {
                    kind: "doc",
                    label: String(label),
                    text: String(text),
                });
            },
            user(text) {
                return append(this, { kind: "user", text: String(text) });
            },
            assistant(text) {
                return append(this, { kind: "assistant", text: String(text) });
            },
            toolResult(id, content, isError) {
                return append(this, {
                    kind: "toolResult",
                    id: String(id),
                    content: String(content),
                    isError: !!isError,
                });
            },
            cacheBreakpoint(ttl) {
                return append(this, {
                    kind: "cacheBreakpoint",
                    ttl: ttl === "1h" ? "1h" : "5m",
                });
            },
            async prompt(options) {
                const { out, extended } = await send(this, options, "prompt");
                return { text: out.text, context: extended };
            },
            async respond(options) {
                const { out, extended } = await send(this, options, "respond");
                return { response: out.response, context: extended };
            },
            digest(options) {
                return nativeDigest.call(globalThis.chidori, flatten(this), options || null);
            },
            estimateTokens() {
                let chars = 0;
                for (const segment of flatten(this)) {
                    if (typeof segment.text === "string") chars += segment.text.length;
                    if (segment.kind === "message") chars += JSON.stringify(segment.message).length;
                }
                return Math.ceil(chars / 4);
            },
        };
        function append(parent, segment) {
            const ctx = Object.create(proto);
            Object.defineProperty(ctx, "__parent", { value: parent });
            Object.defineProperty(ctx, "__segment", { value: deepFreeze(segment) });
            return Object.freeze(ctx);
        }
        globalThis.chidori.context = function context(seed) {
            let ctx = append(null, null);
            if (seed && typeof seed.system === "string") ctx = ctx.system(seed.system);
            if (seed && Array.isArray(seed.tools)) ctx = ctx.tools(seed.tools);
            return ctx;
        };
    })();

    if (typeof globalThis.__chidori_workspace_write === "function") {
        globalThis.chidori.workspace = {
            list(options) {
                return globalThis.__chidori_workspace_list(options || {});
            },
            read(path) {
                return globalThis.__chidori_workspace_read(path);
            },
            write(path, content, options) {
                return globalThis.__chidori_workspace_write(path, content, options || {});
            },
            delete(path, reason) {
                return globalThis.__chidori_workspace_delete(path, reason || null);
            },
            remove(path, reason) {
                return globalThis.__chidori_workspace_delete(path, reason || null);
            },
            manifest() {
                return globalThis.__chidori_workspace_manifest();
            },
        };
    }

    return null;
})()
"#;

/// APIs, but `node:buffer` and `node:fs` shims (and lots of real packages) need
/// `TextEncoder`/`TextDecoder`/`atob`/`btoa`. Pure-JS, deterministic, no host
/// access — safe to install unconditionally like `URLSearchParams`.
pub(crate) const TEXT_ENCODING_POLYFILL: &str = r#"
(function () {
    const B64 = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    if (typeof globalThis.btoa !== "function") {
        globalThis.btoa = function (bin) {
            bin = String(bin);
            let out = "";
            for (let i = 0; i < bin.length; i += 3) {
                const a = bin.charCodeAt(i);
                const b = bin.charCodeAt(i + 1);
                const c = bin.charCodeAt(i + 2);
                const e1 = a >> 2;
                const e2 = ((a & 3) << 4) | (b >> 4);
                const e3 = isNaN(b) ? 64 : (((b & 15) << 2) | (c >> 6));
                const e4 = isNaN(c) ? 64 : (c & 63);
                out += B64[e1] + B64[e2] + (e3 === 64 ? "=" : B64[e3]) + (e4 === 64 ? "=" : B64[e4]);
            }
            return out;
        };
    }
    if (typeof globalThis.atob !== "function") {
        globalThis.atob = function (b64) {
            b64 = String(b64).replace(/[^A-Za-z0-9+/]/g, "");
            let out = "";
            let buffer = 0;
            let bits = 0;
            for (let i = 0; i < b64.length; i++) {
                const idx = B64.indexOf(b64[i]);
                if (idx < 0) continue;
                buffer = (buffer << 6) | idx;
                bits += 6;
                if (bits >= 8) {
                    bits -= 8;
                    out += String.fromCharCode((buffer >> bits) & 0xff);
                }
            }
            return out;
        };
    }
    if (typeof globalThis.TextEncoder !== "function") {
        globalThis.TextEncoder = class TextEncoder {
            get encoding() { return "utf-8"; }
            encode(str) {
                str = String(str === undefined ? "" : str);
                const out = [];
                for (let i = 0; i < str.length; i++) {
                    let c = str.charCodeAt(i);
                    if (c < 0x80) {
                        out.push(c);
                    } else if (c < 0x800) {
                        out.push(0xc0 | (c >> 6), 0x80 | (c & 0x3f));
                    } else if (c >= 0xd800 && c <= 0xdbff) {
                        const c2 = str.charCodeAt(++i);
                        const cp = 0x10000 + ((c - 0xd800) << 10) + (c2 - 0xdc00);
                        out.push(
                            0xf0 | (cp >> 18),
                            0x80 | ((cp >> 12) & 0x3f),
                            0x80 | ((cp >> 6) & 0x3f),
                            0x80 | (cp & 0x3f)
                        );
                    } else {
                        out.push(0xe0 | (c >> 12), 0x80 | ((c >> 6) & 0x3f), 0x80 | (c & 0x3f));
                    }
                }
                return new Uint8Array(out);
            }
        };
    }
    if (typeof globalThis.TextDecoder !== "function") {
        globalThis.TextDecoder = class TextDecoder {
            get encoding() { return "utf-8"; }
            decode(buf) {
                if (buf === undefined) return "";
                const bytes = buf instanceof Uint8Array
                    ? buf
                    : new Uint8Array(buf.buffer ? buf.buffer : buf);
                let out = "";
                let i = 0;
                while (i < bytes.length) {
                    const c = bytes[i++];
                    if (c < 0x80) {
                        out += String.fromCharCode(c);
                    } else if (c < 0xe0) {
                        out += String.fromCharCode(((c & 0x1f) << 6) | (bytes[i++] & 0x3f));
                    } else if (c < 0xf0) {
                        out += String.fromCharCode(
                            ((c & 0x0f) << 12) | ((bytes[i++] & 0x3f) << 6) | (bytes[i++] & 0x3f)
                        );
                    } else {
                        const cp =
                            ((c & 0x07) << 18) |
                            ((bytes[i++] & 0x3f) << 12) |
                            ((bytes[i++] & 0x3f) << 6) |
                            (bytes[i++] & 0x3f);
                        const u = cp - 0x10000;
                        out += String.fromCharCode(0xd800 + (u >> 10), 0xdc00 + (u & 0x3ff));
                    }
                }
                return out;
            }
        };
    }
})();
"#;

/// `globalThis.crypto` (Web Crypto subset): `getRandomValues`, `randomUUID`,
/// and `subtle.digest`. Randomness routes through the captured native, so it is
/// flagged and replayed like `node:crypto`. Installed unconditionally; the
/// native throws if the crypto policy is `disabled`.
pub(crate) const WEB_CRYPTO_POLYFILL: &str = r#"
(function () {
    if (globalThis.crypto && typeof globalThis.crypto.getRandomValues === "function") return;
    function base64ToBytes(b64) {
        const bin = atob(b64);
        const out = new Uint8Array(bin.length);
        for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
        return out;
    }
    function bytesToBase64(bytes) {
        let s = "";
        for (let i = 0; i < bytes.length; i++) s += String.fromCharCode(bytes[i]);
        return btoa(s);
    }
    const cryptoObj = {
        getRandomValues(typedArray) {
            if (!ArrayBuffer.isView(typedArray)) {
                throw new TypeError("crypto.getRandomValues expects a typed array");
            }
            const view = new Uint8Array(
                typedArray.buffer,
                typedArray.byteOffset,
                typedArray.byteLength
            );
            const bytes = base64ToBytes(globalThis.__chidori_crypto_random(view.length));
            view.set(bytes.subarray(0, view.length));
            return typedArray;
        },
        randomUUID() {
            const b = base64ToBytes(globalThis.__chidori_crypto_random(16));
            b[6] = (b[6] & 0x0f) | 0x40;
            b[8] = (b[8] & 0x3f) | 0x80;
            const h = [];
            for (let i = 0; i < 16; i++) h.push(b[i].toString(16).padStart(2, "0"));
            return `${h[0]}${h[1]}${h[2]}${h[3]}-${h[4]}${h[5]}-${h[6]}${h[7]}-${h[8]}${h[9]}-${h[10]}${h[11]}${h[12]}${h[13]}${h[14]}${h[15]}`;
        },
        subtle: {
            async digest(algorithm, data) {
                const alg = typeof algorithm === "string" ? algorithm : (algorithm && algorithm.name);
                let bytes;
                if (typeof data === "string") bytes = new TextEncoder().encode(data);
                else if (ArrayBuffer.isView(data)) bytes = new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
                else bytes = new Uint8Array(data);
                const out = base64ToBytes(globalThis.__chidori_crypto_hash(alg, bytesToBase64(bytes)));
                return out.buffer;
            },
        },
    };
    Object.defineProperty(globalThis, "crypto", {
        value: cryptoObj,
        writable: true,
        configurable: true,
    });
})();
"#;

pub(crate) const URL_SEARCH_PARAMS_POLYFILL: &str = r#"
globalThis.URLSearchParams = class URLSearchParams {
    constructor(init) {
        this._p = [];
        if (typeof init === "string") {
            const s = init.charAt(0) === "?" ? init.slice(1) : init;
            if (s.length) {
                for (const pair of s.split("&")) {
                    const i = pair.indexOf("=");
                    const k = i === -1 ? pair : pair.slice(0, i);
                    const v = i === -1 ? "" : pair.slice(i + 1);
                    this._p.push([decodeURIComponent(k), decodeURIComponent(v.replace(/\+/g, " "))]);
                }
            }
        } else if (init && typeof init === "object") {
            const entries = typeof init.forEach === "function" && !Array.isArray(init)
                ? Array.from(init)
                : (Array.isArray(init) ? init : Object.entries(init));
            for (const [k, v] of entries) this._p.push([String(k), String(v)]);
        }
    }
    append(k, v) { this._p.push([String(k), String(v)]); }
    set(k, v) { this.delete(k); this._p.push([String(k), String(v)]); }
    get(k) { const e = this._p.find((p) => p[0] === k); return e ? e[1] : null; }
    getAll(k) { return this._p.filter((p) => p[0] === k).map((p) => p[1]); }
    has(k) { return this._p.some((p) => p[0] === k); }
    delete(k) { this._p = this._p.filter((p) => p[0] !== k); }
    forEach(cb) { for (const [k, v] of this._p) cb(v, k, this); }
    toString() {
        return this._p
            .map(([k, v]) => encodeURIComponent(k) + "=" + encodeURIComponent(v))
            .join("&");
    }
};
"#;

/// Virtual timer queue: deterministic, driven by the logical clock. Timers fire
/// in `(deadline, id)` order via a self-rescheduling microtask pump, so they
/// run inside the engine's normal job drain without any real wall-clock sleep.
/// Uses plain arrays (not `Map`/`Set`, which the snapshot policy may disable).
pub(crate) const TIMER_VIRTUAL_POLYFILL: &str = r#"
(function () {
    const tasks = [];
    let nextId = 1;
    let pumping = false;
    let fired = 0;
    const MAX_FIRES = 1000000;
    function schedule(cb, delay, args, repeat) {
        if (typeof cb !== "function") {
            throw new TypeError("timer callback must be a function");
        }
        const d = Math.max(0, Math.floor(Number(delay) || 0));
        const id = nextId++;
        tasks.push({ id, deadline: globalThis.__chidori_now + d, interval: repeat ? d : null, cb, args });
        if (typeof globalThis.__chidori_note_capability === "function") {
            globalThis.__chidori_note_capability("timer");
        }
        if (!pumping) {
            pumping = true;
            Promise.resolve().then(pump);
        }
        return id;
    }
    function earliestIndex() {
        let best = -1;
        for (let i = 0; i < tasks.length; i++) {
            if (best === -1 ||
                tasks[i].deadline < tasks[best].deadline ||
                (tasks[i].deadline === tasks[best].deadline && tasks[i].id < tasks[best].id)) {
                best = i;
            }
        }
        return best;
    }
    function pump() {
        if (tasks.length === 0) { pumping = false; return; }
        if (fired++ > MAX_FIRES) {
            pumping = false;
            tasks.length = 0;
            throw new Error("Chidori timer pump exceeded " + MAX_FIRES + " firings (runaway setInterval?)");
        }
        const idx = earliestIndex();
        const task = tasks[idx];
        if (task.deadline > globalThis.__chidori_now) {
            globalThis.__chidori_now = task.deadline;
        }
        if (task.interval != null) {
            task.deadline = globalThis.__chidori_now + task.interval;
        } else {
            tasks.splice(idx, 1);
        }
        // Reschedule before invoking so the pump survives a throwing callback.
        Promise.resolve().then(pump);
        task.cb.apply(undefined, task.args);
    }
    globalThis.setTimeout = function setTimeout(cb, delay, ...args) {
        return schedule(cb, delay, args, false);
    };
    globalThis.setInterval = function setInterval(cb, delay, ...args) {
        return schedule(cb, delay, args, true);
    };
    globalThis.setImmediate = function setImmediate(cb, ...args) {
        return schedule(cb, 0, args, false);
    };
    function clear(id) {
        for (let i = 0; i < tasks.length; i++) {
            if (tasks[i].id === id) { tasks.splice(i, 1); return; }
        }
    }
    globalThis.clearTimeout = clear;
    globalThis.clearInterval = clear;
    globalThis.clearImmediate = clear;
    if (typeof globalThis.queueMicrotask !== "function") {
        globalThis.queueMicrotask = function queueMicrotask(cb) {
            if (typeof cb !== "function") throw new TypeError("queueMicrotask callback must be a function");
            if (typeof globalThis.__chidori_note_capability === "function") {
                globalThis.__chidori_note_capability("microtask");
            }
            Promise.resolve().then(cb);
        };
    }
})();
"#;

/// Timer surface under `timers=disabled`: scheduling throws, so an agent that
/// must not schedule fails loudly rather than silently no-op'ing.
pub(crate) const TIMER_DISABLED_POLYFILL: &str = r#"
(function () {
    const blocked = function () {
        throw new Error("timers are disabled by Chidori runtime policy (timers=disabled)");
    };
    globalThis.setTimeout = blocked;
    globalThis.setInterval = blocked;
    globalThis.setImmediate = blocked;
    globalThis.clearTimeout = function () {};
    globalThis.clearInterval = function () {};
    globalThis.clearImmediate = function () {};
})();
"#;
