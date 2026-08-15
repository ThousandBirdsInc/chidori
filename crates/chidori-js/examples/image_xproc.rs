//! Cross-process image verification: `record <file>` suspends an agent and
//! writes the durable artifact; `resume <file>` — run as a SEPARATE process —
//! restores it and finishes the run. Different address space, so any hidden
//! dependence on pointer values or process-local hash order breaks it.
use chidori_js::replay::{DriveOutcome, ReplayRuntime, RestorePath};
use serde_json::{json, Value as Json};

const BUNDLE: &str = r#"
    class Tracker { #hits = 0; hit() { return ++this.#hits; } }
    const t = new Tracker();
    const seen = [];
    function* ids() { let i = 0; while (true) yield ++i; }
    const gen = ids();
    gen.next(); gen.next();
    async function main() {
        t.hit(); t.hit();
        seen.push(await fetchValue('a'));
        seen.push(await fetchValue('b'));
        report({ seen, hits: t.hit(), id: gen.next().value });
    }
    main();
"#;
const EFFECTS: &[&str] = &["fetchValue", "report"];

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args[1].as_str() {
        "record" => {
            let mut rt = ReplayRuntime::record(BUNDLE, EFFECTS);
            rt.enable_imaging();
            // Serve 'a' live, suspend at 'b'.
            let mut h = |name: &str, a: &Json| -> Option<Result<Json, String>> {
                match name {
                    "fetchValue" if a[0] == json!("a") => Some(Ok(json!(10))),
                    "fetchValue" => None,
                    _ => Some(Ok(json!(null))),
                }
            };
            let op = match rt.drive(&mut h).unwrap() {
                DriveOutcome::Suspended { op_id, .. } => op_id,
                _ => panic!("expected suspension"),
            };
            std::fs::write(&args[2], rt.to_blob(EFFECTS)).unwrap();
            println!("suspended op={op}");
        }
        "resume" => {
            let blob = std::fs::read(&args[2]).unwrap();
            let (mut rt, path) = ReplayRuntime::from_blob_reporting(&blob).unwrap();
            assert_eq!(
                path,
                RestorePath::Image,
                "must restore via image, got {path:?}"
            );
            let op: u64 = args[3].parse().unwrap();
            let mut out = None;
            let mut h = |name: &str, a: &Json| -> Option<Result<Json, String>> {
                if name == "report" {
                    out = Some(a[0].clone());
                }
                Some(Ok(json!(32)))
            };
            match rt.provide_and_drive(op, Ok(json!(20)), &mut h).unwrap() {
                DriveOutcome::Completed => {}
                other => panic!("expected completion, got {other:?}"),
            }
            let got = out.expect("report ran");
            assert_eq!(
                got,
                json!({"seen": [10, 20], "hits": 3, "id": 3}),
                "wrong result: {got}"
            );
            println!("cross-process resume OK: {got}");
        }
        _ => panic!("usage: image_xproc record|resume <file> [op]"),
    }
}
