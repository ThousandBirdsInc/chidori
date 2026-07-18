import { chidori, run } from "chidori:agent";

run(async (input: { request: string }) => {
  const approval = await chidori.input("Approve this request?", {
    type: "approval",
    choices: ["yes", "no"],
  });
  return {
    request: input.request,
    approved: approval.toLowerCase() === "yes",
  };
});
