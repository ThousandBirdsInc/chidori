//! The lazily-installed builtin sections (`builtins::install_lazy_globals`):
//! Date, the ArrayBuffer/TypedArray/DataView/Atomics family, Intl, and
//! Temporal materialize on first use and are indistinguishable from an eager
//! build afterwards.

use chidori_js::Engine;

fn eval_console(src: &str) -> Vec<String> {
    let mut e = Engine::new();
    match e.eval(src) {
        Ok(_) => e.console().to_vec(),
        Err(err) => panic!("eval failed: {err}\nconsole: {:?}", e.console()),
    }
}

#[test]
fn first_read_materializes_and_becomes_data_property() {
    let out = eval_console(
        r#"
        // A plain read of ONE name of the typedarray section...
        const u8 = new Uint8Array([1, 2, 3]);
        console.log(u8.reduce((a, b) => a + b, 0));
        // ...materializes the siblings as ordinary data properties.
        const d = Object.getOwnPropertyDescriptor(globalThis, "DataView");
        console.log("value" in d, d.writable, d.enumerable, d.configurable);
        const dd = Object.getOwnPropertyDescriptor(globalThis, "Uint8Array");
        console.log("value" in dd);
        "#,
    );
    assert_eq!(out, ["6", "true true false true", "true"]);
}

#[test]
fn each_lazy_section_works_through_varied_entry_points() {
    let out = eval_console(
        r#"
        console.log(new Date(86400000).getUTCDate());          // Date ctor
        console.log(typeof Date.now());                        // static method
        const dv = new DataView(new ArrayBuffer(8));
        dv.setInt32(0, 42);
        console.log(dv.getInt32(0));                           // two more family names
        console.log(Temporal.Duration.from({ minutes: 90 }).total("hours"));
        console.log(Intl.getCanonicalLocales("fr-fr")[0]);
        console.log(typeof Atomics);
        "#,
    );
    assert_eq!(out, ["2", "number", "42", "1.5", "fr-FR", "object"]);
}

#[test]
fn instanceof_subclassing_and_json_behave_as_eager() {
    let out = eval_console(
        r#"
        class Buf extends Uint8Array {}
        console.log(new Buf(4).length);
        console.log(new Date(0) instanceof Date);
        console.log(JSON.stringify(new Date(86400000)));
        "#,
    );
    assert_eq!(out, ["4", "true", "\"1970-01-02T00:00:00.000Z\""]);
}

#[test]
fn assignment_before_first_read_wins_and_materializes_siblings() {
    let out = eval_console(
        r#"
        globalThis.Uint16Array = 7;                 // set before any read
        console.log(Uint16Array === 7);             // the assignment sticks...
        console.log(typeof Int32Array);             // ...and siblings exist
        console.log(new Int32Array(2).length);
        "#,
    );
    assert_eq!(out, ["true", "function", "2"]);
}

#[test]
fn reflection_never_observes_a_stub() {
    let out = eval_console(
        r#"
        // Descriptor read BEFORE any use: must look like an eager build's
        // ordinary data property (test262 Date/prop-desc.js shape).
        const d = Object.getOwnPropertyDescriptor(globalThis, "Date");
        console.log(typeof d.value, d.writable, d.enumerable, d.configurable);
        console.log(globalThis.__lookupGetter__("DataView") === undefined);
        const r = Reflect.getOwnPropertyDescriptor(globalThis, "Temporal");
        console.log("value" in r && !("get" in r));
        // defineProperty over an untouched stub sticks — a later sibling
        // read must not clobber it.
        Object.defineProperty(globalThis, "Uint8Array", { value: 9 });
        const ab = new ArrayBuffer(4);
        console.log(Uint8Array === 9, ab.byteLength);
        "#,
    );
    assert_eq!(out, ["function true false true", "true", "true", "true 4"]);
}

#[test]
fn stubs_are_deletable_typeof_safe_and_non_enumerable() {
    let out = eval_console(
        r#"
        console.log(delete globalThis.Temporal, typeof globalThis.Temporal);
        console.log("Date" in globalThis);
        console.log(Object.getOwnPropertyNames(globalThis).includes("Intl"));
        // Non-enumerable: lazy names never show up in enumeration.
        console.log(Object.keys(globalThis).includes("Date"));
        "#,
    );
    assert_eq!(out, ["true undefined", "true", "true", "false"]);
}
