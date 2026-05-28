import type { Chidori } from "chidori";

export async function agent(input: { value?: number }, chidori: Chidori) {
  const value = input.value ?? 40;
  const result = await chidori.execJs(
    "const input = JSON.parse(ARGV[0]); JSON.stringify({ answer: input.value + 2 });",
    { timeoutMs: 1000 },
  );
  return { result, value };
}
