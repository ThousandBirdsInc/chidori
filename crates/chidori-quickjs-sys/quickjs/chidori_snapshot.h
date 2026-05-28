#ifndef CHIDORI_SNAPSHOT_H
#define CHIDORI_SNAPSHOT_H

#include <stddef.h>
#include <stdint.h>
#include "quickjs.h"

typedef int CHIDORI_JSSnapshotWriteFn(void *opaque, const uint8_t *buf, size_t len);
typedef int CHIDORI_JSSnapshotReadFn(void *opaque, uint8_t *buf, size_t len);
typedef void CHIDORI_JSUnsupportedFn(void *opaque,
                                    const char *path,
                                    const char *type_name,
                                    const char *message);

typedef struct CHIDORI_JSSnapshotWriter {
    void *opaque;
    CHIDORI_JSSnapshotWriteFn *write;
} CHIDORI_JSSnapshotWriter;

typedef struct CHIDORI_JSSnapshotReader {
    void *opaque;
    CHIDORI_JSSnapshotReadFn *read;
} CHIDORI_JSSnapshotReader;

typedef struct CHIDORI_JSUnsupportedHook {
    void *opaque;
    CHIDORI_JSUnsupportedFn *unsupported;
} CHIDORI_JSUnsupportedHook;

int CHIDORI_JS_SnapshotRuntime(JSRuntime *rt, CHIDORI_JSSnapshotWriter *writer);
JSRuntime *CHIDORI_JS_RestoreRuntime(CHIDORI_JSSnapshotReader *reader);

int CHIDORI_JS_SnapshotContext(JSContext *ctx, CHIDORI_JSSnapshotWriter *writer);
JSContext *CHIDORI_JS_RestoreContext(JSRuntime *rt, CHIDORI_JSSnapshotReader *reader);

JSValue CHIDORI_JS_NewHostPromise(JSContext *ctx, uint64_t host_operation_id);
int CHIDORI_JS_ResolveHostPromise(JSContext *ctx, uint64_t host_operation_id, JSValue value);
int CHIDORI_JS_RejectHostPromise(JSContext *ctx, uint64_t host_operation_id, JSValue reason);

int CHIDORI_JS_RunJobsUntilBlocked(JSRuntime *rt, JSContext **ctx);
int CHIDORI_JS_SetSnapshotUnsupportedHook(JSRuntime *rt, CHIDORI_JSUnsupportedHook *hook);
uint8_t *CHIDORI_JS_WriteMicrotaskQueue(JSContext *ctx, size_t *psize);
int CHIDORI_JS_ReadMicrotaskQueue(JSContext *ctx, const uint8_t *buf, size_t buf_len);

#endif
