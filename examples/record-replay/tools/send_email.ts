import type { Chidori, ToolDefinition } from "chidori";

// A side-effecting tool. The `chidori.log` line marks where a real send would
// happen — on replay this tool is NOT re-invoked, so the email is sent exactly
// once across the original run and any number of replays.
export const tool: ToolDefinition = {
  name: "send_email",
  description: "Send a transactional email (external side effect).",
  parameters: {
    type: "object",
    properties: {
      to: { type: "string", description: "Recipient address" },
      ticket: { type: "string", description: "Related ticket id" },
    },
    required: ["to"],
  },
};

// The real send happens here and is recorded once (the chidori.log marks it);
// replay serves the result without re-sending. See open_ticket.ts.
export async function run(args: { to: string; ticket?: string }, chidori: Chidori) {
  await chidori.log("send_email (SIDE EFFECT)", { to: args.to, ticket: args.ticket ?? null });
  return { delivered: true, id: "msg-1", to: args.to };
}
