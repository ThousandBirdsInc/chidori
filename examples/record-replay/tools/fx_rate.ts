import type { Chidori, ToolDefinition } from "chidori:agent";

// Offline stand-in for an FX-rate API. Fixed table keeps the fan-out example
// deterministic.
export const tool: ToolDefinition = {
  name: "fx_rate",
  description: "Get the USD exchange rate for a currency.",
  parameters: {
    type: "object",
    properties: { currency: { type: "string", description: "ISO currency code" } },
    required: ["currency"],
  },
};

const RATES: Record<string, number> = { EUR: 1.08, JPY: 0.0064, KES: 0.0077 };

export async function run(args: { currency: string }, chidori: Chidori) {
  await chidori.log("fx_rate", { currency: args.currency });
  return { currency: args.currency, usdPerUnit: RATES[args.currency] ?? 1 };
}
