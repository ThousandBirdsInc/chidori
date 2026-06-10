use std::collections::HashMap;
use std::ffi::{c_void, CStr, CString};
use std::marker::PhantomData;
use std::ptr::NonNull;

use serde_json::Value;
use thiserror::Error;

pub use chidori_quickjs_sys as sys;

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub struct HostPromiseId(pub u64);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeSnapshot(pub Vec<u8>);

impl RuntimeSnapshot {
    pub fn from_payload(payload: &[u8]) -> Self {
        Self::from_parts(payload, payload)
    }

    pub fn from_parts(runtime_payload: &[u8], context_payload: &[u8]) -> Self {
        Self(encode_runtime_snapshot_payload(
            runtime_payload,
            context_payload,
        ))
    }

    pub fn payload(&self) -> Result<&[u8]> {
        Ok(decode_runtime_snapshot_payload(&self.0)?.runtime_payload)
    }

    pub fn context_payload(&self) -> Result<&[u8]> {
        Ok(decode_runtime_snapshot_payload(&self.0)?.context_payload)
    }

    pub fn ensure_restorable(&self) -> Result<()> {
        let payloads = decode_runtime_snapshot_payload(&self.0)?;
        validate_runtime_snapshot_payloads(&payloads)
    }
}

#[derive(Copy, Clone)]
pub struct JsValue(sys::JSValue);

impl JsValue {
    pub fn raw(self) -> sys::JSValue {
        self.0
    }
}

/// Returns the typed state pointer previously installed with
/// `SnapshotContext::set_context_opaque`.
///
/// # Safety
///
/// The caller must ensure the context opaque pointer is either null or points
/// to a valid `T` for the duration of the native callback.
pub unsafe fn context_opaque_mut<T>(ctx: *mut sys::JSContext) -> Option<&'static mut T> {
    let opaque = sys::JS_GetContextOpaque(ctx);
    if opaque.is_null() {
        None
    } else {
        Some(&mut *opaque.cast::<T>())
    }
}

/// Converts a JavaScript callback argument to a Rust string using QuickJS'
/// normal string coercion.
///
/// # Safety
///
/// `argv` must point to the `argc` values supplied to a QuickJS native callback.
pub unsafe fn callback_arg_to_string(
    ctx: *mut sys::JSContext,
    argc: i32,
    argv: *mut sys::JSValue,
    index: usize,
) -> Result<String> {
    let Some(value) = callback_arg(argc, argv, index) else {
        return Err(QuickJsError::EvalFailed(format!(
            "missing callback argument {index}"
        )));
    };
    js_value_to_string(ctx, value)
}

/// Converts a JavaScript callback argument to a JSON-compatible Rust value.
///
/// # Safety
///
/// `argv` must point to the `argc` values supplied to a QuickJS native callback.
pub unsafe fn callback_arg_to_json(
    ctx: *mut sys::JSContext,
    argc: i32,
    argv: *mut sys::JSValue,
    index: usize,
) -> Result<Value> {
    let Some(value) = callback_arg(argc, argv, index) else {
        return Err(QuickJsError::EvalFailed(format!(
            "missing callback argument {index}"
        )));
    };
    js_borrowed_value_to_json(ctx, value)
}

/// Creates a JavaScript value from JSON for native callback return values.
///
/// # Safety
///
/// `ctx` must be a valid QuickJS context.
pub unsafe fn json_to_js_value(ctx: *mut sys::JSContext, value: Value) -> Result<sys::JSValue> {
    json_to_js(ctx, value)
}

/// Throws a JavaScript `Error` value. Native callbacks can return this directly.
///
/// # Safety
///
/// `ctx` must be a valid QuickJS context.
pub unsafe fn throw_string(ctx: *mut sys::JSContext, message: &str) -> sys::JSValue {
    let error = unsafe { sys::JS_NewError(ctx) };
    if js_value_is_exception(error) {
        return js_undefined();
    }
    let Ok(message_value) = string_to_js(ctx, message) else {
        unsafe {
            sys::JS_FreeValue(ctx, error);
        }
        return js_undefined();
    };
    let property = CString::new("message").expect("static property name has no interior NUL");
    if unsafe { sys::JS_SetPropertyStr(ctx, error, property.as_ptr(), message_value) } < 0 {
        unsafe {
            sys::JS_FreeValue(ctx, error);
        }
        return js_undefined();
    }
    unsafe { sys::JS_Throw(ctx, error) }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeLimits {
    pub memory_limit_bytes: usize,
    pub interrupt_budget: u64,
}

impl Default for RuntimeLimits {
    fn default() -> Self {
        Self {
            memory_limit_bytes: 128 * 1024 * 1024,
            interrupt_budget: 50_000_000,
        }
    }
}

impl RuntimeLimits {
    pub const MEMORY_LIMIT_ENV: &'static str = "CHIDORI_QJS_MEMORY_LIMIT_BYTES";
    pub const INTERRUPT_BUDGET_ENV: &'static str = "CHIDORI_QJS_INTERRUPT_BUDGET";

    pub fn from_env() -> Result<Self> {
        Self::from_env_values(
            std::env::var(Self::MEMORY_LIMIT_ENV).ok().as_deref(),
            std::env::var(Self::INTERRUPT_BUDGET_ENV).ok().as_deref(),
        )
    }

    pub fn from_env_values(
        memory_limit_bytes: Option<&str>,
        interrupt_budget: Option<&str>,
    ) -> Result<Self> {
        let default = Self::default();
        let memory_limit_bytes = match memory_limit_bytes {
            Some(raw) => raw.parse::<usize>().map_err(|_| {
                QuickJsError::InvalidRuntimeLimits(format!(
                    "{} must be a positive integer byte count",
                    Self::MEMORY_LIMIT_ENV
                ))
            })?,
            None => default.memory_limit_bytes,
        };
        let interrupt_budget = match interrupt_budget {
            Some(raw) => raw.parse::<u64>().map_err(|_| {
                QuickJsError::InvalidRuntimeLimits(format!(
                    "{} must be a positive integer instruction budget",
                    Self::INTERRUPT_BUDGET_ENV
                ))
            })?,
            None => default.interrupt_budget,
        };
        let limits = Self {
            memory_limit_bytes,
            interrupt_budget,
        };
        limits.validate()?;
        Ok(limits)
    }

    pub fn validate(&self) -> Result<()> {
        if self.memory_limit_bytes == 0 {
            return Err(QuickJsError::InvalidRuntimeLimits(
                "memory_limit_bytes must be greater than zero".to_string(),
            ));
        }
        if self.interrupt_budget == 0 {
            return Err(QuickJsError::InvalidRuntimeLimits(
                "interrupt_budget must be greater than zero".to_string(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum RunState {
    Completed(Value),
    BlockedOnHostOperation(HostPromiseId),
}

#[derive(Debug, Clone, PartialEq)]
pub enum PromiseState {
    Pending,
    Fulfilled(Value),
    Rejected(Value),
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum QuickJsError {
    #[error(
        "durable TypeScript VM snapshots require Chidori's patched QuickJS fork; the current scaffold exposes the ABI but does not implement VM serialization"
    )]
    SnapshotUnsupported,
    #[error("unsupported QuickJS snapshot value at {path} ({type_name}): {message}")]
    SnapshotUnsupportedDetail {
        path: String,
        type_name: String,
        message: String,
    },
    #[error("invalid QuickJS runtime limits: {0}")]
    InvalidRuntimeLimits(String),
    #[error("failed to allocate QuickJS runtime")]
    RuntimeAllocationFailed,
    #[error("failed to allocate QuickJS context")]
    ContextAllocationFailed,
    #[error("restored QuickJS context is unavailable for runtime-level host promise operation")]
    RestoredContextUnavailable,
    #[error("JavaScript source contains an interior NUL byte")]
    InteriorNul,
    #[error("QuickJS evaluation failed: {0}")]
    EvalFailed(String),
    #[error("invalid QuickJS context snapshot: {0}")]
    InvalidSnapshot(String),
}

pub type Result<T> = std::result::Result<T, QuickJsError>;

const CONTEXT_SNAPSHOT_MAGIC: &[u8; 8] = b"CHQJSC01";
const RUNTIME_SNAPSHOT_MAGIC: &[u8; 8] = b"CHQJSR01";

/// How a source string fed to [`SnapshotContext::eval_for_conformance`] should
/// be parsed and (optionally) executed. Mirrors the Test262 `flags` metadata:
/// scripts can be sloppy or strict, modules are always strict, and the
/// `Compile*` variants parse without running for `negative: { phase: parse }`
/// tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvalMode {
    /// Global script in sloppy mode.
    Script,
    /// Global script forced into strict mode (as if `"use strict";` led it).
    StrictScript,
    /// ECMAScript module (implicitly strict).
    Module,
    /// Parse a sloppy global script without executing it.
    CompileScript,
    /// Parse a strict global script without executing it.
    CompileStrictScript,
    /// Parse a module without executing it.
    CompileModule,
}

impl EvalMode {
    fn eval_flags(self) -> i32 {
        match self {
            EvalMode::Script => JS_EVAL_TYPE_GLOBAL,
            EvalMode::StrictScript => JS_EVAL_TYPE_GLOBAL | JS_EVAL_FLAG_STRICT,
            EvalMode::Module => JS_EVAL_TYPE_MODULE,
            EvalMode::CompileScript => JS_EVAL_TYPE_GLOBAL | JS_EVAL_FLAG_COMPILE_ONLY,
            EvalMode::CompileStrictScript => {
                JS_EVAL_TYPE_GLOBAL | JS_EVAL_FLAG_STRICT | JS_EVAL_FLAG_COMPILE_ONLY
            }
            EvalMode::CompileModule => JS_EVAL_TYPE_MODULE | JS_EVAL_FLAG_COMPILE_ONLY,
        }
    }
}

/// A value thrown by JavaScript, captured for conformance assertions. `name` is
/// the thrown value's `name` property (or constructor name) — e.g.
/// `"SyntaxError"`, `"TypeError"`, or `"Test262Error"` — which is exactly what
/// Test262 negative tests match against.
#[derive(Debug, Clone)]
pub struct JsThrow {
    pub name: String,
    pub message: String,
    pub to_string: String,
}

impl JsThrow {
    fn host(message: &str) -> Self {
        JsThrow {
            name: "HostError".to_string(),
            message: message.to_string(),
            to_string: message.to_string(),
        }
    }
}

impl std::fmt::Display for JsThrow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.to_string)
    }
}

#[derive(Debug)]
pub struct SnapshotRuntime {
    rt: NonNull<sys::JSRuntime>,
    restored_context: Option<NonNull<sys::JSContext>>,
    _interrupt_budget: Box<u64>,
    limits: RuntimeLimits,
}

impl SnapshotRuntime {
    pub fn new(limits: RuntimeLimits) -> Result<Self> {
        limits.validate()?;
        let rt = unsafe { sys::JS_NewRuntime() };
        let Some(rt) = NonNull::new(rt) else {
            return Err(QuickJsError::RuntimeAllocationFailed);
        };

        let mut interrupt_budget = Box::new(limits.interrupt_budget);
        unsafe {
            sys::JS_SetMemoryLimit(rt.as_ptr(), limits.memory_limit_bytes);
            sys::JS_SetInterruptHandler(
                rt.as_ptr(),
                Some(interrupt_budget_handler),
                (&mut *interrupt_budget as *mut u64).cast::<c_void>(),
            );
        }
        Ok(Self {
            rt,
            restored_context: None,
            _interrupt_budget: interrupt_budget,
            limits,
        })
    }

    pub fn raw_runtime(&self) -> *mut sys::JSRuntime {
        self.rt.as_ptr()
    }

    pub fn limits(&self) -> &RuntimeLimits {
        &self.limits
    }

    pub fn new_context(&self) -> Result<SnapshotContext<'_>> {
        let ctx = unsafe { sys::JS_NewContext(self.rt.as_ptr()) };
        let Some(ctx) = NonNull::new(ctx) else {
            return Err(QuickJsError::ContextAllocationFailed);
        };
        Ok(SnapshotContext {
            rt: self.rt,
            ctx,
            host_promises: HashMap::new(),
            _runtime: PhantomData,
        })
    }

    pub fn restore(snapshot: &[u8]) -> Result<Self> {
        let payloads = decode_runtime_snapshot_payload(snapshot)?;
        validate_runtime_snapshot_payloads(&payloads)?;
        let mut reader_state = SnapshotReaderState::new(payloads.runtime_payload);
        let mut reader = snapshot_reader(&mut reader_state);
        let rt = unsafe { sys::CHIDORI_JS_RestoreRuntime(&mut reader) };
        let Some(rt) = NonNull::new(rt) else {
            return Err(QuickJsError::SnapshotUnsupported);
        };
        let mut context_reader_state = SnapshotReaderState::new(payloads.context_payload);
        let mut context_reader = snapshot_reader(&mut context_reader_state);
        let restored_context =
            unsafe { sys::CHIDORI_JS_RestoreContext(rt.as_ptr(), &mut context_reader) };
        let restored_context = match NonNull::new(restored_context) {
            Some(ctx) => ctx,
            None => {
                unsafe {
                    sys::JS_FreeRuntime(rt.as_ptr());
                }
                return Err(QuickJsError::SnapshotUnsupported);
            }
        };
        let limits = RuntimeLimits::default();
        let mut interrupt_budget = Box::new(limits.interrupt_budget);
        unsafe {
            sys::JS_SetMemoryLimit(rt.as_ptr(), limits.memory_limit_bytes);
            sys::JS_SetInterruptHandler(
                rt.as_ptr(),
                Some(interrupt_budget_handler),
                (&mut *interrupt_budget as *mut u64).cast::<c_void>(),
            );
        }
        Ok(Self {
            rt,
            restored_context: Some(restored_context),
            _interrupt_budget: interrupt_budget,
            limits,
        })
    }

    pub fn snapshot(&mut self) -> Result<RuntimeSnapshot> {
        let runtime_payload = snapshot_runtime_payload(self.rt)?;
        let context_payload = self.snapshot_restored_context_payload()?;
        Ok(RuntimeSnapshot::from_parts(
            &runtime_payload,
            &context_payload,
        ))
    }

    fn snapshot_restored_context_payload(&mut self) -> Result<Vec<u8>> {
        let Some(ctx) = self.restored_context else {
            return Ok(Vec::new());
        };
        snapshot_context_payload(self.rt, ctx)
    }

    pub fn restore_context(&self, snapshot: &[u8]) -> Result<SnapshotContext<'_>> {
        let mut reader_state = SnapshotReaderState::new(snapshot);
        let mut reader = snapshot_reader(&mut reader_state);
        let ctx = unsafe { sys::CHIDORI_JS_RestoreContext(self.rt.as_ptr(), &mut reader) };
        let Some(ctx) = NonNull::new(ctx) else {
            return Err(QuickJsError::SnapshotUnsupported);
        };
        Ok(SnapshotContext {
            rt: self.rt,
            ctx,
            host_promises: HashMap::new(),
            _runtime: PhantomData,
        })
    }

    pub fn run_jobs_until_blocked(&mut self) -> Result<RunState> {
        drain_jobs_for_runtime(self.rt.as_ptr())?;
        let Some(ctx) = self.restored_context else {
            return Ok(RunState::Completed(Value::Null));
        };
        if let Some(value) = global_json(ctx.as_ptr(), "__chidori_active_host_operation_id")? {
            return match value.as_u64() {
                Some(id) => Ok(RunState::BlockedOnHostOperation(HostPromiseId(id))),
                None => Err(QuickJsError::EvalFailed(
                    "__chidori_active_host_operation_id is not a number".to_string(),
                )),
            };
        }
        if let Some(error) = global_json(ctx.as_ptr(), "__chidori_call_error")? {
            if !error.is_null() {
                let message = error.as_str().unwrap_or("unknown JavaScript rejection");
                return Err(QuickJsError::EvalFailed(message.to_string()));
            }
        }
        Ok(RunState::Completed(
            global_json(ctx.as_ptr(), "__chidori_call_result")?.unwrap_or(Value::Null),
        ))
    }

    pub fn resolve_host_promise(&mut self, id: HostPromiseId, value: Value) -> Result<()> {
        let Some(ctx) = self.restored_context else {
            return Err(QuickJsError::RestoredContextUnavailable);
        };
        let value = json_to_js(ctx.as_ptr(), value)?;
        let result = unsafe { sys::CHIDORI_JS_ResolveHostPromise(ctx.as_ptr(), id.0, value) };
        unsafe {
            sys::JS_FreeValue(ctx.as_ptr(), value);
        }
        if result < 0 {
            return Err(QuickJsError::EvalFailed(exception_string(ctx.as_ptr())));
        }
        drain_jobs_for_runtime(self.rt.as_ptr())
    }

    pub fn reject_host_promise(&mut self, id: HostPromiseId, error: String) -> Result<()> {
        let Some(ctx) = self.restored_context else {
            return Err(QuickJsError::RestoredContextUnavailable);
        };
        let value = string_to_js(ctx.as_ptr(), &error)?;
        let result = unsafe { sys::CHIDORI_JS_RejectHostPromise(ctx.as_ptr(), id.0, value) };
        unsafe {
            sys::JS_FreeValue(ctx.as_ptr(), value);
        }
        if result < 0 {
            return Err(QuickJsError::EvalFailed(exception_string(ctx.as_ptr())));
        }
        drain_jobs_for_runtime(self.rt.as_ptr())
    }

    pub fn restored_global_json(&mut self, prop: &str) -> Result<Option<Value>> {
        let Some(ctx) = self.restored_context else {
            return Err(QuickJsError::RestoredContextUnavailable);
        };
        global_json(ctx.as_ptr(), prop)
    }
}

fn snapshot_context_payload(
    rt: NonNull<sys::JSRuntime>,
    ctx: NonNull<sys::JSContext>,
) -> Result<Vec<u8>> {
    let mut writer_state = SnapshotWriterState::default();
    let mut writer = snapshot_writer(&mut writer_state);
    let mut unsupported_reports = Vec::new();
    let mut unsupported_hook = snapshot_unsupported_hook(&mut unsupported_reports);
    unsafe {
        sys::CHIDORI_JS_SetSnapshotUnsupportedHook(rt.as_ptr(), &mut unsupported_hook);
    }
    let status = unsafe { sys::CHIDORI_JS_SnapshotContext(ctx.as_ptr(), &mut writer) };
    unsafe {
        sys::CHIDORI_JS_SetSnapshotUnsupportedHook(rt.as_ptr(), std::ptr::null_mut());
    }
    if status < 0 {
        return Err(snapshot_unsupported_error(unsupported_reports));
    }
    Ok(writer_state.bytes)
}

fn snapshot_runtime_payload(rt: NonNull<sys::JSRuntime>) -> Result<Vec<u8>> {
    let mut writer_state = SnapshotWriterState::default();
    let mut writer = snapshot_writer(&mut writer_state);
    let mut unsupported_reports = Vec::new();
    let mut unsupported_hook = snapshot_unsupported_hook(&mut unsupported_reports);
    unsafe {
        sys::CHIDORI_JS_SetSnapshotUnsupportedHook(rt.as_ptr(), &mut unsupported_hook);
    }
    let status = unsafe { sys::CHIDORI_JS_SnapshotRuntime(rt.as_ptr(), &mut writer) };
    unsafe {
        sys::CHIDORI_JS_SetSnapshotUnsupportedHook(rt.as_ptr(), std::ptr::null_mut());
    }
    if status < 0 {
        return Err(snapshot_unsupported_error(unsupported_reports));
    }
    Ok(writer_state.bytes)
}

#[derive(Default)]
struct SnapshotWriterState {
    bytes: Vec<u8>,
}

fn snapshot_writer(state: &mut SnapshotWriterState) -> sys::CHIDORI_JSSnapshotWriter {
    sys::CHIDORI_JSSnapshotWriter {
        opaque: (state as *mut SnapshotWriterState).cast::<c_void>(),
        write: Some(snapshot_writer_write),
    }
}

unsafe extern "C" fn snapshot_writer_write(opaque: *mut c_void, buf: *const u8, len: usize) -> i32 {
    if opaque.is_null() || (buf.is_null() && len > 0) {
        return -1;
    }
    let state = &mut *opaque.cast::<SnapshotWriterState>();
    let bytes = std::slice::from_raw_parts(buf, len);
    state.bytes.extend_from_slice(bytes);
    0
}

struct SnapshotReaderState {
    bytes: *const u8,
    len: usize,
    offset: usize,
}

#[derive(Debug)]
struct SnapshotUnsupportedReport {
    path: String,
    type_name: String,
    message: String,
}

fn snapshot_unsupported_hook(
    reports: &mut Vec<SnapshotUnsupportedReport>,
) -> sys::CHIDORI_JSUnsupportedHook {
    sys::CHIDORI_JSUnsupportedHook {
        opaque: (reports as *mut Vec<SnapshotUnsupportedReport>).cast::<c_void>(),
        unsupported: Some(snapshot_unsupported_report),
    }
}

unsafe extern "C" fn snapshot_unsupported_report(
    opaque: *mut c_void,
    path: *const std::ffi::c_char,
    type_name: *const std::ffi::c_char,
    message: *const std::ffi::c_char,
) {
    if opaque.is_null() {
        return;
    }
    let reports = &mut *opaque.cast::<Vec<SnapshotUnsupportedReport>>();
    reports.push(SnapshotUnsupportedReport {
        path: c_string_or_empty(path),
        type_name: c_string_or_empty(type_name),
        message: c_string_or_empty(message),
    });
}

fn c_string_or_empty(ptr: *const std::ffi::c_char) -> String {
    if ptr.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(ptr).to_string_lossy().into_owned() }
    }
}

fn snapshot_unsupported_error(mut reports: Vec<SnapshotUnsupportedReport>) -> QuickJsError {
    reports
        .pop()
        .map(|report| QuickJsError::SnapshotUnsupportedDetail {
            path: report.path,
            type_name: report.type_name,
            message: report.message,
        })
        .unwrap_or(QuickJsError::SnapshotUnsupported)
}

impl SnapshotReaderState {
    fn new(snapshot: &[u8]) -> Self {
        Self {
            bytes: snapshot.as_ptr(),
            len: snapshot.len(),
            offset: 0,
        }
    }
}

fn snapshot_reader(state: &mut SnapshotReaderState) -> sys::CHIDORI_JSSnapshotReader {
    sys::CHIDORI_JSSnapshotReader {
        opaque: (state as *mut SnapshotReaderState).cast::<c_void>(),
        read: Some(snapshot_reader_read),
    }
}

unsafe extern "C" fn snapshot_reader_read(opaque: *mut c_void, buf: *mut u8, len: usize) -> i32 {
    if opaque.is_null() || (buf.is_null() && len > 0) {
        return -1;
    }
    let state = &mut *opaque.cast::<SnapshotReaderState>();
    let Some(end) = state.offset.checked_add(len) else {
        return -1;
    };
    if end > state.len {
        return -1;
    }
    std::ptr::copy_nonoverlapping(state.bytes.add(state.offset), buf, len);
    state.offset = end;
    0
}

impl Drop for SnapshotRuntime {
    fn drop(&mut self) {
        unsafe {
            if let Some(ctx) = self.restored_context.take() {
                sys::JS_FreeContext(ctx.as_ptr());
            }
            sys::JS_FreeRuntime(self.rt.as_ptr());
        }
    }
}

unsafe extern "C" fn interrupt_budget_handler(
    _rt: *mut sys::JSRuntime,
    opaque: *mut c_void,
) -> i32 {
    if opaque.is_null() {
        return 0;
    }
    let remaining = &mut *(opaque.cast::<u64>());
    if *remaining == 0 {
        return 1;
    }
    *remaining -= 1;
    0
}

pub struct SnapshotContext<'runtime> {
    rt: NonNull<sys::JSRuntime>,
    ctx: NonNull<sys::JSContext>,
    host_promises: HashMap<HostPromiseId, HostPromiseEntry>,
    _runtime: PhantomData<&'runtime SnapshotRuntime>,
}

struct HostPromiseEntry {
    promise: sys::JSValue,
}

impl SnapshotContext<'_> {
    pub fn raw_context(&self) -> *mut sys::JSContext {
        self.ctx.as_ptr()
    }

    pub unsafe fn set_context_opaque(&mut self, opaque: *mut c_void) {
        sys::JS_SetContextOpaque(self.ctx.as_ptr(), opaque);
    }

    pub fn install_global_native_function(
        &mut self,
        name: &str,
        function: sys::JSCFunction,
        arity: i32,
    ) -> Result<()> {
        let function = self.new_native_function(name, function, arity)?;
        let property_name = CString::new(name).map_err(|_| QuickJsError::InteriorNul)?;
        unsafe {
            let global = sys::JS_GetGlobalObject(self.ctx.as_ptr());
            if js_value_is_exception(global) {
                sys::JS_FreeValue(self.ctx.as_ptr(), function);
                return Err(QuickJsError::EvalFailed(exception_string(
                    self.ctx.as_ptr(),
                )));
            }
            let status =
                sys::JS_SetPropertyStr(self.ctx.as_ptr(), global, property_name.as_ptr(), function);
            sys::JS_FreeValue(self.ctx.as_ptr(), global);
            if status < 0 {
                return Err(QuickJsError::EvalFailed(exception_string(
                    self.ctx.as_ptr(),
                )));
            }
        }
        Ok(())
    }

    pub fn install_global_object_native_function(
        &mut self,
        object_name: &str,
        function_name: &str,
        function: sys::JSCFunction,
        arity: i32,
    ) -> Result<()> {
        let object_property = CString::new(object_name).map_err(|_| QuickJsError::InteriorNul)?;
        let function_property =
            CString::new(function_name).map_err(|_| QuickJsError::InteriorNul)?;
        let function = self.new_native_function(function_name, function, arity)?;
        unsafe {
            let global = sys::JS_GetGlobalObject(self.ctx.as_ptr());
            if js_value_is_exception(global) {
                sys::JS_FreeValue(self.ctx.as_ptr(), function);
                return Err(QuickJsError::EvalFailed(exception_string(
                    self.ctx.as_ptr(),
                )));
            }
            let mut object =
                sys::JS_GetPropertyStr(self.ctx.as_ptr(), global, object_property.as_ptr());
            if js_value_is_exception(object) {
                sys::JS_FreeValue(self.ctx.as_ptr(), global);
                sys::JS_FreeValue(self.ctx.as_ptr(), function);
                return Err(QuickJsError::EvalFailed(exception_string(
                    self.ctx.as_ptr(),
                )));
            }
            let created = js_value_is_undefined(object);
            if created {
                sys::JS_FreeValue(self.ctx.as_ptr(), object);
                object = sys::JS_NewObject(self.ctx.as_ptr());
                if js_value_is_exception(object) {
                    sys::JS_FreeValue(self.ctx.as_ptr(), global);
                    sys::JS_FreeValue(self.ctx.as_ptr(), function);
                    return Err(QuickJsError::EvalFailed(exception_string(
                        self.ctx.as_ptr(),
                    )));
                }
            }
            let status = sys::JS_SetPropertyStr(
                self.ctx.as_ptr(),
                object,
                function_property.as_ptr(),
                function,
            );
            if status < 0 {
                sys::JS_FreeValue(self.ctx.as_ptr(), global);
                sys::JS_FreeValue(self.ctx.as_ptr(), object);
                return Err(QuickJsError::EvalFailed(exception_string(
                    self.ctx.as_ptr(),
                )));
            }
            if created {
                let status = sys::JS_SetPropertyStr(
                    self.ctx.as_ptr(),
                    global,
                    object_property.as_ptr(),
                    object,
                );
                sys::JS_FreeValue(self.ctx.as_ptr(), global);
                if status < 0 {
                    return Err(QuickJsError::EvalFailed(exception_string(
                        self.ctx.as_ptr(),
                    )));
                }
            } else {
                sys::JS_FreeValue(self.ctx.as_ptr(), global);
                sys::JS_FreeValue(self.ctx.as_ptr(), object);
            }
        }
        Ok(())
    }

    pub fn eval_module(&mut self, name: &str, source: &str) -> Result<()> {
        let source = module_facade_source(source);
        self.eval(name, &source, JS_EVAL_TYPE_GLOBAL)
    }

    /// Evaluate raw source for conformance testing (e.g. Test262) against the
    /// bare ECMAScript context, with no chidori module facade or host object.
    ///
    /// Unlike [`SnapshotContext::eval`], a thrown value is returned as a
    /// structured [`JsThrow`] (carrying the error's `name`/constructor, message,
    /// and string form) instead of being flattened to a single string. That
    /// distinction is what conformance negative tests assert on. The pending
    /// job (microtask) queue is *not* drained here; call
    /// [`SnapshotContext::run_pending_jobs`] afterward for async tests.
    pub fn eval_for_conformance(
        &mut self,
        name: &str,
        source: &str,
        mode: EvalMode,
    ) -> std::result::Result<(), JsThrow> {
        let source = CString::new(source).map_err(|_| JsThrow::host("source contains a NUL byte"))?;
        let name = CString::new(name).map_err(|_| JsThrow::host("name contains a NUL byte"))?;
        let value = unsafe {
            sys::JS_Eval(
                self.ctx.as_ptr(),
                source.as_ptr(),
                source.as_bytes().len(),
                name.as_ptr(),
                mode.eval_flags(),
            )
        };
        if js_value_is_exception(value) {
            return Err(self.take_exception());
        }
        unsafe {
            sys::JS_FreeValue(self.ctx.as_ptr(), value);
        }
        Ok(())
    }

    /// Drain the pending job (microtask/promise) queue. Returns the first
    /// thrown value if a job rejects/throws. Used to settle async conformance
    /// tests before reading their `$DONE` signal.
    pub fn run_pending_jobs(&mut self) -> std::result::Result<(), JsThrow> {
        loop {
            let mut ctx = std::ptr::null_mut();
            let status = unsafe { sys::JS_ExecutePendingJob(self.rt.as_ptr(), &mut ctx) };
            if status > 0 {
                continue;
            }
            if status < 0 {
                return Err(self.take_exception());
            }
            return Ok(());
        }
    }

    /// Read a global property as JSON, returning `None` when it is absent or
    /// `undefined`. Used by the conformance harness to read the captured
    /// `print()` buffer that async tests signal completion through.
    pub fn read_global_json(&mut self, prop: &str) -> Option<Value> {
        self.global_json(prop).ok().flatten()
    }

    /// Pull the currently-pending exception off the context and describe it.
    fn take_exception(&mut self) -> JsThrow {
        let ctx = self.ctx.as_ptr();
        unsafe {
            let exception = sys::JS_GetException(ctx);
            let to_string =
                js_value_to_string(ctx, exception).unwrap_or_else(|_| "<exception>".to_string());
            let name = read_property_string(ctx, exception, "name");
            let message = read_property_string(ctx, exception, "message");
            let constructor_name = read_constructor_name(ctx, exception);
            sys::JS_FreeValue(ctx, exception);
            // Reading properties off a thrown primitive (e.g. `throw null`) can
            // leave a fresh exception pending; clear it so the next eval starts
            // clean.
            let leftover = sys::JS_GetException(ctx);
            sys::JS_FreeValue(ctx, leftover);
            let name = name
                .or(constructor_name)
                .unwrap_or_else(|| to_string.clone());
            JsThrow {
                name,
                message: message.unwrap_or_default(),
                to_string,
            }
        }
    }

    pub fn call_export_json(&mut self, export: &str, args: Value) -> Result<RunState> {
        let export_name = serde_json::to_string(export)
            .map_err(|err| QuickJsError::EvalFailed(err.to_string()))?;
        let args = serde_json::to_string(&args).map_err(|err| {
            QuickJsError::EvalFailed(format!("failed to encode call arguments: {err}"))
        })?;
        let source = format!(
            r#"
            globalThis.__chidori_call_result = undefined;
            globalThis.__chidori_call_error = undefined;
            globalThis.__chidori_active_host_operation_id = undefined;
            Promise.resolve(globalThis.__chidori_exports[{export_name}]({args}, globalThis.chidori)).then(
                value => {{
                    globalThis.__chidori_active_host_operation_id = undefined;
                    globalThis.__chidori_call_result = value;
                }},
                error => {{
                    globalThis.__chidori_active_host_operation_id = undefined;
                    globalThis.__chidori_call_error = String(error && error.message ? error.message : error);
                }}
            );
            "#
        );
        self.eval("__chidori_call.js", &source, JS_EVAL_TYPE_GLOBAL)?;
        self.drain_jobs()?;

        if let Some(error) = self.global_json("__chidori_call_error")? {
            if !error.is_null() {
                let message = error.as_str().unwrap_or("unknown JavaScript rejection");
                return Err(QuickJsError::EvalFailed(message.to_string()));
            }
        }

        if let Some(value) = self.global_json("__chidori_call_result")? {
            return Ok(RunState::Completed(value));
        }

        match self.global_json("__chidori_active_host_operation_id")? {
            Some(value) => match value.as_u64() {
                Some(id) => Ok(RunState::BlockedOnHostOperation(HostPromiseId(id))),
                None => Err(QuickJsError::EvalFailed(
                    "__chidori_active_host_operation_id is not a number".to_string(),
                )),
            },
            None => Ok(RunState::Completed(Value::Null)),
        }
    }

    pub fn new_host_promise(&mut self, id: HostPromiseId) -> Result<JsValue> {
        let promise = unsafe { sys::CHIDORI_JS_NewHostPromise(self.ctx.as_ptr(), id.0) };
        if js_value_is_exception(promise) {
            return Err(QuickJsError::EvalFailed(exception_string(
                self.ctx.as_ptr(),
            )));
        }

        self.host_promises.insert(id, HostPromiseEntry { promise });
        Ok(JsValue(promise))
    }

    pub fn resolve_host_promise(&mut self, id: HostPromiseId, value: Value) -> Result<()> {
        let value = self.json_to_js(value)?;
        let result = unsafe { sys::CHIDORI_JS_ResolveHostPromise(self.ctx.as_ptr(), id.0, value) };
        unsafe {
            sys::JS_FreeValue(self.ctx.as_ptr(), value);
        }
        if result < 0 {
            return Err(QuickJsError::EvalFailed(exception_string(
                self.ctx.as_ptr(),
            )));
        }
        self.drain_jobs()
    }

    pub fn resolve_host_promise_and_run(
        &mut self,
        id: HostPromiseId,
        value: Value,
    ) -> Result<RunState> {
        self.resolve_host_promise(id, value)?;
        self.run_jobs_until_blocked()
    }

    pub fn reject_host_promise(&mut self, id: HostPromiseId, error: String) -> Result<()> {
        let value = self.string_to_js(&error)?;
        let result = unsafe { sys::CHIDORI_JS_RejectHostPromise(self.ctx.as_ptr(), id.0, value) };
        unsafe {
            sys::JS_FreeValue(self.ctx.as_ptr(), value);
        }
        if result < 0 {
            return Err(QuickJsError::EvalFailed(exception_string(
                self.ctx.as_ptr(),
            )));
        }
        self.drain_jobs()
    }

    pub fn reject_host_promise_and_run(
        &mut self,
        id: HostPromiseId,
        error: String,
    ) -> Result<RunState> {
        self.reject_host_promise(id, error)?;
        self.run_jobs_until_blocked()
    }

    pub fn host_promise_state(&mut self, id: HostPromiseId) -> Result<PromiseState> {
        let Some(entry) = self.host_promises.get(&id) else {
            return Err(QuickJsError::EvalFailed(format!(
                "unknown host promise id {}",
                id.0
            )));
        };
        let promise = entry.promise;
        let state = unsafe { sys::JS_PromiseState(self.ctx.as_ptr(), promise) };
        match state {
            0 => Ok(PromiseState::Pending),
            1 => {
                let value = unsafe { sys::JS_PromiseResult(self.ctx.as_ptr(), promise) };
                Ok(PromiseState::Fulfilled(self.js_to_json(value)?))
            }
            2 => {
                let value = unsafe { sys::JS_PromiseResult(self.ctx.as_ptr(), promise) };
                Ok(PromiseState::Rejected(self.js_to_json(value)?))
            }
            other => Err(QuickJsError::EvalFailed(format!(
                "unknown QuickJS promise state {other}"
            ))),
        }
    }

    pub fn run_jobs_until_blocked(&mut self) -> Result<RunState> {
        self.drain_jobs()?;
        if let Some(value) = self.global_json("__chidori_active_host_operation_id")? {
            return match value.as_u64() {
                Some(id) => Ok(RunState::BlockedOnHostOperation(HostPromiseId(id))),
                None => Err(QuickJsError::EvalFailed(
                    "__chidori_active_host_operation_id is not a number".to_string(),
                )),
            };
        }
        if let Some(error) = self.global_json("__chidori_call_error")? {
            if !error.is_null() {
                let message = error.as_str().unwrap_or("unknown JavaScript rejection");
                return Err(QuickJsError::EvalFailed(message.to_string()));
            }
        }
        Ok(RunState::Completed(
            self.global_json("__chidori_call_result")?
                .unwrap_or(Value::Null),
        ))
    }

    pub fn snapshot_microtask_queue(&mut self) -> Result<Vec<u8>> {
        let mut len = 0usize;
        let ptr = unsafe { sys::CHIDORI_JS_WriteMicrotaskQueue(self.ctx.as_ptr(), &mut len) };
        if ptr.is_null() {
            return Err(QuickJsError::EvalFailed(exception_string(
                self.ctx.as_ptr(),
            )));
        }
        let bytes = unsafe {
            let bytes = std::slice::from_raw_parts(ptr, len).to_vec();
            sys::js_free(self.ctx.as_ptr(), ptr.cast::<c_void>());
            bytes
        };
        Ok(bytes)
    }

    pub fn snapshot_context_payload(&mut self) -> Result<Vec<u8>> {
        snapshot_context_payload(self.rt, self.ctx)
    }

    pub fn snapshot_runtime(&mut self) -> Result<RuntimeSnapshot> {
        let runtime_payload = snapshot_runtime_payload(self.rt)?;
        let context_payload = self.snapshot_context_payload()?;
        Ok(RuntimeSnapshot::from_parts(
            &runtime_payload,
            &context_payload,
        ))
    }

    pub fn restore_microtask_queue(&mut self, snapshot: &[u8]) -> Result<()> {
        let ret = unsafe {
            sys::CHIDORI_JS_ReadMicrotaskQueue(self.ctx.as_ptr(), snapshot.as_ptr(), snapshot.len())
        };
        if ret < 0 {
            return Err(QuickJsError::EvalFailed(exception_string(
                self.ctx.as_ptr(),
            )));
        }
        Ok(())
    }

    pub fn snapshot_globals(&mut self, root_names: &[&str]) -> Result<Vec<u8>> {
        let root_names = root_names
            .iter()
            .map(|root_name| CString::new(*root_name).map_err(|_| QuickJsError::InteriorNul))
            .collect::<Result<Vec<_>>>()?;
        let mut roots = Vec::with_capacity(root_names.len());
        let holder = unsafe { sys::JS_NewObject(self.ctx.as_ptr()) };
        if js_value_is_exception(holder) {
            return Err(QuickJsError::EvalFailed(exception_string(
                self.ctx.as_ptr(),
            )));
        }

        let global = unsafe { sys::JS_GetGlobalObject(self.ctx.as_ptr()) };
        if js_value_is_exception(global) {
            unsafe {
                sys::JS_FreeValue(self.ctx.as_ptr(), holder);
            }
            return Err(QuickJsError::EvalFailed(exception_string(
                self.ctx.as_ptr(),
            )));
        }

        for root_name in root_names {
            let value =
                unsafe { sys::JS_GetPropertyStr(self.ctx.as_ptr(), global, root_name.as_ptr()) };
            if js_value_is_exception(value) {
                unsafe {
                    sys::JS_FreeValue(self.ctx.as_ptr(), global);
                    sys::JS_FreeValue(self.ctx.as_ptr(), holder);
                }
                return Err(QuickJsError::EvalFailed(exception_string(
                    self.ctx.as_ptr(),
                )));
            }
            let status = unsafe {
                sys::JS_SetPropertyStr(self.ctx.as_ptr(), holder, root_name.as_ptr(), value)
            };
            if status < 0 {
                unsafe {
                    sys::JS_FreeValue(self.ctx.as_ptr(), global);
                    sys::JS_FreeValue(self.ctx.as_ptr(), holder);
                }
                return Err(QuickJsError::EvalFailed(exception_string(
                    self.ctx.as_ptr(),
                )));
            }
            roots.push(root_name.into_bytes());
        }
        unsafe {
            sys::JS_FreeValue(self.ctx.as_ptr(), global);
        }

        let roots_snapshot = self.snapshot_js_value(holder)?;
        let microtask_snapshot = self.snapshot_microtask_queue()?;
        Ok(encode_context_snapshot(
            &roots,
            &roots_snapshot,
            &microtask_snapshot,
        ))
    }

    pub fn restore_globals(&mut self, snapshot: &[u8]) -> Result<Vec<String>> {
        let decoded = decode_context_snapshot(snapshot)?;
        let holder = unsafe {
            sys::JS_ReadObject(
                self.ctx.as_ptr(),
                decoded.roots_snapshot.as_ptr(),
                decoded.roots_snapshot.len(),
                JS_READ_OBJ_BYTECODE | JS_READ_OBJ_REFERENCE,
            )
        };
        if js_value_is_exception(holder) {
            return Err(QuickJsError::EvalFailed(exception_string(
                self.ctx.as_ptr(),
            )));
        }

        let global = unsafe { sys::JS_GetGlobalObject(self.ctx.as_ptr()) };
        if js_value_is_exception(global) {
            unsafe {
                sys::JS_FreeValue(self.ctx.as_ptr(), holder);
            }
            return Err(QuickJsError::EvalFailed(exception_string(
                self.ctx.as_ptr(),
            )));
        }

        let mut restored = Vec::with_capacity(decoded.root_names.len());
        for root_name in &decoded.root_names {
            let property =
                CString::new(root_name.as_str()).map_err(|_| QuickJsError::InteriorNul)?;
            let value =
                unsafe { sys::JS_GetPropertyStr(self.ctx.as_ptr(), holder, property.as_ptr()) };
            if js_value_is_exception(value) {
                unsafe {
                    sys::JS_FreeValue(self.ctx.as_ptr(), global);
                    sys::JS_FreeValue(self.ctx.as_ptr(), holder);
                }
                return Err(QuickJsError::EvalFailed(exception_string(
                    self.ctx.as_ptr(),
                )));
            }
            let status = unsafe {
                sys::JS_SetPropertyStr(self.ctx.as_ptr(), global, property.as_ptr(), value)
            };
            if status < 0 {
                unsafe {
                    sys::JS_FreeValue(self.ctx.as_ptr(), global);
                    sys::JS_FreeValue(self.ctx.as_ptr(), holder);
                }
                return Err(QuickJsError::EvalFailed(exception_string(
                    self.ctx.as_ptr(),
                )));
            }
            restored.push(root_name.clone());
        }

        unsafe {
            sys::JS_FreeValue(self.ctx.as_ptr(), global);
            sys::JS_FreeValue(self.ctx.as_ptr(), holder);
        }
        self.restore_microtask_queue(&decoded.microtask_snapshot)?;
        Ok(restored)
    }

    pub fn snapshot_json_value(&mut self, value: Value) -> Result<Vec<u8>> {
        let value = self.json_to_js(value)?;
        self.snapshot_js_value(value)
    }

    pub fn snapshot_expression(&mut self, name: &str, expression: &str) -> Result<Vec<u8>> {
        let source = format!("globalThis.__chidori_snapshot_value = ({expression});");
        self.eval(name, &source, JS_EVAL_TYPE_GLOBAL)?;
        let value = self.global_value("__chidori_snapshot_value")?;
        self.snapshot_js_value(value)
    }

    pub fn restore_snapshot_to_global(&mut self, property: &str, snapshot: &[u8]) -> Result<()> {
        let property = CString::new(property).map_err(|_| QuickJsError::InteriorNul)?;
        let value = unsafe {
            sys::JS_ReadObject(
                self.ctx.as_ptr(),
                snapshot.as_ptr(),
                snapshot.len(),
                JS_READ_OBJ_BYTECODE | JS_READ_OBJ_REFERENCE,
            )
        };
        if js_value_is_exception(value) {
            return Err(QuickJsError::EvalFailed(exception_string(
                self.ctx.as_ptr(),
            )));
        }

        unsafe {
            let global = sys::JS_GetGlobalObject(self.ctx.as_ptr());
            if js_value_is_exception(global) {
                sys::JS_FreeValue(self.ctx.as_ptr(), value);
                return Err(QuickJsError::EvalFailed(exception_string(
                    self.ctx.as_ptr(),
                )));
            }
            let status =
                sys::JS_SetPropertyStr(self.ctx.as_ptr(), global, property.as_ptr(), value);
            sys::JS_FreeValue(self.ctx.as_ptr(), global);
            if status < 0 {
                return Err(QuickJsError::EvalFailed(exception_string(
                    self.ctx.as_ptr(),
                )));
            }
        }
        Ok(())
    }

    pub fn eval_json_expression(&mut self, name: &str, expression: &str) -> Result<Value> {
        let source = format!("globalThis.__chidori_eval_json = ({expression});");
        self.eval(name, &source, JS_EVAL_TYPE_GLOBAL)?;
        self.global_json("__chidori_eval_json")?.ok_or_else(|| {
            QuickJsError::EvalFailed("expression evaluated to undefined".to_string())
        })
    }

    pub fn snapshot_global_bytecode(&mut self, name: &str, source: &str) -> Result<Vec<u8>> {
        self.snapshot_compiled_bytecode(name, source, JS_EVAL_TYPE_GLOBAL)
    }

    pub fn snapshot_module_bytecode(&mut self, name: &str, source: &str) -> Result<Vec<u8>> {
        self.snapshot_compiled_bytecode(name, source, JS_EVAL_TYPE_MODULE)
    }

    pub fn eval_bytecode_json(&mut self, snapshot: &[u8]) -> Result<Value> {
        let value = self.eval_bytecode(snapshot)?;
        self.js_to_json(value)
    }

    pub fn eval_module_bytecode_namespace_json(&mut self, snapshot: &[u8]) -> Result<Value> {
        let value = self.read_bytecode(snapshot)?;
        if value.tag != JS_TAG_MODULE {
            unsafe {
                sys::JS_FreeValue(self.ctx.as_ptr(), value);
            }
            return Err(QuickJsError::EvalFailed(
                "bytecode snapshot did not contain a module".to_string(),
            ));
        }
        if unsafe { sys::JS_ResolveModule(self.ctx.as_ptr(), value) } < 0 {
            unsafe {
                sys::JS_FreeValue(self.ctx.as_ptr(), value);
            }
            return Err(QuickJsError::EvalFailed(exception_string(
                self.ctx.as_ptr(),
            )));
        }
        let module = unsafe { value.u.ptr.cast::<sys::JSModuleDef>() };
        let result = unsafe { sys::JS_EvalFunction(self.ctx.as_ptr(), value) };
        if js_value_is_exception(result) {
            return Err(QuickJsError::EvalFailed(exception_string(
                self.ctx.as_ptr(),
            )));
        }
        unsafe {
            sys::JS_FreeValue(self.ctx.as_ptr(), result);
        }
        self.drain_jobs()?;

        let namespace = unsafe { sys::JS_GetModuleNamespace(self.ctx.as_ptr(), module) };
        if js_value_is_exception(namespace) {
            return Err(QuickJsError::EvalFailed(exception_string(
                self.ctx.as_ptr(),
            )));
        }
        self.js_to_json(namespace)
    }

    pub fn snapshot_evaluated_module_namespace(
        &mut self,
        name: &str,
        source: &str,
    ) -> Result<Vec<u8>> {
        let bytecode = self.snapshot_module_bytecode(name, source)?;
        let value = self.read_bytecode(&bytecode)?;
        if value.tag != JS_TAG_MODULE {
            unsafe {
                sys::JS_FreeValue(self.ctx.as_ptr(), value);
            }
            return Err(QuickJsError::EvalFailed(
                "bytecode snapshot did not contain a module".to_string(),
            ));
        }
        if unsafe { sys::JS_ResolveModule(self.ctx.as_ptr(), value) } < 0 {
            unsafe {
                sys::JS_FreeValue(self.ctx.as_ptr(), value);
            }
            return Err(QuickJsError::EvalFailed(exception_string(
                self.ctx.as_ptr(),
            )));
        }
        let module = unsafe { value.u.ptr.cast::<sys::JSModuleDef>() };
        let result = unsafe { sys::JS_EvalFunction(self.ctx.as_ptr(), value) };
        if js_value_is_exception(result) {
            return Err(QuickJsError::EvalFailed(exception_string(
                self.ctx.as_ptr(),
            )));
        }
        unsafe {
            sys::JS_FreeValue(self.ctx.as_ptr(), result);
        }
        self.drain_jobs()?;
        let namespace = unsafe { sys::JS_GetModuleNamespace(self.ctx.as_ptr(), module) };
        if js_value_is_exception(namespace) {
            return Err(QuickJsError::EvalFailed(exception_string(
                self.ctx.as_ptr(),
            )));
        }
        self.snapshot_js_value(namespace)
    }

    pub fn eval_bytecode(&mut self, snapshot: &[u8]) -> Result<sys::JSValue> {
        let value = self.read_bytecode(snapshot)?;
        if value.tag == JS_TAG_MODULE
            && unsafe { sys::JS_ResolveModule(self.ctx.as_ptr(), value) } < 0
        {
            unsafe {
                sys::JS_FreeValue(self.ctx.as_ptr(), value);
            }
            return Err(QuickJsError::EvalFailed(exception_string(
                self.ctx.as_ptr(),
            )));
        }
        let result = unsafe { sys::JS_EvalFunction(self.ctx.as_ptr(), value) };
        if js_value_is_exception(result) {
            return Err(QuickJsError::EvalFailed(exception_string(
                self.ctx.as_ptr(),
            )));
        }
        self.drain_jobs()?;
        Ok(result)
    }

    fn read_bytecode(&mut self, snapshot: &[u8]) -> Result<sys::JSValue> {
        let value = unsafe {
            sys::JS_ReadObject(
                self.ctx.as_ptr(),
                snapshot.as_ptr(),
                snapshot.len(),
                JS_READ_OBJ_BYTECODE | JS_READ_OBJ_REFERENCE,
            )
        };
        if js_value_is_exception(value) {
            return Err(QuickJsError::EvalFailed(exception_string(
                self.ctx.as_ptr(),
            )));
        }
        Ok(value)
    }

    fn snapshot_js_value(&mut self, value: sys::JSValue) -> Result<Vec<u8>> {
        let mut len = 0usize;
        let bytes = unsafe {
            let ptr = sys::JS_WriteObject(
                self.ctx.as_ptr(),
                &mut len,
                value,
                JS_WRITE_OBJ_BYTECODE | JS_WRITE_OBJ_REFERENCE,
            );
            sys::JS_FreeValue(self.ctx.as_ptr(), value);
            if ptr.is_null() {
                return Err(QuickJsError::EvalFailed(exception_string(
                    self.ctx.as_ptr(),
                )));
            }
            let bytes = std::slice::from_raw_parts(ptr, len).to_vec();
            sys::js_free(self.ctx.as_ptr(), ptr.cast::<c_void>());
            bytes
        };
        Ok(bytes)
    }

    fn snapshot_compiled_bytecode(
        &mut self,
        name: &str,
        source: &str,
        eval_type: i32,
    ) -> Result<Vec<u8>> {
        let source = CString::new(source).map_err(|_| QuickJsError::InteriorNul)?;
        let name = CString::new(name).map_err(|_| QuickJsError::InteriorNul)?;
        let compiled = unsafe {
            sys::JS_Eval(
                self.ctx.as_ptr(),
                source.as_ptr(),
                source.as_bytes().len(),
                name.as_ptr(),
                eval_type | JS_EVAL_FLAG_COMPILE_ONLY,
            )
        };
        if js_value_is_exception(compiled) {
            return Err(QuickJsError::EvalFailed(exception_string(
                self.ctx.as_ptr(),
            )));
        }

        let mut len = 0usize;
        let bytes = unsafe {
            let ptr = sys::JS_WriteObject(
                self.ctx.as_ptr(),
                &mut len,
                compiled,
                JS_WRITE_OBJ_BYTECODE | JS_WRITE_OBJ_REFERENCE,
            );
            sys::JS_FreeValue(self.ctx.as_ptr(), compiled);
            if ptr.is_null() {
                return Err(QuickJsError::EvalFailed(exception_string(
                    self.ctx.as_ptr(),
                )));
            }
            let bytes = std::slice::from_raw_parts(ptr, len).to_vec();
            sys::js_free(self.ctx.as_ptr(), ptr.cast::<c_void>());
            bytes
        };
        Ok(bytes)
    }

    pub fn restore_json_value(&mut self, snapshot: &[u8]) -> Result<Value> {
        let value = unsafe {
            sys::JS_ReadObject(
                self.ctx.as_ptr(),
                snapshot.as_ptr(),
                snapshot.len(),
                JS_READ_OBJ_BYTECODE | JS_READ_OBJ_REFERENCE,
            )
        };
        if js_value_is_exception(value) {
            return Err(QuickJsError::EvalFailed(exception_string(
                self.ctx.as_ptr(),
            )));
        }
        self.js_to_json(value)
    }
}

impl Drop for SnapshotContext<'_> {
    fn drop(&mut self) {
        unsafe {
            for (_, entry) in self.host_promises.drain() {
                sys::JS_FreeValue(self.ctx.as_ptr(), entry.promise);
            }
            sys::JS_FreeContext(self.ctx.as_ptr());
        }
    }
}

struct DecodedContextSnapshot {
    root_names: Vec<String>,
    roots_snapshot: Vec<u8>,
    microtask_snapshot: Vec<u8>,
}

struct DecodedRuntimeSnapshot<'a> {
    runtime_payload: &'a [u8],
    context_payload: &'a [u8],
}

fn encode_runtime_snapshot_payload(runtime_payload: &[u8], context_payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(RUNTIME_SNAPSHOT_MAGIC);
    put_u64(&mut out, runtime_payload.len() as u64);
    out.extend_from_slice(runtime_payload);
    put_u64(&mut out, context_payload.len() as u64);
    out.extend_from_slice(context_payload);
    out
}

fn decode_runtime_snapshot_payload(snapshot: &[u8]) -> Result<DecodedRuntimeSnapshot<'_>> {
    let mut cursor = 0usize;
    if read_bytes(snapshot, &mut cursor, RUNTIME_SNAPSHOT_MAGIC.len())? != RUNTIME_SNAPSHOT_MAGIC {
        return Err(QuickJsError::InvalidSnapshot(
            "runtime snapshot magic mismatch".to_string(),
        ));
    }
    let runtime_payload_len = snapshot_len_to_usize(read_u64(snapshot, &mut cursor)?)?;
    let runtime_payload = read_bytes(snapshot, &mut cursor, runtime_payload_len)?;
    let context_payload_len = snapshot_len_to_usize(read_u64(snapshot, &mut cursor)?)?;
    let context_payload = read_bytes(snapshot, &mut cursor, context_payload_len)?;
    if cursor != snapshot.len() {
        return Err(QuickJsError::InvalidSnapshot(
            "runtime snapshot has trailing bytes".to_string(),
        ));
    }
    Ok(DecodedRuntimeSnapshot {
        runtime_payload,
        context_payload,
    })
}

fn validate_runtime_snapshot_payloads(payloads: &DecodedRuntimeSnapshot<'_>) -> Result<()> {
    if payloads.runtime_payload.is_empty() {
        return Err(QuickJsError::InvalidSnapshot(
            "runtime snapshot runtime payload is empty".to_string(),
        ));
    }
    if payloads.context_payload.is_empty() {
        return Err(QuickJsError::InvalidSnapshot(
            "runtime snapshot context payload is empty".to_string(),
        ));
    }
    Ok(())
}

fn encode_context_snapshot(
    root_names: &[Vec<u8>],
    roots_snapshot: &[u8],
    microtask_snapshot: &[u8],
) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(CONTEXT_SNAPSHOT_MAGIC);
    put_u32(&mut out, root_names.len() as u32);
    for root_name in root_names {
        put_u32(&mut out, root_name.len() as u32);
        out.extend_from_slice(root_name);
    }
    put_u64(&mut out, roots_snapshot.len() as u64);
    out.extend_from_slice(roots_snapshot);
    put_u64(&mut out, microtask_snapshot.len() as u64);
    out.extend_from_slice(microtask_snapshot);
    out
}

fn decode_context_snapshot(snapshot: &[u8]) -> Result<DecodedContextSnapshot> {
    let mut cursor = 0usize;
    if read_bytes(snapshot, &mut cursor, CONTEXT_SNAPSHOT_MAGIC.len())? != CONTEXT_SNAPSHOT_MAGIC {
        return Err(QuickJsError::InvalidSnapshot(
            "context snapshot magic mismatch".to_string(),
        ));
    }

    let root_count = read_u32(snapshot, &mut cursor)? as usize;
    let mut root_names = Vec::with_capacity(root_count);
    for _ in 0..root_count {
        let name_len = read_u32(snapshot, &mut cursor)? as usize;
        let name = read_bytes(snapshot, &mut cursor, name_len)?;
        let name = std::str::from_utf8(name)
            .map_err(|err| QuickJsError::InvalidSnapshot(err.to_string()))?
            .to_string();
        root_names.push(name);
    }

    let roots_len = snapshot_len_to_usize(read_u64(snapshot, &mut cursor)?)?;
    let roots_snapshot = read_bytes(snapshot, &mut cursor, roots_len)?.to_vec();
    let microtask_len = snapshot_len_to_usize(read_u64(snapshot, &mut cursor)?)?;
    let microtask_snapshot = read_bytes(snapshot, &mut cursor, microtask_len)?.to_vec();

    if cursor != snapshot.len() {
        return Err(QuickJsError::InvalidSnapshot(
            "trailing bytes after context snapshot".to_string(),
        ));
    }

    Ok(DecodedContextSnapshot {
        root_names,
        roots_snapshot,
        microtask_snapshot,
    })
}

fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn read_u32(snapshot: &[u8], cursor: &mut usize) -> Result<u32> {
    let bytes = read_bytes(snapshot, cursor, std::mem::size_of::<u32>())?;
    let mut fixed = [0u8; 4];
    fixed.copy_from_slice(bytes);
    Ok(u32::from_le_bytes(fixed))
}

fn read_u64(snapshot: &[u8], cursor: &mut usize) -> Result<u64> {
    let bytes = read_bytes(snapshot, cursor, std::mem::size_of::<u64>())?;
    let mut fixed = [0u8; 8];
    fixed.copy_from_slice(bytes);
    Ok(u64::from_le_bytes(fixed))
}

fn snapshot_len_to_usize(len: u64) -> Result<usize> {
    usize::try_from(len).map_err(|_| QuickJsError::InvalidSnapshot("length overflow".to_string()))
}

fn read_bytes<'a>(snapshot: &'a [u8], cursor: &mut usize, len: usize) -> Result<&'a [u8]> {
    let end = cursor
        .checked_add(len)
        .ok_or_else(|| QuickJsError::InvalidSnapshot("length overflow".to_string()))?;
    if end > snapshot.len() {
        return Err(QuickJsError::InvalidSnapshot(
            "snapshot ended early".to_string(),
        ));
    }
    let bytes = &snapshot[*cursor..end];
    *cursor = end;
    Ok(bytes)
}

impl SnapshotContext<'_> {
    fn eval(&mut self, name: &str, source: &str, flags: i32) -> Result<()> {
        let source = CString::new(source).map_err(|_| QuickJsError::InteriorNul)?;
        let name = CString::new(name).map_err(|_| QuickJsError::InteriorNul)?;
        let value = unsafe {
            sys::JS_Eval(
                self.ctx.as_ptr(),
                source.as_ptr(),
                source.as_bytes().len(),
                name.as_ptr(),
                flags,
            )
        };
        if js_value_is_exception(value) {
            return Err(QuickJsError::EvalFailed(exception_string(
                self.ctx.as_ptr(),
            )));
        }
        unsafe {
            sys::JS_FreeValue(self.ctx.as_ptr(), value);
        }
        Ok(())
    }

    fn drain_jobs(&mut self) -> Result<()> {
        drain_jobs_for_runtime(self.rt.as_ptr())
    }

    fn json_to_js(&mut self, value: Value) -> Result<sys::JSValue> {
        json_to_js(self.ctx.as_ptr(), value)
    }

    fn new_native_function(
        &mut self,
        name: &str,
        function: sys::JSCFunction,
        arity: i32,
    ) -> Result<sys::JSValue> {
        let function_name = CString::new(name).map_err(|_| QuickJsError::InteriorNul)?;
        let function = unsafe {
            sys::JS_NewCFunction2(
                self.ctx.as_ptr(),
                function,
                function_name.as_ptr(),
                arity,
                JS_CFUNC_GENERIC,
                0,
            )
        };
        if js_value_is_exception(function) {
            return Err(QuickJsError::EvalFailed(exception_string(
                self.ctx.as_ptr(),
            )));
        }
        Ok(function)
    }

    fn string_to_js(&mut self, value: &str) -> Result<sys::JSValue> {
        string_to_js(self.ctx.as_ptr(), value)
    }

    fn js_to_json(&mut self, value: sys::JSValue) -> Result<Value> {
        unsafe {
            if let Err(err) = validate_json_boundary_value(self.ctx.as_ptr(), value) {
                sys::JS_FreeValue(self.ctx.as_ptr(), value);
                return Err(err);
            }
            let json =
                sys::JS_JSONStringify(self.ctx.as_ptr(), value, js_undefined(), js_undefined());
            sys::JS_FreeValue(self.ctx.as_ptr(), value);
            if js_value_is_exception(json) {
                return Err(QuickJsError::EvalFailed(exception_string(
                    self.ctx.as_ptr(),
                )));
            }
            if js_value_is_undefined(json) {
                sys::JS_FreeValue(self.ctx.as_ptr(), json);
                return Ok(Value::Null);
            }
            let raw = sys::JS_ToCStringLen2(self.ctx.as_ptr(), std::ptr::null_mut(), json, false);
            if raw.is_null() {
                sys::JS_FreeValue(self.ctx.as_ptr(), json);
                return Err(QuickJsError::EvalFailed(
                    "JSON.stringify failed".to_string(),
                ));
            }
            let text = CStr::from_ptr(raw).to_string_lossy().into_owned();
            sys::JS_FreeCString(self.ctx.as_ptr(), raw);
            sys::JS_FreeValue(self.ctx.as_ptr(), json);
            serde_json::from_str(&text).map_err(|err| QuickJsError::EvalFailed(err.to_string()))
        }
    }

    fn global_value(&mut self, prop: &str) -> Result<sys::JSValue> {
        let prop = CString::new(prop).map_err(|_| QuickJsError::InteriorNul)?;
        unsafe {
            let global = sys::JS_GetGlobalObject(self.ctx.as_ptr());
            if js_value_is_exception(global) {
                return Err(QuickJsError::EvalFailed(exception_string(
                    self.ctx.as_ptr(),
                )));
            }
            let value = sys::JS_GetPropertyStr(self.ctx.as_ptr(), global, prop.as_ptr());
            sys::JS_FreeValue(self.ctx.as_ptr(), global);
            if js_value_is_exception(value) {
                return Err(QuickJsError::EvalFailed(exception_string(
                    self.ctx.as_ptr(),
                )));
            }
            Ok(value)
        }
    }

    fn global_json(&mut self, prop: &str) -> Result<Option<serde_json::Value>> {
        global_json(self.ctx.as_ptr(), prop)
    }
}

fn validate_json_boundary_value(ctx: *mut sys::JSContext, value: sys::JSValue) -> Result<()> {
    let source = CString::new(
        r#"
        (value) => {
            const seen = [];
            function visit(current) {
                if (typeof current === "function") {
                    return "functions and unsupported native values cannot cross the Chidori host boundary";
                }
                if (current && typeof current === "object") {
                    if (seen.indexOf(current) !== -1) {
                        return "cyclic JavaScript values cannot cross the Chidori host boundary";
                    }
                    seen.push(current);
                    const proto = Object.getPrototypeOf(current);
                    if (proto && proto !== Object.prototype && proto !== Array.prototype) {
                        return "unsupported external class cannot cross the Chidori host boundary";
                    }
                    const keys = Object.keys(current);
                    for (const key of keys) {
                        const error = visit(current[key]);
                        if (error) return error;
                    }
                    seen.pop();
                }
                return undefined;
            }
            return visit(value);
        }
        "#,
    )
    .map_err(|_| QuickJsError::InteriorNul)?;
    let name = CString::new("chidori-json-boundary-validator.js")
        .map_err(|_| QuickJsError::InteriorNul)?;
    unsafe {
        let validator = sys::JS_Eval(
            ctx,
            source.as_ptr(),
            source.as_bytes().len(),
            name.as_ptr(),
            JS_EVAL_TYPE_GLOBAL,
        );
        if js_value_is_exception(validator) {
            return Err(QuickJsError::EvalFailed(exception_string(ctx)));
        }
        let mut args = [sys::JS_DupValue(ctx, value)];
        let result = sys::JS_Call(ctx, validator, js_undefined(), 1, args.as_mut_ptr());
        sys::JS_FreeValue(ctx, args[0]);
        sys::JS_FreeValue(ctx, validator);
        if js_value_is_exception(result) {
            return Err(QuickJsError::EvalFailed(exception_string(ctx)));
        }
        if js_value_is_undefined(result) {
            sys::JS_FreeValue(ctx, result);
            return Ok(());
        }
        let raw = sys::JS_ToCStringLen2(ctx, std::ptr::null_mut(), result, false);
        if raw.is_null() {
            sys::JS_FreeValue(ctx, result);
            return Err(QuickJsError::EvalFailed(
                "failed to read JavaScript JSON validation result".to_string(),
            ));
        }
        let message = CStr::from_ptr(raw).to_string_lossy().into_owned();
        sys::JS_FreeCString(ctx, raw);
        sys::JS_FreeValue(ctx, result);
        Err(QuickJsError::EvalFailed(message))
    }
}

fn global_json(ctx: *mut sys::JSContext, prop: &str) -> Result<Option<serde_json::Value>> {
    let prop = CString::new(prop).map_err(|_| QuickJsError::InteriorNul)?;
    unsafe {
        let global = sys::JS_GetGlobalObject(ctx);
        if js_value_is_exception(global) {
            return Err(QuickJsError::EvalFailed(exception_string(ctx)));
        }
        let value = sys::JS_GetPropertyStr(ctx, global, prop.as_ptr());
        sys::JS_FreeValue(ctx, global);
        if js_value_is_exception(value) {
            return Err(QuickJsError::EvalFailed(exception_string(ctx)));
        }
        if js_value_is_undefined(value) {
            sys::JS_FreeValue(ctx, value);
            return Ok(None);
        }

        if let Err(err) = validate_json_boundary_value(ctx, value) {
            sys::JS_FreeValue(ctx, value);
            return Err(err);
        }
        let json = sys::JS_JSONStringify(ctx, value, js_undefined(), js_undefined());
        sys::JS_FreeValue(ctx, value);
        if js_value_is_exception(json) {
            return Err(QuickJsError::EvalFailed(exception_string(ctx)));
        }
        if js_value_is_undefined(json) {
            sys::JS_FreeValue(ctx, json);
            return Ok(None);
        }
        let raw = sys::JS_ToCStringLen2(ctx, std::ptr::null_mut(), json, false);
        if raw.is_null() {
            sys::JS_FreeValue(ctx, json);
            return Err(QuickJsError::EvalFailed(
                "JSON.stringify failed".to_string(),
            ));
        }
        let text = CStr::from_ptr(raw).to_string_lossy().into_owned();
        sys::JS_FreeCString(ctx, raw);
        sys::JS_FreeValue(ctx, json);
        serde_json::from_str(&text)
            .map(Some)
            .map_err(|err| QuickJsError::EvalFailed(err.to_string()))
    }
}

unsafe fn js_borrowed_value_to_json(
    ctx: *mut sys::JSContext,
    value: sys::JSValue,
) -> Result<Value> {
    if js_value_is_undefined(value) {
        return Ok(Value::Null);
    }
    validate_json_boundary_value(ctx, value)?;
    let json = sys::JS_JSONStringify(ctx, value, js_undefined(), js_undefined());
    if js_value_is_exception(json) {
        return Err(QuickJsError::EvalFailed(exception_string(ctx)));
    }
    if js_value_is_undefined(json) {
        sys::JS_FreeValue(ctx, json);
        return Ok(Value::Null);
    }
    let raw = sys::JS_ToCStringLen2(ctx, std::ptr::null_mut(), json, false);
    if raw.is_null() {
        sys::JS_FreeValue(ctx, json);
        return Err(QuickJsError::EvalFailed(
            "JSON.stringify failed".to_string(),
        ));
    }
    let text = CStr::from_ptr(raw).to_string_lossy().into_owned();
    sys::JS_FreeCString(ctx, raw);
    sys::JS_FreeValue(ctx, json);
    serde_json::from_str(&text).map_err(|err| QuickJsError::EvalFailed(err.to_string()))
}

fn drain_jobs_for_runtime(rt: *mut sys::JSRuntime) -> Result<()> {
    loop {
        let mut ctx = std::ptr::null_mut();
        let status = unsafe { sys::JS_ExecutePendingJob(rt, &mut ctx) };
        if status > 0 {
            continue;
        }
        if status < 0 && !ctx.is_null() {
            return Err(QuickJsError::EvalFailed(exception_string(ctx)));
        }
        if status < 0 {
            return Err(QuickJsError::EvalFailed(
                "QuickJS pending job failed".to_string(),
            ));
        }
        return Ok(());
    }
}

fn json_to_js(ctx: *mut sys::JSContext, value: Value) -> Result<sys::JSValue> {
    let text = serde_json::to_string(&value)
        .map_err(|err| QuickJsError::EvalFailed(format!("failed to encode JSON value: {err}")))?;
    let text = CString::new(text).map_err(|_| QuickJsError::InteriorNul)?;
    let filename = CString::new("<host-json>").expect("static string has no NUL");
    let value =
        unsafe { sys::JS_ParseJSON(ctx, text.as_ptr(), text.as_bytes().len(), filename.as_ptr()) };
    if js_value_is_exception(value) {
        return Err(QuickJsError::EvalFailed(exception_string(ctx)));
    }
    Ok(value)
}

unsafe fn callback_arg(argc: i32, argv: *mut sys::JSValue, index: usize) -> Option<sys::JSValue> {
    if argv.is_null() || index >= usize::try_from(argc).ok()? {
        return None;
    }
    Some(*argv.add(index))
}

unsafe fn js_value_to_string(ctx: *mut sys::JSContext, value: sys::JSValue) -> Result<String> {
    let raw = sys::JS_ToCStringLen2(ctx, std::ptr::null_mut(), value, false);
    if raw.is_null() {
        return Err(QuickJsError::EvalFailed(exception_string(ctx)));
    }
    let text = CStr::from_ptr(raw).to_string_lossy().into_owned();
    sys::JS_FreeCString(ctx, raw);
    Ok(text)
}

fn string_to_js(ctx: *mut sys::JSContext, value: &str) -> Result<sys::JSValue> {
    let value = CString::new(value).map_err(|_| QuickJsError::InteriorNul)?;
    let js = unsafe { sys::JS_NewStringLen(ctx, value.as_ptr(), value.as_bytes().len()) };
    if js_value_is_exception(js) {
        return Err(QuickJsError::EvalFailed(exception_string(ctx)));
    }
    Ok(js)
}

/// Read `obj[prop]` as a string, returning `None` when it is absent,
/// `undefined`, or unreadable. Does not consume `obj`.
unsafe fn read_property_string(
    ctx: *mut sys::JSContext,
    obj: sys::JSValue,
    prop: &str,
) -> Option<String> {
    let prop = CString::new(prop).ok()?;
    let value = sys::JS_GetPropertyStr(ctx, obj, prop.as_ptr());
    if value.tag == JS_TAG_EXCEPTION || value.tag == JS_TAG_UNDEFINED {
        sys::JS_FreeValue(ctx, value);
        return None;
    }
    let text = js_value_to_string(ctx, value).ok();
    sys::JS_FreeValue(ctx, value);
    text
}

/// Best-effort read of `obj.constructor.name`, the fallback identity for a
/// thrown value whose own `name` property is absent. Does not consume `obj`.
unsafe fn read_constructor_name(ctx: *mut sys::JSContext, obj: sys::JSValue) -> Option<String> {
    let constructor = CString::new("constructor").ok()?;
    let ctor = sys::JS_GetPropertyStr(ctx, obj, constructor.as_ptr());
    if ctor.tag == JS_TAG_EXCEPTION || ctor.tag == JS_TAG_UNDEFINED {
        sys::JS_FreeValue(ctx, ctor);
        return None;
    }
    let name = read_property_string(ctx, ctor, "name");
    sys::JS_FreeValue(ctx, ctor);
    name
}

fn exception_string(ctx: *mut sys::JSContext) -> String {
    unsafe {
        let exception = sys::JS_GetException(ctx);
        let message = sys::JS_ToCStringLen2(ctx, std::ptr::null_mut(), exception, false);
        let out = if message.is_null() {
            "unknown exception".to_string()
        } else {
            CStr::from_ptr(message).to_string_lossy().into_owned()
        };
        if !message.is_null() {
            sys::JS_FreeCString(ctx, message);
        }
        sys::JS_FreeValue(ctx, exception);
        out
    }
}

fn js_value_is_exception(value: sys::JSValue) -> bool {
    value.tag == JS_TAG_EXCEPTION
}

fn js_value_is_undefined(value: sys::JSValue) -> bool {
    value.tag == JS_TAG_UNDEFINED
}

fn js_undefined() -> sys::JSValue {
    sys::JSValue {
        u: sys::JSValueUnion { int32: 0 },
        tag: JS_TAG_UNDEFINED,
    }
}

fn module_facade_source(source: &str) -> String {
    let mut out =
        String::from("globalThis.__chidori_exports = globalThis.__chidori_exports || {};\n");
    for line in source.lines() {
        let trimmed = line.trim_start();
        let prefix = &line[..line.len() - trimmed.len()];
        if let Some(rest) = trimmed.strip_prefix("export async function ") {
            let name = export_name_before_paren(rest);
            out.push_str(prefix);
            out.push_str("globalThis.__chidori_exports.");
            out.push_str(name);
            out.push_str(" = async function ");
            out.push_str(rest);
        } else if let Some(rest) = trimmed.strip_prefix("export function ") {
            let name = export_name_before_paren(rest);
            out.push_str(prefix);
            out.push_str("globalThis.__chidori_exports.");
            out.push_str(name);
            out.push_str(" = function ");
            out.push_str(rest);
        } else if let Some(rest) = trimmed.strip_prefix("export const ") {
            append_exported_binding(&mut out, prefix, rest);
        } else if let Some(rest) = trimmed.strip_prefix("export let ") {
            append_exported_binding(&mut out, prefix, rest);
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

fn append_exported_binding(out: &mut String, prefix: &str, rest: &str) {
    let name_end = rest
        .find(|c: char| c.is_whitespace() || c == '=')
        .unwrap_or(rest.len());
    let name = &rest[..name_end];
    let rhs = rest[name_end..].trim_start();
    out.push_str(prefix);
    out.push_str("globalThis.__chidori_exports.");
    out.push_str(name);
    out.push(' ');
    out.push_str(rhs);
}

fn export_name_before_paren(rest: &str) -> &str {
    rest.find('(')
        .map(|idx| rest[..idx].trim())
        .unwrap_or_else(|| rest.trim())
}

const JS_TAG_EXCEPTION: sys::JSValueTag = 6;
const JS_TAG_MODULE: sys::JSValueTag = -3;
const JS_TAG_UNDEFINED: sys::JSValueTag = 3;
const JS_CFUNC_GENERIC: i32 = 0;
const JS_EVAL_TYPE_GLOBAL: i32 = 0;
const JS_EVAL_TYPE_MODULE: i32 = 1 << 0;
const JS_EVAL_FLAG_STRICT: i32 = 1 << 3;
const JS_EVAL_FLAG_COMPILE_ONLY: i32 = 1 << 5;
const JS_WRITE_OBJ_BYTECODE: i32 = 1 << 0;
const JS_WRITE_OBJ_REFERENCE: i32 = 1 << 3;
const JS_READ_OBJ_BYTECODE: i32 = 1 << 0;
const JS_READ_OBJ_REFERENCE: i32 = 1 << 3;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_runtime_entrypoint_writes_runtime_payload() {
        let mut runtime = SnapshotRuntime::new(RuntimeLimits::default()).unwrap();
        let snapshot = runtime.snapshot().unwrap();

        assert_eq!(
            snapshot.payload().unwrap(),
            b"CHIDORI_QJS_RUNTIME_SNAPSHOT_V1"
        );
        assert_eq!(snapshot.context_payload().unwrap(), b"");
    }

    #[test]
    fn restore_rejects_invalid_runtime_payload() {
        assert_eq!(
            SnapshotRuntime::restore(&RuntimeSnapshot::from_payload(b"not-a-real-snapshot").0)
                .unwrap_err(),
            QuickJsError::SnapshotUnsupported
        );
    }

    #[test]
    fn restore_rejects_invalid_runtime_snapshot_envelope() {
        assert!(matches!(
            SnapshotRuntime::restore(b"not-a-real-snapshot").unwrap_err(),
            QuickJsError::InvalidSnapshot(_)
        ));
    }

    #[test]
    fn restore_rejects_empty_runtime_snapshot_payloads_before_fork_boundary() {
        assert!(matches!(
            SnapshotRuntime::restore(&RuntimeSnapshot::from_parts(b"", b"context-payload").0)
                .unwrap_err(),
            QuickJsError::InvalidSnapshot(message)
                if message.contains("runtime payload is empty")
        ));
        assert!(matches!(
            SnapshotRuntime::restore(&RuntimeSnapshot::from_parts(b"runtime-payload", b"").0)
                .unwrap_err(),
            QuickJsError::InvalidSnapshot(message)
                if message.contains("context payload is empty")
        ));
    }

    #[test]
    fn runtime_snapshot_envelope_round_trips_payload() {
        let snapshot = RuntimeSnapshot::from_payload(b"fork-payload");

        assert_eq!(snapshot.payload().unwrap(), b"fork-payload");
        assert_eq!(snapshot.context_payload().unwrap(), b"fork-payload");
        assert!(snapshot.0.starts_with(RUNTIME_SNAPSHOT_MAGIC));
    }

    #[test]
    fn runtime_snapshot_envelope_round_trips_split_payloads() {
        let snapshot = RuntimeSnapshot::from_parts(b"runtime-payload", b"context-payload");

        assert_eq!(snapshot.payload().unwrap(), b"runtime-payload");
        assert_eq!(snapshot.context_payload().unwrap(), b"context-payload");
    }

    #[test]
    fn runtime_snapshot_envelope_rejects_trailing_bytes() {
        let mut bytes = RuntimeSnapshot::from_payload(b"fork-payload").0;
        bytes.push(0);

        assert!(matches!(
            RuntimeSnapshot(bytes).payload().unwrap_err(),
            QuickJsError::InvalidSnapshot(_)
        ));
    }

    #[test]
    fn runtime_snapshot_ensure_restorable_rejects_empty_payload_sections() {
        assert!(matches!(
            RuntimeSnapshot::from_parts(b"", b"context-payload")
                .ensure_restorable()
                .unwrap_err(),
            QuickJsError::InvalidSnapshot(message)
                if message.contains("runtime payload is empty")
        ));
        assert!(matches!(
            RuntimeSnapshot::from_parts(b"runtime-payload", b"")
                .ensure_restorable()
                .unwrap_err(),
            QuickJsError::InvalidSnapshot(message)
                if message.contains("context payload is empty")
        ));
        RuntimeSnapshot::from_parts(b"runtime-payload", b"context-payload")
            .ensure_restorable()
            .unwrap();
    }

    #[test]
    fn runtime_level_host_promise_resolution_requires_restored_context() {
        let mut runtime = SnapshotRuntime::new(RuntimeLimits::default()).unwrap();

        assert_eq!(
            runtime
                .resolve_host_promise(HostPromiseId(7), serde_json::json!({ "ok": true }))
                .unwrap_err(),
            QuickJsError::RestoredContextUnavailable
        );
        assert_eq!(
            runtime
                .reject_host_promise(HostPromiseId(7), "failed".to_string())
                .unwrap_err(),
            QuickJsError::RestoredContextUnavailable
        );
    }

    #[test]
    fn snapshot_unsupported_error_preserves_context_detail() {
        let err = snapshot_unsupported_error(vec![SnapshotUnsupportedReport {
            path: "$context".to_string(),
            type_name: "context".to_string(),
            message: "context snapshot serialization is not implemented".to_string(),
        }]);

        assert!(matches!(
            err,
            QuickJsError::SnapshotUnsupportedDetail {
                path,
                type_name,
                message,
            } if path == "$context"
                && type_name == "context"
                && message.contains("context snapshot serialization is not implemented")
        ));
    }

    #[test]
    fn runtime_without_restored_context_snapshots_empty_context_payload() {
        let mut runtime = SnapshotRuntime::new(RuntimeLimits::default()).unwrap();

        assert_eq!(runtime.snapshot_restored_context_payload().unwrap(), b"");
    }

    #[test]
    fn snapshot_writer_callback_appends_bytes() {
        let mut state = SnapshotWriterState { bytes: Vec::new() };
        let first = b"runtime";
        let second = b"-snapshot";

        unsafe {
            assert_eq!(
                snapshot_writer_write(
                    (&mut state as *mut SnapshotWriterState).cast::<c_void>(),
                    first.as_ptr(),
                    first.len(),
                ),
                0
            );
            assert_eq!(
                snapshot_writer_write(
                    (&mut state as *mut SnapshotWriterState).cast::<c_void>(),
                    second.as_ptr(),
                    second.len(),
                ),
                0
            );
        }

        assert_eq!(state.bytes, b"runtime-snapshot");
    }

    #[test]
    fn snapshot_reader_callback_reads_sequential_bytes() {
        let bytes = b"abcdef";
        let mut state = SnapshotReaderState {
            bytes: bytes.as_ptr(),
            len: bytes.len(),
            offset: 0,
        };
        let mut first = [0; 2];
        let mut second = [0; 4];

        unsafe {
            assert_eq!(
                snapshot_reader_read(
                    (&mut state as *mut SnapshotReaderState).cast::<c_void>(),
                    first.as_mut_ptr(),
                    first.len(),
                ),
                0
            );
            assert_eq!(
                snapshot_reader_read(
                    (&mut state as *mut SnapshotReaderState).cast::<c_void>(),
                    second.as_mut_ptr(),
                    second.len(),
                ),
                0
            );
        }

        assert_eq!(&first, b"ab");
        assert_eq!(&second, b"cdef");
        assert_eq!(state.offset, bytes.len());
    }

    #[test]
    fn snapshot_reader_callback_rejects_overread() {
        let bytes = b"abc";
        let mut state = SnapshotReaderState {
            bytes: bytes.as_ptr(),
            len: bytes.len(),
            offset: 0,
        };
        let mut out = [0; 4];

        let status = unsafe {
            snapshot_reader_read(
                (&mut state as *mut SnapshotReaderState).cast::<c_void>(),
                out.as_mut_ptr(),
                out.len(),
            )
        };

        assert_eq!(status, -1);
        assert_eq!(state.offset, 0);
    }

    #[test]
    fn runtime_limits_parse_env_values_and_reject_zero() {
        let limits = RuntimeLimits::from_env_values(Some("1048576"), Some("1234")).unwrap();
        assert_eq!(limits.memory_limit_bytes, 1_048_576);
        assert_eq!(limits.interrupt_budget, 1_234);

        assert!(matches!(
            RuntimeLimits::from_env_values(Some("0"), Some("1")).unwrap_err(),
            QuickJsError::InvalidRuntimeLimits(_)
        ));
        assert!(matches!(
            RuntimeLimits::from_env_values(Some("1"), Some("0")).unwrap_err(),
            QuickJsError::InvalidRuntimeLimits(_)
        ));
        assert!(matches!(
            RuntimeLimits::from_env_values(Some("nope"), Some("1")).unwrap_err(),
            QuickJsError::InvalidRuntimeLimits(_)
        ));
    }

    #[test]
    fn snapshot_runtime_allocates_real_quickjs_runtime_with_limits() {
        let limits = RuntimeLimits {
            memory_limit_bytes: 4 * 1024 * 1024,
            interrupt_budget: 1024,
        };
        let runtime = SnapshotRuntime::new(limits.clone()).unwrap();

        assert!(!runtime.raw_runtime().is_null());
        assert_eq!(runtime.limits(), &limits);
    }

    #[test]
    fn snapshot_runtime_run_jobs_until_blocked_returns_when_idle() {
        let mut runtime = SnapshotRuntime::new(RuntimeLimits::default()).unwrap();

        assert_eq!(
            runtime.run_jobs_until_blocked().unwrap(),
            RunState::Completed(serde_json::Value::Null)
        );
    }

    #[test]
    fn snapshot_context_evaluates_quickjs_module_source() {
        let runtime = SnapshotRuntime::new(RuntimeLimits::default()).unwrap();
        let mut context = runtime.new_context().unwrap();

        context
            .eval_module("module.mjs", "export const value = 42;")
            .unwrap();

        assert!(!context.raw_context().is_null());
    }

    unsafe extern "C" fn native_context_label(
        ctx: *mut sys::JSContext,
        _this_val: sys::JSValue,
        _argc: std::ffi::c_int,
        _argv: *mut sys::JSValue,
    ) -> sys::JSValue {
        let opaque = sys::JS_GetContextOpaque(ctx);
        if opaque.is_null() {
            return string_to_js(ctx, "missing context opaque").unwrap_or_else(|_| js_undefined());
        }
        let label = &*opaque.cast::<String>();
        string_to_js(ctx, label).unwrap_or_else(|_| js_undefined())
    }

    unsafe extern "C" fn native_record_log(
        ctx: *mut sys::JSContext,
        _this_val: sys::JSValue,
        argc: std::ffi::c_int,
        argv: *mut sys::JSValue,
    ) -> sys::JSValue {
        let Some(calls) = context_opaque_mut::<Vec<String>>(ctx) else {
            return throw_string(ctx, "missing native callback state");
        };
        match callback_arg_to_string(ctx, argc, argv, 0) {
            Ok(message) => {
                calls.push(message);
                js_undefined()
            }
            Err(err) => throw_string(ctx, &err.to_string()),
        }
    }

    unsafe extern "C" fn native_echo_json(
        ctx: *mut sys::JSContext,
        _this_val: sys::JSValue,
        argc: std::ffi::c_int,
        argv: *mut sys::JSValue,
    ) -> sys::JSValue {
        match callback_arg_to_string(ctx, argc, argv, 0)
            .and_then(|message| json_to_js_value(ctx, serde_json::json!({ "message": message })))
        {
            Ok(value) => value,
            Err(err) => throw_string(ctx, &err.to_string()),
        }
    }

    unsafe extern "C" fn native_wrap_json_arg(
        ctx: *mut sys::JSContext,
        _this_val: sys::JSValue,
        argc: std::ffi::c_int,
        argv: *mut sys::JSValue,
    ) -> sys::JSValue {
        match callback_arg_to_json(ctx, argc, argv, 0)
            .and_then(|value| json_to_js_value(ctx, serde_json::json!({ "value": value })))
        {
            Ok(value) => value,
            Err(err) => throw_string(ctx, &err.to_string()),
        }
    }

    #[test]
    fn snapshot_context_installs_native_rust_callback_with_context_opaque() {
        let runtime = SnapshotRuntime::new(RuntimeLimits::default()).unwrap();
        let mut context = runtime.new_context().unwrap();
        let label = "from-rust".to_string();
        unsafe {
            context.set_context_opaque((&label as *const String).cast_mut().cast());
        }
        context
            .install_global_native_function("nativeContextLabel", Some(native_context_label), 0)
            .unwrap();

        assert_eq!(
            context
                .eval_json_expression("native-callback.js", "globalThis.nativeContextLabel()")
                .unwrap(),
            serde_json::json!("from-rust")
        );

        unsafe {
            context.set_context_opaque(std::ptr::null_mut());
        }
    }

    #[test]
    fn snapshot_context_installs_native_rust_callback_on_global_object() {
        let runtime = SnapshotRuntime::new(RuntimeLimits::default()).unwrap();
        let mut context = runtime.new_context().unwrap();
        let label = "from-chidori".to_string();
        unsafe {
            context.set_context_opaque((&label as *const String).cast_mut().cast());
        }
        context
            .install_global_object_native_function(
                "chidori",
                "nativeContextLabel",
                Some(native_context_label),
                0,
            )
            .unwrap();

        assert_eq!(
            context
                .eval_json_expression(
                    "native-object-callback.js",
                    "globalThis.chidori.nativeContextLabel()"
                )
                .unwrap(),
            serde_json::json!("from-chidori")
        );

        unsafe {
            context.set_context_opaque(std::ptr::null_mut());
        }
    }

    #[test]
    fn snapshot_context_native_object_callbacks_read_args_and_return_json() {
        let runtime = SnapshotRuntime::new(RuntimeLimits::default()).unwrap();
        let mut calls = Vec::new();
        let mut context = runtime.new_context().unwrap();
        unsafe {
            context.set_context_opaque((&mut calls as *mut Vec<String>).cast());
        }
        context
            .install_global_object_native_function("chidori", "log", Some(native_record_log), 1)
            .unwrap();
        context
            .install_global_object_native_function("chidori", "echo", Some(native_echo_json), 1)
            .unwrap();
        context
            .install_global_object_native_function("chidori", "wrap", Some(native_wrap_json_arg), 1)
            .unwrap();

        assert_eq!(
            context
                .eval_json_expression(
                    "native-chidori-object-callbacks.js",
                    r#"
                    (globalThis.chidori.log("hello"),
                     globalThis.chidori.echo("world"))
                    "#,
                )
                .unwrap(),
            serde_json::json!({ "message": "world" })
        );
        assert_eq!(
            context
                .eval_json_expression(
                    "native-chidori-json-callback.js",
                    r#"globalThis.chidori.wrap({ nested: ["ok", 3] })"#,
                )
                .unwrap(),
            serde_json::json!({ "value": { "nested": ["ok", 3] } })
        );
        assert_eq!(calls, vec!["hello".to_string()]);

        unsafe {
            context.set_context_opaque(std::ptr::null_mut());
        }
    }

    #[test]
    fn snapshot_context_calls_export_with_json_round_trip() {
        let runtime = SnapshotRuntime::new(RuntimeLimits::default()).unwrap();
        let mut context = runtime.new_context().unwrap();
        context
            .eval_module(
                "agent.mjs",
                r#"
                export function agent(input) {
                    return { greeting: "Hello, " + input.name, count: input.count + 1 };
                }
                "#,
            )
            .unwrap();

        let result = context
            .call_export_json(
                "agent",
                serde_json::json!({ "name": "Chidori", "count": 2 }),
            )
            .unwrap();

        assert_eq!(
            result,
            RunState::Completed(serde_json::json!({ "greeting": "Hello, Chidori", "count": 3 }))
        );
    }

    #[test]
    fn snapshot_context_rejects_function_export_result_at_json_boundary() {
        let runtime = SnapshotRuntime::new(RuntimeLimits::default()).unwrap();
        let mut context = runtime.new_context().unwrap();
        context
            .eval_module(
                "agent.mjs",
                r#"
                export function agent() {
                    return { value: function unsupported() {} };
                }
                "#,
            )
            .unwrap();

        let err = context
            .call_export_json("agent", serde_json::json!({}))
            .unwrap_err();

        assert!(err
            .to_string()
            .contains("functions and unsupported native values"));
    }

    #[test]
    fn snapshot_context_rejects_class_export_result_at_json_boundary() {
        let runtime = SnapshotRuntime::new(RuntimeLimits::default()).unwrap();
        let mut context = runtime.new_context().unwrap();
        context
            .eval_module(
                "agent.mjs",
                r#"
                class ExternalValue {
                    constructor() {
                        this.ok = true;
                    }
                }
                export function agent() {
                    return { value: new ExternalValue() };
                }
                "#,
            )
            .unwrap();

        let err = context
            .call_export_json("agent", serde_json::json!({}))
            .unwrap_err();

        assert!(err.to_string().contains("unsupported external class"));
    }

    #[test]
    fn snapshot_context_calls_async_export_after_draining_jobs() {
        let runtime = SnapshotRuntime::new(RuntimeLimits::default()).unwrap();
        let mut context = runtime.new_context().unwrap();
        context
            .eval_module(
                "agent.mjs",
                r#"
                export async function agent(input) {
                    return { value: input.value + 1 };
                }
                "#,
            )
            .unwrap();

        let result = context
            .call_export_json("agent", serde_json::json!({ "value": 41 }))
            .unwrap();

        assert_eq!(
            result,
            RunState::Completed(serde_json::json!({ "value": 42 }))
        );
    }

    #[test]
    fn snapshot_context_round_trips_primitive_json_value() {
        let runtime = SnapshotRuntime::new(RuntimeLimits::default()).unwrap();
        let mut context = runtime.new_context().unwrap();
        for value in [
            serde_json::json!("chidori"),
            serde_json::json!(42),
            serde_json::json!(true),
            serde_json::Value::Null,
        ] {
            let snapshot = context.snapshot_json_value(value.clone()).unwrap();
            assert!(!snapshot.is_empty());

            let restored = context.restore_json_value(&snapshot).unwrap();
            assert_eq!(restored, value);
        }
    }

    #[test]
    fn snapshot_context_round_trips_nested_json_object() {
        let runtime = SnapshotRuntime::new(RuntimeLimits::default()).unwrap();
        let mut context = runtime.new_context().unwrap();
        let value = serde_json::json!({
            "items": [
                { "name": "first", "count": 1 },
                { "name": "second", "count": 2 }
            ],
            "enabled": true,
            "meta": null
        });

        let snapshot = context.snapshot_json_value(value.clone()).unwrap();
        assert!(!snapshot.is_empty());

        let restored = context.restore_json_value(&snapshot).unwrap();
        assert_eq!(restored, value);
    }

    #[test]
    fn snapshot_context_round_trips_json_array() {
        let runtime = SnapshotRuntime::new(RuntimeLimits::default()).unwrap();
        let mut context = runtime.new_context().unwrap();
        let value = serde_json::json!([1, true, null, { "name": "array" }]);

        let snapshot = context.snapshot_json_value(value.clone()).unwrap();
        assert!(!snapshot.is_empty());

        let restored = context.restore_json_value(&snapshot).unwrap();
        assert_eq!(restored, value);
    }

    #[test]
    fn snapshot_context_round_trips_sparse_array_shape() {
        let runtime = SnapshotRuntime::new(RuntimeLimits::default()).unwrap();
        let mut context = runtime.new_context().unwrap();

        let snapshot = context
            .snapshot_expression(
                "sparse-array.js",
                "(() => { const arr = [1,,3]; arr.extra = 4; return arr; })()",
            )
            .unwrap();
        context
            .restore_snapshot_to_global("__chidori_restored", &snapshot)
            .unwrap();

        assert_eq!(
            context
                .eval_json_expression(
                    "sparse-array-check.js",
                    "({ length: globalThis.__chidori_restored.length, hasHole: !(1 in globalThis.__chidori_restored), values: [globalThis.__chidori_restored[0], globalThis.__chidori_restored[2]], extra: globalThis.__chidori_restored.extra })",
                )
                .unwrap(),
            serde_json::json!({ "length": 3, "hasHole": true, "values": [1, 3], "extra": 4 })
        );
    }

    #[test]
    fn snapshot_context_preserves_object_identity() {
        let runtime = SnapshotRuntime::new(RuntimeLimits::default()).unwrap();
        let mut context = runtime.new_context().unwrap();

        let snapshot = context
            .snapshot_expression(
                "identity.js",
                "(() => { const shared = { marker: 1 }; return { a: shared, b: shared }; })()",
            )
            .unwrap();
        context
            .restore_snapshot_to_global("__chidori_restored", &snapshot)
            .unwrap();

        assert_eq!(
            context
                .eval_json_expression(
                    "identity-check.js",
                    "globalThis.__chidori_restored.a === globalThis.__chidori_restored.b",
                )
                .unwrap(),
            serde_json::json!(true)
        );
    }

    #[test]
    fn snapshot_context_round_trips_cyclic_object_graph() {
        let runtime = SnapshotRuntime::new(RuntimeLimits::default()).unwrap();
        let mut context = runtime.new_context().unwrap();

        let snapshot = context
            .snapshot_expression(
                "cycle.js",
                "(() => { const obj = { name: 'cycle' }; obj.self = obj; return obj; })()",
            )
            .unwrap();
        context
            .restore_snapshot_to_global("__chidori_restored", &snapshot)
            .unwrap();

        assert_eq!(
            context
                .eval_json_expression(
                    "cycle-check.js",
                    "globalThis.__chidori_restored.self === globalThis.__chidori_restored",
                )
                .unwrap(),
            serde_json::json!(true)
        );
    }

    #[test]
    fn snapshot_context_round_trips_typed_array() {
        let runtime = SnapshotRuntime::new(RuntimeLimits::default()).unwrap();
        let mut context = runtime.new_context().unwrap();

        let snapshot = context
            .snapshot_expression("typed-array.js", "new Uint8Array([1, 2, 3])")
            .unwrap();
        context
            .restore_snapshot_to_global("__chidori_restored", &snapshot)
            .unwrap();

        assert_eq!(
            context
                .eval_json_expression(
                    "typed-array-check.js",
                    "({ isTypedArray: globalThis.__chidori_restored instanceof Uint8Array, values: Array.from(globalThis.__chidori_restored) })",
                )
                .unwrap(),
            serde_json::json!({ "isTypedArray": true, "values": [1, 2, 3] })
        );
    }

    #[test]
    fn snapshot_context_restores_closure_locals() {
        let runtime = SnapshotRuntime::new(RuntimeLimits::default()).unwrap();
        let mut context = runtime.new_context().unwrap();

        let snapshot = context
            .snapshot_expression(
                "closure-locals.js",
                "(() => { let count = 40; return () => ++count; })()",
            )
            .unwrap();
        context
            .restore_snapshot_to_global("__chidori_restored", &snapshot)
            .unwrap();

        assert_eq!(
            context
                .eval_json_expression(
                    "closure-calls.js",
                    "[globalThis.__chidori_restored(), globalThis.__chidori_restored()]",
                )
                .unwrap(),
            serde_json::json!([41, 42])
        );
    }

    #[test]
    fn snapshot_context_preserves_shared_closure_environment() {
        let runtime = SnapshotRuntime::new(RuntimeLimits::default()).unwrap();
        let mut context = runtime.new_context().unwrap();

        let snapshot = context
            .snapshot_expression(
                "shared-closure-env.js",
                "(() => { let count = 40; return { inc: () => ++count, get: () => count }; })()",
            )
            .unwrap();
        context
            .restore_snapshot_to_global("__chidori_restored", &snapshot)
            .unwrap();

        assert_eq!(
            context
                .eval_json_expression(
                    "shared-closure-env-check.js",
                    "[globalThis.__chidori_restored.get(), globalThis.__chidori_restored.inc(), globalThis.__chidori_restored.get()]",
                )
                .unwrap(),
            serde_json::json!([40, 41, 41])
        );
    }

    #[test]
    fn snapshot_context_round_trips_fulfilled_promise_state() {
        let runtime = SnapshotRuntime::new(RuntimeLimits::default()).unwrap();
        let mut context = runtime.new_context().unwrap();

        let snapshot = context
            .snapshot_expression("fulfilled-promise.js", "Promise.resolve({ ok: true })")
            .unwrap();
        context
            .restore_snapshot_to_global("__chidori_restored", &snapshot)
            .unwrap();

        assert_eq!(
            context
                .eval_json_expression(
                    "fulfilled-promise-instance.js",
                    "globalThis.__chidori_restored instanceof Promise",
                )
                .unwrap(),
            serde_json::json!(true)
        );

        context
            .eval(
                "fulfilled-promise-then.js",
                "globalThis.__promise_value = null; globalThis.__chidori_restored.then(value => { globalThis.__promise_value = value; });",
                JS_EVAL_TYPE_GLOBAL,
            )
            .unwrap();
        context.drain_jobs().unwrap();

        assert_eq!(
            context.global_json("__promise_value").unwrap(),
            Some(serde_json::json!({ "ok": true }))
        );
    }

    #[test]
    fn snapshot_context_round_trips_rejected_promise_state() {
        let runtime = SnapshotRuntime::new(RuntimeLimits::default()).unwrap();
        let mut context = runtime.new_context().unwrap();

        let snapshot = context
            .snapshot_expression("rejected-promise.js", "Promise.reject('host failed')")
            .unwrap();
        context
            .restore_snapshot_to_global("__chidori_restored", &snapshot)
            .unwrap();

        context
            .eval(
                "rejected-promise-catch.js",
                "globalThis.__promise_error = null; globalThis.__chidori_restored.catch(error => { globalThis.__promise_error = error; });",
                JS_EVAL_TYPE_GLOBAL,
            )
            .unwrap();
        context.drain_jobs().unwrap();

        assert_eq!(
            context.global_json("__promise_error").unwrap(),
            Some(serde_json::json!("host failed"))
        );
    }

    #[test]
    fn snapshot_context_round_trips_pending_promise_state() {
        let runtime = SnapshotRuntime::new(RuntimeLimits::default()).unwrap();
        let mut context = runtime.new_context().unwrap();

        let snapshot = context
            .snapshot_expression("pending-promise.js", "new Promise(() => {})")
            .unwrap();
        context
            .restore_snapshot_to_global("__chidori_restored", &snapshot)
            .unwrap();

        assert_eq!(
            context
                .eval_json_expression(
                    "pending-promise-check.js",
                    "globalThis.__chidori_restored instanceof Promise",
                )
                .unwrap(),
            serde_json::json!(true)
        );
    }

    #[test]
    fn snapshot_context_restores_host_promise_and_resolves_it() {
        let id = HostPromiseId(44);
        let snapshot = {
            let runtime = SnapshotRuntime::new(RuntimeLimits::default()).unwrap();
            let mut context = runtime.new_context().unwrap();
            let promise = context.new_host_promise(id).unwrap().raw();
            let property = CString::new("__chidori_host_promise").unwrap();
            unsafe {
                let global = sys::JS_GetGlobalObject(context.ctx.as_ptr());
                let status = sys::JS_SetPropertyStr(
                    context.ctx.as_ptr(),
                    global,
                    property.as_ptr(),
                    sys::JS_DupValue(context.ctx.as_ptr(), promise),
                );
                sys::JS_FreeValue(context.ctx.as_ptr(), global);
                assert!(status >= 0);
            }
            context
                .snapshot_expression("host-promise.js", "globalThis.__chidori_host_promise")
                .unwrap()
        };

        let runtime = SnapshotRuntime::new(RuntimeLimits::default()).unwrap();
        let mut context = runtime.new_context().unwrap();
        context
            .restore_snapshot_to_global("__chidori_restored", &snapshot)
            .unwrap();
        context
            .eval(
                "host-promise-then.js",
                "globalThis.__host_value = null; globalThis.__chidori_restored.then(value => { globalThis.__host_value = value; });",
                JS_EVAL_TYPE_GLOBAL,
            )
            .unwrap();

        context
            .resolve_host_promise(id, serde_json::json!({ "ok": true }))
            .unwrap();

        assert_eq!(
            context.global_json("__host_value").unwrap(),
            Some(serde_json::json!({ "ok": true }))
        );
    }

    #[test]
    fn snapshot_context_restores_host_promise_and_rejects_it() {
        let id = HostPromiseId(45);
        let snapshot = {
            let runtime = SnapshotRuntime::new(RuntimeLimits::default()).unwrap();
            let mut context = runtime.new_context().unwrap();
            let promise = context.new_host_promise(id).unwrap().raw();
            let property = CString::new("__chidori_host_promise").unwrap();
            unsafe {
                let global = sys::JS_GetGlobalObject(context.ctx.as_ptr());
                let status = sys::JS_SetPropertyStr(
                    context.ctx.as_ptr(),
                    global,
                    property.as_ptr(),
                    sys::JS_DupValue(context.ctx.as_ptr(), promise),
                );
                sys::JS_FreeValue(context.ctx.as_ptr(), global);
                assert!(status >= 0);
            }
            context
                .snapshot_expression("host-promise.js", "globalThis.__chidori_host_promise")
                .unwrap()
        };

        let runtime = SnapshotRuntime::new(RuntimeLimits::default()).unwrap();
        let mut context = runtime.new_context().unwrap();
        context
            .restore_snapshot_to_global("__chidori_restored", &snapshot)
            .unwrap();
        context
            .eval(
                "host-promise-catch.js",
                "globalThis.__host_error = null; globalThis.__chidori_restored.catch(error => { globalThis.__host_error = error; });",
                JS_EVAL_TYPE_GLOBAL,
            )
            .unwrap();

        context
            .reject_host_promise(id, "host failed".to_string())
            .unwrap();

        assert_eq!(
            context.global_json("__host_error").unwrap(),
            Some(serde_json::json!("host failed"))
        );
    }

    #[test]
    fn snapshot_context_restores_host_promise_reaction_handler() {
        let id = HostPromiseId(46);
        let snapshot = {
            let runtime = SnapshotRuntime::new(RuntimeLimits::default()).unwrap();
            let mut context = runtime.new_context().unwrap();
            let promise = context.new_host_promise(id).unwrap().raw();
            let property = CString::new("__chidori_host_promise").unwrap();
            unsafe {
                let global = sys::JS_GetGlobalObject(context.ctx.as_ptr());
                let status = sys::JS_SetPropertyStr(
                    context.ctx.as_ptr(),
                    global,
                    property.as_ptr(),
                    sys::JS_DupValue(context.ctx.as_ptr(), promise),
                );
                sys::JS_FreeValue(context.ctx.as_ptr(), global);
                assert!(status >= 0);
            }
            context
                .eval(
                    "host-promise-reaction-setup.js",
                    "globalThis.__chidori_host_promise.then(value => { globalThis.__reaction_value = value.answer; });",
                    JS_EVAL_TYPE_GLOBAL,
                )
                .unwrap();
            context
                .snapshot_expression(
                    "host-promise-reaction.js",
                    "globalThis.__chidori_host_promise",
                )
                .unwrap()
        };

        let runtime = SnapshotRuntime::new(RuntimeLimits::default()).unwrap();
        let mut context = runtime.new_context().unwrap();
        context
            .restore_snapshot_to_global("__chidori_restored", &snapshot)
            .unwrap();
        context
            .resolve_host_promise(id, serde_json::json!({ "answer": 42 }))
            .unwrap();

        assert_eq!(
            context.global_json("__reaction_value").unwrap(),
            Some(serde_json::json!(42))
        );
    }

    #[test]
    fn snapshot_context_round_trips_array_buffer_contents() {
        let runtime = SnapshotRuntime::new(RuntimeLimits::default()).unwrap();
        let mut context = runtime.new_context().unwrap();

        let snapshot = context
            .snapshot_expression("array-buffer.js", "new Uint8Array([4, 5, 6]).buffer")
            .unwrap();
        context
            .restore_snapshot_to_global("__chidori_restored", &snapshot)
            .unwrap();

        assert_eq!(
            context
                .eval_json_expression(
                    "array-buffer-check.js",
                    "({ byteLength: globalThis.__chidori_restored.byteLength, values: Array.from(new Uint8Array(globalThis.__chidori_restored)) })",
                )
                .unwrap(),
            serde_json::json!({ "byteLength": 3, "values": [4, 5, 6] })
        );
    }

    #[test]
    fn snapshot_context_round_trips_bigint_value() {
        let runtime = SnapshotRuntime::new(RuntimeLimits::default()).unwrap();
        let mut context = runtime.new_context().unwrap();

        let snapshot = context
            .snapshot_expression("bigint.js", "12345678901234567890n")
            .unwrap();
        context
            .restore_snapshot_to_global("__chidori_restored", &snapshot)
            .unwrap();

        assert_eq!(
            context
                .eval_json_expression(
                    "bigint-check.js",
                    "({ type: typeof globalThis.__chidori_restored, value: String(globalThis.__chidori_restored) })",
                )
                .unwrap(),
            serde_json::json!({ "type": "bigint", "value": "12345678901234567890" })
        );
    }

    #[test]
    fn snapshot_context_round_trips_symbol_value() {
        let runtime = SnapshotRuntime::new(RuntimeLimits::default()).unwrap();
        let mut context = runtime.new_context().unwrap();

        let snapshot = context
            .snapshot_expression("symbol.js", "Symbol.for('chidori.snapshot')")
            .unwrap();
        context
            .restore_snapshot_to_global("__chidori_restored", &snapshot)
            .unwrap();

        assert_eq!(
            context
                .eval_json_expression(
                    "symbol-check.js",
                    "({ type: typeof globalThis.__chidori_restored, key: Symbol.keyFor(globalThis.__chidori_restored) })",
                )
                .unwrap(),
            serde_json::json!({ "type": "symbol", "key": "chidori.snapshot" })
        );
    }

    #[test]
    fn snapshot_context_round_trips_property_and_symbol_atoms() {
        let runtime = SnapshotRuntime::new(RuntimeLimits::default()).unwrap();
        let mut context = runtime.new_context().unwrap();

        let snapshot = context
            .snapshot_expression(
                "atoms.js",
                "(() => {
                    const symbol = Symbol.for('chidori.atom');
                    const obj = {
                        alpha: 1,
                        repeated_name: 2,
                        nested: { alpha: 3, repeated_name: 4 },
                    };
                    obj[symbol] = 5;
                    return obj;
                })()",
            )
            .unwrap();
        context
            .restore_snapshot_to_global("__chidori_restored", &snapshot)
            .unwrap();

        assert_eq!(
            context
                .eval_json_expression(
                    "atoms-check.js",
                    "(() => {
                        const restored = globalThis.__chidori_restored;
                        const symbol = Symbol.for('chidori.atom');
                        return {
                            keys: Object.keys(restored),
                            alpha: restored.alpha,
                            repeated: restored.repeated_name,
                            nestedAlpha: restored.nested.alpha,
                            nestedRepeated: restored.nested.repeated_name,
                            symbolValue: restored[symbol],
                        };
                    })()",
                )
                .unwrap(),
            serde_json::json!({
                "keys": ["alpha", "repeated_name", "nested"],
                "alpha": 1,
                "repeated": 2,
                "nestedAlpha": 3,
                "nestedRepeated": 4,
                "symbolValue": 5,
            })
        );
    }

    #[test]
    fn snapshot_context_round_trips_object_property_descriptors() {
        let runtime = SnapshotRuntime::new(RuntimeLimits::default()).unwrap();
        let mut context = runtime.new_context().unwrap();

        let snapshot = context
            .snapshot_expression(
                "descriptors.js",
                "(() => {
                    const obj = { visible: 1 };
                    Object.defineProperty(obj, 'hidden', {
                        value: 2,
                        enumerable: false,
                        writable: false,
                        configurable: false,
                    });
                    return obj;
                })()",
            )
            .unwrap();
        context
            .restore_snapshot_to_global("__chidori_restored", &snapshot)
            .unwrap();

        assert_eq!(
            context
                .eval_json_expression(
                    "descriptors-check.js",
                    "(() => {
                        const descriptor = Object.getOwnPropertyDescriptor(globalThis.__chidori_restored, 'hidden');
                        return {
                            keys: Object.keys(globalThis.__chidori_restored),
                            value: descriptor.value,
                            enumerable: descriptor.enumerable,
                            writable: descriptor.writable,
                            configurable: descriptor.configurable,
                        };
                    })()",
                )
                .unwrap(),
            serde_json::json!({
                "keys": ["visible"],
                "value": 2,
                "enumerable": false,
                "writable": false,
                "configurable": false,
            })
        );
    }

    #[test]
    fn snapshot_context_round_trips_map_and_set_values() {
        let runtime = SnapshotRuntime::new(RuntimeLimits::default()).unwrap();
        let mut context = runtime.new_context().unwrap();

        let snapshot = context
            .snapshot_expression(
                "map-set.js",
                "(() => { const key = { id: 1 }; return { map: new Map([[key, 'value']]), set: new Set([key]) }; })()",
            )
            .unwrap();
        context
            .restore_snapshot_to_global("__chidori_restored", &snapshot)
            .unwrap();

        assert_eq!(
            context
                .eval_json_expression(
                    "map-set-check.js",
                    "(() => { const mapKey = Array.from(globalThis.__chidori_restored.map.keys())[0]; const setValue = Array.from(globalThis.__chidori_restored.set.values())[0]; return { mapSize: globalThis.__chidori_restored.map.size, setSize: globalThis.__chidori_restored.set.size, value: globalThis.__chidori_restored.map.get(mapKey), sharedIdentity: mapKey === setValue }; })()",
                )
                .unwrap(),
            serde_json::json!({ "mapSize": 1, "setSize": 1, "value": "value", "sharedIdentity": true })
        );
    }

    #[test]
    fn snapshot_context_fuzzes_simple_json_object_graphs() {
        let runtime = SnapshotRuntime::new(RuntimeLimits::default()).unwrap();
        let mut context = runtime.new_context().unwrap();

        for seed in 0..64 {
            let value = generated_json_value(seed, 0);
            let snapshot = context.snapshot_json_value(value.clone()).unwrap();
            assert!(!snapshot.is_empty());
            let restored = context.restore_json_value(&snapshot).unwrap();
            assert_eq!(restored, value);
        }
    }

    #[test]
    fn snapshot_context_rejects_native_function_snapshot_clearly() {
        let runtime = SnapshotRuntime::new(RuntimeLimits::default()).unwrap();
        let mut context = runtime.new_context().unwrap();

        let err = context
            .snapshot_expression("native-function.js", "Math.max")
            .unwrap_err();

        assert!(
            matches!(err, QuickJsError::EvalFailed(message) if message.contains("unsupported object class"))
        );
    }

    #[test]
    fn snapshot_context_round_trips_global_function_bytecode() {
        let runtime = SnapshotRuntime::new(RuntimeLimits::default()).unwrap();
        let mut compiler = runtime.new_context().unwrap();

        let snapshot = compiler
            .snapshot_global_bytecode(
                "compiled-global.js",
                "function add(a, b) { return a + b; } add(20, 22);",
            )
            .unwrap();
        assert!(!snapshot.is_empty());

        let mut restored = runtime.new_context().unwrap();
        assert_eq!(
            restored.eval_bytecode_json(&snapshot).unwrap(),
            serde_json::json!(42)
        );
    }

    #[test]
    fn snapshot_context_round_trips_module_bytecode_side_effects() {
        let runtime = SnapshotRuntime::new(RuntimeLimits::default()).unwrap();
        let mut compiler = runtime.new_context().unwrap();

        let snapshot = compiler
            .snapshot_module_bytecode(
                "compiled-module.mjs",
                "export const value = 42; globalThis.__chidori_module_value = { value };",
            )
            .unwrap();
        assert!(!snapshot.is_empty());

        let mut restored = runtime.new_context().unwrap();
        let result = restored.eval_bytecode(&snapshot).unwrap();
        unsafe {
            sys::JS_FreeValue(restored.ctx.as_ptr(), result);
        }

        assert_eq!(
            restored.global_json("__chidori_module_value").unwrap(),
            Some(serde_json::json!({ "value": 42 }))
        );
    }

    #[test]
    fn snapshot_context_restores_module_bytecode_exports() {
        let runtime = SnapshotRuntime::new(RuntimeLimits::default()).unwrap();
        let mut compiler = runtime.new_context().unwrap();

        let snapshot = compiler
            .snapshot_module_bytecode(
                "compiled-exports.mjs",
                "export const value = 42; export const nested = { ok: true };",
            )
            .unwrap();
        assert!(!snapshot.is_empty());

        let mut restored = runtime.new_context().unwrap();
        assert_eq!(
            restored
                .eval_module_bytecode_namespace_json(&snapshot)
                .unwrap(),
            serde_json::json!({ "nested": { "ok": true }, "value": 42 })
        );
    }

    #[test]
    fn snapshot_context_restores_evaluated_module_namespace_exports() {
        let runtime = SnapshotRuntime::new(RuntimeLimits::default()).unwrap();
        let mut context = runtime.new_context().unwrap();

        let snapshot = context
            .snapshot_evaluated_module_namespace(
                "live-module.mjs",
                "export let count = 3; export function inc() { count += 1; return count; }",
            )
            .unwrap();
        context
            .restore_snapshot_to_global("__chidori_restored_module", &snapshot)
            .unwrap();

        assert_eq!(
            context
                .eval_json_expression(
                    "live-module-check.js",
                    "({ tag: Object.prototype.toString.call(globalThis.__chidori_restored_module), extensible: Object.isExtensible(globalThis.__chidori_restored_module), values: [globalThis.__chidori_restored_module.count, globalThis.__chidori_restored_module.inc(), globalThis.__chidori_restored_module.count] })",
                )
                .unwrap(),
            serde_json::json!({
                "tag": "[object Module]",
                "extensible": false,
                "values": [3, 4, 4],
            })
        );
    }

    #[test]
    fn snapshot_context_run_jobs_until_blocked_drains_microtasks() {
        let runtime = SnapshotRuntime::new(RuntimeLimits::default()).unwrap();
        let mut context = runtime.new_context().unwrap();
        context
            .eval_module(
                "jobs.mjs",
                r#"
                globalThis.__chidori_call_result = null;
                Promise.resolve().then(() => { globalThis.__chidori_call_result = { done: true }; });
                "#,
            )
            .unwrap();

        assert_eq!(
            context.run_jobs_until_blocked().unwrap(),
            RunState::Completed(serde_json::json!({ "done": true }))
        );
        assert_eq!(
            context.global_json("__chidori_call_result").unwrap(),
            Some(serde_json::json!({ "done": true }))
        );
    }

    #[test]
    fn snapshot_context_round_trips_microtask_queue_order() {
        let snapshot = {
            let runtime = SnapshotRuntime::new(RuntimeLimits::default()).unwrap();
            let mut context = runtime.new_context().unwrap();
            context
                .eval(
                    "microtask-queue-setup.js",
                    r#"
                    globalThis.__microtask_events = [];
                    queueMicrotask(() => { globalThis.__microtask_events.push("first"); });
                    queueMicrotask(() => { globalThis.__microtask_events.push("second"); });
                    "#,
                    JS_EVAL_TYPE_GLOBAL,
                )
                .unwrap();
            context.snapshot_microtask_queue().unwrap()
        };

        let runtime = SnapshotRuntime::new(RuntimeLimits::default()).unwrap();
        let mut context = runtime.new_context().unwrap();
        context
            .eval(
                "microtask-queue-restore-setup.js",
                "globalThis.__microtask_events = [];",
                JS_EVAL_TYPE_GLOBAL,
            )
            .unwrap();
        context.restore_microtask_queue(&snapshot).unwrap();
        context.drain_jobs().unwrap();

        assert_eq!(
            context.global_json("__microtask_events").unwrap(),
            Some(serde_json::json!(["first", "second"]))
        );
    }

    #[test]
    fn snapshot_context_round_trips_queued_promise_reaction_job() {
        let snapshot = {
            let runtime = SnapshotRuntime::new(RuntimeLimits::default()).unwrap();
            let mut context = runtime.new_context().unwrap();
            context
                .eval(
                    "promise-reaction-queue-setup.js",
                    r#"
                    globalThis.__promise_reaction_events = [];
                    Promise.resolve(41).then(value => {
                        globalThis.__promise_reaction_events.push(value + 1);
                    });
                    "#,
                    JS_EVAL_TYPE_GLOBAL,
                )
                .unwrap();
            context.snapshot_microtask_queue().unwrap()
        };

        let runtime = SnapshotRuntime::new(RuntimeLimits::default()).unwrap();
        let mut context = runtime.new_context().unwrap();
        context
            .eval(
                "promise-reaction-queue-restore-setup.js",
                "globalThis.__promise_reaction_events = [];",
                JS_EVAL_TYPE_GLOBAL,
            )
            .unwrap();
        context.restore_microtask_queue(&snapshot).unwrap();
        context.drain_jobs().unwrap();

        assert_eq!(
            context.global_json("__promise_reaction_events").unwrap(),
            Some(serde_json::json!([42]))
        );
    }

    #[test]
    fn snapshot_context_round_trips_named_globals_and_microtasks() {
        let snapshot = {
            let runtime = SnapshotRuntime::new(RuntimeLimits::default()).unwrap();
            let mut context = runtime.new_context().unwrap();
            context
                .eval(
                    "context-snapshot-setup.js",
                    r#"
                    globalThis.__shared_a = { count: 7 };
                    globalThis.__shared_b = globalThis.__shared_a;
                    globalThis.__events = [];
                    queueMicrotask(() => {
                        globalThis.__shared_a.count += 1;
                        globalThis.__events.push(
                            globalThis.__shared_a === globalThis.__shared_b
                                ? globalThis.__shared_a.count
                                : -1
                        );
                    });
                    "#,
                    JS_EVAL_TYPE_GLOBAL,
                )
                .unwrap();
            context
                .snapshot_globals(&["__shared_a", "__shared_b", "__events"])
                .unwrap()
        };

        let runtime = SnapshotRuntime::new(RuntimeLimits::default()).unwrap();
        let mut context = runtime.new_context().unwrap();
        let restored = context.restore_globals(&snapshot).unwrap();
        assert_eq!(restored, vec!["__shared_a", "__shared_b", "__events"]);
        context.drain_jobs().unwrap();

        assert_eq!(
            context
                .eval_json_expression(
                    "context-snapshot-check.js",
                    r#"({
                        same: globalThis.__shared_a === globalThis.__shared_b,
                        count: globalThis.__shared_a.count,
                        events: globalThis.__events
                    })"#,
                )
                .unwrap(),
            serde_json::json!({ "same": true, "count": 8, "events": [8] })
        );
    }

    #[test]
    fn snapshot_context_restores_host_promise_registry_from_named_globals() {
        let id = HostPromiseId(177);
        let snapshot = {
            let runtime = SnapshotRuntime::new(RuntimeLimits::default()).unwrap();
            let mut context = runtime.new_context().unwrap();
            let promise = context.new_host_promise(id).unwrap().raw();
            let property = CString::new("__host_promise").unwrap();
            unsafe {
                let global = sys::JS_GetGlobalObject(context.raw_context());
                assert!(
                    sys::JS_SetPropertyStr(
                        context.raw_context(),
                        global,
                        property.as_ptr(),
                        sys::JS_DupValue(context.raw_context(), promise),
                    ) >= 0
                );
                sys::JS_FreeValue(context.raw_context(), global);
            }
            context
                .eval(
                    "host-promise-context-snapshot-setup.js",
                    r#"
                    globalThis.__events = [];
                    globalThis.__host_promise.then(value => {
                        globalThis.__events.push(value.answer);
                    });
                    "#,
                    JS_EVAL_TYPE_GLOBAL,
                )
                .unwrap();
            context
                .snapshot_globals(&["__host_promise", "__events"])
                .unwrap()
        };

        let runtime = SnapshotRuntime::new(RuntimeLimits::default()).unwrap();
        let mut context = runtime.new_context().unwrap();
        context.restore_globals(&snapshot).unwrap();
        context
            .resolve_host_promise(id, serde_json::json!({ "answer": 42 }))
            .unwrap();

        assert_eq!(
            context.global_json("__events").unwrap(),
            Some(serde_json::json!([42]))
        );
    }

    #[test]
    fn snapshot_context_restores_pending_host_promise_downstream_reaction() {
        let id = HostPromiseId(178);
        let snapshot = {
            let runtime = SnapshotRuntime::new(RuntimeLimits::default()).unwrap();
            let mut context = runtime.new_context().unwrap();
            let promise = context.new_host_promise(id).unwrap().raw();
            let property = CString::new("__host_promise").unwrap();
            unsafe {
                let global = sys::JS_GetGlobalObject(context.raw_context());
                assert!(
                    sys::JS_SetPropertyStr(
                        context.raw_context(),
                        global,
                        property.as_ptr(),
                        sys::JS_DupValue(context.raw_context(), promise),
                    ) >= 0
                );
                sys::JS_FreeValue(context.raw_context(), global);
            }
            context
                .eval(
                    "host-promise-downstream-setup.js",
                    r#"
                    globalThis.__events = [];
                    globalThis.__downstream = globalThis.__host_promise
                        .then(value => value.answer + 1);
                    globalThis.__downstream.then(value => {
                        globalThis.__events.push(value);
                    });
                    "#,
                    JS_EVAL_TYPE_GLOBAL,
                )
                .unwrap();
            context
                .snapshot_globals(&["__host_promise", "__downstream", "__events"])
                .unwrap()
        };

        let runtime = SnapshotRuntime::new(RuntimeLimits::default()).unwrap();
        let mut context = runtime.new_context().unwrap();
        context.restore_globals(&snapshot).unwrap();
        context
            .resolve_host_promise(id, serde_json::json!({ "answer": 41 }))
            .unwrap();

        assert_eq!(
            context
                .eval_json_expression(
                    "host-promise-downstream-check.js",
                    r#"
                    (globalThis.__downstream.then(value => {
                        globalThis.__events.push(value);
                    }),
                    globalThis.__events
                    )
                    "#,
                )
                .unwrap(),
            serde_json::json!([42])
        );
        context.drain_jobs().unwrap();
        assert_eq!(
            context.global_json("__events").unwrap(),
            Some(serde_json::json!([42, 42]))
        );
    }

    #[test]
    fn snapshot_context_restores_ordinary_pending_promise_with_resolver_root() {
        let snapshot = {
            let runtime = SnapshotRuntime::new(RuntimeLimits::default()).unwrap();
            let mut context = runtime.new_context().unwrap();
            context
                .eval(
                    "ordinary-pending-promise-setup.js",
                    r#"
                    globalThis.__events = [];
                    globalThis.__promise = new Promise(resolve => {
                        globalThis.__resolve = resolve;
                    });
                    globalThis.__promise.then(value => {
                        globalThis.__events.push(value + 1);
                    });
                    "#,
                    JS_EVAL_TYPE_GLOBAL,
                )
                .unwrap();
            context
                .snapshot_globals(&["__promise", "__resolve", "__events"])
                .unwrap()
        };

        let runtime = SnapshotRuntime::new(RuntimeLimits::default()).unwrap();
        let mut context = runtime.new_context().unwrap();
        context.restore_globals(&snapshot).unwrap();
        context
            .eval(
                "ordinary-pending-promise-resolve.js",
                "globalThis.__resolve(41);",
                JS_EVAL_TYPE_GLOBAL,
            )
            .unwrap();
        context.drain_jobs().unwrap();

        assert_eq!(
            context.global_json("__events").unwrap(),
            Some(serde_json::json!([42]))
        );
    }

    #[test]
    fn snapshot_context_restores_async_function_suspended_on_host_promise() {
        let id = HostPromiseId(179);
        let snapshot = {
            let runtime = SnapshotRuntime::new(RuntimeLimits::default()).unwrap();
            let mut context = runtime.new_context().unwrap();
            let promise = context.new_host_promise(id).unwrap().raw();
            let property = CString::new("__host_promise").unwrap();
            unsafe {
                let global = sys::JS_GetGlobalObject(context.raw_context());
                assert!(
                    sys::JS_SetPropertyStr(
                        context.raw_context(),
                        global,
                        property.as_ptr(),
                        sys::JS_DupValue(context.raw_context(), promise),
                    ) >= 0
                );
                sys::JS_FreeValue(context.raw_context(), global);
            }
            context
                .eval(
                    "async-host-promise-setup.js",
                    r#"
                    globalThis.__events = [];
                    async function run() {
                        globalThis.__events.push("before");
                        const value = await globalThis.__host_promise;
                        globalThis.__events.push(value.answer);
                        return value.answer + 1;
                    }
                    globalThis.__result = run();
                    globalThis.__result.then(value => {
                        globalThis.__events.push(value);
                    });
                    "#,
                    JS_EVAL_TYPE_GLOBAL,
                )
                .unwrap();
            context
                .snapshot_globals(&["__host_promise", "__result", "__events"])
                .unwrap()
        };

        let runtime = SnapshotRuntime::new(RuntimeLimits::default()).unwrap();
        let mut context = runtime.new_context().unwrap();
        context.restore_globals(&snapshot).unwrap();
        context
            .resolve_host_promise(id, serde_json::json!({ "answer": 41 }))
            .unwrap();

        assert_eq!(
            context.global_json("__events").unwrap(),
            Some(serde_json::json!(["before", 41, 42]))
        );
    }

    #[test]
    fn snapshot_context_entrypoint_restores_suspended_chidori_host_call() {
        let id = HostPromiseId(701);
        let snapshot = {
            let runtime = SnapshotRuntime::new(RuntimeLimits::default()).unwrap();
            let mut context = runtime.new_context().unwrap();
            context.new_host_promise(id).unwrap();
            context
                .eval(
                    "install-chidori-input.js",
                    r#"
                    globalThis.chidori = {
                        input() {
                            globalThis.__chidori_active_host_operation_id = 701;
                            return globalThis.__chidori_host_promises["701"].promise;
                        }
                    };
                    "#,
                    JS_EVAL_TYPE_GLOBAL,
                )
                .unwrap();
            context
                .eval_module(
                    "agent.mjs",
                    r#"
                    export async function agent(input, chidori) {
                        const answer = await chidori.input("Continue?");
                        return { answer: answer.value + input.delta };
                    }
                    "#,
                )
                .unwrap();

            assert_eq!(
                context
                    .call_export_json("agent", serde_json::json!({ "delta": 2 }))
                    .unwrap(),
                RunState::BlockedOnHostOperation(id)
            );
            context.snapshot_context_payload().unwrap()
        };

        let runtime = SnapshotRuntime::new(RuntimeLimits::default()).unwrap();
        let mut context = runtime.restore_context(&snapshot).unwrap();
        let state = context
            .resolve_host_promise_and_run(id, serde_json::json!({ "value": 40 }))
            .unwrap();

        assert_eq!(
            state,
            RunState::Completed(serde_json::json!({ "answer": 42 }))
        );
    }

    #[test]
    fn snapshot_context_entrypoint_reports_unsupported_root_detail() {
        let runtime = SnapshotRuntime::new(RuntimeLimits::default()).unwrap();
        let mut context = runtime.new_context().unwrap();
        context
            .eval(
                "unsupported-root.js",
                "globalThis.__chidori_call_result = Math.max;",
                JS_EVAL_TYPE_GLOBAL,
            )
            .unwrap();

        let err = context.snapshot_context_payload().unwrap_err();

        assert!(matches!(
            err,
            QuickJsError::SnapshotUnsupportedDetail {
                path,
                type_name,
                message,
            } if path == "$context.roots"
                && type_name == "roots"
                && message.contains("unsupported object class")
        ));
    }

    #[test]
    fn snapshot_runtime_restore_composes_runtime_and_context_payloads() {
        let id = HostPromiseId(702);
        let context_payload = {
            let runtime = SnapshotRuntime::new(RuntimeLimits::default()).unwrap();
            let mut context = runtime.new_context().unwrap();
            context.new_host_promise(id).unwrap();
            context
                .eval(
                    "install-chidori-input.js",
                    r#"
                    globalThis.chidori = {
                        input() {
                            globalThis.__chidori_active_host_operation_id = 702;
                            return globalThis.__chidori_host_promises["702"].promise;
                        }
                    };
                    "#,
                    JS_EVAL_TYPE_GLOBAL,
                )
                .unwrap();
            context
                .eval_module(
                    "agent.mjs",
                    r#"
                    export async function agent(input, chidori) {
                        const answer = await chidori.input("Continue?");
                        return { answer: answer.value + input.delta };
                    }
                    "#,
                )
                .unwrap();
            assert_eq!(
                context
                    .call_export_json("agent", serde_json::json!({ "delta": 3 }))
                    .unwrap(),
                RunState::BlockedOnHostOperation(id)
            );
            context.snapshot_context_payload().unwrap()
        };
        let runtime_payload = {
            let mut runtime = SnapshotRuntime::new(RuntimeLimits::default()).unwrap();
            runtime.snapshot().unwrap().payload().unwrap().to_vec()
        };
        let snapshot = RuntimeSnapshot::from_parts(&runtime_payload, &context_payload);
        let mut restored = SnapshotRuntime::restore(&snapshot.0).unwrap();

        assert_eq!(
            restored.run_jobs_until_blocked().unwrap(),
            RunState::BlockedOnHostOperation(id)
        );
        restored
            .resolve_host_promise(id, serde_json::json!({ "value": 39 }))
            .unwrap();

        assert_eq!(
            restored.run_jobs_until_blocked().unwrap(),
            RunState::Completed(serde_json::json!({ "answer": 42 }))
        );
    }

    #[test]
    fn snapshot_context_restores_nested_async_function_stack() {
        let id = HostPromiseId(180);
        let snapshot = {
            let runtime = SnapshotRuntime::new(RuntimeLimits::default()).unwrap();
            let mut context = runtime.new_context().unwrap();
            let promise = context.new_host_promise(id).unwrap().raw();
            let property = CString::new("__host_promise").unwrap();
            unsafe {
                let global = sys::JS_GetGlobalObject(context.raw_context());
                assert!(
                    sys::JS_SetPropertyStr(
                        context.raw_context(),
                        global,
                        property.as_ptr(),
                        sys::JS_DupValue(context.raw_context(), promise),
                    ) >= 0
                );
                sys::JS_FreeValue(context.raw_context(), global);
            }
            context
                .eval(
                    "nested-async-host-promise-setup.js",
                    r#"
                    globalThis.__events = [];
                    async function inner() {
                        globalThis.__events.push("inner-before");
                        const value = await globalThis.__host_promise;
                        globalThis.__events.push("inner-after");
                        return value.answer + 1;
                    }
                    async function outer() {
                        globalThis.__events.push("outer-before");
                        const value = await inner();
                        globalThis.__events.push("outer-after");
                        return value + 1;
                    }
                    globalThis.__result = outer();
                    globalThis.__result.then(value => {
                        globalThis.__events.push(value);
                    });
                    "#,
                    JS_EVAL_TYPE_GLOBAL,
                )
                .unwrap();
            context
                .snapshot_globals(&["__host_promise", "__result", "__events"])
                .unwrap()
        };

        let runtime = SnapshotRuntime::new(RuntimeLimits::default()).unwrap();
        let mut context = runtime.new_context().unwrap();
        context.restore_globals(&snapshot).unwrap();
        context
            .resolve_host_promise(id, serde_json::json!({ "answer": 40 }))
            .unwrap();

        assert_eq!(
            context.global_json("__events").unwrap(),
            Some(serde_json::json!([
                "outer-before",
                "inner-before",
                "inner-after",
                "outer-after",
                42
            ]))
        );
    }

    #[test]
    fn snapshot_context_rejects_invalid_context_snapshot() {
        let runtime = SnapshotRuntime::new(RuntimeLimits::default()).unwrap();
        let mut context = runtime.new_context().unwrap();
        let err = context
            .restore_globals(b"not-a-context-snapshot")
            .unwrap_err();
        assert!(matches!(err, QuickJsError::InvalidSnapshot(_)));
    }

    #[test]
    fn snapshot_context_round_trips_thenable_resolution_job() {
        let snapshot = {
            let runtime = SnapshotRuntime::new(RuntimeLimits::default()).unwrap();
            let mut context = runtime.new_context().unwrap();
            context
                .eval(
                    "thenable-job-setup.js",
                    r#"
                    globalThis.__thenable_events = [];
                    Promise.resolve({
                        then(resolve) {
                            resolve(41);
                        },
                    }).then(value => {
                        globalThis.__thenable_events.push(value + 1);
                    });
                    "#,
                    JS_EVAL_TYPE_GLOBAL,
                )
                .unwrap();
            context.snapshot_globals(&["__thenable_events"]).unwrap()
        };

        let runtime = SnapshotRuntime::new(RuntimeLimits::default()).unwrap();
        let mut context = runtime.new_context().unwrap();
        context.restore_globals(&snapshot).unwrap();
        context.drain_jobs().unwrap();

        assert_eq!(
            context.global_json("__thenable_events").unwrap(),
            Some(serde_json::json!([42]))
        );
    }

    #[test]
    fn snapshot_context_round_trips_mixed_pending_job_queue_order() {
        let snapshot = {
            let runtime = SnapshotRuntime::new(RuntimeLimits::default()).unwrap();
            let mut context = runtime.new_context().unwrap();
            context
                .eval(
                    "mixed-job-queue-setup.js",
                    r#"
                    globalThis.__mixed_job_events = [];
                    queueMicrotask(() => {
                        globalThis.__mixed_job_events.push("microtask");
                    });
                    Promise.resolve(1).then(() => {
                        globalThis.__mixed_job_events.push("reaction");
                    });
                    Promise.resolve({
                        then(resolve) {
                            globalThis.__mixed_job_events.push("thenable");
                            resolve(2);
                        },
                    }).then(() => {
                        globalThis.__mixed_job_events.push("thenable-reaction");
                    });
                    "#,
                    JS_EVAL_TYPE_GLOBAL,
                )
                .unwrap();
            context.snapshot_globals(&["__mixed_job_events"]).unwrap()
        };

        let runtime = SnapshotRuntime::new(RuntimeLimits::default()).unwrap();
        let mut context = runtime.new_context().unwrap();
        context.restore_globals(&snapshot).unwrap();
        context.drain_jobs().unwrap();

        assert_eq!(
            context.global_json("__mixed_job_events").unwrap(),
            Some(serde_json::json!([
                "microtask",
                "reaction",
                "thenable",
                "thenable-reaction"
            ]))
        );
    }

    #[test]
    fn snapshot_context_resolves_host_promise_by_id() {
        let runtime = SnapshotRuntime::new(RuntimeLimits::default()).unwrap();
        let mut context = runtime.new_context().unwrap();
        let id = HostPromiseId(7);

        let _promise = context.new_host_promise(id).unwrap();
        assert_eq!(
            context.host_promise_state(id).unwrap(),
            PromiseState::Pending
        );

        context
            .resolve_host_promise(id, serde_json::json!({ "ok": true }))
            .unwrap();

        assert_eq!(
            context.host_promise_state(id).unwrap(),
            PromiseState::Fulfilled(serde_json::json!({ "ok": true }))
        );
    }

    #[test]
    fn snapshot_context_rejects_host_promise_by_id() {
        let runtime = SnapshotRuntime::new(RuntimeLimits::default()).unwrap();
        let mut context = runtime.new_context().unwrap();
        let id = HostPromiseId(8);

        let _promise = context.new_host_promise(id).unwrap();
        context
            .reject_host_promise(id, "host failed".to_string())
            .unwrap();

        assert_eq!(
            context.host_promise_state(id).unwrap(),
            PromiseState::Rejected(serde_json::json!("host failed"))
        );
    }

    #[test]
    fn snapshot_context_reports_eval_errors() {
        let runtime = SnapshotRuntime::new(RuntimeLimits::default()).unwrap();
        let mut context = runtime.new_context().unwrap();

        let err = context
            .eval_module("broken.mjs", "export const value = ;")
            .unwrap_err();

        assert!(matches!(err, QuickJsError::EvalFailed(_)));
    }

    fn generated_json_value(seed: u64, depth: usize) -> serde_json::Value {
        if depth >= 3 {
            return match seed % 4 {
                0 => serde_json::json!(seed as i64),
                1 => serde_json::json!(seed % 2 == 0),
                2 => serde_json::json!(format!("value-{seed}")),
                _ => serde_json::Value::Null,
            };
        }

        match seed % 6 {
            0 => serde_json::json!(seed as i64),
            1 => serde_json::json!(seed % 2 == 0),
            2 => serde_json::json!(format!("value-{seed}")),
            3 => serde_json::Value::Array(vec![
                generated_json_value(seed.wrapping_mul(3).wrapping_add(1), depth + 1),
                generated_json_value(seed.wrapping_mul(5).wrapping_add(2), depth + 1),
            ]),
            _ => {
                let mut object = serde_json::Map::new();
                object.insert(
                    format!("k{seed}"),
                    generated_json_value(seed.wrapping_mul(7).wrapping_add(3), depth + 1),
                );
                object.insert(
                    "nested".to_string(),
                    generated_json_value(seed.wrapping_mul(11).wrapping_add(4), depth + 1),
                );
                serde_json::Value::Object(object)
            }
        }
    }
}
