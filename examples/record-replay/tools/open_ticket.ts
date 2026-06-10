import type { Chidori, ToolDefinition } from "chidori";

// A side-effecting tool: in a real system this would POST to a ticketing API.
// Here it just mints a deterministic id so the example runs offline. The point
// is that the call is recorded once and served from the journal on replay — it
// never double-files a ticket.
export const tool: ToolDefinition = {
  name: "open_ticket",
  description: "Open a support ticket (external side effect).",
  parameters: {
    type: "object",
    properties: {
      subject: { type: "string", description: "Ticket subject" },
    },
    required: ["subject"],
  },
};

// Tool bodies may make nested host calls — here `chidori.log` marks the real
// side effect (the POST to the ticketing API would live right here). The replay
// path absorbs the tool's recorded subtree, so this logs once during record and
// is served from the call log on replay: the ticket is filed exactly once across
// the original run and any number of replays. (`console` is not available in the
// tool sandbox — use chidori.log.)
export async function run(args: { subject: string }, chidori: Chidori) {
  await chidori.log("open_ticket (SIDE EFFECT)", { subject: args.subject });
  return { id: "TICKET-0001", subject: args.subject, status: "open" };
}
