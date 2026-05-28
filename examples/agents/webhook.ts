import type { Chidori } from "chidori";

export async function agent(input: { url: string; payload?: { [key: string]: unknown } }, chidori: Chidori) {
  const response = await chidori.http(input.url, {
    method: "POST",
    body: input.payload ?? { source: "chidori" },
  });
  return {
    status: response.status,
    body: response.body,
  };
}
