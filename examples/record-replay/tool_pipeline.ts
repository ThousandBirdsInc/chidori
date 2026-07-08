import type { Chidori } from "chidori:agent";

/**
 * Deterministic fan-out + a durable artifact.
 *
 * Agents often fan out to several tools at once, then combine the results. Here
 * we geocode a city, then in parallel fetch weather and an FX rate, and stash a
 * briefing in durable memory. `parallel()` runs the branches concurrently, but
 * each underlying tool call is recorded with a stable key, so replay reproduces
 * the exact same combined result regardless of which branch finished first.
 *
 * (To write a real file artifact instead of memory, use
 * `chidori.workspace.write(...)` and run with CHIDORI_WORKSPACE_ROOT set to a
 * directory — see the README.)
 */
export async function agent(
  input: { city?: string; currency?: string },
  chidori: Chidori,
) {
  const city = input.city ?? "Berlin";
  const currency = input.currency ?? "EUR";

  type Weather = { tempC: number; conditions: string };
  type Fx = { usdPerUnit: number };

  const loc = await chidori.tool<{ city: string }, { lat: number; lng: number }>("geocode", {
    city,
  });

  // The explicit tuple type keeps each branch's result type distinct.
  const [weather, fx] = await chidori.util.parallel<
    [() => Promise<Weather>, () => Promise<Fx>]
  >([
    () => chidori.tool<{ lat: number; lng: number }, Weather>("weather", {
      lat: loc.lat,
      lng: loc.lng,
    }),
    () => chidori.tool<{ currency: string }, Fx>("fx_rate", { currency }),
  ]);

  const briefing =
    `${city}: ${weather.tempC}°C, ${weather.conditions}. ` +
    `1 ${currency} = $${fx.usdPerUnit.toFixed(4)}.`;

  await chidori.memory.set(`briefing:${city}`, { city, currency, briefing });

  return { city, currency, briefing };
}
