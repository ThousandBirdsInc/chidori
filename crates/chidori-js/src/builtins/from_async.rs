//! `Array.fromAsync` (ES2024).
//!
//! The algorithm is *self-hosted*: the spec's abstract closure is one long
//! `await`-per-element loop with iterator-close on every abrupt path, which the
//! engine's own `async function` machinery already expresses exactly — a native
//! implementation would have to hand-roll that state machine as a chain of
//! promise reactions and re-derive `Await` semantics (thenable jobs, rejection
//! ordering) by hand. Instead the body below is compiled once per thread
//! (`compile_script_cached`) and instantiated per realm.
//!
//! Two properties make the self-hosting non-observable:
//!
//! * Every intrinsic the body needs — the two well-known symbols, plus small
//!   native shims for the spec operations that have no safe surface syntax
//!   (`IsConstructor`, `Construct`, `Call`, `CreateDataPropertyOrThrow`,
//!   `LengthOfArrayLike`, `ToObject`, `ArrayCreate`) — is **captured at install
//!   time** as a factory argument. Later user tampering (`Symbol.iterator = …`,
//!   `Reflect.apply = …`, `Object.defineProperty = …`, a poisoned
//!   `Array.prototype[Symbol.iterator]`) cannot reach it.
//! * The installed `Array.fromAsync` is a real **native** function object that
//!   forwards to the self-hosted body, so it has `Function.prototype` as its
//!   `[[Prototype]]`, no own `prototype` property, and no `[[Construct]]` —
//!   the built-in-function characteristics an async-function object would fail.
//!
//! The `AsyncFromSyncIterator` wrapper the spec interposes on the sync-iterable
//! path is inlined into the sync loop (await the yielded value, close the sync
//! iterator if that await rejects), because the engine has no such wrapper
//! intrinsic and it is not otherwise reachable.

use super::arg;
use super::fundamental::create_data_property_or_throw;
use crate::value::*;
use crate::vm::Vm;

/// The self-hosted body, as a factory over its captured intrinsics.
///
/// Kept deliberately close to the spec text (sec-array.fromasync); the branch
/// structure is `usingAsyncIterator` / `usingSyncIterator` / array-like, in
/// that order, with `k` counted as a Number so index keys stringify exactly
/// like `! ToString(𝔽(k))`.
const FROM_ASYNC_SRC: &str = r#"(function (
  symAsyncIterator, symIterator, isConstructor, construct, call0, call2,
  defineDataProp, lengthOfArrayLike, toObject, isObject, makeTypeError, arrayCreate
) {
  "use strict";
  const MAX_SAFE_INTEGER = 9007199254740991;

  // GetMethod(V, P): nullish -> undefined, non-callable -> TypeError.
  function getMethod(o, k) {
    const m = o[k];
    if (m === undefined || m === null) return undefined;
    if (typeof m !== "function") {
      throw makeTypeError("Array.fromAsync: iterator method is not callable");
    }
    return m;
  }

  // IteratorClose(iteratorRecord, throwCompletion): the pending throw always
  // wins, so an abrupt `return` is swallowed.
  function closeSyncThrow(iterator, err) {
    try {
      const ret = iterator.return;
      if (ret !== undefined && ret !== null) call0(ret, iterator);
    } catch (ignored) {}
    throw err;
  }

  // AsyncIteratorClose(iteratorRecord, throwCompletion): as above, but the
  // result of `return` is awaited before the original error is rethrown.
  async function closeAsyncThrow(iterator, err) {
    try {
      const ret = iterator.return;
      if (ret !== undefined && ret !== null) await call0(ret, iterator);
    } catch (ignored) {}
    throw err;
  }

  return async function fromAsync(asyncItems, mapfn, thisArg) {
    const C = this;
    let mapping = false;
    if (mapfn !== undefined) {
      if (typeof mapfn !== "function") {
        throw makeTypeError("Array.fromAsync: mapfn is not a function");
      }
      mapping = true;
    }
    const usingAsyncIterator = getMethod(asyncItems, symAsyncIterator);
    let usingSyncIterator;
    if (usingAsyncIterator === undefined) {
      usingSyncIterator = getMethod(asyncItems, symIterator);
    }
    const isCtor = isConstructor(C);

    if (usingAsyncIterator !== undefined) {
      // GetIterator(asyncItems, async, usingAsyncIterator).
      const iterator = call0(usingAsyncIterator, asyncItems);
      if (!isObject(iterator)) {
        throw makeTypeError("Array.fromAsync: iterator method did not return an object");
      }
      const nextMethod = iterator.next;
      const A = isCtor ? construct(C, undefined) : arrayCreate(0);
      let k = 0;
      for (;;) {
        if (k >= MAX_SAFE_INTEGER) {
          await closeAsyncThrow(iterator, makeTypeError("Array.fromAsync: length exceeded"));
        }
        // `next()` must return an Object both before and after the Await —
        // fromAsync checks twice, unlike for-await's single post-Await check.
        let nextResult = call0(nextMethod, iterator);
        if (!isObject(nextResult)) {
          throw makeTypeError("Array.fromAsync: iterator result is not an object");
        }
        nextResult = await nextResult;
        if (!isObject(nextResult)) {
          throw makeTypeError("Array.fromAsync: iterator result is not an object");
        }
        if (nextResult.done) {
          A.length = k;
          return A;
        }
        const nextValue = nextResult.value;
        let mappedValue = nextValue;
        if (mapping) {
          try {
            mappedValue = await call2(mapfn, thisArg, nextValue, k);
          } catch (e) {
            await closeAsyncThrow(iterator, e);
          }
        }
        try {
          defineDataProp(A, k, mappedValue);
        } catch (e) {
          await closeAsyncThrow(iterator, e);
        }
        k = k + 1;
      }
    }

    if (usingSyncIterator !== undefined) {
      // CreateAsyncFromSyncIterator(GetIterator(asyncItems, sync, …)), inlined:
      // each yielded value is awaited, and a rejection there closes the sync
      // iterator (AsyncFromSyncIteratorContinuation with closeOnRejection).
      const iterator = call0(usingSyncIterator, asyncItems);
      if (!isObject(iterator)) {
        throw makeTypeError("Array.fromAsync: iterator method did not return an object");
      }
      const nextMethod = iterator.next;
      const A = isCtor ? construct(C, undefined) : arrayCreate(0);
      let k = 0;
      for (;;) {
        if (k >= MAX_SAFE_INTEGER) {
          closeSyncThrow(iterator, makeTypeError("Array.fromAsync: length exceeded"));
        }
        const nextResult = call0(nextMethod, iterator);
        if (!isObject(nextResult)) {
          throw makeTypeError("Array.fromAsync: iterator result is not an object");
        }
        const done = !!nextResult.done;
        const rawValue = nextResult.value;
        if (done) {
          // The wrapper still unwraps the final value; a rejection there is
          // observable, but the (exhausted) iterator is not closed.
          await rawValue;
          A.length = k;
          return A;
        }
        let nextValue;
        try {
          nextValue = await rawValue;
        } catch (e) {
          closeSyncThrow(iterator, e);
        }
        let mappedValue = nextValue;
        if (mapping) {
          try {
            mappedValue = await call2(mapfn, thisArg, nextValue, k);
          } catch (e) {
            closeSyncThrow(iterator, e);
          }
        }
        try {
          defineDataProp(A, k, mappedValue);
        } catch (e) {
          closeSyncThrow(iterator, e);
        }
        k = k + 1;
      }
    }

    // Neither async-iterable nor iterable: treat asyncItems as an array-like.
    const arrayLike = toObject(asyncItems);
    const len = lengthOfArrayLike(arrayLike);
    const A = isCtor ? construct(C, len) : arrayCreate(len);
    let k = 0;
    while (k < len) {
      let kValue = await arrayLike[k];
      if (mapping) {
        kValue = await call2(mapfn, thisArg, kValue, k);
      }
      defineDataProp(A, k, kValue);
      k = k + 1;
    }
    A.length = len;
    return A;
  };
})"#;

/// Install `Array.fromAsync` on the Array constructor. Runs as the last
/// builtin section: the body is a compiled script, so the global object must
/// already be populated before it can be evaluated.
pub(super) fn install(vm: &mut Vm) {
    let ctor = vm
        .realm
        .array_proto
        .borrow()
        .own_get(&PropertyKey::str("constructor"))
        .and_then(|p| p.value().cloned());
    let Some(Value::Object(array_ctor)) = ctor else {
        debug_assert!(false, "Array.prototype.constructor missing");
        return;
    };
    let Some(body) = instantiate(vm) else {
        return;
    };
    let f = vm.new_native("fromAsync", 1, move |vm, this, args| {
        // The body is an async function: it never completes abruptly, so the
        // spec's "all errors reject the returned promise" holds by construction.
        vm.call(body.clone(), this, args)
    });
    array_ctor.borrow_mut().own_insert(
        PropertyKey::str("fromAsync"),
        Property::builtin(Value::Object(f)),
    );
}

/// Compile the factory (memoized per thread) and call it with this realm's
/// intrinsics, yielding the realm's own `fromAsync` body closure.
fn instantiate(vm: &mut Vm) -> Option<Value> {
    let proto = match crate::compiler::compile_script_cached(FROM_ASYNC_SRC) {
        Ok(p) => p,
        Err(e) => {
            debug_assert!(false, "Array.fromAsync source failed to compile: {e}");
            return None;
        }
    };
    let script = vm.make_closure(proto, Vec::new());
    let factory = match vm.call(Value::Object(script), Value::Undefined, &[]) {
        Ok(f) => f,
        Err(e) => {
            let msg = vm.error_to_string(&e);
            debug_assert!(false, "Array.fromAsync factory failed to evaluate: {msg}");
            return None;
        }
    };
    let deps = intrinsics(vm);
    match vm.call(factory, Value::Undefined, &deps) {
        Ok(f) => Some(f),
        Err(e) => {
            let msg = vm.error_to_string(&e);
            debug_assert!(false, "Array.fromAsync factory call failed: {msg}");
            None
        }
    }
}

/// The captured factory arguments, in the order the source destructures them.
fn intrinsics(vm: &mut Vm) -> Vec<Value> {
    let is_constructor = vm.new_native("", 1, |vm, _t, args| {
        Ok(Value::Bool(vm.is_constructor(&arg(args, 0))))
    });
    // Construct(C) / Construct(C, «len») — `undefined` selects the empty list,
    // which is the only distinction either call site needs.
    let construct = vm.new_native("", 2, |vm, _t, args| {
        let c = arg(args, 0);
        let len = arg(args, 1);
        let ctor_args: Vec<Value> = if len.is_undefined() {
            Vec::new()
        } else {
            vec![len]
        };
        vm.construct(&c, &ctor_args, &c)
    });
    let call0 = vm.new_native("", 2, |vm, _t, args| {
        vm.call(arg(args, 0), arg(args, 1), &[])
    });
    let call2 = vm.new_native("", 4, |vm, _t, args| {
        vm.call(arg(args, 0), arg(args, 1), &[arg(args, 2), arg(args, 3)])
    });
    let define_data_prop = vm.new_native("", 3, |vm, _t, args| {
        let Value::Object(o) = arg(args, 0) else {
            return Err(vm.throw_type("Array.fromAsync: target is not an object"));
        };
        let key = vm.to_property_key(&arg(args, 1))?;
        create_data_property_or_throw(vm, &o, &key, arg(args, 2))?;
        Ok(Value::Undefined)
    });
    let length_of_array_like = vm.new_native("", 1, |vm, _t, args| {
        let raw = vm.get_prop(&arg(args, 0), &PropertyKey::str("length"))?;
        Ok(Value::Number(vm.to_length(&raw)? as f64))
    });
    let to_object = vm.new_native("", 1, |vm, _t, args| {
        Ok(Value::Object(vm.to_object(&arg(args, 0))?))
    });
    let is_object = vm.new_native("", 1, |_vm, _t, args| {
        Ok(Value::Bool(matches!(arg(args, 0), Value::Object(_))))
    });
    let make_type_error = vm.new_native("", 1, |vm, _t, args| {
        let msg = vm.to_string_lossy(&arg(args, 0));
        Ok(vm.throw_type(&msg))
    });
    // ArrayCreate(len): the engine's dense-storage bound stands in for the
    // spec's 2^32-1 ceiling (both surface as a RangeError).
    let array_create = vm.new_native("", 1, |vm, _t, args| {
        let n = match arg(args, 0) {
            Value::Number(n) => n,
            other => vm.to_number(&other)?,
        };
        let len = n as u32;
        if (len as f64) != n {
            return Err(vm.throw_range("Invalid array length"));
        }
        if len as usize > crate::value::MAX_DENSE_ARRAY {
            return Err(vm.throw_range("Array allocation exceeds engine limit"));
        }
        Ok(Value::Object(vm.new_array(vec![Value::Hole; len as usize])))
    });

    vec![
        Value::Symbol(vm.realm.symbol_async_iterator.clone()),
        Value::Symbol(vm.realm.symbol_iterator.clone()),
        Value::Object(is_constructor),
        Value::Object(construct),
        Value::Object(call0),
        Value::Object(call2),
        Value::Object(define_data_prop),
        Value::Object(length_of_array_like),
        Value::Object(to_object),
        Value::Object(is_object),
        Value::Object(make_type_error),
        Value::Object(array_create),
    ]
}
