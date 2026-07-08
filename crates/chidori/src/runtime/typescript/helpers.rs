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

    // In-VM convenience helpers live under chidori.util, NOT flat on the host
    // object: everything else on `chidori` is a recorded durable host call,
    // and these three are pure JS control flow that records nothing.
    globalThis.chidori.util = globalThis.chidori.util || {};

    globalThis.chidori.util.tryCall = async function tryCall(fn) {
        try {
            return { ok: true, value: await fn() };
        } catch (err) {
            return {
                ok: false,
                error: String(err && err.message ? err.message : err),
            };
        }
    };

    globalThis.chidori.util.retry = async function retry(fn, options) {
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

    globalThis.chidori.util.parallel = async function parallel(tasks, options) {
        if (!Array.isArray(tasks)) {
            throw new Error("chidori.util.parallel expects an array of task functions");
        }
        // Accept the Promise.all idiom too: a task may be a thunk (preferred —
        // it lets the scheduler control start time and concurrency), an
        // already-started promise, or a plain value. Promises/values are
        // wrapped as pre-resolved tasks instead of throwing.
        const normalized = tasks.map((task) =>
            typeof task === "function" ? task : () => task,
        );
        const concurrency = Math.max(
            1,
            Math.min(
                normalized.length || 1,
                Number(options && options.concurrency) || normalized.length || 1,
            ),
        );
        const results = new Array(normalized.length);
        let next = 0;

        async function worker() {
            while (next < normalized.length) {
                const index = next;
                next += 1;
                results[index] = await normalized[index]();
            }
        }

        await Promise.all(Array.from({ length: concurrency }, () => worker()));
        return results;
    };

    // chidori.memory.<method> — the persistent KV store as a plain namespace.
    // The native binding is a single action-dispatch function; it is wrapped
    // here so authors never see the positional (action, key, value, options)
    // form.
    globalThis.__chidori_install_memory_helpers = function installMemoryHelpers() {
        const current = globalThis.chidori && globalThis.chidori.memory;
        if (typeof current !== "function") {
            return null;
        }
        const memoryCall = current.__chidori_call || current;
        const call = (...args) => memoryCall.call(globalThis.chidori, ...args);
        globalThis.chidori.memory = {
            __chidori_call: memoryCall,
            set(key, value, options) {
                return call("set", key, value, options);
            },
            get(key, options) {
                return call("get", key, null, options);
            },
            delete(key, options) {
                return call("delete", key, null, options);
            },
            list(options) {
                return call("list", null, null, options);
            },
            clear(options) {
                return call("clear", null, null, options);
            },
        };
        return null;
    };
    globalThis.__chidori_install_memory_helpers();

    // chidori.actors: wrap the native durable ops so spawn/lookup hand back a
    // HANDLE — an object with the actor's address plus send/join/stop/status
    // methods — instead of making authors thread pid strings around. The
    // string-addressed forms remain on the namespace for actors known only by
    // pid or registered name. Handles are plain in-VM sugar: every method
    // still bottoms out in one recorded durable host call.
    globalThis.__chidori_install_actor_helpers = function installActorHelpers() {
        const native = globalThis.chidori && globalThis.chidori.actors;
        if (!native || typeof native.spawn !== "function" || native.__chidori_wrapped) {
            return null;
        }
        function makeHandle(info) {
            const pid = info.pid;
            const handle = {
                pid,
                name: info.name ?? null,
                send(name, payload) {
                    return native.send(pid, name, payload);
                },
                join(options) {
                    return native.join(pid, options);
                },
                stop(options) {
                    return native.stop(pid, options);
                },
                status() {
                    return native.status(pid);
                },
            };
            return handle;
        }
        globalThis.chidori.actors = {
            __chidori_wrapped: true,
            async spawn(source, input, options) {
                return makeHandle(await native.spawn(source, input, options));
            },
            send(to, name, payload) {
                return native.send(to, name, payload);
            },
            join(target, options) {
                return native.join(target, options);
            },
            stop(target, options) {
                return native.stop(target, options);
            },
            status(target) {
                return native.status(target);
            },
            async lookup(name) {
                const found = await native.lookup(name);
                return found && found.pid ? makeHandle({ pid: found.pid, name }) : null;
            },
        };
        return null;
    };
    globalThis.__chidori_install_actor_helpers();

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
        // A "turn" is a segment that lands in the conversation `messages`
        // array (vs the stable system/tools head). `doc` is intentionally not
        // a turn: docs anchor the cacheable head, but a doc appended after the
        // conversation started is still summarizable content (see compact()).
        function isTurn(segment) {
            return segment.kind === "user" || segment.kind === "assistant" ||
                segment.kind === "toolResult" || segment.kind === "message" ||
                segment.kind === "summary";
        }
        function renderBlock(block) {
            if (!block || typeof block !== "object") return String(block);
            if (block.type === "text") return block.text;
            if (block.type === "tool_use") {
                return "[tool call " + block.name + ": " + JSON.stringify(block.input) + "]";
            }
            if (block.type === "tool_result") {
                return "[tool result: " + block.content + "]";
            }
            return JSON.stringify(block);
        }
        function renderSegment(segment) {
            if (segment.kind === "user") return "User: " + segment.text;
            if (segment.kind === "assistant") return "Assistant: " + segment.text;
            if (segment.kind === "summary") {
                return "Summary of earlier conversation: " + segment.text;
            }
            if (segment.kind === "toolResult") {
                return "Tool result (" + segment.id + "): " + segment.content;
            }
            if (segment.kind === "doc") {
                return "Document \"" + segment.label + "\": " + segment.text;
            }
            if (segment.kind === "message") {
                const role = segment.message.role === "assistant" ? "Assistant" : "User";
                const content = segment.message.content || [];
                return role + ": " + content.map(renderBlock).join("\n");
            }
            return "";
        }
        const DEFAULT_COMPACT_INSTRUCTIONS =
            "You compact conversation history. Summarize the transcript into a " +
            "concise brief that preserves every fact, decision, constraint, open " +
            "question, and commitment needed to continue the conversation " +
            "faithfully. Reply with only the summary.";
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
            // Explicit, opt-in window compaction: summarize the old turns into
            // ONE summary segment (via a recorded `prompt` host call, so the
            // result is durable and replays deterministically) and rebuild the
            // chain as head + summary + fresh cache breakpoint + the kept
            // tail. Pure no-op (no host call) when there is nothing to
            // compact or the context is still within `budgetTokens`.
            async compact(options) {
                const opts = options || {};
                const keepTurns = Number.isFinite(Number(opts.keepTurns))
                    ? Math.max(0, Math.floor(Number(opts.keepTurns)))
                    : 2;
                const budget = Number(opts.budgetTokens);
                if (Number.isFinite(budget) && budget > 0 && this.estimateTokens() <= budget) {
                    return this;
                }
                const flat = flatten(this);
                let headEnd = 0;
                while (headEnd < flat.length && !isTurn(flat[headEnd])) headEnd += 1;
                const head = flat.slice(0, headEnd);
                const tail = flat.slice(headEnd);
                const turnCount = tail.filter(isTurn).length;
                if (turnCount <= keepTurns) {
                    return this;
                }
                // Cut so the last `keepTurns` turns (plus anything appended
                // after them) survive verbatim; everything older is
                // summarized. Breakpoint markers in the old region are
                // placement metadata, not content — drop them.
                let cut = tail.length;
                let seen = 0;
                for (let i = tail.length - 1; i >= 0; i -= 1) {
                    if (isTurn(tail[i])) {
                        seen += 1;
                        if (seen === keepTurns) { cut = i; break; }
                    }
                }
                const old = tail.slice(0, cut).filter((s) => s.kind !== "cacheBreakpoint");
                const kept = tail.slice(cut);
                if (old.length === 0) {
                    return this;
                }
                const transcript = old.map(renderSegment).join("\n");
                const promptOpts = {
                    system: typeof opts.instructions === "string" && opts.instructions
                        ? opts.instructions
                        : DEFAULT_COMPACT_INSTRUCTIONS,
                };
                if (typeof opts.model === "string") promptOpts.model = opts.model;
                if (opts.maxTokens !== undefined) promptOpts.maxTokens = opts.maxTokens;
                if (opts.cache !== undefined) promptOpts.cache = opts.cache;
                const summary = await nativePrompt.call(
                    globalThis.chidori,
                    transcript,
                    promptOpts,
                );
                let ctx = append(null, null);
                for (const segment of head) ctx = append(ctx, segment);
                ctx = append(ctx, { kind: "summary", text: String(summary) });
                ctx = append(ctx, {
                    kind: "cacheBreakpoint",
                    ttl: opts.ttl === "1h" ? "1h" : "5m",
                });
                for (const segment of kept) ctx = append(ctx, segment);
                return ctx;
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

        // chidori.conversation(): a small stateful wrapper over context() for the
        // most common shape — a multi-turn chat assistant. It owns the current
        // immutable context (system + tools head frozen as a cacheable prefix)
        // and threads each turn through it, so authors write `chat.say(msg)`
        // instead of re-plumbing `ctx = (await ctx.user(msg).prompt()).context`
        // by hand. Every turn is still one durable `prompt`/`respond` host call,
        // replays for free, and reads the shared prefix at the cached rate.
        globalThis.chidori.conversation = function conversation(options) {
            const opts = options || {};
            // Per-turn LLM options carried forward on every turn (a per-call
            // override still wins). `tools` live in the context head instead.
            const defaults = {};
            for (const key of ["model", "maxTokens", "max_tokens", "temperature", "cache"]) {
                if (opts[key] !== undefined) defaults[key] = opts[key];
            }
            const defaultType = typeof opts.type === "string" ? opts.type : "final";
            // Opt-in window management: when set, each turn first runs the same
            // budgeted `context.compact()` no-op until the tail exceeds budget.
            const compactOptions =
                opts.compact && typeof opts.compact === "object" ? opts.compact : null;

            let ctx = globalThis.chidori.context({
                system: opts.system,
                tools: opts.tools,
            });
            // Freeze the stable head (system + tools) as a cacheable prefix so
            // every turn after the first reads it at the discounted cached rate.
            if (opts.system || (Array.isArray(opts.tools) && opts.tools.length)) {
                ctx = ctx.cacheBreakpoint(opts.cacheTtl === "1h" ? "1h" : "5m");
            }
            const turns = [];

            function turnOptions(perTurn) {
                return Object.assign({ type: defaultType }, defaults, perTurn || {});
            }

            const api = {
                // The underlying immutable context, for dropping down to the
                // lower-level API (digest, estimateTokens, manual compact, fork).
                get context() {
                    return ctx;
                },
                // Number of completed exchanges (user+assistant pairs) recorded.
                get length() {
                    return turns.filter((t) => t.role === "assistant").length;
                },
                // The transcript so far as plain {role, text} entries.
                history() {
                    return turns.map((t) => ({ role: t.role, text: t.text }));
                },
                // Send one user message, return the assistant's reply text. The
                // conversation advances in place: the user and assistant turns
                // are appended to the context for the next call.
                async say(message, perTurn) {
                    const text = String(message == null ? "" : message);
                    if (compactOptions) ctx = await ctx.compact(compactOptions);
                    ctx = ctx.user(text);
                    turns.push({ role: "user", text });
                    const result = await ctx.prompt(turnOptions(perTurn));
                    ctx = result.context;
                    turns.push({ role: "assistant", text: result.text });
                    return result.text;
                },
                // Like say(), but returns the structured response (toolCalls,
                // blocks) for author-driven tool loops. Append tool results with
                // `chat.context.toolResult(...)` then call `chat.say(...)` again.
                async respond(message, perTurn) {
                    const text = String(message == null ? "" : message);
                    if (compactOptions) ctx = await ctx.compact(compactOptions);
                    ctx = ctx.user(text);
                    turns.push({ role: "user", text });
                    const result = await ctx.respond(turnOptions(perTurn));
                    ctx = result.context;
                    const reply =
                        result.response && typeof result.response.content === "string"
                            ? result.response.content
                            : "";
                    turns.push({ role: "assistant", text: reply });
                    return result.response;
                },
                // Drive an interactive loop: read a human message via
                // chidori.input() (terminal stdin under `chidori run`, a paused
                // session resume under `chidori serve`), reply with say(), repeat
                // until the user types an exit word or `until` returns true.
                // Returns the full transcript.
                async loop(loopOptions) {
                    const lo = loopOptions || {};
                    const exits = (
                        Array.isArray(lo.exit)
                            ? lo.exit
                            : lo.exit != null
                              ? [lo.exit]
                              : ["exit", "quit"]
                    ).map((s) => String(s).toLowerCase());
                    const maxTurns = Number.isFinite(Number(lo.maxTurns))
                        ? Number(lo.maxTurns)
                        : Infinity;
                    let turn = 0;
                    while (turn < maxTurns) {
                        const promptText =
                            typeof lo.prompt === "function"
                                ? lo.prompt(turn)
                                : lo.prompt || "You:";
                        const raw = await globalThis.chidori.input(
                            promptText,
                            lo.inputOptions || { type: "message" },
                        );
                        const message = String(raw == null ? "" : raw).trim();
                        if (message === "" && lo.skipEmpty !== false) continue;
                        if (exits.indexOf(message.toLowerCase()) !== -1) break;
                        const reply = await api.say(message, lo.turn);
                        if (typeof lo.onReply === "function") {
                            await lo.onReply(reply, message);
                        }
                        turn += 1;
                        if (typeof lo.until === "function" && lo.until(message, reply)) {
                            break;
                        }
                    }
                    return api.history();
                },
            };
            return api;
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

/// The WHATWG `fetch` surface — `fetch` plus the `Headers`/`Request`/`Response`
/// classes — implemented on top of the captured networking host op
/// (`globalThis.__chidori_http`). This is the runtime's *replacement* for the
/// base networking APIs Node/the platform would otherwise provide: there is no
/// public `chidori.http`. Because real packages reach the network through
/// `fetch` (and `node:http`/`node:https`, whose shims route through the same
/// host op), installing the capture here means every dependent library
/// automatically inherits the security policy (allow / ask / deny), the
/// approval-pause path, and deterministic record/replay — without any per-call
/// opt-in.
///
/// `__chidori_http` is synchronous (the host dispatch returns the response
/// object inline). `fetch` performs that call *outside* any `try`/`catch` so an
/// AskBefore policy's pause sentinel keeps unwinding to the engine exactly as it
/// does for the `node:http` shim; the result is then wrapped in a resolved
/// `Promise<Response>`. Installed after `install_chidori_effects` (which defines
/// `__chidori_http`), so it is absent from the side-effect-free tool-metadata
/// prelude where `globalThis.fetch` is explicitly nulled.
pub(crate) const FETCH_POLYFILL: &str = r#"
(function () {
    if (typeof globalThis.fetch === "function" && globalThis.fetch.__chidori) return;

    // Case-insensitive header bag — the WHATWG Headers subset packages touch.
    class Headers {
        constructor(init) {
            this._map = {};
            if (init instanceof Headers) {
                init.forEach((v, k) => this.append(k, v));
            } else if (Array.isArray(init)) {
                for (const pair of init) this.append(pair[0], pair[1]);
            } else if (init && typeof init === "object") {
                for (const k of Object.keys(init)) this.append(k, init[k]);
            }
        }
        append(name, value) {
            const key = String(name).toLowerCase();
            const v = String(value);
            this._map[key] = this._map[key] === undefined ? v : this._map[key] + ", " + v;
        }
        set(name, value) { this._map[String(name).toLowerCase()] = String(value); }
        get(name) {
            const v = this._map[String(name).toLowerCase()];
            return v === undefined ? null : v;
        }
        has(name) { return Object.prototype.hasOwnProperty.call(this._map, String(name).toLowerCase()); }
        delete(name) { delete this._map[String(name).toLowerCase()]; }
        forEach(cb, thisArg) {
            for (const k of Object.keys(this._map)) cb.call(thisArg, this._map[k], k, this);
        }
        keys() { return Object.keys(this._map)[Symbol.iterator](); }
        values() { return Object.keys(this._map).map((k) => this._map[k])[Symbol.iterator](); }
        entries() { return Object.keys(this._map).map((k) => [k, this._map[k]])[Symbol.iterator](); }
        [Symbol.iterator]() { return this.entries(); }
    }

    // Normalize a request body to what the captured host op accepts: a string on
    // the wire (objects JSON-encoded, matching the node:http shim), or undefined.
    function normalizeBody(body) {
        if (body === undefined || body === null) return undefined;
        if (typeof body === "string") return body;
        if (typeof URLSearchParams !== "undefined" && body instanceof URLSearchParams) {
            return body.toString();
        }
        if (body instanceof ArrayBuffer) return new TextDecoder().decode(new Uint8Array(body));
        if (ArrayBuffer.isView(body)) {
            return new TextDecoder().decode(new Uint8Array(body.buffer, body.byteOffset, body.byteLength));
        }
        return JSON.stringify(body);
    }

    // The host returns a parsed JSON value when the body is JSON, else a string.
    // Recover text for `.text()`/`.arrayBuffer()` symmetrically with the shim.
    function bodyToText(raw) {
        if (raw === undefined || raw === null) return "";
        return typeof raw === "string" ? raw : JSON.stringify(raw);
    }

    class Response {
        constructor(body, init) {
            init = init || {};
            this._raw = body;
            this.status = init.status === undefined ? 200 : init.status;
            this.statusText = init.statusText || "";
            this.headers = init.headers instanceof Headers ? init.headers : new Headers(init.headers || {});
            this.ok = this.status >= 200 && this.status < 300;
            this.url = init.url || "";
            this.redirected = false;
            this.bodyUsed = false;
            this.type = "basic";
        }
        async text() { this.bodyUsed = true; return bodyToText(this._raw); }
        async json() {
            this.bodyUsed = true;
            if (this._raw && typeof this._raw === "object") return this._raw;
            const t = bodyToText(this._raw);
            return t === "" ? null : JSON.parse(t);
        }
        async arrayBuffer() {
            this.bodyUsed = true;
            return new TextEncoder().encode(bodyToText(this._raw)).buffer;
        }
        clone() {
            return new Response(this._raw, {
                status: this.status, statusText: this.statusText,
                headers: this.headers, url: this.url,
            });
        }
    }

    class Request {
        constructor(input, init) {
            init = init || {};
            this.url = typeof input === "string" ? input : (input && input.url) || String(input);
            const inheritedMethod = (input && input.method) || "GET";
            this.method = String(init.method || inheritedMethod).toUpperCase();
            this.headers = new Headers(init.headers || (input && input.headers) || {});
            this.body = init.body !== undefined ? init.body : (input && input.body);
        }
    }

    function fetch(input, init) {
        init = init || {};
        let url, method, headers, body;
        if (input instanceof Request) {
            url = input.url;
            method = String(init.method || input.method || "GET").toUpperCase();
            headers = new Headers(input.headers);
            if (init.headers) new Headers(init.headers).forEach((v, k) => headers.set(k, v));
            body = init.body !== undefined ? init.body : input.body;
        } else {
            url = typeof input === "string" ? input : (input && input.href) || String(input);
            method = String(init.method || "GET").toUpperCase();
            headers = new Headers(init.headers || {});
            body = init.body;
        }
        const headerObj = {};
        headers.forEach((v, k) => { headerObj[k] = v; });
        const options = { method: method, headers: headerObj };
        const normalized = normalizeBody(body);
        if (normalized !== undefined) options.body = normalized;

        // Synchronous, policy-gated, captured host call. Deliberately not wrapped
        // in try/catch: an AskBefore policy throws the pause sentinel here and it
        // must keep unwinding to the engine (same contract as the node:http shim).
        const res = globalThis.__chidori_http(url, options);
        // fetch only rejects on transport failure (status 0 + error), never on a
        // non-2xx HTTP status — that surfaces via `response.ok`/`response.status`.
        if (res && res.status === 0 && res.error) {
            return Promise.reject(new TypeError("fetch failed: " + res.error));
        }
        return Promise.resolve(new Response(res ? res.body : null, {
            status: res ? res.status : 0,
            headers: res ? res.headers : {},
            url: url,
        }));
    }
    fetch.__chidori = true;

    globalThis.fetch = fetch;
    globalThis.Headers = Headers;
    globalThis.Request = Request;
    globalThis.Response = Response;
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
