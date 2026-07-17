import { chidori, run } from "chidori:agent";
import { geocode, weather, fxRate } from "./tools.ts";

/**
 * Deterministic fan-out + a durable artifact.
 *
 * Agents often fan out to several tools at once, then combine the results. Here
 * we geocode a city, then in parallel fetch weather and an FX rate, and stash a
 * briefing in durable memory. `parallel()` runs the branches concurrently, but
 * each tool's host calls are recorded, so replay reproduces the exact same
 * combined result regardless of which branch finished first.
 *
 * (To write a real file artifact instead of memory, use
 * `chidori.workspace.write(...)` and run with CHIDORI_WORKSPACE_ROOT set to a
 * directory — see the README.)
 */
run(async (input: { city?: string; currency?: string }) => {
  const city = input.city ?? "Berlin";
  const currency = input.currency ?? "EUR";

  type Weather = { tempC: number; conditions: string };
  type Fx = { usdPerUnit: number };

  const loc = (await geocode.run({ city }, chidori)) as { lat: number; lng: number };

  // The explicit tuple type keeps each branch's result type distinct.
  const [w, fx] = await chidori.util.parallel<[() => Promise<Weather>, () => Promise<Fx>]>([
    () => weather.run({ lat: loc.lat, lng: loc.lng }, chidori) as Promise<Weather>,
    () => fxRate.run({ currency }, chidori) as Promise<Fx>,
  ]);

  const briefing =
    `${city}: ${w.tempC}°C, ${w.conditions}. ` +
    `1 ${currency} = $${fx.usdPerUnit.toFixed(4)}.`;

  await chidori.memory.set(`briefing:${city}`, { city, currency, briefing });

  return { city, currency, briefing };
});
