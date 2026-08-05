//! ES2025 duplicate named capture groups: the same `(?<name>…)` may appear more
//! than once as long as the groups sit in different alternatives of some
//! enclosing disjunction, so at most one of them can ever participate in a
//! match. These pins cover the three surfaces that observe it — the parser's
//! early error, `groups`/`indices.groups` on a match result, and `\k<name>`
//! backreference resolution — plus the RepeatMatcher capture reset the feature
//! leans on (each iteration of a quantifier starts with its body's groups
//! marked as not-participating).

use chidori_js::Engine;

/// Evaluate `src` and return its `console.log` output, one entry per line.
/// A thrown error is reported as `ERR: <message>` so syntax errors can be
/// asserted the same way as values.
fn eval_console(src: &str) -> String {
    let mut e = Engine::new();
    match e.eval(src) {
        Ok(_) => e.console().join("\n"),
        Err(err) => format!("ERR: {err}"),
    }
}

#[test]
fn duplicates_in_separate_alternatives_parse_but_same_alternative_is_a_syntax_error() {
    let out = eval_console(
        r#"
        const ok = new RegExp("(?<x>a)|(?<x>b)");
        console.log(ok.source);
        try {
            new RegExp("(?<x>a)(?<x>b)");
            console.log("no-error");
        } catch (e) {
            console.log(e instanceof SyntaxError ? "SyntaxError" : "other:" + e);
        }
        // A group outside the disjunction can participate alongside either
        // duplicate, so it clashes with both.
        try {
            new RegExp("(?:(?<x>a)|(?<x>b))(?<x>c)");
            console.log("no-error");
        } catch (e) {
            console.log(e instanceof SyntaxError ? "SyntaxError" : "other:" + e);
        }
        "#,
    );
    assert_eq!(out, "(?<x>a)|(?<x>b)\nSyntaxError\nSyntaxError");
}

#[test]
fn groups_reports_whichever_duplicate_participated() {
    // Source order fixes the enumeration order of `groups`, independently of
    // which alternative matched.
    let out = eval_console(
        r#"
        const re = /(?:(?<x>a)|(?<y>a)(?<x>b))(?:(?<z>c)|(?<z>d))/;
        const show = (g) => [g.x, g.y, g.z].map(String).join(",");
        const three = re.exec("abc");
        console.log(show(three.groups));
        console.log(Object.keys(three.groups).join(","));
        console.log(show(re.exec("ad").groups));
        console.log(JSON.stringify(/(?<x>a)|(?<x>b)/.exec("bab")));
        "#,
    );
    assert_eq!(out, "b,a,c\nx,y,z\na,undefined,d\n[\"b\",null,\"b\"]");
}

#[test]
fn named_backreference_follows_the_participating_duplicate() {
    let out = eval_console(
        r#"
        const re = /(?:(?<x>a)|(?<x>b))\k<x>/;
        console.log(JSON.stringify(re.exec("aa")));
        console.log(JSON.stringify(re.exec("bb")));
        console.log(String(re.exec("abab")));
        // No duplicate participated: `\k<a>` matches the empty string.
        console.log(JSON.stringify(/(?<a>x)|(?:zy\k<a>)/.exec("zy")));
        "#,
    );
    assert_eq!(
        out,
        "[\"aa\",\"a\",null]\n[\"bb\",null,\"b\"]\nnull\n[\"zy\",null]"
    );
}

#[test]
fn quantifier_resets_its_body_captures_each_iteration() {
    // Only the last iteration's captures survive, so the group that matched in
    // an earlier round reads as undefined — the behavior `\k<name>` and
    // `groups.name` both depend on across repetitions.
    let out = eval_console(
        r#"
        const m = /(?:(?:(?<x>a)|(?<x>b))\k<x>){2}/.exec("aabb");
        console.log(JSON.stringify(m) + "|" + m.groups.x);
        console.log(String(/(?:(?:(?<x>a)|(?<x>b))\k<x>){2}/.exec("abab")));
        console.log(String(/(?:(?:(?<x>a)|(?<x>b)|c)\k<x>){2}/.exec("aac").groups.x));
        console.log(JSON.stringify(/^(?:(?<a>x)|(?<a>y)|z){2}\k<a>$/.exec("xz")));
        // The reset is not specific to named groups.
        console.log(JSON.stringify(/(?:(a)|(b))+/.exec("ab")));
        "#,
    );
    assert_eq!(
        out,
        "[\"aabb\",null,\"b\"]|b\nnull\nundefined\n[\"xz\",null,null]\n[\"ab\",null,\"b\"]"
    );
}

#[test]
fn match_indices_mirror_the_participating_duplicate() {
    let out = eval_console(
        r#"
        console.log(JSON.stringify("..ab".match(/(?<x>a)|(?<x>b)/d).indices.groups.x));
        console.log(JSON.stringify("..ba".match(/(?<x>a)|(?<x>b)/d).indices.groups.x));
        const r = /(?:(?<x>a)|(?<y>a)(?<x>b))(?:(?<z>c)|(?<z>d))/d.exec("ad");
        console.log(JSON.stringify(r.indices.groups.x) + "|" + r.indices.groups.y);
        console.log(Object.keys(r.indices.groups).join(","));
        "#,
    );
    assert_eq!(out, "[2,3]\n[2,3]\n[0,1]|undefined\nx,y,z");
}

#[test]
fn replace_and_split_see_the_participating_duplicate() {
    let out = eval_console(
        r#"
        console.log("ab".replace(/(?<x>a)|(?<x>b)/, "[$<x>]"));
        console.log("ba".replace(/(?<x>a)|(?<x>b)/, "[$<x>][$1][$2]"));
        console.log("ab".replace(/(?<x>a)|(?<x>b)/g, "[$<x>]"));
        console.log(JSON.stringify("xab".split(/(?<x>a)|(?<x>b)/)));
        "#,
    );
    assert_eq!(
        out,
        "[a]b\n[b][][b]a\n[a][b]\n[\"x\",\"a\",null,\"\",null,\"b\",\"\"]"
    );
}
