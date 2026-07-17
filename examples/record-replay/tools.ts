import { chidori, defineTool } from "chidori:agent";

// Offline stand-in tools for the record-replay examples, defined with
// `defineTool` and imported like any other module — no `tools/` directory, no
// `--tools` flag. Each body runs in the agent's own VM and routes its
// observable effect through a recorded host call (`chidori.log`), so replay
// reproduces the exact history without touching a live service.

// Deliberately flaky, deterministic on the attempt number the caller passes:
// attempts 1 and 2 report failure, the third succeeds. Failure is a returned
// `{ ok: false }` flag (not a throw): the value is what the retry loop branches
// on, and a return-flag failure replays cleanly.
export const flakyFetch = defineTool({
  name: "flaky_fetch",
  description: "Fetch a config value from a flaky upstream service.",
  parameters: {
    type: "object",
    properties: {
      key: { type: "string", description: "Config key to fetch" },
      attempt: { type: "integer", description: "1-based attempt number" },
    },
    required: ["key", "attempt"],
  },
  run: async (args: { key: string; attempt: number }) => {
    await chidori.log("flaky_fetch attempt", { key: args.key, attempt: args.attempt });
    if (args.attempt < 3) {
      return { ok: false, status: 503, error: "Service Unavailable" };
    }
    return { ok: true, status: 200, value: { flag: true }, servedOnAttempt: args.attempt };
  },
});

const GEO: Record<string, { lat: number; lng: number }> = {
  Berlin: { lat: 52.52, lng: 13.405 },
  Tokyo: { lat: 35.6762, lng: 139.6503 },
  Nairobi: { lat: -1.2921, lng: 36.8219 },
};

export const geocode = defineTool({
  name: "geocode",
  description: "Resolve a city name to coordinates.",
  parameters: {
    type: "object",
    properties: { city: { type: "string", description: "City name" } },
    required: ["city"],
  },
  run: async (args: { city: string }) => {
    await chidori.log("geocode", { city: args.city });
    const coords = GEO[args.city] ?? { lat: 0, lng: 0 };
    return { city: args.city, ...coords };
  },
});

export const weather = defineTool({
  name: "weather",
  description: "Look up current weather for coordinates.",
  parameters: {
    type: "object",
    properties: {
      lat: { type: "number", description: "Latitude" },
      lng: { type: "number", description: "Longitude" },
    },
    required: ["lat", "lng"],
  },
  run: async (args: { lat: number; lng: number }) => {
    await chidori.log("weather", { lat: args.lat, lng: args.lng });
    const tempC = Math.round((15 + args.lat / 5) * 10) / 10;
    return { tempC, conditions: tempC > 18 ? "clear" : "cloudy" };
  },
});

const RATES: Record<string, number> = { EUR: 1.08, JPY: 0.0064, KES: 0.0077 };

export const fxRate = defineTool({
  name: "fx_rate",
  description: "Get the USD exchange rate for a currency.",
  parameters: {
    type: "object",
    properties: { currency: { type: "string", description: "ISO currency code" } },
    required: ["currency"],
  },
  run: async (args: { currency: string }) => {
    await chidori.log("fx_rate", { currency: args.currency });
    return { currency: args.currency, usdPerUnit: RATES[args.currency] ?? 1 };
  },
});
