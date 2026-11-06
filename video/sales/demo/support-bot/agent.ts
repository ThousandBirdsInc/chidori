// The agent written on screen in the sales video (scenes "Write", "Run", "Recover", "Iterate").
// Identical to the on-screen code except for the API constant, which the video leaves out.
import { chidori, run, defineTool } from "chidori:agent";
const API = "https://support.example.com/api";

const lookupOrder = defineTool({
  name: "lookup_order",
  description: "Fetch an order by id",
  parameters: { type: "object", properties: { id: { type: "string" } } },
  run: async ({ id }) => (await fetch(`${API}/orders/${id}`)).json(),
});

run(async ({ ticket }: { ticket: string }) => {
  const reply = await chidori.prompt(`Resolve this ticket: ${ticket}`, {
    model: "claude-sonnet-4-6", tools: [lookupOrder], maxTurns: 6,
  });

  const ok = await chidori.input("Send this reply?", {
    type: "approval", choices: ["yes", "no"], details: reply,
  });

  if (ok === "yes") await fetch(`${API}/replies`, { method: "POST", body: reply });
  return { reply, sent: ok === "yes" };
});
