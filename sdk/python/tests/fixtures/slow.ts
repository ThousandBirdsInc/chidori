export async function agent(input, chidori) {
  await chidori.prompt("Hold concurrency slot for " + input.label, {
    model: "mock-model",
  });
  return { label: input.label };
}
