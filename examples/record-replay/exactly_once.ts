import { chidori, run, defineTool } from "chidori:agent";

/**
 * Exactly-once side effects — the core durability guarantee for agents.
 *
 * This agent files a ticket and emails a user: two real-world actions, modelled
 * as `defineTool` tools. A tool body runs in the agent's own VM, so what makes
 * a side effect exactly-once is that it goes through a RECORDED HOST CALL —
 * here `chidori.log` stands in for the real API call. When the run is replayed
 * (`chidori resume`, or `AgentClient.replay`), those host calls are served from
 * the call log instead of re-executing — so the ticket isn't filed twice and
 * the email isn't sent twice, no matter how many times you replay. (The tool's
 * wrapper code re-runs on replay; only its host calls are journaled — which is
 * exactly why the side effect must be a host call.)
 *
 *   chidori run examples/record-replay/exactly_once.ts -i name=Ada --trusted
 *   chidori trace <run-id>            # see the recorded host calls
 *   chidori resume examples/record-replay/exactly_once.ts <run-id>   # no re-send
 */

const openTicket = defineTool({
  name: "open_ticket",
  description: "Open a support ticket (external side effect).",
  parameters: {
    type: "object",
    properties: { subject: { type: "string" } },
    required: ["subject"],
  },
  run: async (args: { subject: string }) => {
    // The real create-ticket call would happen here, through a host call
    // (chidori.fetch / chidori.log) so it is recorded and replayed once.
    await chidori.log("open_ticket (SIDE EFFECT)", { subject: args.subject });
    return { id: "ticket-1" };
  },
});

const sendEmail = defineTool({
  name: "send_email",
  description: "Send a transactional email (external side effect).",
  parameters: {
    type: "object",
    properties: { to: { type: "string" }, ticket: { type: "string" } },
    required: ["to"],
  },
  run: async (args: { to: string; ticket?: string }) => {
    await chidori.log("send_email (SIDE EFFECT)", { to: args.to, ticket: args.ticket ?? null });
    return { id: "msg-1", delivered: true, to: args.to };
  },
});

run(async (input: { name?: string }) => {
  const name = input.name ?? "Ada";

  const ticket = (await openTicket.run({ subject: `onboard ${name}` }, chidori)) as {
    id: string;
  };
  const email = (await sendEmail.run(
    { to: `${name.toLowerCase()}@example.com`, ticket: ticket.id },
    chidori,
  )) as { id: string; delivered: boolean };

  // Persist the outcome in durable memory so the result survives resume too.
  await chidori.memory.set("onboarding", { ticket: ticket.id, email: email.id });

  return { ticket: ticket.id, emailId: email.id, delivered: email.delivered };
});
