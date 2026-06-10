import type { Chidori, ToolDefinition } from "chidori";

// Offline stand-in for a geocoding API. Returns a fixed lat/lng per city so the
// fan-out example is deterministic without network access.
export const tool: ToolDefinition = {
  name: "geocode",
  description: "Resolve a city name to coordinates.",
  parameters: {
    type: "object",
    properties: { city: { type: "string", description: "City name" } },
    required: ["city"],
  },
};

const TABLE: Record<string, { lat: number; lng: number }> = {
  Berlin: { lat: 52.52, lng: 13.405 },
  Tokyo: { lat: 35.6762, lng: 139.6503 },
  Nairobi: { lat: -1.2921, lng: 36.8219 },
};

export async function run(args: { city: string }, chidori: Chidori) {
  await chidori.log("geocode", { city: args.city });
  const coords = TABLE[args.city] ?? { lat: 0, lng: 0 };
  return { city: args.city, ...coords };
}
