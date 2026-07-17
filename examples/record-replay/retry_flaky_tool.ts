import { run } from "chidori:agent";
import { flakyFetch } from "./tools.ts";

/**
 * Resilient retries with a reproducible history.
 *
 * `flakyFetch` fails on attempts 1 and 2 and succeeds on attempt 3. The agent
 * retries it in a loop, and each attempt's `chidori.log` is recorded in the
 * call log. On replay the exact path — 503, 503, 200 — is reproduced (the tool
 * is deterministic on the attempt number, and its host calls are served from
 * the log), without hitting a live service that may since have changed
 * behaviour. A flaky failure becomes reproducible.
 */
type FetchResult = {
  ok: boolean;
  status: number;
  error?: string;
  value?: { flag: boolean };
};

run(async (input: { maxAttempts?: number }) => {
  const maxAttempts = input.maxAttempts ?? 5;
  const attempts: string[] = [];
  let value: FetchResult["value"] | null = null;

  for (let i = 1; i <= maxAttempts; i++) {
    const r = (await flakyFetch.run({ key: "config", attempt: i })) as FetchResult;
    if (r.ok) {
      value = r.value ?? null;
      attempts.push(`attempt ${i}: ${r.status} ok`);
      break;
    }
    attempts.push(`attempt ${i}: ${r.status} ${r.error}`);
  }

  return { attempts, value, succeeded: value !== null };
});
