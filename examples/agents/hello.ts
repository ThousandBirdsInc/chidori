import { chidori, run } from "chidori:agent";

run(async (input: { name?: string }) => {
  const name = input.name ?? "world";
  await chidori.log("Saying hello", { name });
  return { greeting: `Hello, ${name}!` };
});
