/**
 * Host-side implementations for the agent's `chidori.tool()` calls. Results
 * are journaled by the runtime, so the network-backed ones (weather) run
 * live exactly once and replay offline forever after.
 */
import { type DocsIndex, type Json, hashString, paletteFor, searchDocs } from './brain';

// WMO weather interpretation codes → something a card can render.
const WMO: [number, string, string][] = [
  [0, 'Clear', '☀️'],
  [1, 'Mostly clear', '🌤️'],
  [2, 'Partly cloudy', '⛅'],
  [3, 'Overcast', '☁️'],
  [45, 'Fog', '🌫️'],
  [51, 'Drizzle', '🌦️'],
  [61, 'Rain', '🌧️'],
  [71, 'Snow', '🌨️'],
  [80, 'Showers', '🌧️'],
  [95, 'Thunderstorm', '⛈️'],
];

function condition(code: number): { label: string; emoji: string } {
  let best = WMO[0];
  for (const row of WMO) if (code >= row[0]) best = row;
  return { label: best[1], emoji: best[2] };
}

const WEEKDAYS = ['Sun', 'Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat'];

async function liveWeather(city: string): Promise<Json> {
  const geoRes = await fetch(
    `https://geocoding-api.open-meteo.com/v1/search?name=${encodeURIComponent(city)}&count=1`,
  );
  if (!geoRes.ok) throw new Error(`geocoding failed: ${geoRes.status}`);
  const geo = (await geoRes.json()).results?.[0];
  if (!geo) throw new Error(`no such place: ${city}`);
  const fcRes = await fetch(
    `https://api.open-meteo.com/v1/forecast?latitude=${geo.latitude}&longitude=${geo.longitude}` +
      '&current=temperature_2m,weather_code,wind_speed_10m,relative_humidity_2m' +
      '&daily=temperature_2m_max,temperature_2m_min,weather_code&timezone=auto&forecast_days=5',
  );
  if (!fcRes.ok) throw new Error(`forecast failed: ${fcRes.status}`);
  const fc = await fcRes.json();
  return {
    city: geo.name,
    country: geo.country ?? '',
    tempC: Math.round(fc.current.temperature_2m),
    condition: condition(fc.current.weather_code),
    windKph: Math.round(fc.current.wind_speed_10m),
    humidity: Math.round(fc.current.relative_humidity_2m),
    daily: (fc.daily.time as string[]).map((date, i) => ({
      day: WEEKDAYS[new Date(`${date}T12:00:00Z`).getUTCDay()],
      min: Math.round(fc.daily.temperature_2m_min[i]),
      max: Math.round(fc.daily.temperature_2m_max[i]),
      emoji: condition(fc.daily.weather_code[i]).emoji,
    })),
  };
}

/** Offline stand-in so the tool still demos without network access. */
function simulatedWeather(city: string): Json {
  const seed = hashString(city.toLowerCase());
  const temp = 4 + (seed % 24);
  const conds = [condition(0), condition(2), condition(3), condition(61)];
  return {
    city,
    country: '',
    tempC: temp,
    condition: conds[seed % conds.length],
    windKph: 4 + ((seed >>> 4) % 28),
    humidity: 35 + ((seed >>> 8) % 55),
    daily: WEEKDAYS.slice(0, 5).map((day, i) => ({
      day,
      min: temp - 4 + ((seed >>> i) % 4),
      max: temp + 1 + ((seed >>> (i + 2)) % 5),
      emoji: conds[(seed >>> i) % conds.length].emoji,
    })),
    simulated: true,
  };
}

// ---------------------------------------------------------------------------
// calculate: a tiny recursive-descent evaluator — no eval(), no Function().

const CALC_FNS: Record<string, (x: number) => number> = {
  sqrt: Math.sqrt,
  sin: Math.sin,
  cos: Math.cos,
  tan: Math.tan,
  abs: Math.abs,
  ln: Math.log,
  log: Math.log10,
  exp: Math.exp,
  round: Math.round,
  floor: Math.floor,
  ceil: Math.ceil,
};
const CALC_CONSTS: Record<string, number> = { pi: Math.PI, e: Math.E };

export function evaluateExpression(input: string): number {
  const s = input.replace(/\s+/g, '').replace(/×/g, '*').replace(/÷/g, '/').toLowerCase();
  let i = 0;
  const fail = (msg: string): never => {
    throw new Error(`${msg} at position ${i} in "${s}"`);
  };
  const expr = (): number => {
    let v = term();
    while (s[i] === '+' || s[i] === '-') v = s[i++] === '+' ? v + term() : v - term();
    return v;
  };
  const term = (): number => {
    let v = unary();
    for (;;) {
      if (s[i] === '*') { i++; v *= unary(); }
      else if (s[i] === '/') { i++; v /= unary(); }
      else if (s[i] === '%') { i++; v %= unary(); }
      else return v;
    }
  };
  // Unary minus binds looser than ^ (so -3^2 = -9), while exponents may be
  // signed (2^-3 works): the exponent recurses through unary, not factor.
  const unary = (): number => {
    if (s[i] === '-') { i++; return -unary(); }
    if (s[i] === '+') { i++; return unary(); }
    return factor();
  };
  const factor = (): number => {
    const base = atom();
    if (s[i] === '^') { i++; return Math.pow(base, unary()); }
    return base;
  };
  const atom = (): number => {
    if (s[i] === '(') {
      i++;
      const v = expr();
      if (s[i] !== ')') fail('expected )');
      i++;
      return v;
    }
    const word = /^[a-z]+/.exec(s.slice(i));
    if (word) {
      const name = word[0];
      i += name.length;
      if (name in CALC_CONSTS) return CALC_CONSTS[name];
      const fn = CALC_FNS[name];
      if (!fn) fail(`unknown name "${name}"`);
      if (s[i] !== '(') fail(`expected ( after ${name}`);
      i++;
      const v = expr();
      if (s[i] !== ')') fail('expected )');
      i++;
      return fn(v);
    }
    const num = /^\d*\.?\d+(?:e[+-]?\d+)?/.exec(s.slice(i));
    if (!num) fail('expected a number');
    i += num![0].length;
    return Number(num![0]);
  };
  const value = expr();
  if (i !== s.length) fail('unexpected input');
  if (!Number.isFinite(value)) throw new Error('result is not a finite number');
  // Trim float noise (0.30000000000000004 → 0.3) without losing precision.
  return Number(value.toPrecision(12));
}

// ---------------------------------------------------------------------------

const asObj = (kwargs: Json): Record<string, Json> =>
  kwargs && typeof kwargs === 'object' && !Array.isArray(kwargs) ? kwargs : {};

/**
 * Build the tool table for a BrowserAgent host. `getIndex` is read at call
 * time so tools see the docs index even if it loads after the agent starts.
 */
export function makeTools(getIndex: () => DocsIndex | null): Record<string, (kwargs: Json) => Json | Promise<Json>> {
  return {
    search_docs: (kwargs) => {
      const query = String(asObj(kwargs).query ?? '');
      const index = getIndex();
      return {
        query,
        hits: searchDocs(index, query, 4) as unknown as Json,
        ...(index ? {} : { note: 'docs index not loaded' }),
      };
    },

    weather: async (kwargs) => {
      const city = String(asObj(kwargs).city ?? 'Tokyo').trim() || 'Tokyo';
      try {
        return await liveWeather(city);
      } catch {
        return simulatedWeather(city);
      }
    },

    calculate: (kwargs) => {
      const expression = String(asObj(kwargs).expression ?? '');
      return { expression, value: evaluateExpression(expression) };
    },

    chart: (kwargs) => {
      const { series } = asObj(kwargs);
      if (!Array.isArray(series) || !series.length) throw new Error('chart needs a non-empty series array');
      const bad = series.find(
        (p) => !p || typeof p !== 'object' || Array.isArray(p) || typeof (p as { value?: Json }).value !== 'number',
      );
      if (bad !== undefined) throw new Error('every series point needs a numeric value');
      return { ok: true, points: series.length };
    },

    color_palette: (kwargs) => {
      const o = asObj(kwargs);
      const mood = String(o.mood ?? 'chidori');
      let colors = Array.isArray(o.colors)
        ? o.colors.filter(
            (c) => c && typeof c === 'object' && /^#[0-9a-f]{6}$/i.test(String((c as { hex?: Json }).hex ?? '')),
          )
        : [];
      // A model may send only a mood; derive swatches deterministically.
      if (!colors.length) colors = paletteFor(mood) as unknown as Json[];
      return { mood, colors: colors.slice(0, 6) };
    },

    roll_dice: (kwargs) => {
      const o = asObj(kwargs);
      const count = Math.max(1, Math.min(12, Math.round(Number(o.count ?? 2)) || 2));
      const sides = Math.max(2, Math.min(1000, Math.round(Number(o.sides ?? 6)) || 6));
      const rolls = Array.from({ length: count }, () => 1 + Math.floor(Math.random() * sides));
      return { count, sides, rolls, total: rolls.reduce((a, b) => a + b, 0) };
    },
  };
}
