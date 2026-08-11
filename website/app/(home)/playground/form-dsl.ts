/**
 * The playground's inline-form DSL — generative UI that collects *input*.
 *
 * Forms live entirely in the page host: the agent calls the `form` tool with
 * a spec written in this DSL, the parsed form is journaled as the tool's
 * result (so restores and offline replays repaint it), cards.tsx renders it
 * live in the feed, and the user's submission flows back through the ordinary
 * chat box path — arriving at the agent as one journaled `chidori.input()`
 * shaped `/form <id> {"name":value,...}`. The chidori runtime never learns
 * that forms exist; rewind past a submission and the form comes back to life.
 *
 * The DSL is line-based — directives first, then one field per line:
 *
 *   id: trip                        (optional; defaults to a spec hash)
 *   title: Plan a weekend trip      (optional card heading)
 *   submit: Plan it                 (optional button label)
 *   text destination "Where to?" required
 *   number days "How many days?" min=1 max=14 default=2
 *   select budget "Budget" [shoestring|comfortable|splurge]
 *   radio pace "Pace" [chill|balanced|packed]
 *   check flexible "My dates are flexible"
 *   range heat "Spice level" min=0 max=10 step=1 default=5
 *   date depart "Leaving on"
 *   textarea notes "Anything else?" placeholder="dealbreakers…"
 *
 * Field grammar: `<kind> <name> "Label" [opt|opt] key=value… required`.
 * `#` and `//` open comment lines; `*` at the end of a line also marks the
 * field required.
 */
import type { FeedEvent, Json } from './brain';

export type FieldKind =
  | 'text'
  | 'textarea'
  | 'number'
  | 'range'
  | 'select'
  | 'radio'
  | 'check'
  | 'date';

export interface FormField {
  kind: FieldKind;
  name: string;
  label: string;
  required?: boolean;
  placeholder?: string;
  /** select / radio choices. */
  options?: string[];
  /** number / range bounds. */
  min?: number;
  max?: number;
  step?: number;
  default?: string | number | boolean;
}

export interface FormSpec {
  id: string;
  title?: string;
  submit: string;
  fields: FormField[];
}

/** What a submitted form sends back: primitives only, keyed by field name. */
export type FormValues = Record<string, string | number | boolean>;

const KINDS: readonly FieldKind[] = [
  'text',
  'textarea',
  'number',
  'range',
  'select',
  'radio',
  'check',
  'date',
];
const KIND_SET: ReadonlySet<string> = new Set(KINDS);
// Spellings a model plausibly reaches for, mapped onto the real kinds.
const KIND_ALIASES: Record<string, FieldKind> = {
  checkbox: 'check',
  boolean: 'check',
  slider: 'range',
  dropdown: 'select',
  choice: 'radio',
  email: 'text',
  string: 'text',
  input: 'text',
  int: 'number',
  integer: 'number',
};

const MAX_FIELDS = 12;

const ATTR_RE = /\b(min|max|step|default|placeholder)\s*=\s*(?:"([^"]*)"|'([^']*)'|(\S+))/g;

function fnv(s: string): number {
  let h = 2166136261;
  for (let i = 0; i < s.length; i++) {
    h ^= s.charCodeAt(i);
    h = Math.imul(h, 16777619);
  }
  return h >>> 0;
}

export function parseFormDsl(source: string): FormSpec {
  const meta: { id?: string; title?: string; submit?: string } = {};
  const fields: FormField[] = [];
  const seen = new Set<string>();
  const lines = source.split('\n');
  for (let ln = 0; ln < lines.length; ln++) {
    const line = lines[ln].trim();
    if (!line || line.startsWith('#') || line.startsWith('//')) continue;
    const fail = (msg: string): never => {
      throw new Error(`form spec line ${ln + 1} ("${line.slice(0, 48)}"): ${msg}`);
    };

    const directive = /^(id|title|submit)\s*:\s*(.+)$/i.exec(line);
    if (directive) {
      meta[directive[1].toLowerCase() as 'id' | 'title' | 'submit'] = directive[2].trim();
      continue;
    }

    const head = /^([A-Za-z]+)\s+([A-Za-z_][\w-]*)\s*(.*)$/.exec(line);
    if (!head) {
      fail('expected `<kind> <name> "Label" …` or a directive (id: / title: / submit:)');
    }
    const [, rawKind, name, restRaw] = head!;
    const kindKey = rawKind.toLowerCase();
    const kind = KIND_SET.has(kindKey) ? (kindKey as FieldKind) : KIND_ALIASES[kindKey];
    if (!kind) fail(`unknown field kind "${rawKind}" — use one of: ${KINDS.join(', ')}`);
    if (seen.has(name)) fail(`duplicate field name "${name}"`);
    seen.add(name);
    if (fields.length >= MAX_FIELDS) fail(`too many fields — at most ${MAX_FIELDS}`);

    const field: FormField = { kind, name, label: name };
    let rest = restRaw;

    const label = /^(?:"([^"]*)"|“([^”]*)”|'([^']*)')/.exec(rest);
    if (label) {
      field.label = (label[1] ?? label[2] ?? label[3] ?? '').trim() || name;
      rest = rest.slice(label[0].length);
    }

    const opts = /\[([^\]]*)\]/.exec(rest);
    if (opts) {
      field.options = opts[1]
        .split(/[|,]/)
        .map((s) => s.trim())
        .filter(Boolean);
      rest = rest.slice(0, opts.index) + ' ' + rest.slice(opts.index + opts[0].length);
    }

    let defaultRaw: string | undefined;
    let m: RegExpExecArray | null;
    const attrRe = new RegExp(ATTR_RE.source, 'g');
    while ((m = attrRe.exec(rest)) !== null) {
      const value = m[2] ?? m[3] ?? m[4] ?? '';
      if (m[1] === 'placeholder') {
        field.placeholder = value;
      } else if (m[1] === 'default') {
        defaultRaw = value;
      } else {
        const num = Number(value);
        if (!Number.isFinite(num)) fail(`${m[1]} needs a number, got "${value}"`);
        field[m[1] as 'min' | 'max' | 'step'] = num;
      }
    }
    const flags = rest.replace(new RegExp(ATTR_RE.source, 'g'), ' ');
    if (/(^|\s)(required|\*)(?=\s|$)/.test(flags)) field.required = true;

    if (kind === 'select' || kind === 'radio') {
      if (!field.options || field.options.length < 2) {
        fail(`${kind} needs at least two [option|option] choices`);
      }
    } else if (field.options) {
      fail('[options] only apply to select and radio fields');
    }
    if (kind === 'range') {
      field.min ??= 0;
      field.max ??= 10;
      field.step ??= 1;
    }
    if (field.min !== undefined && field.max !== undefined && field.min > field.max) {
      fail(`min (${field.min}) is greater than max (${field.max})`);
    }
    if (defaultRaw !== undefined) {
      if (kind === 'check') field.default = defaultRaw === 'true';
      else if (kind === 'number' || kind === 'range') {
        const num = Number(defaultRaw);
        if (!Number.isFinite(num)) fail(`default needs a number, got "${defaultRaw}"`);
        field.default = num;
      } else if (field.options && !field.options.includes(defaultRaw)) {
        fail(`default "${defaultRaw}" is not one of the options`);
      } else field.default = defaultRaw;
    }
    fields.push(field);
  }

  if (!fields.length) {
    throw new Error('form spec has no fields — add lines like `text name "Your name" required`');
  }
  const id = (meta.id ?? '').replace(/[^\w-]+/g, '-').replace(/^-+|-+$/g, '');
  return {
    id: id || `f${fnv(source).toString(36)}`,
    ...(meta.title ? { title: meta.title } : {}),
    submit: meta.submit ?? 'Submit',
    fields,
  };
}

/**
 * Structured fallback for models that send `{fields: [...]}` instead of DSL
 * text: each field is serialized onto a DSL line and fed through the one true
 * parser, so both entry points share validation (and its error messages).
 */
export function normalizeFormSpec(raw: Json): FormSpec {
  const o = (raw && typeof raw === 'object' && !Array.isArray(raw) ? raw : {}) as Record<
    string,
    Json
  >;
  if (!Array.isArray(o.fields) || !o.fields.length) {
    throw new Error(
      'form needs {spec: "<dsl text>"} (preferred) or {fields: [{kind, name, …}]} — e.g. spec: \'text name "Your name" required\'',
    );
  }
  const clean = (v: Json | undefined) => String(v ?? '').replace(/["“”[\]\n]/g, ' ').trim();
  const lines: string[] = [];
  if (typeof o.id === 'string') lines.push(`id: ${o.id}`);
  if (typeof o.title === 'string') lines.push(`title: ${clean(o.title)}`);
  if (typeof o.submit === 'string') lines.push(`submit: ${clean(o.submit)}`);
  for (const f of o.fields) {
    const fo = (f && typeof f === 'object' && !Array.isArray(f) ? f : {}) as Record<string, Json>;
    const parts = [
      `${String(fo.kind ?? fo.type ?? 'text')} ${String(fo.name ?? '')}`,
      `"${clean(fo.label ?? fo.name)}"`,
    ];
    if (Array.isArray(fo.options)) parts.push(`[${fo.options.map(clean).join('|')}]`);
    for (const key of ['min', 'max', 'step', 'default'] as const) {
      if (fo[key] !== undefined && fo[key] !== null) parts.push(`${key}=${clean(fo[key])}`);
    }
    if (typeof fo.placeholder === 'string') parts.push(`placeholder="${clean(fo.placeholder)}"`);
    if (fo.required === true) parts.push('required');
    lines.push(parts.join(' '));
  }
  return parseFormDsl(lines.join('\n'));
}

// ---------------------------------------------------------------------------
// The response protocol: a submitted form is just a chat message.

const RESPONSE_RE = /^\/form\s+([\w-]+)\s+(\{[\s\S]*\})$/;

export function formResponseMessage(id: string, values: FormValues): string {
  return `/form ${id} ${JSON.stringify(values)}`;
}

export function parseFormResponse(text: string): { id: string; values: FormValues } | null {
  const m = RESPONSE_RE.exec(text.trim());
  if (!m) return null;
  try {
    const parsed: unknown = JSON.parse(m[2]);
    if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed)) return null;
    const values: FormValues = {};
    for (const [k, v] of Object.entries(parsed)) {
      if (typeof v === 'string' || typeof v === 'number' || typeof v === 'boolean') values[k] = v;
    }
    return { id: m[1], values };
  } catch {
    return null;
  }
}

/** Recover a FormSpec from a journaled `form` tool result (or null). */
export function asFormSpec(result: Json | undefined): FormSpec | null {
  if (!result || typeof result !== 'object' || Array.isArray(result)) return null;
  const o = result as { id?: Json; title?: Json; submit?: Json; fields?: Json };
  if (typeof o.id !== 'string' || !Array.isArray(o.fields)) return null;
  const fields = o.fields.filter(
    (f): f is FormField & { [key: string]: Json } =>
      !!f &&
      typeof f === 'object' &&
      !Array.isArray(f) &&
      typeof (f as { name?: Json }).name === 'string' &&
      KIND_SET.has(String((f as { kind?: Json }).kind)),
  );
  if (!fields.length) return null;
  return {
    id: o.id,
    ...(typeof o.title === 'string' ? { title: o.title } : {}),
    submit: typeof o.submit === 'string' ? o.submit : 'Submit',
    fields,
  };
}

export interface FormPairing {
  /** Feed index of a `form` tool event → the submitted values, once answered. */
  answers: Map<number, FormValues>;
  /** Feed index of a user event that is a form response → what to display. */
  responses: Map<number, { values: FormValues; spec: FormSpec | null }>;
}

/**
 * Pair each rendered form with the submission that answered it, from the feed
 * alone — so live chats, restored saves, offline replays, and rewound
 * timelines all agree. Forms open in feed order and the first later response
 * with a matching id claims the oldest open form (FIFO), which keeps repeated
 * ids — the same form rendered twice — pairing sensibly.
 */
export function pairFormResponses(feed: FeedEvent[]): FormPairing {
  const open: { index: number; spec: FormSpec }[] = [];
  const answers: FormPairing['answers'] = new Map();
  const responses: FormPairing['responses'] = new Map();
  for (let i = 0; i < feed.length; i++) {
    const event = feed[i];
    if (event.kind === 'tool' && event.name === 'form') {
      const spec = asFormSpec(event.result);
      if (spec) open.push({ index: i, spec });
    } else if (event.kind === 'user') {
      const parsed = parseFormResponse(event.text);
      if (!parsed) continue;
      const at = open.findIndex((o) => o.spec.id === parsed.id);
      if (at === -1) {
        responses.set(i, { values: parsed.values, spec: null });
        continue;
      }
      const [owner] = open.splice(at, 1);
      answers.set(owner.index, parsed.values);
      responses.set(i, { values: parsed.values, spec: owner.spec });
    }
  }
  return { answers, responses };
}
