/// <reference types="@1kbirds/chidori/agent-env" />
import { chidori, run, defineTool, type JsonObject } from "chidori:agent";

/**
 * War Room — an incident commander your on-call team talks to while it works.
 *
 * The agent triages an alert, investigates with real tools against the ops
 * API, proposes a mitigation, then opens the run to the humans in the war
 * room: anyone can push context notes, demand escalation, or approve the
 * mitigation — all as signals delivered over HTTP while the run streams to a
 * dashboard. Nobody answers in time? The commander pages the secondary
 * on-call and keeps waiting.
 *
 * Serve it:
 *   CHIDORI_HTTP_ALLOW_HOSTS=127.0.0.1 \
 *   CHIDORI_POLICY_FILE=policy.json \
 *   chidori serve commander.ts --port 8787
 *
 * Signals it listens for (fan-in, with a timeout):
 *   approve  { decision: "mitigate" | "abandon" }
 *   note     { text: string }        — context from any responder, any time
 *   escalate {}                      — page the secondary now
 */

type Alert = {
  id: string;
  service: string;
  summary: string;
  errorRate?: number;
  logs?: string;
};

type Triage = {
  severity: "SEV1" | "SEV2" | "SEV3";
  hypothesis: string;
  customerImpact: string;
};

const OPS_DEFAULT = "http://127.0.0.1:9911";

// Tools the commander can use while investigating. Both hit the (mock) ops
// API over the captured `fetch`, so every probe is recorded and replayable.
function makeTools(opsBase: string) {
  const serviceStatus = defineTool({
    name: "service_status",
    description:
      "Current health of a service: error rate, latency, recent deploys, upstream dependencies.",
    parameters: {
      type: "object",
      properties: { service: { type: "string", description: "Service name" } },
      required: ["service"],
    },
    run: async (args: { service: string }) => {
      const resp = await fetch(`${opsBase}/status/${encodeURIComponent(args.service)}`);
      return (await resp.json()) as JsonObject;
    },
  });

  const runbook = defineTool({
    name: "runbook",
    description: "Fetch the operational runbook for a service (known failure modes and safe mitigations).",
    parameters: {
      type: "object",
      properties: { service: { type: "string", description: "Service name" } },
      required: ["service"],
    },
    run: async (args: { service: string }) => {
      const resp = await fetch(`${opsBase}/runbook/${encodeURIComponent(args.service)}`);
      return (await resp.json()) as JsonObject;
    },
  });

  return { serviceStatus, runbook };
}

type WebhookEvent = {
  method: string;
  path: string;
  headers: Record<string, string>;
  query: Record<string, string>;
  body: { alert?: Alert; approvalTimeoutMs?: number; maxPages?: number } | string | null;
};

run(
  async (input: {
    alert?: Alert;
    event?: WebhookEvent;
    opsBase?: string;
    approvalTimeoutMs?: number;
    maxPages?: number;
  }) => {
    // Accept either a session input ({ alert }) or the server's event-driven
    // surface (ANY /* folds the request into { event: { method, path, headers,
    // query, body } } — undocumented; shape read from server/events.rs).
    const eventBody =
      input.event && typeof input.event.body === "object" && input.event.body !== null
        ? input.event.body
        : undefined;
    const alert = input.alert ?? eventBody?.alert;
    if (!alert) {
      // Non-incident traffic (health probes, scanners) ends here — cheaply.
      return { status: 400, body: { error: "no alert in request" } };
    }
    input = {
      ...input,
      approvalTimeoutMs: input.approvalTimeoutMs ?? eventBody?.approvalTimeoutMs,
      maxPages: input.maxPages ?? eventBody?.maxPages,
    };
    const opsBase = input.opsBase ?? OPS_DEFAULT;
    const approvalTimeoutMs = input.approvalTimeoutMs ?? 120_000;
    const maxPages = input.maxPages ?? 2;
    const { serviceStatus, runbook } = makeTools(opsBase);

    const timeline: string[] = [`alert received: [${alert.id}] ${alert.service} — ${alert.summary}`];
    await chidori.log("incident opened", { id: alert.id, service: alert.service });

    // 1. Triage — structured verdict, streamed to the dashboard as progress.
    const triage = (await chidori.prompt(
      "You are an incident commander. Triage this production alert.\n" +
        `Service: ${alert.service}\nSummary: ${alert.summary}\n` +
        `Error rate: ${alert.errorRate ?? "unknown"}\nLog excerpt:\n${alert.logs ?? "(none)"}\n\n` +
        'Reply as JSON: {"severity": "SEV1"|"SEV2"|"SEV3", "hypothesis": string, "customerImpact": string}',
      { type: "progress", format: "json", maxTokens: 2000 },
    )) as unknown as Triage;
    timeline.push(`triaged ${triage.severity}: ${triage.hypothesis}`);
    await chidori.log("triage", { severity: triage.severity });

    // 2. Investigate with tools and propose a mitigation. The provider tool
    // loop probes the ops API on its own; every probe is a recorded call.
    let plan = await chidori.prompt(
      "Investigate this incident using the tools, then propose ONE concrete, " +
        "reversible mitigation. End with a line 'MITIGATION: <imperative one-liner>'.\n" +
        `Alert: ${JSON.stringify(alert)}\nTriage: ${JSON.stringify(triage)}`,
      { type: "progress", tools: [serviceStatus, runbook], maxTurns: 6, maxTokens: 3000 },
    );
    timeline.push("mitigation proposed");
    await chidori.log("plan ready", { chars: plan.length });

    // 3. The war room: wait for the humans. Fan-in on three named signals —
    // approval, context notes, and escalation — with a paging timeout.
    let approved = false;
    let abandoned = false;
    let approver = "unknown";
    let pagesSent = 0;
    let rounds = 0;

    while (!approved && !abandoned) {
      rounds++;
      const sig = await chidori.signal<{ decision?: string; text?: string }>(
        ["approve", "note", "escalate"],
        { timeoutMs: approvalTimeoutMs },
      );

      if (sig.timedOut) {
        if (pagesSent >= maxPages) {
          timeline.push(`nobody responded after ${pagesSent} pages — standing down`);
          abandoned = true;
          break;
        }
        pagesSent++;
        const page = await fetch(`${opsBase}/page`, {
          method: "POST",
          headers: { "content-type": "application/json" },
          body: JSON.stringify({ incident: alert.id, level: "secondary", attempt: pagesSent }),
        });
        timeline.push(`no response in ${approvalTimeoutMs}ms — paged secondary (ack: ${page.status})`);
        await chidori.log("paged secondary", { attempt: pagesSent });
        continue;
      }

      const who = sig.from ? `${sig.from.kind}:${sig.from.id}` : "unknown";
      if (sig.name === "note") {
        const note = sig.payload?.text ?? "";
        timeline.push(`note from ${who}: ${note}`);
        // Fold the responder's context into the plan.
        plan = await chidori.prompt(
          "Revise the mitigation given this new information from a responder. " +
            "Keep it to one concrete, reversible action; end with 'MITIGATION: <one-liner>'.\n" +
            `Current plan:\n${plan}\n\nNew information from ${who}: ${note}`,
          { type: "progress", maxTokens: 2000 },
        );
        timeline.push("plan revised with responder context");
        continue;
      }

      if (sig.name === "escalate") {
        pagesSent++;
        await fetch(`${opsBase}/page`, {
          method: "POST",
          headers: { "content-type": "application/json" },
          body: JSON.stringify({ incident: alert.id, level: "secondary", requestedBy: who }),
        });
        timeline.push(`escalation requested by ${who} — paged secondary`);
        continue;
      }

      // approve
      if (sig.payload?.decision === "abandon") {
        timeline.push(`${who} ordered stand-down`);
        abandoned = true;
      } else {
        approved = true;
        approver = who;
        timeline.push(`mitigation approved by ${who}`);
      }
    }

    // 4. Execute (only with a human approval on the record).
    let mitigationAck: JsonObject | null = null;
    if (approved) {
      const mitigation = plan.match(/MITIGATION:\s*(.+)/)?.[1] ?? plan.slice(0, 200);
      const resp = await fetch(`${opsBase}/mitigate`, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ incident: alert.id, action: mitigation, approvedBy: approver }),
      });
      mitigationAck = (await resp.json()) as JsonObject;
      timeline.push(`mitigation executed (ops ack: ${resp.status})`);
      await chidori.log("mitigation executed", { approver });
    }

    // 5. Postmortem — streamed as the final answer, then published.
    const postmortem = await chidori.prompt(
      "Write a concise incident postmortem in Markdown: summary, timeline, " +
        "root-cause hypothesis, mitigation, and two follow-up actions.\n" +
        `Alert: ${JSON.stringify(alert)}\nTriage: ${JSON.stringify(triage)}\n` +
        `Final plan:\n${plan}\n\nTimeline:\n- ${timeline.join("\n- ")}\n` +
        `Outcome: ${approved ? `mitigated, approved by ${approver}` : "stood down without action"}`,
      { type: "final", maxTokens: 4000 },
    );
    const reportPath = `incidents/${alert.id}.md`;
    await chidori.workspace.write(reportPath, postmortem);

    return {
      incident: alert.id,
      severity: triage.severity,
      outcome: approved ? "mitigated" : "stood-down",
      approvedBy: approved ? approver : null,
      warRoomRounds: rounds,
      pagesSent,
      mitigationAck,
      report: reportPath,
      timeline,
    };
  },
);
