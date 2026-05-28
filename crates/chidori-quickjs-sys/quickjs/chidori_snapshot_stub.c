#include "chidori_snapshot.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

enum {
    CHIDORI_JS_UNSUPPORTED = -1,
};

#define CHIDORI_STUB_SNAPSHOT_MAGIC "CHIDORI_QJS_STUB_SNAPSHOT_V1"
#define CHIDORI_RUNTIME_SNAPSHOT_MAGIC "CHIDORI_QJS_RUNTIME_SNAPSHOT_V1"
#define CHIDORI_CONTEXT_SNAPSHOT_MAGIC "CHIDORI_QJS_CONTEXT_SNAPSHOT_V1"

static const char *chidori_context_snapshot_roots[] = {
    "__chidori_exports",
    "__chidori_modules",
    "__chidori_call_result",
    "__chidori_call_error",
    "__chidori_active_host_operation_id",
    "__chidori_host_promises",
    "__chidori_host_calls",
    "__chidori_host_method_queues",
};

typedef struct ChidoriUnsupportedHookEntry {
    JSRuntime *rt;
    CHIDORI_JSUnsupportedHook hook;
    struct ChidoriUnsupportedHookEntry *next;
} ChidoriUnsupportedHookEntry;

static ChidoriUnsupportedHookEntry *chidori_unsupported_hooks;

static ChidoriUnsupportedHookEntry *chidori_find_unsupported_hook(JSRuntime *rt) {
    ChidoriUnsupportedHookEntry *entry = chidori_unsupported_hooks;
    while (entry) {
        if (entry->rt == rt) {
            return entry;
        }
        entry = entry->next;
    }
    return NULL;
}

static void chidori_report_unsupported(JSRuntime *rt,
                                       const char *path,
                                       const char *type_name,
                                       const char *message) {
    ChidoriUnsupportedHookEntry *entry = chidori_find_unsupported_hook(rt);
    if (entry && entry->hook.unsupported) {
        entry->hook.unsupported(entry->hook.opaque, path, type_name, message);
    }
}

static void chidori_report_current_exception(JSContext *ctx,
                                             const char *path,
                                             const char *type_name,
                                             const char *fallback) {
    JSValue exception;
    const char *message;

    if (!ctx) {
        return;
    }
    exception = JS_GetException(ctx);
    message = JS_ToCString(ctx, exception);
    chidori_report_unsupported(JS_GetRuntime(ctx),
                               path,
                               type_name,
                               message ? message : fallback);
    if (message) {
        JS_FreeCString(ctx, message);
    }
    JS_FreeValue(ctx, exception);
}

int CHIDORI_JS_SetSnapshotUnsupportedHook(JSRuntime *rt, CHIDORI_JSUnsupportedHook *hook) {
    if (!rt) {
        return -1;
    }

    ChidoriUnsupportedHookEntry *entry = chidori_find_unsupported_hook(rt);
    if (!hook) {
        if (!entry) {
            return 0;
        }
        ChidoriUnsupportedHookEntry **cursor = &chidori_unsupported_hooks;
        while (*cursor && *cursor != entry) {
            cursor = &(*cursor)->next;
        }
        if (*cursor) {
            *cursor = entry->next;
        }
        free(entry);
        return 0;
    }

    if (!entry) {
        entry = (ChidoriUnsupportedHookEntry *)calloc(1, sizeof(*entry));
        if (!entry) {
            return -1;
        }
        entry->rt = rt;
        entry->next = chidori_unsupported_hooks;
        chidori_unsupported_hooks = entry;
    }
    entry->hook = *hook;
    return 0;
}

static JSValue chidori_host_promise_registry(JSContext *ctx) {
    JSValue global = JS_GetGlobalObject(ctx);
    JSValue registry = JS_GetPropertyStr(ctx, global, "__chidori_host_promises");
    if (JS_IsException(registry)) {
        JS_FreeValue(ctx, global);
        return registry;
    }
    if (JS_IsUndefined(registry)) {
        JS_FreeValue(ctx, registry);
        registry = JS_NewObject(ctx);
        if (JS_IsException(registry)) {
            JS_FreeValue(ctx, global);
            return registry;
        }
        if (JS_SetPropertyStr(ctx, global, "__chidori_host_promises", JS_DupValue(ctx, registry)) < 0) {
            JS_FreeValue(ctx, registry);
            JS_FreeValue(ctx, global);
            return JS_EXCEPTION;
        }
    }
    JS_FreeValue(ctx, global);
    return registry;
}

static void chidori_host_promise_key(uint64_t host_operation_id, char *buf, size_t len) {
    snprintf(buf, len, "%llu", (unsigned long long)host_operation_id);
}

static int chidori_snapshot_write_all(CHIDORI_JSSnapshotWriter *writer,
                                      const void *buf,
                                      size_t len) {
    if (!writer || !writer->write || (!buf && len > 0)) {
        return -1;
    }
    return writer->write(writer->opaque, (const uint8_t *)buf, len);
}

static int chidori_snapshot_read_all(CHIDORI_JSSnapshotReader *reader,
                                     void *buf,
                                     size_t len) {
    if (!reader || !reader->read || (!buf && len > 0)) {
        return -1;
    }
    return reader->read(reader->opaque, (uint8_t *)buf, len);
}

static void chidori_put_u64_le(uint8_t *buf, uint64_t value) {
    int i;
    for (i = 0; i < 8; i++) {
        buf[i] = (uint8_t)(value >> (i * 8));
    }
}

static uint64_t chidori_get_u64_le(const uint8_t *buf) {
    uint64_t value = 0;
    int i;
    for (i = 0; i < 8; i++) {
        value |= ((uint64_t)buf[i]) << (i * 8);
    }
    return value;
}

static int chidori_snapshot_write_u64(CHIDORI_JSSnapshotWriter *writer,
                                      uint64_t value) {
    uint8_t buf[8];
    chidori_put_u64_le(buf, value);
    return chidori_snapshot_write_all(writer, buf, sizeof(buf));
}

static int chidori_snapshot_read_u64(CHIDORI_JSSnapshotReader *reader,
                                     uint64_t *value) {
    uint8_t buf[8];
    if (!value || chidori_snapshot_read_all(reader, buf, sizeof(buf)) < 0) {
        return -1;
    }
    *value = chidori_get_u64_le(buf);
    return 0;
}

static int chidori_snapshot_write_bytes(CHIDORI_JSSnapshotWriter *writer,
                                        const uint8_t *bytes,
                                        size_t len) {
    if (chidori_snapshot_write_u64(writer, (uint64_t)len) < 0) {
        return -1;
    }
    if (len == 0) {
        return 0;
    }
    return chidori_snapshot_write_all(writer, bytes, len);
}

static uint8_t *chidori_snapshot_read_bytes(CHIDORI_JSSnapshotReader *reader,
                                            size_t *len) {
    uint64_t encoded_len;
    uint8_t *bytes;
    if (!len || chidori_snapshot_read_u64(reader, &encoded_len) < 0) {
        return NULL;
    }
    if (encoded_len > SIZE_MAX) {
        return NULL;
    }
    *len = (size_t)encoded_len;
    if (*len == 0) {
        return NULL;
    }
    bytes = (uint8_t *)malloc(*len);
    if (!bytes) {
        return NULL;
    }
    if (chidori_snapshot_read_all(reader, bytes, *len) < 0) {
        free(bytes);
        return NULL;
    }
    return bytes;
}

static JSValue chidori_host_promise_entry(JSContext *ctx, uint64_t host_operation_id) {
    char key[32];
    JSValue registry = chidori_host_promise_registry(ctx);
    if (JS_IsException(registry)) {
        return registry;
    }
    chidori_host_promise_key(host_operation_id, key, sizeof(key));
    JSValue entry = JS_GetPropertyStr(ctx, registry, key);
    JS_FreeValue(ctx, registry);
    return entry;
}

int CHIDORI_JS_SnapshotRuntime(JSRuntime *rt, CHIDORI_JSSnapshotWriter *writer) {
    (void)rt;
    if (!writer || !writer->write) {
        return -1;
    }
    return chidori_snapshot_write_all(writer,
                                      CHIDORI_RUNTIME_SNAPSHOT_MAGIC,
                                      sizeof(CHIDORI_RUNTIME_SNAPSHOT_MAGIC) - 1);
}

JSRuntime *CHIDORI_JS_RestoreRuntime(CHIDORI_JSSnapshotReader *reader) {
    uint8_t magic[sizeof(CHIDORI_RUNTIME_SNAPSHOT_MAGIC) - 1];
    if (!reader || !reader->read) {
        return NULL;
    }
    if (chidori_snapshot_read_all(reader, magic, sizeof(magic)) < 0 ||
        memcmp(magic, CHIDORI_RUNTIME_SNAPSHOT_MAGIC, sizeof(magic)) != 0) {
        return NULL;
    }
    return JS_NewRuntime();
}

int CHIDORI_JS_SnapshotContext(JSContext *ctx, CHIDORI_JSSnapshotWriter *writer) {
    JSValue global = JS_UNDEFINED;
    JSValue holder = JS_UNDEFINED;
    uint8_t *roots_snapshot = NULL;
    uint8_t *microtask_snapshot = NULL;
    size_t roots_snapshot_len = 0;
    size_t microtask_snapshot_len = 0;
    size_t i;
    int ret = -1;

    if (!ctx || !writer || !writer->write) {
        return -1;
    }

    holder = JS_NewObject(ctx);
    if (JS_IsException(holder)) {
        return -1;
    }
    global = JS_GetGlobalObject(ctx);
    if (JS_IsException(global)) {
        goto done;
    }

    for (i = 0; i < sizeof(chidori_context_snapshot_roots) / sizeof(chidori_context_snapshot_roots[0]); i++) {
        const char *root = chidori_context_snapshot_roots[i];
        JSValue value = JS_GetPropertyStr(ctx, global, root);
        if (JS_IsException(value)) {
            goto done;
        }
        if (JS_SetPropertyStr(ctx, holder, root, value) < 0) {
            goto done;
        }
    }

    roots_snapshot = JS_WriteObject(ctx, &roots_snapshot_len, holder,
                                    JS_WRITE_OBJ_BYTECODE | JS_WRITE_OBJ_REFERENCE);
    if (!roots_snapshot) {
        chidori_report_current_exception(ctx,
                                         "$context.roots",
                                         "roots",
                                         "failed to serialize selected context roots");
        goto done;
    }
    microtask_snapshot = CHIDORI_JS_WriteMicrotaskQueue(ctx, &microtask_snapshot_len);
    if (!microtask_snapshot) {
        chidori_report_current_exception(ctx,
                                         "$context.microtask_queue",
                                         "microtask_queue",
                                         "failed to serialize pending microtask queue");
        goto done;
    }

    if (chidori_snapshot_write_all(writer,
                                   CHIDORI_CONTEXT_SNAPSHOT_MAGIC,
                                   sizeof(CHIDORI_CONTEXT_SNAPSHOT_MAGIC) - 1) < 0 ||
        chidori_snapshot_write_bytes(writer, roots_snapshot, roots_snapshot_len) < 0 ||
        chidori_snapshot_write_bytes(writer, microtask_snapshot, microtask_snapshot_len) < 0) {
        goto done;
    }
    ret = 0;

done:
    if (roots_snapshot) {
        js_free(ctx, roots_snapshot);
    }
    if (microtask_snapshot) {
        js_free(ctx, microtask_snapshot);
    }
    JS_FreeValue(ctx, global);
    JS_FreeValue(ctx, holder);
    return ret;
}

JSContext *CHIDORI_JS_RestoreContext(JSRuntime *rt, CHIDORI_JSSnapshotReader *reader) {
    JSContext *ctx = NULL;
    JSValue holder = JS_UNDEFINED;
    JSValue global = JS_UNDEFINED;
    uint8_t magic[sizeof(CHIDORI_CONTEXT_SNAPSHOT_MAGIC) - 1];
    uint8_t *roots_snapshot = NULL;
    uint8_t *microtask_snapshot = NULL;
    size_t roots_snapshot_len = 0;
    size_t microtask_snapshot_len = 0;
    size_t i;

    if (!rt || !reader || !reader->read) {
        return NULL;
    }
    if (chidori_snapshot_read_all(reader, magic, sizeof(magic)) < 0 ||
        memcmp(magic, CHIDORI_CONTEXT_SNAPSHOT_MAGIC, sizeof(magic)) != 0) {
        return NULL;
    }
    roots_snapshot = chidori_snapshot_read_bytes(reader, &roots_snapshot_len);
    if (!roots_snapshot || roots_snapshot_len == 0) {
        goto fail;
    }
    microtask_snapshot = chidori_snapshot_read_bytes(reader, &microtask_snapshot_len);
    if (!microtask_snapshot || microtask_snapshot_len == 0) {
        goto fail;
    }

    ctx = JS_NewContext(rt);
    if (!ctx) {
        goto fail;
    }
    holder = JS_ReadObject(ctx,
                           roots_snapshot,
                           roots_snapshot_len,
                           JS_READ_OBJ_BYTECODE | JS_READ_OBJ_REFERENCE);
    if (JS_IsException(holder)) {
        goto fail;
    }
    global = JS_GetGlobalObject(ctx);
    if (JS_IsException(global)) {
        goto fail;
    }
    for (i = 0; i < sizeof(chidori_context_snapshot_roots) / sizeof(chidori_context_snapshot_roots[0]); i++) {
        const char *root = chidori_context_snapshot_roots[i];
        JSValue value = JS_GetPropertyStr(ctx, holder, root);
        if (JS_IsException(value)) {
            goto fail;
        }
        if (JS_SetPropertyStr(ctx, global, root, value) < 0) {
            goto fail;
        }
    }
    if (CHIDORI_JS_ReadMicrotaskQueue(ctx, microtask_snapshot, microtask_snapshot_len) < 0) {
        goto fail;
    }

    free(roots_snapshot);
    free(microtask_snapshot);
    JS_FreeValue(ctx, global);
    JS_FreeValue(ctx, holder);
    return ctx;

fail:
    free(roots_snapshot);
    free(microtask_snapshot);
    if (ctx) {
        JS_FreeValue(ctx, global);
        JS_FreeValue(ctx, holder);
        JS_FreeContext(ctx);
    }
    return NULL;
}

JSValue CHIDORI_JS_NewHostPromise(JSContext *ctx, uint64_t host_operation_id) {
    char key[32];
    JSValue resolving_funcs[2] = { JS_UNDEFINED, JS_UNDEFINED };
    JSValue promise = JS_NewPromiseCapability(ctx, resolving_funcs);
    if (JS_IsException(promise) || JS_IsException(resolving_funcs[0]) || JS_IsException(resolving_funcs[1])) {
        JS_FreeValue(ctx, resolving_funcs[0]);
        JS_FreeValue(ctx, resolving_funcs[1]);
        return JS_EXCEPTION;
    }
    if (JS_DefinePropertyValueStr(ctx, promise, "__chidori_host_operation_id",
                                  JS_NewBigUint64(ctx, host_operation_id), 0) < 0) {
        JS_FreeValue(ctx, promise);
        JS_FreeValue(ctx, resolving_funcs[0]);
        JS_FreeValue(ctx, resolving_funcs[1]);
        return JS_EXCEPTION;
    }

    JSValue registry = chidori_host_promise_registry(ctx);
    if (JS_IsException(registry)) {
        JS_FreeValue(ctx, promise);
        JS_FreeValue(ctx, resolving_funcs[0]);
        JS_FreeValue(ctx, resolving_funcs[1]);
        return JS_EXCEPTION;
    }

    JSValue entry = JS_NewObject(ctx);
    if (JS_IsException(entry)) {
        JS_FreeValue(ctx, registry);
        JS_FreeValue(ctx, promise);
        JS_FreeValue(ctx, resolving_funcs[0]);
        JS_FreeValue(ctx, resolving_funcs[1]);
        return JS_EXCEPTION;
    }

    if (JS_SetPropertyStr(ctx, entry, "promise", JS_DupValue(ctx, promise)) < 0 ||
        JS_SetPropertyStr(ctx, entry, "resolve", resolving_funcs[0]) < 0 ||
        JS_SetPropertyStr(ctx, entry, "reject", resolving_funcs[1]) < 0) {
        JS_FreeValue(ctx, registry);
        JS_FreeValue(ctx, entry);
        JS_FreeValue(ctx, promise);
        return JS_EXCEPTION;
    }

    chidori_host_promise_key(host_operation_id, key, sizeof(key));
    if (JS_SetPropertyStr(ctx, registry, key, entry) < 0) {
        JS_FreeValue(ctx, registry);
        JS_FreeValue(ctx, promise);
        return JS_EXCEPTION;
    }

    JS_FreeValue(ctx, registry);
    return promise;
}

int CHIDORI_JS_ResolveHostPromise(JSContext *ctx, uint64_t host_operation_id, JSValue value) {
    JSValue entry = chidori_host_promise_entry(ctx, host_operation_id);
    if (JS_IsException(entry)) {
        return -1;
    }
    if (JS_IsUndefined(entry)) {
        JS_FreeValue(ctx, entry);
        return -1;
    }
    JSValue resolve = JS_GetPropertyStr(ctx, entry, "resolve");
    if (JS_IsException(resolve)) {
        JS_FreeValue(ctx, entry);
        return -1;
    }
    JSValue ret = JS_Call(ctx, resolve, JS_UNDEFINED, 1, &value);
    JS_FreeValue(ctx, resolve);
    JS_FreeValue(ctx, entry);
    if (JS_IsException(ret)) {
        return -1;
    }
    JS_FreeValue(ctx, ret);
    return 0;
}

int CHIDORI_JS_RejectHostPromise(JSContext *ctx, uint64_t host_operation_id, JSValue reason) {
    JSValue entry = chidori_host_promise_entry(ctx, host_operation_id);
    if (JS_IsException(entry)) {
        return -1;
    }
    if (JS_IsUndefined(entry)) {
        JS_FreeValue(ctx, entry);
        return -1;
    }
    JSValue reject = JS_GetPropertyStr(ctx, entry, "reject");
    if (JS_IsException(reject)) {
        JS_FreeValue(ctx, entry);
        return -1;
    }
    JSValue ret = JS_Call(ctx, reject, JS_UNDEFINED, 1, &reason);
    JS_FreeValue(ctx, reject);
    JS_FreeValue(ctx, entry);
    if (JS_IsException(ret)) {
        return -1;
    }
    JS_FreeValue(ctx, ret);
    return 0;
}

int CHIDORI_JS_RunJobsUntilBlocked(JSRuntime *rt, JSContext **ctx) {
    int status;
    while ((status = JS_ExecutePendingJob(rt, ctx)) > 0) {}
    return status;
}
