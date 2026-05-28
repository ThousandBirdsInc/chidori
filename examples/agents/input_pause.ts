import type { Chidori } from "chidori";

export async function agent(input: { request: string }, chidori: Chidori) {
  const approval = await chidori.input("Approve this request?", {
    type: "approval",
    choices: ["yes", "no"],
  });
  return {
    request: input.request,
    approved: approval.toLowerCase() === "yes",
  };
}
