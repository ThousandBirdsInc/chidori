import type { Chidori, ToolDefinition } from "chidori:agent";

// Offline stand-in for a weather API, keyed on coordinates so it pairs with
// geocode in the fan-out example.
export const tool: ToolDefinition = {
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
};

export async function run(args: { lat: number; lng: number }, chidori: Chidori) {
  await chidori.log("weather", { lat: args.lat, lng: args.lng });
  // Deterministic pseudo-temperature derived from the coordinates.
  const tempC = Math.round((15 + args.lat / 5) * 10) / 10;
  return { tempC, conditions: tempC > 18 ? "clear" : "cloudy" };
}
