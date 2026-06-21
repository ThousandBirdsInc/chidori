//! The realm: intrinsic prototype objects, well-known symbols, the global
//! object, and builtin installation. `placeholder()` constructs the bare objects
//! (so `Vm::new` can hold a `Realm` before anything is wired), then `init_realm`
//! links the prototype chain and installs builtins using the live `Vm`.

use std::rc::Rc;

use crate::value::*;
use crate::vm::Vm;

pub struct Realm {
    pub global: JsObject,
    /// The %eval% intrinsic function object — `Op::DirectEval` compares the
    /// callee against it to decide direct-vs-ordinary call semantics.
    pub eval_fn: Option<JsObject>,

    pub object_proto: JsObject,
    pub function_proto: JsObject,
    pub array_proto: JsObject,
    pub string_proto: JsObject,
    pub number_proto: JsObject,
    pub boolean_proto: JsObject,
    pub symbol_proto: JsObject,
    pub bigint_proto: JsObject,

    pub error_proto: JsObject,
    pub type_error_proto: JsObject,
    pub range_error_proto: JsObject,
    pub reference_error_proto: JsObject,
    pub syntax_error_proto: JsObject,
    pub uri_error_proto: JsObject,

    pub promise_proto: JsObject,
    pub iterator_proto: JsObject,
    pub array_iterator_proto: JsObject,
    pub string_iterator_proto: JsObject,
    pub map_iterator_proto: JsObject,
    pub set_iterator_proto: JsObject,
    pub generator_proto: JsObject,
    pub async_generator_proto: JsObject,
    /// `%GeneratorFunction.prototype%` etc. — the `[[Prototype]]` of generator /
    /// async function *objects* (distinct from `%Generator%`/`%AsyncGenerator%`,
    /// which are the prototypes of the *instances* those functions return).
    pub generator_function_proto: JsObject,
    pub async_function_proto: JsObject,
    pub async_generator_function_proto: JsObject,

    pub map_proto: JsObject,
    pub set_proto: JsObject,
    pub weak_map_proto: JsObject,
    pub weak_set_proto: JsObject,
    pub regexp_proto: JsObject,
    pub date_proto: JsObject,
    pub array_buffer_proto: JsObject,
    /// %SharedArrayBuffer.prototype%. Distinct from `array_buffer_proto` so the
    /// two byte-length/slice surfaces brand-check against each other.
    pub shared_array_buffer_proto: JsObject,
    pub data_view_proto: JsObject,
    /// Base %TypedArray%.prototype (shared by all typed-array prototypes).
    pub typed_array_proto: JsObject,
    /// %ThrowTypeError%: the unique-per-realm restricted-property accessor
    /// (Function.prototype.caller/arguments, strict arguments.callee).
    pub throw_type_error: JsObject,

    // Well-known symbols.
    pub symbol_iterator: JsSymbol,
    pub symbol_async_iterator: JsSymbol,
    pub symbol_to_primitive: JsSymbol,
    pub symbol_to_string_tag: JsSymbol,
    pub symbol_has_instance: JsSymbol,
    pub symbol_match: JsSymbol,
    pub symbol_replace: JsSymbol,
    pub symbol_search: JsSymbol,
    pub symbol_split: JsSymbol,
    pub symbol_match_all: JsSymbol,
    pub symbol_species: JsSymbol,
    pub symbol_unscopables: JsSymbol,
    pub symbol_is_concat_spreadable: JsSymbol,
    pub symbol_dispose: JsSymbol,
    pub symbol_async_dispose: JsSymbol,
    /// Engine-private key for a (Async)DisposableStack's internal state (a JS
    /// array `[disposedBool, ...disposerFns]`). A symbol so it stays out of
    /// `getOwnPropertyNames` — fresh stacks must have no own string keys.
    pub symbol_disposable_state: JsSymbol,
    /// Engine-private brand marking a buffer object as a SharedArrayBuffer
    /// (`IsSharedArrayBuffer`). A symbol — script-unreachable — so it cannot be
    /// observed; `own_keys` also filters it from `getOwnPropertySymbols`.
    pub symbol_array_buffer_shared: JsSymbol,

    /// Registry for `Symbol.for`.
    pub symbol_registry: indexmap::IndexMap<String, JsSymbol>,
}

impl Realm {
    /// Every realm-resident object (the global + all intrinsic prototypes). Used
    /// as the roots for `Vm::dispose`, which walks and empties the object graph to
    /// break the `Rc` cycles (ctor↔prototype, global→builtins) that
    /// reference-counting cannot reclaim on drop.
    pub fn object_roots(&self) -> Vec<JsObject> {
        vec![
            self.global.clone(),
            self.object_proto.clone(),
            self.function_proto.clone(),
            self.array_proto.clone(),
            self.string_proto.clone(),
            self.number_proto.clone(),
            self.boolean_proto.clone(),
            self.symbol_proto.clone(),
            self.bigint_proto.clone(),
            self.error_proto.clone(),
            self.type_error_proto.clone(),
            self.range_error_proto.clone(),
            self.reference_error_proto.clone(),
            self.syntax_error_proto.clone(),
            self.uri_error_proto.clone(),
            self.promise_proto.clone(),
            self.iterator_proto.clone(),
            self.array_iterator_proto.clone(),
            self.string_iterator_proto.clone(),
            self.map_iterator_proto.clone(),
            self.set_iterator_proto.clone(),
            self.generator_proto.clone(),
            self.async_generator_proto.clone(),
            self.generator_function_proto.clone(),
            self.async_function_proto.clone(),
            self.async_generator_function_proto.clone(),
            self.map_proto.clone(),
            self.set_proto.clone(),
            self.weak_map_proto.clone(),
            self.weak_set_proto.clone(),
            self.regexp_proto.clone(),
            self.date_proto.clone(),
            self.array_buffer_proto.clone(),
            self.shared_array_buffer_proto.clone(),
            self.data_view_proto.clone(),
            self.typed_array_proto.clone(),
            self.throw_type_error.clone(),
        ]
    }
}

fn bare() -> JsObject {
    JsObject::ordinary(None)
}

fn bare_symbol(id: u64, desc: &str) -> JsSymbol {
    JsSymbol(Rc::new(SymbolData {
        description: Some(Rc::from(desc)),
        id,
    }))
}

impl Realm {
    pub fn placeholder() -> Realm {
        Realm {
            global: bare(),
            eval_fn: None,
            object_proto: bare(),
            function_proto: bare(),
            array_proto: bare(),
            string_proto: bare(),
            number_proto: bare(),
            boolean_proto: bare(),
            symbol_proto: bare(),
            bigint_proto: bare(),
            error_proto: bare(),
            type_error_proto: bare(),
            range_error_proto: bare(),
            reference_error_proto: bare(),
            syntax_error_proto: bare(),
            uri_error_proto: bare(),
            promise_proto: bare(),
            iterator_proto: bare(),
            array_iterator_proto: bare(),
            string_iterator_proto: bare(),
            map_iterator_proto: bare(),
            set_iterator_proto: bare(),
            generator_proto: bare(),
            async_generator_proto: bare(),
            generator_function_proto: bare(),
            async_function_proto: bare(),
            async_generator_function_proto: bare(),
            map_proto: bare(),
            set_proto: bare(),
            weak_map_proto: bare(),
            weak_set_proto: bare(),
            regexp_proto: bare(),
            date_proto: bare(),
            array_buffer_proto: bare(),
            shared_array_buffer_proto: bare(),
            data_view_proto: bare(),
            typed_array_proto: bare(),
            throw_type_error: bare(),
            symbol_iterator: bare_symbol(1, "Symbol.iterator"),
            symbol_async_iterator: bare_symbol(2, "Symbol.asyncIterator"),
            symbol_to_primitive: bare_symbol(3, "Symbol.toPrimitive"),
            symbol_to_string_tag: bare_symbol(4, "Symbol.toStringTag"),
            symbol_has_instance: bare_symbol(5, "Symbol.hasInstance"),
            symbol_match: bare_symbol(6, "Symbol.match"),
            symbol_replace: bare_symbol(7, "Symbol.replace"),
            symbol_search: bare_symbol(8, "Symbol.search"),
            symbol_split: bare_symbol(9, "Symbol.split"),
            symbol_match_all: bare_symbol(10, "Symbol.matchAll"),
            symbol_species: bare_symbol(11, "Symbol.species"),
            symbol_unscopables: bare_symbol(12, "Symbol.unscopables"),
            symbol_is_concat_spreadable: bare_symbol(13, "Symbol.isConcatSpreadable"),
            symbol_dispose: bare_symbol(14, "Symbol.dispose"),
            symbol_async_dispose: bare_symbol(15, "Symbol.asyncDispose"),
            symbol_disposable_state: bare_symbol(16, "[[DisposableState]]"),
            symbol_array_buffer_shared: bare_symbol(17, "[[ArrayBufferShared]]"),
            symbol_registry: indexmap::IndexMap::new(),
        }
    }
}

/// Set the `[[Prototype]]` link of `obj`.
fn set_proto(obj: &JsObject, proto: &JsObject) {
    obj.borrow_mut().proto = Some(proto.clone());
}

pub fn init_realm(vm: &mut Vm) {
    // Prototype chain wiring. object_proto has null proto; everything else
    // ultimately chains to it.
    let op = vm.realm.object_proto.clone();
    for p in [
        &vm.realm.function_proto,
        &vm.realm.array_proto,
        &vm.realm.string_proto,
        &vm.realm.number_proto,
        &vm.realm.boolean_proto,
        &vm.realm.symbol_proto,
        &vm.realm.bigint_proto,
        &vm.realm.error_proto,
        &vm.realm.promise_proto,
        &vm.realm.iterator_proto,
        &vm.realm.map_proto,
        &vm.realm.set_proto,
        &vm.realm.weak_map_proto,
        &vm.realm.weak_set_proto,
        &vm.realm.regexp_proto,
        &vm.realm.date_proto,
        &vm.realm.array_buffer_proto,
        &vm.realm.shared_array_buffer_proto,
        &vm.realm.data_view_proto,
        &vm.realm.typed_array_proto,
    ] {
        set_proto(p, &op);
    }
    // Error subtype prototypes chain to error_proto.
    let ep = vm.realm.error_proto.clone();
    for p in [
        &vm.realm.type_error_proto,
        &vm.realm.range_error_proto,
        &vm.realm.reference_error_proto,
        &vm.realm.syntax_error_proto,
        &vm.realm.uri_error_proto,
    ] {
        set_proto(p, &ep);
    }
    // Iterator-derived prototypes chain to the shared iterator prototype.
    let ip = vm.realm.iterator_proto.clone();
    for p in [
        &vm.realm.array_iterator_proto,
        &vm.realm.string_iterator_proto,
        &vm.realm.map_iterator_proto,
        &vm.realm.set_iterator_proto,
        &vm.realm.generator_proto,
        &vm.realm.async_generator_proto,
    ] {
        set_proto(p, &ip);
    }
    // The generator/async function-kind prototypes chain to %Function.prototype%.
    let fp = vm.realm.function_proto.clone();
    for p in [
        &vm.realm.generator_function_proto,
        &vm.realm.async_function_proto,
        &vm.realm.async_generator_function_proto,
    ] {
        set_proto(p, &fp);
    }
    set_proto(&vm.realm.global, &op);

    crate::builtins::install(vm);
}
