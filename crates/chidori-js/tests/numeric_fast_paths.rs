//! Pins the integer fast paths added to `ToInt32`/`ToUint32` and Number `%`
//! against their spec-shaped reference computations: the fast paths must be
//! bit-identical (including the sign of zero) to the general `rem_euclid` /
//! libm `fmod` routes they short-circuit, across the i32/u32 boundaries,
//! the 2^53 exactness bound, and the 2^63 cast bound.

use chidori_js::vm::{to_int32, to_uint32};
use chidori_js::Engine;

/// The pre-fast-path `ToInt32`: truncate, reduce modulo 2^32 via
/// `rem_euclid`, reinterpret signed.
fn ref_to_int32(n: f64) -> i32 {
    if !n.is_finite() || n == 0.0 {
        return 0;
    }
    let m = n.trunc().rem_euclid(4294967296.0);
    let m = if m >= 2147483648.0 {
        m - 4294967296.0
    } else {
        m
    };
    m as i32
}

fn ref_to_uint32(n: f64) -> u32 {
    if !n.is_finite() || n == 0.0 {
        return 0;
    }
    n.trunc().rem_euclid(4294967296.0) as u32
}

/// The spec `%` for Numbers, straight off libm `fmod` plus the special cases.
fn ref_mod(a: f64, b: f64) -> f64 {
    if b == 0.0 || a.is_nan() || b.is_nan() || a.is_infinite() {
        f64::NAN
    } else if b.is_infinite() || a == 0.0 {
        a
    } else {
        a % b
    }
}

fn probe_values() -> Vec<f64> {
    let mut v = vec![
        0.0,
        -0.0,
        1.0,
        -1.0,
        0.5,
        -0.5,
        1.5,
        -2.5,
        3.0,
        -3.0,
        7.0,
        10.0,
        65521.0,
        1e6,
        2147483647.0,
        2147483648.0,
        -2147483648.0,
        -2147483649.0,
        4294967295.0,
        4294967296.0,
        4294967297.0,
        -4294967296.0,
        1103515245.0 * 4294967295.0 + 12345.0,
        9007199254740991.0,
        9007199254740992.0,
        9007199254740993.0,
        -9007199254740992.0,
        1e18,
        9.2e18,
        9223372036854775807.0,
        9223372036854775808.0,
        -9223372036854775808.0,
        1e19,
        1e20,
        -1e20,
        1e300,
        f64::MAX,
        f64::MIN,
        f64::EPSILON,
        f64::MIN_POSITIVE,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NAN,
    ];
    // A deterministic LCG sweep, including the >>> 0 idiom's full-width
    // products.
    let mut seed: u64 = 123456789;
    for _ in 0..400 {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let x = (seed >> 11) as f64;
        v.push(x);
        v.push(-x);
        v.push(x / 1024.0);
        v.push((seed % 100000) as f64 * 1103515245.0 + 12345.0);
    }
    v
}

#[test]
fn to_int32_and_to_uint32_match_reference() {
    for &n in &probe_values() {
        assert_eq!(to_int32(n), ref_to_int32(n), "to_int32({n:?})");
        assert_eq!(to_uint32(n), ref_to_uint32(n), "to_uint32({n:?})");
    }
}

#[test]
fn number_mod_matches_reference_bitwise() {
    let vals = probe_values();
    let mut e = Engine::new();
    // Drive `%` through the engine (register/kernel tiers included) in one
    // batch: a JS loop over the same operand table, emitting the result bits
    // via a Float64Array/BigUint64Array view so -0 and NaN compare exactly.
    let mut src = String::from("(function(){ const v = [");
    for n in &vals {
        if n.is_nan() {
            src.push_str("NaN,");
        } else if n.is_infinite() {
            src.push_str(if *n > 0.0 { "Infinity," } else { "-Infinity," });
        } else if *n == 0.0 && n.is_sign_negative() {
            src.push_str("-0,");
        } else {
            src.push_str(&format!("{n:e},"));
        }
    }
    src.push_str(
        "]; const f = new Float64Array(1); const u = new BigUint64Array(f.buffer); \
         const out = []; for (let i = 0; i < v.length; i++) { for (let j = 0; j < v.length; j += 13) { \
         f[0] = v[i] % v[j]; out.push(u[0].toString(16)); } } return out.join(','); })()",
    );
    let got = e.eval(&src).expect("eval");
    let got = e.vm.to_string_lossy(&got);
    let mut expected = Vec::new();
    for a in &vals {
        let mut j = 0;
        while j < vals.len() {
            let r = ref_mod(*a, vals[j]);
            // NaN payloads are not observable in JS; canonicalize.
            let bits = if r.is_nan() {
                f64::NAN.to_bits()
            } else {
                r.to_bits()
            };
            expected.push(format!("{bits:x}"));
            j += 13;
        }
    }
    let got: Vec<&str> = got.split(',').collect();
    assert_eq!(got.len(), expected.len());
    for (k, (g, e)) in got.iter().zip(expected.iter()).enumerate() {
        // The engine may surface a NaN with any payload; compare canonically.
        let g_bits = u64::from_str_radix(g, 16).unwrap();
        let g_f = f64::from_bits(g_bits);
        let g = if g_f.is_nan() {
            format!("{:x}", f64::NAN.to_bits())
        } else {
            g.to_string()
        };
        assert_eq!(&g, e, "pair #{k}");
    }
}

#[test]
fn unsigned_shift_lcg_matches_node() {
    // The `sort` benchmark's LCG: products past 2^32 reduced by `>>> 0`.
    let mut e = Engine::new();
    let v = e
        .eval(
            "(function(){ let seed = 123456789; let acc = 0; for (let i = 0; i < 1000; i++) { \
             seed = (seed * 1103515245 + 12345) >>> 0; acc = (acc + seed) % 1000000007; } \
             return acc + ',' + seed + ',' + ((seed * 3 - 5e9) | 0) + ',' + ((-seed * 7) >>> 0); })()",
        )
        .expect("eval");
    let s = e.vm.to_string_lossy(&v);
    // Reference computed with the pre-fast-path definitions.
    let mut seed: f64 = 123456789.0;
    let mut acc: f64 = 0.0;
    for _ in 0..1000 {
        seed = ref_to_uint32(seed * 1103515245.0 + 12345.0) as f64;
        acc = (acc + seed) % 1000000007.0;
    }
    let expected = format!(
        "{},{},{},{}",
        acc,
        seed,
        ref_to_int32(seed * 3.0 - 5e9),
        ref_to_uint32(-seed * 7.0)
    );
    assert_eq!(s, expected);
}
