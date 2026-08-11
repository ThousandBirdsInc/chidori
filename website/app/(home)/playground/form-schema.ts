/**
 * The playground's inline forms — JSON Schema in, journaled input out.
 *
 * Forms live entirely in the page host: the agent calls the `form` tool with
 * a JSON Schema (plus an optional RJSF uiSchema), the validated spec is
 * journaled as the tool's result (so restores and offline replays repaint
 * it), cards.tsx renders it with react-jsonschema-form, and the user's
 * submission flows back through the ordinary chat-box path — arriving at the
 * agent as one journaled `chidori.input()` shaped `/form <id> {...}`. The
 * chidori runtime never learns that forms exist; rewind past a submission
 * and the form comes back to life.
 */
import type { FeedEvent, Json } from './brain';

/** A validated `form` tool call: what gets journaled and what gets rendered. */
export interface FormSpec {
  id: string;
  /** Submit button label. */
  submit: string;
  /** JSON Schema (draft-07 flavored, as RJSF consumes it). */
  schema: Record<string, Json>;
  /** Optional react-jsonschema-form uiSchema (widgets, placeholders, order). */
  uiSchema?: Record<string, Json>;
}

/** A submitted form: RJSF formData — arbitrary JSON keyed by property name. */
export type FormData = Record<string, Json>;

const MAX_SPEC_BYTES = 20_000;
const MAX_PROPERTIES = 24;

function fnv(s: string): number {
  let h = 2166136261;
  for (let i = 0; i < s.length; i++) {
    h ^= s.charCodeAt(i);
    h = Math.imul(h, 16777619);
  }
  return h >>> 0;
}

const asRecord = (v: Json | undefined): Record<string, Json> | null =>
  v && typeof v === 'object' && !Array.isArray(v) ? (v as Record<string, Json>) : null;

/**
 * Validate a `form` tool call. Shape checks are synchronous and produce
 * errors the model can act on; the schema itself is then compiled with ajv
 * (loaded on demand — it only ships to visitors whose chat renders a form)
 * so malformed schemas fail here, inside the tool call, rather than at
 * render time.
 */
export async function prepareFormSpec(kwargs: Json): Promise<FormSpec> {
  const o = asRecord(kwargs) ?? {};
  const schema = asRecord(o.schema);
  if (!schema) {
    throw new Error(
      'form needs {schema: <JSON Schema>} — e.g. {"schema": {"title": "RSVP", "type": "object", "properties": {"name": {"type": "string", "title": "Your name"}}, "required": ["name"]}}',
    );
  }
  if ((schema.type ?? 'object') !== 'object') {
    throw new Error('the form schema must have "type": "object" at the top level');
  }
  const properties = asRecord(schema.properties);
  if (!properties || !Object.keys(properties).length) {
    throw new Error('the form schema needs a non-empty "properties" object');
  }
  if (Object.keys(properties).length > MAX_PROPERTIES) {
    throw new Error(`too many top-level properties — at most ${MAX_PROPERTIES}`);
  }
  if (JSON.stringify(schema).length > MAX_SPEC_BYTES) {
    throw new Error(`the schema is too large — keep it under ${MAX_SPEC_BYTES} characters`);
  }
  // Compile-check with ajv, configured like RJSF's ajv8 validator but lax
  // about formats (RJSF renders "format": "date" etc. as widgets; ajv need
  // not validate them here for the schema to be usable).
  const { default: Ajv } = await import('ajv');
  try {
    new Ajv({ strict: false, validateFormats: false }).compile(
      schema as unknown as object,
    );
  } catch (err) {
    throw new Error(`the schema does not compile: ${String(err instanceof Error ? err.message : err)}`);
  }
  // The journal canonicalizes JSON objects (sorted keys) so replays are
  // byte-identical — which would alphabetize the fields. Pin the authored
  // property order in `ui:order`: arrays survive canonicalization intact.
  const uiSchema: Record<string, Json> = { ...(asRecord(o.uiSchema) ?? {}) };
  if (!Array.isArray(uiSchema['ui:order'])) {
    uiSchema['ui:order'] = Object.keys(properties);
  }
  const id = String(o.id ?? '')
    .replace(/[^\w-]+/g, '-')
    .replace(/^-+|-+$/g, '');
  return {
    id: id || `f${fnv(JSON.stringify({ schema, uiSchema })).toString(36)}`,
    submit: typeof o.submit === 'string' && o.submit.trim() ? o.submit : 'Submit',
    schema,
    uiSchema,
  };
}

// ---------------------------------------------------------------------------
// The response protocol: a submitted form is just a chat message.

const RESPONSE_RE = /^\/form\s+([\w-]+)\s+(\{[\s\S]*\})$/;

export function formResponseMessage(id: string, values: FormData): string {
  return `/form ${id} ${JSON.stringify(values)}`;
}

export function parseFormResponse(text: string): { id: string; values: FormData } | null {
  const m = RESPONSE_RE.exec(text.trim());
  if (!m) return null;
  try {
    const parsed: unknown = JSON.parse(m[2]);
    const values = asRecord(parsed as Json);
    return values ? { id: m[1], values } : null;
  } catch {
    return null;
  }
}

/** Recover a FormSpec from a journaled `form` tool result (or null). */
export function asFormSpec(result: Json | undefined): FormSpec | null {
  const o = asRecord(result ?? null);
  if (!o || typeof o.id !== 'string') return null;
  const schema = asRecord(o.schema);
  if (!schema || !asRecord(schema.properties)) return null;
  return {
    id: o.id,
    submit: typeof o.submit === 'string' ? o.submit : 'Submit',
    schema,
    ...(asRecord(o.uiSchema) ? { uiSchema: asRecord(o.uiSchema)! } : {}),
  };
}

/** The schema's card heading, if it has one. */
export function formTitle(spec: FormSpec): string | undefined {
  return typeof spec.schema.title === 'string' ? spec.schema.title : undefined;
}

/**
 * Flatten submitted values into displayable label/value rows, using the
 * schema's top-level property titles as labels (schema property order).
 * Nested objects and arrays render as compact JSON.
 */
export function formRows(spec: FormSpec | null, values: FormData): [string, string][] {
  const display = (v: Json | undefined): string => {
    if (v === undefined || v === '') return '—';
    if (typeof v === 'boolean') return v ? 'yes' : 'no';
    if (v === null || typeof v !== 'object') return String(v);
    if (Array.isArray(v) && v.every((x) => x === null || typeof x !== 'object')) {
      return v.map((x) => String(x)).join(', ');
    }
    return JSON.stringify(v);
  };
  const properties = spec ? asRecord(spec.schema.properties) : null;
  if (!properties) return Object.entries(values).map(([k, v]) => [k, display(v)]);
  // Journaled objects have canonicalized (sorted) keys; `ui:order` carries
  // the authored field order (see prepareFormSpec).
  const declared = Object.keys(properties);
  const uiOrder = spec?.uiSchema?.['ui:order'];
  const order = Array.isArray(uiOrder)
    ? [
        ...uiOrder.filter((n): n is string => typeof n === 'string' && n in properties),
        ...declared.filter((n) => !uiOrder.includes(n)),
      ]
    : declared;
  const rows: [string, string][] = [];
  for (const name of order) {
    if (!(name in values)) continue;
    const title = asRecord(properties[name])?.title;
    rows.push([typeof title === 'string' ? title : name, display(values[name])]);
  }
  // Values for properties the schema doesn't declare still show up.
  for (const [name, v] of Object.entries(values)) {
    if (!(name in properties)) rows.push([name, display(v)]);
  }
  return rows;
}

export interface FormPairing {
  /** Feed index of a `form` tool event → the submitted values, once answered. */
  answers: Map<number, FormData>;
  /** Feed index of a user event that is a form response → what to display. */
  responses: Map<number, { values: FormData; spec: FormSpec | null }>;
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
