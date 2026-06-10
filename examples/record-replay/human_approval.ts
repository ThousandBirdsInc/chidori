import type { Chidori } from "chidori";

/**
 * Durable pause / resume — human-in-the-loop across process restarts.
 *
 * The agent assembles a refund and then calls `input()` to ask a human to
 * approve it. `input()` SUSPENDS the run: the server persists a checkpoint and
 * the process is free to exit. Later, a human answer drives a resume — over the
 * SDK with `client.resume(sessionId, "approve")`, or on the CLI with
 * `chidori resume`. Everything before the pause is replayed from the checkpoint;
 * the refund is only issued after approval, exactly once.
 *
 *   # via SDK (see driver.mjs):
 *   const s = await client.run({ order: "A-1007" });   // -> status "paused"
 *   const done = await client.resume(s.id, "approve");  // -> status "completed"
 */
export async function agent(input: { order?: string }, chidori: Chidori) {
  const order = input.order ?? "A-1007";

  // Pretend this came from an orders service; kept inline to stay offline.
  const refund = { order, amount: 4200, currency: "USD" };

  const decision = await chidori.input(`Approve a ${refund.amount} cent refund for ${order}?`, {
    type: "approval",
    choices: ["approve", "deny"],
  });

  if (decision.toLowerCase() === "approve") {
    // A real refund call would be a tool() here — recorded, so it never
    // double-refunds on replay.
    await chidori.memory("set", `refund:${order}`, { status: "refunded", ...refund });
    return { order, status: "refunded", amount: refund.amount };
  }

  return { order, status: "denied" };
}
