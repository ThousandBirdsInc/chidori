// Tests for the authoring entrypoints (`chidori` and `run`). Outside the
// chidori runtime these are inert stand-ins that throw a helpful error: the
// runtime strips the import and supplies the real implementations at execution
// time.

import assert from "node:assert/strict";
import { describe, it } from "node:test";

import { chidori, run } from "../dist/index.js";

describe("authoring stand-ins", () => {
  it("accessing any chidori member throws outside the runtime", () => {
    assert.throws(() => chidori.log, /only available inside the chidori runtime/);
    assert.throws(() => chidori.prompt, /only available inside the chidori runtime/);
  });

  it("calling run() throws outside the runtime", () => {
    assert.throws(() => run(async () => ({ ok: true })), /only available inside the chidori runtime/);
  });
});
