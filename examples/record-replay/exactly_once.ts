import type { Chidori } from "chidori:agent";

/**
 * Exactly-once side effects — the core durability guarantee for agents.
 *
 * This agent files a ticket and emails a user: two real-world actions, modelled
 * as `tool()` calls. Each tool call is recorded in the run's call log. When the
 * run is replayed (`chidori resume`, or `AgentClient.replay`), those tool calls
 * are served from the log instead of re-executing — so the ticket isn't filed
 * twice and the email isn't sent twice, no matter how many times you replay.
 *
 *   chidori run examples/record-replay/exactly_once.ts -i name=Ada
 *   chidori trace <run-id>            # see the recorded tool calls
 *   chidori resume examples/record-replay/exactly_once.ts <run-id>   # no re-send
 */
export async function agent(input: { name?: string }, chidori: Chidori) {
  const name = input.name ?? "Ada";

  const ticket = await chidori.tool<{ subject: string }, { id: string }>("open_ticket", {
    subject: `onboard ${name}`,
  });

  const email = await chidori.tool<{ to: string; ticket: string }, { id: string; delivered: boolean }>(
    "send_email",
    { to: `${name.toLowerCase()}@example.com`, ticket: ticket.id },
  );

  // Persist the outcome in durable memory so the result survives resume too.
  await chidori.memory.set("onboarding", { ticket: ticket.id, email: email.id });

  return { ticket: ticket.id, emailId: email.id, delivered: email.delivered };
}
