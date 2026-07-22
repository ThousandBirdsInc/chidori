'use client';

/**
 * Generative UI for the chat feed: each tool event renders as a purpose-built
 * card driven by the (journaled) tool args + result, so replays repaint the
 * exact same UI with zero live calls.
 */
import type { DocHit, Json } from './brain';

const BASE = process.env.NEXT_PUBLIC_BASE_PATH ?? '';

const card =
  'w-full max-w-md rounded-xl border border-fd-border bg-fd-card p-4 shadow-sm';

const asObj = (v: Json | undefined): Record<string, Json> =>
  v && typeof v === 'object' && !Array.isArray(v) ? (v as Record<string, Json>) : {};

export function ToolCard({ name, args, result }: { name: string; args?: Json; result?: Json }) {
  const r = asObj(result);
  if ('error' in r) {
    return (
      <div className={card}>
        <CardTitle name={name} />
        <p className="mt-1 text-sm text-fd-muted-foreground">{String(r.error)}</p>
      </div>
    );
  }
  switch (name) {
    case 'weather':
      return <WeatherCard r={r} />;
    case 'search_docs':
      return <DocsCard r={r} />;
    case 'chart':
      return <ChartCard args={asObj(args)} />;
    case 'calculate':
      return <CalcCard r={r} />;
    case 'roll_dice':
      return <DiceCard r={r} />;
    case 'color_palette':
      return <PaletteCard r={r} />;
    default:
      return (
        <div className={card}>
          <CardTitle name={name} />
          <pre className="mt-2 overflow-x-auto text-xs text-fd-muted-foreground">
            {JSON.stringify(result, null, 2)}
          </pre>
        </div>
      );
  }
}

function CardTitle({ name, extra }: { name: string; extra?: string }) {
  return (
    <p className="text-[11px] font-medium uppercase tracking-wider text-fd-muted-foreground">
      ⚙ {name}
      {extra ? <span className="normal-case tracking-normal"> · {extra}</span> : null}
    </p>
  );
}

function WeatherCard({ r }: { r: Record<string, Json> }) {
  const cond = asObj(r.condition);
  const daily = Array.isArray(r.daily) ? (r.daily as Json[]).map(asObj) : [];
  return (
    <div className={card}>
      <CardTitle name="weather" extra={r.simulated ? 'simulated (offline)' : 'open-meteo'} />
      <div className="mt-2 flex items-center gap-3">
        <span className="text-4xl leading-none">{String(cond.emoji ?? '🌡')}</span>
        <div className="min-w-0 flex-1">
          <p className="truncate font-semibold">
            {String(r.city)}
            {r.country ? <span className="font-normal text-fd-muted-foreground"> · {String(r.country)}</span> : null}
          </p>
          <p className="text-sm text-fd-muted-foreground">
            {String(cond.label ?? '')} · wind {String(r.windKph)} km/h · {String(r.humidity)}%
          </p>
        </div>
        <span className="text-3xl font-semibold tabular-nums">{String(r.tempC)}°</span>
      </div>
      {daily.length > 0 && (
        <div className="mt-3 grid grid-cols-5 gap-1 border-t border-fd-border pt-3 text-center">
          {daily.slice(0, 5).map((d, i) => (
            <div key={i} className="text-xs">
              <p className="text-fd-muted-foreground">{String(d.day)}</p>
              <p className="text-base">{String(d.emoji)}</p>
              <p className="tabular-nums">
                {String(d.max)}°<span className="text-fd-muted-foreground">/{String(d.min)}°</span>
              </p>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

function DocsCard({ r }: { r: Record<string, Json> }) {
  const hits = Array.isArray(r.hits) ? (r.hits as unknown as DocHit[]) : [];
  return (
    <div className={card}>
      <CardTitle name="search_docs" extra={`“${String(r.query ?? '')}”`} />
      {hits.length === 0 ? (
        <p className="mt-2 text-sm text-fd-muted-foreground">No matching docs sections.</p>
      ) : (
        <ul className="mt-2 space-y-2.5">
          {hits.map((h, i) => (
            <li key={i}>
              <a href={`${BASE}${h.route}`} className="text-sm font-medium text-fd-primary hover:underline">
                {h.title}
                {h.heading ? <span className="text-fd-muted-foreground"> § {h.heading}</span> : null}
              </a>
              <p className="mt-0.5 line-clamp-2 text-xs text-fd-muted-foreground">{h.excerpt}</p>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

function ChartCard({ args }: { args: Record<string, Json> }) {
  const series = (Array.isArray(args.series) ? (args.series as Json[]).map(asObj) : []).map((p) => ({
    label: String(p.label ?? ''),
    value: Number(p.value ?? 0),
  }));
  if (!series.length) return null;
  const W = 320;
  const H = 120;
  const max = Math.max(...series.map((p) => p.value), 0);
  const min = Math.min(...series.map((p) => p.value), 0);
  const span = max - min || 1;
  const y = (v: number) => H - ((v - min) / span) * H;
  const line = args.kind === 'line';
  const step = W / series.length;
  return (
    <div className={card}>
      <CardTitle name="chart" extra={args.title ? String(args.title) : undefined} />
      <svg viewBox={`0 0 ${W} ${H + 16}`} className="mt-2 w-full text-fd-primary" role="img" aria-label={String(args.title ?? 'chart')}>
        {min < 0 && <line x1={0} x2={W} y1={y(0)} y2={y(0)} stroke="currentColor" strokeOpacity={0.25} />}
        {line ? (
          <polyline
            fill="none"
            stroke="currentColor"
            strokeWidth={2}
            strokeLinejoin="round"
            points={series.map((p, i) => `${i * step + step / 2},${y(p.value)}`).join(' ')}
          />
        ) : (
          series.map((p, i) => (
            <rect
              key={i}
              x={i * step + step * 0.15}
              width={step * 0.7}
              y={Math.min(y(p.value), y(0))}
              height={Math.max(2, Math.abs(y(p.value) - y(0)))}
              rx={2}
              fill="currentColor"
              fillOpacity={0.75}
            >
              <title>{`${p.label}: ${p.value}`}</title>
            </rect>
          ))
        )}
        {series.length <= 16 &&
          series.map((p, i) => (
            <text
              key={i}
              x={i * step + step / 2}
              y={H + 12}
              textAnchor="middle"
              className="fill-fd-muted-foreground"
              fontSize={9}
            >
              {p.label.slice(0, 6)}
            </text>
          ))}
      </svg>
    </div>
  );
}

function CalcCard({ r }: { r: Record<string, Json> }) {
  return (
    <div className={card}>
      <CardTitle name="calculate" />
      <p className="mt-1 font-mono text-sm">
        {String(r.expression)} = <span className="text-base font-semibold text-fd-primary">{String(r.value)}</span>
      </p>
    </div>
  );
}

const D6 = ['⚀', '⚁', '⚂', '⚃', '⚄', '⚅'];

function DiceCard({ r }: { r: Record<string, Json> }) {
  const rolls = Array.isArray(r.rolls) ? (r.rolls as number[]) : [];
  const d6 = Number(r.sides) === 6;
  return (
    <div className={card}>
      <CardTitle name="roll_dice" extra={`${String(r.count)}d${String(r.sides)}`} />
      <div className="mt-2 flex flex-wrap items-center gap-2">
        {rolls.map((v, i) =>
          d6 ? (
            <span key={i} className="text-4xl leading-none" title={String(v)}>
              {D6[v - 1]}
            </span>
          ) : (
            <span key={i} className="rounded-lg border border-fd-border px-2.5 py-1 font-mono text-sm tabular-nums">
              {v}
            </span>
          ),
        )}
        <span className="ml-1 text-sm text-fd-muted-foreground">= {String(r.total)}</span>
      </div>
    </div>
  );
}

function PaletteCard({ r }: { r: Record<string, Json> }) {
  const colors = (Array.isArray(r.colors) ? (r.colors as Json[]).map(asObj) : []).map((c) => ({
    hex: String(c.hex ?? '#888888'),
    name: String(c.name ?? ''),
  }));
  return (
    <div className={card}>
      <CardTitle name="color_palette" extra={`“${String(r.mood ?? '')}”`} />
      <div className="mt-2 flex gap-1.5">
        {colors.map((c, i) => (
          <div key={i} className="min-w-0 flex-1">
            <div className="h-14 rounded-lg border border-fd-border" style={{ backgroundColor: c.hex }} title={c.name} />
            <p className="mt-1 truncate text-center font-mono text-[10px] text-fd-muted-foreground">{c.hex}</p>
          </div>
        ))}
      </div>
    </div>
  );
}
