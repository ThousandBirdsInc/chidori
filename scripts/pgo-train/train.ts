// PGO training agent for the shipped `chidori` binary (scripts/pgo-build.sh
// --bin chidori). Mirrors the interpreter hot paths of the cross-runtime
// benchmark suite (crates/chidori-js/benchmarks/workloads/) — arithmetic,
// calls/recursion, closures, arrays, higher-order functions, strings, JSON,
// property access, comparator sorts — inside a real agent run, so the profile
// also covers the runtime seams a plain script misses (TS strip, journal,
// host-effect boundary). Deterministic and fully offline: no LLM calls, no
// tools, no timers. Sizes are scaled so the whole run stays around a second —
// PGO needs branch *frequencies*, not a long benchmark.
import { chidori, run } from "chidori:agent";

run(async () => {
  const checksums: Record<string, number> = {};

  // arith_loop: integer/double mix in a tight loop.
  {
    let acc = 0;
    for (let i = 0; i < 2_000_000; i++) acc = (acc + i * 3) % 1_000_003;
    checksums.arith = acc;
  }

  // fib_recursive: call/frame setup and teardown.
  {
    const fib = (n: number): number => (n < 2 ? n : fib(n - 1) + fib(n - 2));
    checksums.fib = fib(24);
  }

  // closures: captured-variable access through nested scopes.
  {
    const make = (base: number) => (x: number) => base + x;
    let acc = 0;
    const adders = [make(1), make(2), make(3)];
    for (let i = 0; i < 300_000; i++) acc = (acc + adders[i % 3](i)) % 1_000_003;
    checksums.closures = acc;
  }

  // arrays + HOF: push growth, index reads, map/filter/reduce.
  {
    const a: number[] = [];
    for (let i = 0; i < 120_000; i++) a.push((i * 7) % 251);
    const sum = a
      .map((x) => x * 2)
      .filter((x) => x % 3 !== 0)
      .reduce((s, x) => (s + x) % 1_000_003, 0);
    checksums.arrays = sum;
  }

  // strings: append growth and slicing.
  {
    let s = "";
    for (let i = 0; i < 20_000; i++) s += "ab" + (i % 10);
    checksums.strings = s.length + s.charCodeAt(1234);
  }

  // json_roundtrip: stringify/parse over a nested object.
  {
    const obj = {
      id: 0,
      name: "widget",
      tags: ["a", "b", "c"],
      nested: { x: 1, y: 2, z: { deep: true, items: [1, 2, 3, 4, 5] } },
    };
    let acc = 0;
    for (let i = 0; i < 4_000; i++) {
      obj.id = i;
      const back = JSON.parse(JSON.stringify(obj)) as typeof obj;
      acc = (acc + back.id + back.nested.z.items[i % 5]) % 1_000_003;
    }
    checksums.json = acc;
  }

  // property_access: repeated named get/set on a stable shape.
  {
    const o = { a: 1, b: 2, c: 3, d: 4 };
    let acc = 0;
    for (let i = 0; i < 500_000; i++) {
      o.a = i;
      acc = (acc + o.a + o.b + o.c + o.d) % 1_000_003;
    }
    checksums.props = acc;
  }

  // sort: comparator-driven sort over LCG data (deterministic input).
  {
    let seed = 123456789;
    const rnd = () => (seed = (seed * 1103515245 + 12345) >>> 0);
    let acc = 0;
    for (let r = 0; r < 3; r++) {
      const a = new Array(20_000);
      for (let i = 0; i < a.length; i++) a[i] = rnd();
      a.sort((x: number, y: number) => x - y);
      acc = (acc + a[0] + a[a.length - 1] + a[a.length >> 1]) >>> 0;
    }
    checksums.sort = acc;
  }

  // One host-effect round trip so the journal/effect boundary is in profile.
  await chidori.log("pgo training checksums", checksums);
  return checksums;
});
