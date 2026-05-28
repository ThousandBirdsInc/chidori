export async function agent(input, chidori) {
  const answer = await chidori.prompt("Q: " + input.question, {
    model: "mock-model",
  });
  return { question: input.question, answer };
}
