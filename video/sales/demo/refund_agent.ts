import { chidori, run, defineTool } from "chidori:agent";

const issueRefund = defineTool({
  name: "issue_refund",
  description: "Issue a refund through the payments API.",
  parameters: { type: "object", properties: { order: { type: "string" }, cents: { type: "number" } }, required: ["order", "cents"] },
  run: async (a: { order: string; cents: number }) => {
    await chidori.log("issue_refund (SIDE EFFECT)", a);
    return { refundId: "re_1007", status: "succeeded" };
  },
});

run(async (input: { order: string; complaint: string }) => {
  const decision = await chidori.prompt(
    `Decide a refund for order ${input.order}: ${input.complaint}`,
  );
  const answer = await chidori.input(`Approve? ${decision}`, {
    type: "approval", choices: ["approve", "deny"],
  });
  if (answer !== "approve") return { status: "denied" };
  const refund = await issueRefund.run({ order: input.order, cents: 4200 }, chidori);
  return { decision, refund };
});
