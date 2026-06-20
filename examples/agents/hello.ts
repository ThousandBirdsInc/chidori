import type { Chidori } from "chidori:agent";

export async function agent(input: { name?: string }, chidori: Chidori) {
  const name = input.name ?? "world";
  await chidori.log("Saying hello", { name });
  return { greeting: `Hello, ${name}!` };
}
