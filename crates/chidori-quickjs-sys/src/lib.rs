#![allow(non_camel_case_types)]
#![allow(non_snake_case)]

use std::ffi::{c_char, c_int, c_void};
use std::mem::MaybeUninit;

pub type JSRuntime = c_void;
pub type JSContext = c_void;
pub type JSModuleDef = c_void;

pub type JSInterruptHandler =
    Option<unsafe extern "C" fn(rt: *mut JSRuntime, opaque: *mut c_void) -> c_int>;
pub type JSCFunction = Option<
    unsafe extern "C" fn(
        ctx: *mut JSContext,
        this_val: JSValue,
        argc: c_int,
        argv: *mut JSValue,
    ) -> JSValue,
>;
pub type CHIDORI_JSSnapshotWriteFn =
    Option<unsafe extern "C" fn(opaque: *mut c_void, buf: *const u8, len: usize) -> c_int>;
pub type CHIDORI_JSSnapshotReadFn =
    Option<unsafe extern "C" fn(opaque: *mut c_void, buf: *mut u8, len: usize) -> c_int>;
pub type CHIDORI_JSUnsupportedFn = Option<
    unsafe extern "C" fn(
        opaque: *mut c_void,
        path: *const c_char,
        type_name: *const c_char,
        message: *const c_char,
    ),
>;

#[repr(C)]
#[derive(Copy, Clone)]
pub struct CHIDORI_JSSnapshotWriter {
    pub opaque: *mut c_void,
    pub write: CHIDORI_JSSnapshotWriteFn,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct CHIDORI_JSSnapshotReader {
    pub opaque: *mut c_void,
    pub read: CHIDORI_JSSnapshotReadFn,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct CHIDORI_JSUnsupportedHook {
    pub opaque: *mut c_void,
    pub unsupported: CHIDORI_JSUnsupportedFn,
}

#[cfg(target_pointer_width = "64")]
pub type JSValueTag = i64;

#[cfg(not(target_pointer_width = "64"))]
pub type JSValueTag = i32;

#[repr(C)]
#[derive(Copy, Clone)]
pub union JSValueUnion {
    pub int32: i32,
    pub float64: f64,
    pub ptr: *mut c_void,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct JSValue {
    pub u: JSValueUnion,
    pub tag: JSValueTag,
}

extern "C" {
    pub fn js_free(ctx: *mut JSContext, ptr: *mut c_void);
    pub fn JS_NewRuntime() -> *mut JSRuntime;
    pub fn JS_FreeRuntime(rt: *mut JSRuntime);
    pub fn JS_NewContext(rt: *mut JSRuntime) -> *mut JSContext;
    pub fn JS_FreeContext(ctx: *mut JSContext);
    pub fn JS_GetContextOpaque(ctx: *mut JSContext) -> *mut c_void;
    pub fn JS_SetContextOpaque(ctx: *mut JSContext, opaque: *mut c_void);
    pub fn JS_SetMemoryLimit(rt: *mut JSRuntime, limit: usize);
    pub fn JS_SetMaxStackSize(rt: *mut JSRuntime, stack_size: usize);
    pub fn JS_SetInterruptHandler(rt: *mut JSRuntime, cb: JSInterruptHandler, opaque: *mut c_void);
    pub fn JS_FreeValue(ctx: *mut JSContext, value: JSValue);
    pub fn JS_DupValue(ctx: *mut JSContext, value: JSValue) -> JSValue;
    pub fn JS_GetException(ctx: *mut JSContext) -> JSValue;
    pub fn JS_Throw(ctx: *mut JSContext, obj: JSValue) -> JSValue;
    pub fn JS_NewError(ctx: *mut JSContext) -> JSValue;
    pub fn JS_ToCStringLen2(
        ctx: *mut JSContext,
        plen: *mut usize,
        value: JSValue,
        cesu8: bool,
    ) -> *const c_char;
    pub fn JS_FreeCString(ctx: *mut JSContext, ptr: *const c_char);
    pub fn JS_NewStringLen(ctx: *mut JSContext, str1: *const c_char, len1: usize) -> JSValue;
    pub fn JS_NewObject(ctx: *mut JSContext) -> JSValue;
    pub fn JS_GetGlobalObject(ctx: *mut JSContext) -> JSValue;
    pub fn JS_GetPropertyStr(
        ctx: *mut JSContext,
        this_obj: JSValue,
        prop: *const c_char,
    ) -> JSValue;
    pub fn JS_SetPropertyStr(
        ctx: *mut JSContext,
        this_obj: JSValue,
        prop: *const c_char,
        val: JSValue,
    ) -> c_int;
    pub fn JS_ParseJSON(
        ctx: *mut JSContext,
        buf: *const c_char,
        buf_len: usize,
        filename: *const c_char,
    ) -> JSValue;
    pub fn JS_JSONStringify(
        ctx: *mut JSContext,
        obj: JSValue,
        replacer: JSValue,
        space0: JSValue,
    ) -> JSValue;
    pub fn JS_WriteObject(
        ctx: *mut JSContext,
        psize: *mut usize,
        obj: JSValue,
        flags: c_int,
    ) -> *mut u8;
    pub fn JS_ReadObject(
        ctx: *mut JSContext,
        buf: *const u8,
        buf_len: usize,
        flags: c_int,
    ) -> JSValue;
    pub fn JS_Call(
        ctx: *mut JSContext,
        func_obj: JSValue,
        this_obj: JSValue,
        argc: c_int,
        argv: *mut JSValue,
    ) -> JSValue;
    pub fn JS_NewCFunction2(
        ctx: *mut JSContext,
        func: JSCFunction,
        name: *const c_char,
        length: c_int,
        cproto: c_int,
        magic: c_int,
    ) -> JSValue;
    pub fn JS_NewPromiseCapability(ctx: *mut JSContext, resolving_funcs: *mut JSValue) -> JSValue;
    pub fn JS_PromiseState(ctx: *mut JSContext, promise: JSValue) -> c_int;
    pub fn JS_PromiseResult(ctx: *mut JSContext, promise: JSValue) -> JSValue;
    pub fn JS_Eval(
        ctx: *mut JSContext,
        input: *const c_char,
        input_len: usize,
        filename: *const c_char,
        eval_flags: c_int,
    ) -> JSValue;
    pub fn JS_EvalFunction(ctx: *mut JSContext, fun_obj: JSValue) -> JSValue;
    pub fn JS_ResolveModule(ctx: *mut JSContext, obj: JSValue) -> c_int;
    pub fn JS_GetModuleNamespace(ctx: *mut JSContext, m: *mut JSModuleDef) -> JSValue;
    pub fn JS_ExecutePendingJob(rt: *mut JSRuntime, pctx: *mut *mut JSContext) -> c_int;

    pub fn CHIDORI_JS_SnapshotRuntime(
        rt: *mut JSRuntime,
        writer: *mut CHIDORI_JSSnapshotWriter,
    ) -> c_int;

    pub fn CHIDORI_JS_RestoreRuntime(reader: *mut CHIDORI_JSSnapshotReader) -> *mut JSRuntime;

    pub fn CHIDORI_JS_SnapshotContext(
        ctx: *mut JSContext,
        writer: *mut CHIDORI_JSSnapshotWriter,
    ) -> c_int;

    pub fn CHIDORI_JS_RestoreContext(
        rt: *mut JSRuntime,
        reader: *mut CHIDORI_JSSnapshotReader,
    ) -> *mut JSContext;

    pub fn CHIDORI_JS_NewHostPromise(ctx: *mut JSContext, host_operation_id: u64) -> JSValue;

    pub fn CHIDORI_JS_ResolveHostPromise(
        ctx: *mut JSContext,
        host_operation_id: u64,
        value: JSValue,
    ) -> c_int;

    pub fn CHIDORI_JS_RejectHostPromise(
        ctx: *mut JSContext,
        host_operation_id: u64,
        reason: JSValue,
    ) -> c_int;

    pub fn CHIDORI_JS_RunJobsUntilBlocked(rt: *mut JSRuntime, ctx: *mut *mut JSContext) -> c_int;

    pub fn CHIDORI_JS_WriteMicrotaskQueue(ctx: *mut JSContext, psize: *mut usize) -> *mut u8;

    pub fn CHIDORI_JS_ReadMicrotaskQueue(
        ctx: *mut JSContext,
        buf: *const u8,
        buf_len: usize,
    ) -> c_int;

    pub fn CHIDORI_JS_SetSnapshotUnsupportedHook(
        rt: *mut JSRuntime,
        hook: *mut CHIDORI_JSUnsupportedHook,
    ) -> c_int;
}

impl JSValue {
    pub fn zeroed() -> Self {
        unsafe { MaybeUninit::<Self>::zeroed().assume_init() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::{CStr, CString};

    const JS_PROMISE_FULFILLED: i32 = 1;
    const JS_PROMISE_REJECTED: i32 = 2;

    unsafe extern "C" fn test_snapshot_capture_write(
        opaque: *mut std::ffi::c_void,
        buf: *const u8,
        len: usize,
    ) -> c_int {
        if opaque.is_null() || (buf.is_null() && len > 0) {
            return -1;
        }
        let written = &mut *opaque.cast::<Vec<u8>>();
        written.extend_from_slice(std::slice::from_raw_parts(buf, len));
        0
    }

    unsafe extern "C" fn test_snapshot_read(
        opaque: *mut std::ffi::c_void,
        buf: *mut u8,
        len: usize,
    ) -> c_int {
        if opaque.is_null() || (buf.is_null() && len > 0) {
            return -1;
        }
        let bytes = &mut *opaque.cast::<&[u8]>();
        if len > bytes.len() {
            return -1;
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), buf, len);
        *bytes = &bytes[len..];
        0
    }

    const JS_TAG_UNDEFINED: JSValueTag = 3;

    fn js_undefined() -> JSValue {
        JSValue {
            u: JSValueUnion { int32: 0 },
            tag: JS_TAG_UNDEFINED,
        }
    }

    #[test]
    fn js_value_layout_matches_quickjs_default_for_target() {
        #[cfg(target_pointer_width = "64")]
        {
            assert_eq!(std::mem::size_of::<JSValue>(), 16);
            assert_eq!(std::mem::align_of::<JSValue>(), 8);
            assert_eq!(std::mem::size_of::<JSValueTag>(), 8);
        }

        #[cfg(not(target_pointer_width = "64"))]
        {
            assert_eq!(std::mem::size_of::<JSValueTag>(), 4);
        }
    }

    #[test]
    fn snapshot_reader_writer_layout_is_two_pointers() {
        let pointer_size = std::mem::size_of::<*mut std::ffi::c_void>();
        assert_eq!(
            std::mem::size_of::<CHIDORI_JSSnapshotWriter>(),
            pointer_size * 2
        );
        assert_eq!(
            std::mem::size_of::<CHIDORI_JSSnapshotReader>(),
            pointer_size * 2
        );
        assert_eq!(
            std::mem::align_of::<CHIDORI_JSSnapshotWriter>(),
            std::mem::align_of::<*mut std::ffi::c_void>()
        );
        assert_eq!(
            std::mem::align_of::<CHIDORI_JSSnapshotReader>(),
            std::mem::align_of::<*mut std::ffi::c_void>()
        );
    }

    #[test]
    fn unsupported_hook_layout_is_two_pointers() {
        let pointer_size = std::mem::size_of::<*mut std::ffi::c_void>();
        assert_eq!(
            std::mem::size_of::<CHIDORI_JSUnsupportedHook>(),
            pointer_size * 2
        );
        assert_eq!(
            std::mem::align_of::<CHIDORI_JSUnsupportedHook>(),
            std::mem::align_of::<*mut std::ffi::c_void>()
        );
    }

    #[test]
    fn raw_snapshot_restore_api_accepts_reader_writer_structs() {
        unsafe {
            let rt = JS_NewRuntime();
            assert!(!rt.is_null());

            let mut written = Vec::<u8>::new();
            let mut writer = CHIDORI_JSSnapshotWriter {
                opaque: (&mut written as *mut Vec<u8>).cast::<std::ffi::c_void>(),
                write: Some(test_snapshot_capture_write),
            };
            assert_eq!(CHIDORI_JS_SnapshotRuntime(rt, &mut writer), 0);
            assert_eq!(written, b"CHIDORI_QJS_RUNTIME_SNAPSHOT_V1");

            let mut bytes: &[u8] = &written;
            let mut reader = CHIDORI_JSSnapshotReader {
                opaque: (&mut bytes as *mut &[u8]).cast::<std::ffi::c_void>(),
                read: Some(test_snapshot_read),
            };
            let restored = CHIDORI_JS_RestoreRuntime(&mut reader);
            assert!(!restored.is_null());
            assert!(bytes.is_empty());

            JS_FreeRuntime(restored);
            JS_FreeRuntime(rt);
        }
    }

    #[test]
    fn raw_snapshot_api_rejects_invalid_runtime_payload() {
        unsafe {
            let rt = JS_NewRuntime();
            assert!(!rt.is_null());

            let mut bytes: &[u8] = b"CHIDORI_QJS_STUB_SNAPSHOT_V1";
            let mut reader = CHIDORI_JSSnapshotReader {
                opaque: (&mut bytes as *mut &[u8]).cast::<std::ffi::c_void>(),
                read: Some(test_snapshot_read),
            };
            assert!(CHIDORI_JS_RestoreRuntime(&mut reader).is_null());
            JS_FreeRuntime(rt);
        }
    }

    #[test]
    fn raw_context_snapshot_restore_api_accepts_reader_writer_structs() {
        unsafe {
            let rt = JS_NewRuntime();
            assert!(!rt.is_null());
            let ctx = JS_NewContext(rt);
            assert!(!ctx.is_null());

            let source =
                CString::new("globalThis.__chidori_call_result = { answer: 42 };").unwrap();
            let filename = CString::new("context-snapshot-test.js").unwrap();
            let eval_result = JS_Eval(
                ctx,
                source.as_ptr(),
                source.as_bytes().len(),
                filename.as_ptr(),
                0,
            );
            JS_FreeValue(ctx, eval_result);

            let mut written = Vec::<u8>::new();
            let mut writer = CHIDORI_JSSnapshotWriter {
                opaque: (&mut written as *mut Vec<u8>).cast::<std::ffi::c_void>(),
                write: Some(test_snapshot_capture_write),
            };
            assert_eq!(CHIDORI_JS_SnapshotContext(ctx, &mut writer), 0);
            assert!(written.starts_with(b"CHIDORI_QJS_CONTEXT_SNAPSHOT_V1"));

            let mut bytes: &[u8] = &written;
            let mut reader = CHIDORI_JSSnapshotReader {
                opaque: (&mut bytes as *mut &[u8]).cast::<std::ffi::c_void>(),
                read: Some(test_snapshot_read),
            };
            let restored = CHIDORI_JS_RestoreContext(rt, &mut reader);
            assert!(!restored.is_null());
            assert!(bytes.is_empty());

            let global = JS_GetGlobalObject(restored);
            let prop = CString::new("__chidori_call_result").unwrap();
            let value = JS_GetPropertyStr(restored, global, prop.as_ptr());
            let json = JS_JSONStringify(restored, value, js_undefined(), js_undefined());
            let cstr = JS_ToCStringLen2(restored, std::ptr::null_mut(), json, false);
            assert_eq!(CStr::from_ptr(cstr).to_string_lossy(), "{\"answer\":42}");
            JS_FreeCString(restored, cstr);
            JS_FreeValue(restored, json);
            JS_FreeValue(restored, value);
            JS_FreeValue(restored, global);
            JS_FreeContext(restored);
            JS_FreeContext(ctx);
            JS_FreeRuntime(rt);
        }
    }

    #[test]
    fn raw_context_snapshot_api_rejects_invalid_payload() {
        unsafe {
            let rt = JS_NewRuntime();
            assert!(!rt.is_null());

            let mut bytes: &[u8] = b"CHIDORI_QJS_STUB_SNAPSHOT_V1";
            let mut reader = CHIDORI_JSSnapshotReader {
                opaque: (&mut bytes as *mut &[u8]).cast::<std::ffi::c_void>(),
                read: Some(test_snapshot_read),
            };
            assert!(CHIDORI_JS_RestoreContext(rt, &mut reader).is_null());
            JS_FreeRuntime(rt);
        }
    }

    #[test]
    fn raw_host_promise_api_resolves_by_id() {
        unsafe {
            let rt = JS_NewRuntime();
            assert!(!rt.is_null());
            let ctx = JS_NewContext(rt);
            assert!(!ctx.is_null());

            let promise = CHIDORI_JS_NewHostPromise(ctx, 42);
            let value = parse_json(ctx, r#"{"ok":true}"#);
            assert_eq!(CHIDORI_JS_ResolveHostPromise(ctx, 42, value), 0);
            JS_FreeValue(ctx, value);
            let mut job_ctx = std::ptr::null_mut();
            assert_eq!(CHIDORI_JS_RunJobsUntilBlocked(rt, &mut job_ctx), 0);
            assert_eq!(JS_PromiseState(ctx, promise), JS_PROMISE_FULFILLED);

            JS_FreeValue(ctx, promise);
            JS_FreeContext(ctx);
            JS_FreeRuntime(rt);
        }
    }

    #[test]
    fn raw_host_promise_api_rejects_by_id() {
        unsafe {
            let rt = JS_NewRuntime();
            assert!(!rt.is_null());
            let ctx = JS_NewContext(rt);
            assert!(!ctx.is_null());

            let promise = CHIDORI_JS_NewHostPromise(ctx, 43);
            let reason = parse_json(ctx, r#""failed""#);
            assert_eq!(CHIDORI_JS_RejectHostPromise(ctx, 43, reason), 0);
            JS_FreeValue(ctx, reason);
            let mut job_ctx = std::ptr::null_mut();
            assert_eq!(CHIDORI_JS_RunJobsUntilBlocked(rt, &mut job_ctx), 0);
            assert_eq!(JS_PromiseState(ctx, promise), JS_PROMISE_REJECTED);

            JS_FreeValue(ctx, promise);
            JS_FreeContext(ctx);
            JS_FreeRuntime(rt);
        }
    }

    unsafe fn parse_json(ctx: *mut JSContext, source: &str) -> JSValue {
        let source = CString::new(source).unwrap();
        let filename = CString::new("<test>").unwrap();
        JS_ParseJSON(
            ctx,
            source.as_ptr(),
            source.as_bytes().len(),
            filename.as_ptr(),
        )
    }
}
