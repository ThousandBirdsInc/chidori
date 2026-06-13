export async function agent(input: { url: string; payload?: { [key: string]: unknown } }, _chidori: unknown) {
  // `fetch` is the runtime's captured networking surface: this POST is
  // policy-gated, pausable for approval, and recorded for deterministic replay,
  // exactly like any network call a dependency would make under the hood.
  const response = await fetch(input.url, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(input.payload ?? { source: "chidori" }),
  });
  return {
    status: response.status,
    body: await response.json(),
  };
}
