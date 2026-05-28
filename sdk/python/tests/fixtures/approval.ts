export async function agent(input, chidori) {
  const answer = await chidori.input("Approve `" + input.action + "`?");
  return {
    action: input.action,
    approved: answer.toLowerCase().startsWith("y"),
  };
}
