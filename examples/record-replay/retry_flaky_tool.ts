import type { Chidori } from "chidori";

/**
 * Resilient retries with a reproducible history.
 *
 * `flaky_fetch` fails on attempts 1 and 2 and succeeds on attempt 3. The agent
 * retries it in a loop, recording each attempt's outcome in the call log. On
 * replay the exact path — 503, 503, 200 — is reproduced from the log without
 * hitting the live service, which may since have changed behaviour. A flaky
 * failure becomes reproducible.
 */
type FetchResult = {
  ok: boolean;
  status: number;
  error?: string;
  value?: { flag: boolean };
};

export async function agent(input: { maxAttempts?: number }, chidori: Chidori) {
  const maxAttempts = input.maxAttempts ?? 5;
  const attempts: string[] = [];
  let value: FetchResult["value"] | null = null;

  for (let i = 1; i <= maxAttempts; i++) {
    const r = await chidori.tool<{ key: string; attempt: number }, FetchResult>("flaky_fetch", {
      key: "config",
      attempt: i,
    });
    if (r.ok) {
      value = r.value ?? null;
      attempts.push(`attempt ${i}: ${r.status} ok`);
      break;
    }
    attempts.push(`attempt ${i}: ${r.status} ${r.error}`);
  }

  return { attempts, value, succeeded: value !== null };
}
