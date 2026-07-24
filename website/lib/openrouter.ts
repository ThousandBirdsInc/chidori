/**
 * The site-wide OpenRouter connection, shared by every runnable surface: the
 * /playground chat and the "run this example" panel on docs pages. One PKCE
 * login connects all of them — the exchanged key lives in sessionStorage
 * under a single storage key, so it survives the redirect back from
 * openrouter.ai and is gone when the tab closes.
 *
 * The PKCE flow mirrors sdk/browser's startOpenRouterLogin /
 * completeOpenRouterLogin and shares its sessionStorage verifier key, but is
 * reimplemented here so a docs page can finish a login callback without
 * having to load the wasm assets first.
 */

/** Where the exchanged API key lives (same key the playground has always used). */
export const OPENROUTER_KEY_STORAGE = 'chidori-playground-openrouter-key';
/** PKCE verifier storage — identical to sdk/browser so the flows interoperate. */
const VERIFIER_STORAGE = 'chidori-openrouter-verifier';
/** The model choice is a durable preference, not a secret — localStorage. */
const MODEL_STORAGE = 'chidori-openrouter-model';

export const DEFAULT_OPENROUTER_MODEL = 'openrouter/auto';

/** Fired on window whenever the stored key or model changes. */
const CHANGE_EVENT = 'chidori:openrouter-change';

const hasWindow = () => typeof window !== 'undefined';

function emitChange(): void {
  window.dispatchEvent(new Event(CHANGE_EVENT));
}

export function getOpenRouterKey(): string | null {
  if (!hasWindow()) return null;
  try {
    return sessionStorage.getItem(OPENROUTER_KEY_STORAGE);
  } catch {
    return null;
  }
}

export function setOpenRouterKey(key: string | null): void {
  if (!hasWindow()) return;
  try {
    if (key === null) sessionStorage.removeItem(OPENROUTER_KEY_STORAGE);
    else sessionStorage.setItem(OPENROUTER_KEY_STORAGE, key);
  } catch {
    /* storage blocked — the connection just won't persist */
  }
  emitChange();
}

export function getOpenRouterModel(): string {
  if (!hasWindow()) return DEFAULT_OPENROUTER_MODEL;
  try {
    return localStorage.getItem(MODEL_STORAGE) || DEFAULT_OPENROUTER_MODEL;
  } catch {
    return DEFAULT_OPENROUTER_MODEL;
  }
}

export function setOpenRouterModel(model: string): void {
  if (!hasWindow()) return;
  try {
    const trimmed = model.trim();
    if (!trimmed || trimmed === DEFAULT_OPENROUTER_MODEL) localStorage.removeItem(MODEL_STORAGE);
    else localStorage.setItem(MODEL_STORAGE, trimmed);
  } catch {
    /* storage blocked */
  }
  emitChange();
}

/** Subscribe to key/model changes (shape matches useSyncExternalStore). */
export function subscribeOpenRouter(onChange: () => void): () => void {
  if (!hasWindow()) return () => {};
  window.addEventListener(CHANGE_EVENT, onChange);
  // A login finished in another same-origin tab also counts.
  window.addEventListener('storage', onChange);
  return () => {
    window.removeEventListener(CHANGE_EVENT, onChange);
    window.removeEventListener('storage', onChange);
  };
}

/** RFC 4648 base64url, no padding — the PKCE alphabet. */
function base64url(bytes: Uint8Array): string {
  let bin = '';
  for (const b of bytes) bin += String.fromCharCode(b);
  return btoa(bin).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
}

/**
 * Begin OpenRouter's PKCE login: store a code verifier and navigate to
 * openrouter.ai's consent page, which redirects back to the current page
 * with `?code=`. {@link completeOpenRouterLogin} finishes the exchange.
 */
export async function startOpenRouterLogin(): Promise<void> {
  const verifier = base64url(crypto.getRandomValues(new Uint8Array(32)));
  const digest = await crypto.subtle.digest('SHA-256', new TextEncoder().encode(verifier));
  const challenge = base64url(new Uint8Array(digest));
  sessionStorage.setItem(VERIFIER_STORAGE, verifier);
  const url =
    'https://openrouter.ai/auth' +
    `?callback_url=${encodeURIComponent(location.href)}` +
    `&code_challenge=${challenge}&code_challenge_method=S256`;
  location.assign(url);
}

/**
 * Finish a PKCE login if this page load is the redirect back from
 * openrouter.ai: exchange `?code=` for an API key, store it globally, and
 * scrub the one-time code from the address bar. A no-op (returns null)
 * when the URL carries no code, so call it unconditionally on mount.
 */
export async function completeOpenRouterLogin(): Promise<string | null> {
  if (!hasWindow()) return null;
  const here = new URL(location.href);
  const code = here.searchParams.get('code');
  if (!code) return null;
  const verifier = sessionStorage.getItem(VERIFIER_STORAGE);
  const res = await fetch('https://openrouter.ai/api/v1/auth/keys', {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({
      code,
      ...(verifier ? { code_verifier: verifier, code_challenge_method: 'S256' } : {}),
    }),
  });
  if (!res.ok) throw new Error(`openrouter key exchange: ${res.status} ${await res.text()}`);
  const { key } = (await res.json()) as { key: string };
  sessionStorage.removeItem(VERIFIER_STORAGE);
  here.searchParams.delete('code');
  history.replaceState(null, '', here);
  setOpenRouterKey(key);
  return key;
}
