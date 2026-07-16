// Experiment E6: JS engine coverage probe.
// chidori-js is a from-scratch pure-Rust JS engine. This agent exercises a
// spread of modern ECMAScript features an average agent author will reach
// for, and reports which ones work. Each probe is isolated in try/catch so
// one missing feature doesn't hide the rest.
import { chidori, run } from "chidori:agent";

type ProbeResult = { ok: boolean; detail: string };

function probe(name: string, fn: () => unknown): [string, ProbeResult] {
  try {
    const v = fn();
    return [name, { ok: true, detail: String(v).slice(0, 80) }];
  } catch (e) {
    return [name, { ok: false, detail: String(e).slice(0, 120) }];
  }
}

async function probeAsync(name: string, fn: () => Promise<unknown>): Promise<[string, ProbeResult]> {
  try {
    const v = await fn();
    return [name, { ok: true, detail: String(v).slice(0, 80) }];
  } catch (e) {
    return [name, { ok: false, detail: String(e).slice(0, 120) }];
  }
}

run(async () => {
  const results: [string, ProbeResult][] = [];

  results.push(probe("optional chaining / nullish", () => ({ a: { b: 1 } } as any)?.a?.b ?? 2));
  results.push(probe("BigInt arithmetic", () => (2n ** 64n).toString()));
  results.push(probe("regex lookbehind", () => "price: $42".match(/(?<=\$)\d+/)?.[0]));
  results.push(probe("regex named groups", () => "2026-07-16".match(/(?<y>\d{4})-(?<m>\d{2})/)?.groups?.y));
  results.push(probe("regex unicode property", () => /\p{Script=Greek}/u.test("π")));
  results.push(probe("String.matchAll", () => [..."a1b2".matchAll(/\d/g)].length));
  results.push(probe("Array.flat/flatMap", () => [[1, [2]], [3]].flat(2).length));
  results.push(probe("Array.at(-1)", () => [1, 2, 3].at(-1)));
  results.push(probe("Object.fromEntries", () => Object.fromEntries([["k", 1]]).k));
  results.push(probe("String.replaceAll", () => "aaa".replaceAll("a", "b")));
  results.push(probe("logical assignment", () => { let x: any = null; x ??= 5; return x; }));
  results.push(probe("class private fields", () => {
    class C { #x = 7; get x() { return this.#x; } }
    return new C().x;
  }));
  results.push(probe("static class blocks", () => {
    class C { static v: number; static { C.v = 9; } }
    return (C as any).v;
  }));
  results.push(probe("Array.findLast", () => [1, 2, 3].findLast((x) => x < 3)));
  results.push(probe("structuredClone", () => (globalThis as any).structuredClone({ a: [1] }).a[0]));
  results.push(probe("Intl.NumberFormat", () => new Intl.NumberFormat("en-US").format(1234567)));
  results.push(probe("Intl.DateTimeFormat", () => new Intl.DateTimeFormat("en-US").format(new Date(0))));
  results.push(probe("Symbol.iterator protocol", () => {
    const it = { *[Symbol.iterator]() { yield 1; yield 2; } };
    return [...it].length;
  }));
  results.push(probe("Proxy + Reflect", () => {
    const p = new Proxy({}, { get: (_t, k) => Reflect.ownKeys({ [k]: 1 }).length });
    return (p as any).anything;
  }));
  results.push(probe("WeakRef", () => typeof new WeakRef({}).deref()));
  results.push(probe("Temporal (stage 3)", () => (globalThis as any).Temporal.Now ? "present" : "partial"));
  results.push(probe("Error.cause", () => new Error("outer", { cause: new Error("inner") }).cause instanceof Error));
  results.push(probe("Array.groupBy/Object.groupBy", () => Object.keys((Object as any).groupBy([1, 2, 3], (x: number) => x % 2)).length));
  results.push(probe("TextEncoder", () => new TextEncoder().encode("hi").length));
  results.push(probe("atob/btoa", () => atob(btoa("chidori"))));
  results.push(probe("queueMicrotask", () => { queueMicrotask(() => {}); return "ok"; }));
  results.push(probe("TypedArray + DataView", () => {
    const dv = new DataView(new ArrayBuffer(8));
    dv.setFloat64(0, 3.14);
    return dv.getFloat64(0);
  }));

  results.push(await probeAsync("async generators + for-await", async () => {
    async function* gen() { yield 1; yield 2; yield 3; }
    let sum = 0;
    for await (const v of gen()) sum += v;
    return sum;
  }));
  results.push(await probeAsync("Promise.allSettled", async () =>
    (await Promise.allSettled([Promise.resolve(1), Promise.reject(new Error("x"))])).map((r) => r.status).join(",")));
  results.push(await probeAsync("Promise.any", async () => Promise.any([Promise.reject(new Error("a")), Promise.resolve("b")])));
  results.push(await probeAsync("dynamic import()", async () => {
    try { await import("data:text/javascript,export const v=1"); return "supported"; }
    catch (e) { throw e; }
  }));

  const passed = results.filter(([, r]) => r.ok).length;
  await chidori.log(`JS conformance probe: ${passed}/${results.length} passed`);
  return { passed, total: results.length, results: Object.fromEntries(results) };
});
