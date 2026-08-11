'use client';

/**
 * Generative UI for the chat feed: each tool event renders as a purpose-built
 * card driven by the (journaled) tool args + result, so replays repaint the
 * exact same UI with zero live calls.
 */
import { type FormEvent, useEffect, useState } from 'react';
import type { DocHit, Json } from './brain';
import { asFormSpec, type FormField, type FormSpec, type FormValues } from './form-dsl';

const BASE = process.env.NEXT_PUBLIC_BASE_PATH ?? '';

const card =
  'w-full max-w-md rounded-xl border border-fd-border bg-fd-card p-3.5 shadow-sm sm:p-4';

const asObj = (v: Json | undefined): Record<string, Json> =>
  v && typeof v === 'object' && !Array.isArray(v) ? (v as Record<string, Json>) : {};

/** How a live FormCard talks back to the page hosting the chat. */
export interface FormCardHooks {
  /** The journaled submission that answered this form, if any. */
  answered: FormValues | null;
  disabled: boolean;
  /** Sends the values through the ordinary chat-input path. */
  respond: (id: string, values: FormValues) => void;
}

export function ToolCard({
  name,
  args,
  result,
  formHooks,
}: {
  name: string;
  args?: Json;
  result?: Json;
  formHooks?: FormCardHooks;
}) {
  const r = asObj(result);
  if ('error' in r) {
    return (
      <div className={card}>
        <CardTitle name={name} />
        <p className="mt-1 text-sm text-fd-muted-foreground">{String(r.error)}</p>
      </div>
    );
  }
  if (name === 'form') {
    const spec = asFormSpec(result);
    // Keying by form id resets the draft state if a rewind re-purposes this
    // feed slot for a different form.
    if (spec) return <FormCard key={spec.id} spec={spec} hooks={formHooks} />;
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
    case 'read_source':
      return <SourceCard r={r} />;
    case 'update_source':
      return <SourceUpdateCard r={r} args={asObj(args)} />;
    case 'reset_source':
      return <SourceUpdateCard r={r} args={{}} reset />;
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
    <p className="font-mono text-[11px] font-medium uppercase tracking-wider text-fd-muted-foreground">
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

function SourceCard({ r }: { r: Record<string, Json> }) {
  return (
    <div className={card}>
      <CardTitle
        name="read_source"
        extra={`${String(r.lines)} lines${r.modified ? ' · rewritten via chat' : ''}`}
      />
      <pre className="mt-2 max-h-56 overflow-auto rounded-lg border border-fd-border bg-fd-background p-2.5 text-[10px] leading-relaxed">
        {String(r.source ?? '')}
      </pre>
    </div>
  );
}

function SourceUpdateCard({
  r,
  args,
  reset,
}: {
  r: Record<string, Json>;
  args: Record<string, Json>;
  reset?: boolean;
}) {
  const patch = typeof args.find === 'string';
  const diffPre =
    'mt-1 max-h-32 overflow-auto rounded-lg border border-fd-border bg-fd-background p-2 text-[10px] leading-relaxed';
  return (
    <div className={card}>
      <CardTitle
        name={reset ? 'reset_source' : 'update_source'}
        extra={reset ? undefined : String(r.mode ?? '')}
      />
      {r.unchanged ? (
        <p className="mt-1 text-sm text-fd-muted-foreground">Already running the original source.</p>
      ) : (
        <>
          {patch && (
            <div className="mt-2 font-mono">
              <p className="text-[10px] text-fd-muted-foreground">−</p>
              <pre className={`${diffPre} line-through opacity-60`}>{String(args.find)}</pre>
              <p className="mt-1.5 text-[10px] text-fd-muted-foreground">+</p>
              <pre className={diffPre}>{String(args.replace ?? '')}</pre>
            </div>
          )}
          <p className="mt-2 text-sm text-fd-muted-foreground">
            🧬 {String(r.note ?? 'hot-swaps in when this turn ends')}
            {r.lines ? ` · ${String(r.lines)} lines` : ''}
          </p>
        </>
      )}
    </div>
  );
}

function initialFormValues(spec: FormSpec): FormValues {
  const values: FormValues = {};
  for (const f of spec.fields) {
    if (f.kind === 'check') values[f.name] = f.default === true;
    else if (f.default !== undefined) values[f.name] = f.default as string | number;
    else if (f.kind === 'range') values[f.name] = f.min ?? 0;
    else values[f.name] = '';
  }
  return values;
}

function displayFormValue(f: FormField, v: FormValues[string] | undefined): string {
  if (v === undefined || v === '') return '—';
  if (f.kind === 'check') return v === true ? 'yes' : 'no';
  return String(v);
}

const formInput =
  'w-full rounded-lg border border-fd-border bg-fd-background px-2.5 py-1.5 text-sm outline-none focus:ring-2 focus:ring-fd-primary/40 disabled:opacity-50';

/**
 * A live form in the feed, rendered from the journaled `form` tool result.
 * Submitting sends `/form <id> {...}` through the normal chat-input path, so
 * the answers reach the agent as one journaled `chidori.input()`. Once a
 * later feed event answers this form (hooks.answered), the card collapses to
 * a read-only summary — which is also what replays and restores show.
 */
function FormCard({ spec, hooks }: { spec: FormSpec; hooks?: FormCardHooks }) {
  const [values, setValues] = useState<FormValues>(() => initialFormValues(spec));
  const [sent, setSent] = useState(false);
  const [problem, setProblem] = useState<string | null>(null);
  const answered = hooks?.answered ?? null;
  // A rewind can un-answer a form; make it fillable again.
  useEffect(() => {
    if (!answered) setSent(false);
  }, [answered]);

  if (answered) {
    return (
      <div className={card}>
        <CardTitle name="form" extra={`${spec.title ?? spec.id} · answered`} />
        <dl className="mt-2 space-y-1 text-sm">
          {spec.fields.map((f) => (
            <div key={f.name} className="flex justify-between gap-3">
              <dt className="text-fd-muted-foreground">{f.label}</dt>
              <dd className="text-right font-medium">{displayFormValue(f, answered[f.name])}</dd>
            </div>
          ))}
        </dl>
      </div>
    );
  }

  const locked = sent || !hooks || hooks.disabled;
  const set = (name: string, v: string | number | boolean) =>
    setValues((prev) => ({ ...prev, [name]: v }));
  const submit = (e: FormEvent) => {
    e.preventDefault();
    if (locked) return;
    const missing = spec.fields.filter(
      (f) => f.required && (values[f.name] === '' || values[f.name] === false),
    );
    if (missing.length) {
      setProblem(`Required: ${missing.map((f) => f.label).join(', ')}`);
      return;
    }
    const out: FormValues = {};
    for (const f of spec.fields) {
      const v = values[f.name];
      if (v === '' || v === undefined) continue;
      if (f.kind === 'number' || f.kind === 'range') {
        const num = Number(v);
        if (Number.isFinite(num)) out[f.name] = num;
      } else out[f.name] = v;
    }
    setProblem(null);
    setSent(true);
    hooks?.respond(spec.id, out);
  };

  return (
    <form className={card} onSubmit={submit}>
      <CardTitle name="form" extra={spec.title ?? spec.id} />
      <div className="mt-2.5 space-y-2.5">
        {spec.fields.map((f) => (
          <FormFieldControl
            key={f.name}
            f={f}
            id={`form-${spec.id}-${f.name}`}
            value={values[f.name]}
            locked={locked}
            set={set}
          />
        ))}
      </div>
      {problem && <p className="mt-2 text-xs text-red-600 dark:text-red-400">{problem}</p>}
      <div className="mt-3 flex items-center gap-2.5">
        <button
          type="submit"
          disabled={locked}
          className="h-8 rounded-lg bg-fd-primary px-3.5 text-xs font-medium text-fd-primary-foreground transition-opacity hover:opacity-85 disabled:pointer-events-none disabled:opacity-40"
        >
          {spec.submit}
        </button>
        <span className="text-[11px] text-fd-muted-foreground">
          {sent ? 'sending…' : 'answers go back to the agent as one journaled input'}
        </span>
      </div>
    </form>
  );
}

function FormFieldControl({
  f,
  id,
  value,
  locked,
  set,
}: {
  f: FormField;
  id: string;
  value: FormValues[string] | undefined;
  locked: boolean;
  set: (name: string, v: string | number | boolean) => void;
}) {
  const label = (
    <label htmlFor={id} className="mb-1 block text-xs font-medium">
      {f.label}
      {f.required && <span className="text-fd-muted-foreground"> *</span>}
    </label>
  );
  switch (f.kind) {
    case 'check':
      return (
        <label className="flex cursor-pointer items-center gap-2 text-sm">
          <input
            type="checkbox"
            checked={value === true}
            disabled={locked}
            onChange={(e) => set(f.name, e.target.checked)}
            className="size-4 accent-(--color-fd-primary)"
          />
          {f.label}
          {f.required && <span className="text-fd-muted-foreground"> *</span>}
        </label>
      );
    case 'textarea':
      return (
        <div>
          {label}
          <textarea
            id={id}
            rows={3}
            value={String(value ?? '')}
            placeholder={f.placeholder}
            disabled={locked}
            onChange={(e) => set(f.name, e.target.value)}
            className={formInput}
          />
        </div>
      );
    case 'select':
      return (
        <div>
          {label}
          <select
            id={id}
            value={String(value ?? '')}
            disabled={locked}
            onChange={(e) => set(f.name, e.target.value)}
            className={formInput}
          >
            {value === '' && <option value="">choose…</option>}
            {(f.options ?? []).map((o) => (
              <option key={o} value={o}>
                {o}
              </option>
            ))}
          </select>
        </div>
      );
    case 'radio':
      return (
        <fieldset>
          <legend className="mb-1 text-xs font-medium">
            {f.label}
            {f.required && <span className="text-fd-muted-foreground"> *</span>}
          </legend>
          <div className="flex flex-wrap gap-1.5">
            {(f.options ?? []).map((o) => (
              <label
                key={o}
                className={`cursor-pointer rounded-lg border px-2.5 py-1 text-xs transition-colors ${
                  value === o
                    ? 'border-fd-primary bg-fd-primary/10 font-medium'
                    : 'border-fd-border hover:bg-fd-accent'
                }`}
              >
                <input
                  type="radio"
                  name={id}
                  value={o}
                  checked={value === o}
                  disabled={locked}
                  onChange={() => set(f.name, o)}
                  className="sr-only"
                />
                {o}
              </label>
            ))}
          </div>
        </fieldset>
      );
    case 'range':
      return (
        <div>
          {label}
          <div className="flex items-center gap-2.5">
            <input
              id={id}
              type="range"
              min={f.min}
              max={f.max}
              step={f.step}
              value={Number(value ?? f.min ?? 0)}
              disabled={locked}
              onChange={(e) => set(f.name, Number(e.target.value))}
              className="min-w-0 flex-1 accent-(--color-fd-primary)"
            />
            <span className="w-8 text-right font-mono text-xs tabular-nums">
              {String(value ?? f.min ?? 0)}
            </span>
          </div>
        </div>
      );
    default:
      return (
        <div>
          {label}
          <input
            id={id}
            type={f.kind === 'number' ? 'number' : f.kind === 'date' ? 'date' : 'text'}
            value={String(value ?? '')}
            placeholder={f.placeholder}
            min={f.min}
            max={f.max}
            step={f.step}
            disabled={locked}
            onChange={(e) => set(f.name, e.target.value)}
            className={formInput}
          />
        </div>
      );
  }
}

/**
 * A submitted form response in the feed: the raw message is
 * `/form <id> {...}` (that's what the agent, both brains, and the journal
 * see), but the user's bubble renders it as labeled answers.
 */
export function FormResponseBubble({
  spec,
  values,
}: {
  spec: FormSpec | null;
  values: FormValues;
}) {
  const rows: [string, string][] = spec
    ? spec.fields
        .filter((f) => f.name in values)
        .map((f) => [f.label, displayFormValue(f, values[f.name])])
    : Object.entries(values).map(([k, v]) => [k, String(v)]);
  return (
    <div className="max-w-[85%] rounded-2xl rounded-br-md bg-fd-primary px-3.5 py-2.5 text-sm text-fd-primary-foreground">
      <p className="text-[11px] font-medium uppercase tracking-wider opacity-80">
        📋 {spec?.title ?? spec?.id ?? 'form answers'}
      </p>
      <dl className="mt-1 space-y-0.5">
        {rows.map(([k, v]) => (
          <div key={k} className="flex justify-between gap-4">
            <dt className="opacity-80">{k}</dt>
            <dd className="text-right font-medium">{v}</dd>
          </div>
        ))}
      </dl>
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
