import assert from "node:assert/strict";
import { describe, it } from "node:test";

import { AiMemoryClient } from "../../src/client.js";
import { availability, expectedTools, TOOL_COVERAGE } from "../../src/coverage.js";
import {
  ArgumentError,
  ProtocolError,
  ToolError,
  UnsupportedServerError,
} from "../../src/errors.js";
import { variantProblems, VARIANTS } from "../support/coverage-check.js";
import { CHUNKING_TOOL_SCHEMAS, FakeServer, SNAPSHOT, json } from "../support/fake.js";

const SCOPE = { workspace: "demo", project: "app" };
const OTHER = { workspace: "demo", project: "other" };
const JOB_ID = "6f0b3f1e-0000-4000-8000-000000000001";
const SESSION_ID = "6f0b3f1e-0000-4000-8000-000000000002";

const queued = {
  job_id: JOB_ID,
  session_id: SESSION_ID,
  kind: "consolidate",
  state: "queued",
  chunks_total: 0,
  chunks_done: 0,
  calls_used: 0,
  max_calls: 128,
  wait_ms: 0,
  exec_ms: 0,
  // A queued job has no last_error or result: the server omits both.
};

function setup(chunking: boolean): { server: FakeServer; memory: AiMemoryClient } {
  const server = new FakeServer();
  if (chunking) server.enableChunking();
  const memory = new AiMemoryClient({
    serverUrl: "http://127.0.0.1:49374",
    scope: SCOPE,
    fetch: server.fetch,
  });
  return { server, memory };
}

describe("jobs.start and jobs.status", () => {
  it("start forwards the scope and wire names and decodes the status", async () => {
    const { server, memory } = setup(true);
    server.tools.set("memory_processing_job_start", () => queued);
    const status = await memory.jobs.start({ sessionId: SESSION_ID, kind: "consolidate" });
    assert.deepEqual(server.lastArgs(), {
      workspace: "demo",
      project: "app",
      session_id: SESSION_ID,
      kind: "consolidate",
    });
    assert.equal(status.jobId, JOB_ID);
    assert.equal(status.sessionId, SESSION_ID);
    assert.equal(status.kind, "consolidate");
    assert.equal(status.state, "queued");
    assert.equal(status.chunksTotal, 0);
    assert.equal(status.chunksDone, 0);
    assert.equal(status.callsUsed, 0);
    assert.equal(status.maxCalls, 128);
    assert.equal(status.waitMs, 0);
    assert.equal(status.execMs, 0);
    assert.equal(status.lastError, null);
    assert.equal(status.result, null);
    assert.equal(status.raw["job_id"], JOB_ID);
  });

  it("a call scope overrides the default and is always sent", async () => {
    const { server, memory } = setup(true);
    server.tools.set("memory_processing_job_start", () => queued);
    server.tools.set("memory_processing_job_status", () => queued);
    await memory.jobs.start({ sessionId: SESSION_ID, kind: "auto_improve", scope: OTHER });
    assert.deepEqual(server.lastArgs(), {
      workspace: "demo",
      project: "other",
      session_id: SESSION_ID,
      kind: "auto_improve",
    });
    await memory.jobs.status({ jobId: JOB_ID, scope: OTHER });
    assert.deepEqual(server.lastArgs(), { workspace: "demo", project: "other", job_id: JOB_ID });
  });

  it("a client without a default scope refuses before sending", async () => {
    const server = new FakeServer();
    server.enableChunking();
    const memory = new AiMemoryClient({ serverUrl: "http://127.0.0.1:49374", fetch: server.fetch });
    await assert.rejects(memory.jobs.start({ sessionId: SESSION_ID, kind: "consolidate" }));
    await assert.rejects(memory.jobs.status({ jobId: JOB_ID }));
    assert.equal(server.toolCalls().length, 0);
  });

  it("status decodes progress and a completed job's result", async () => {
    const { server, memory } = setup(true);
    const result = { outcomes: [{ path: "sessions/a.md", title: "A", page_id: "p", tags: [] }] };
    server.tools.set("memory_processing_job_status", () => ({
      ...queued,
      kind: "consolidate_multi",
      state: "completed",
      chunks_total: 7,
      chunks_done: 7,
      calls_used: 9,
      wait_ms: 1500,
      exec_ms: 42000,
      result,
    }));
    const status = await memory.jobs.status({ jobId: JOB_ID });
    assert.deepEqual(server.lastArgs(), { workspace: "demo", project: "app", job_id: JOB_ID });
    assert.equal(status.kind, "consolidate_multi");
    assert.equal(status.state, "completed");
    assert.equal(status.chunksTotal, 7);
    assert.equal(status.chunksDone, 7);
    assert.equal(status.callsUsed, 9);
    assert.equal(status.waitMs, 1500);
    assert.equal(status.execMs, 42000);
    assert.deepEqual(status.result, result);
  });

  it("status keeps a redacted error and a state a newer server added", async () => {
    const { server, memory } = setup(true);
    server.tools.set("memory_processing_job_status", () => ({
      ...queued,
      state: "budget_exceeded",
      calls_used: 128,
      last_error: "call budget exhausted",
    }));
    const status = await memory.jobs.status({ jobId: JOB_ID });
    assert.equal(status.state, "budget_exceeded");
    assert.equal(status.lastError, "call budget exhausted");
    server.tools.set("memory_processing_job_status", () => ({ ...queued, state: "someday" }));
    assert.equal((await memory.jobs.status({ jobId: JOB_ID })).state, "someday");
  });

  it("a status without the contract fields is a ProtocolError", async () => {
    const { server, memory } = setup(true);
    server.tools.set("memory_processing_job_status", () => ({ job_id: JOB_ID }));
    await assert.rejects(memory.jobs.status({ jobId: JOB_ID }), ProtocolError);
    server.tools.set("memory_processing_job_start", () => ({ ...queued, calls_used: "0" }));
    await assert.rejects(
      memory.jobs.start({ sessionId: SESSION_ID, kind: "consolidate" }),
      ProtocolError,
    );
  });

  it("progress counts are required numbers, never a null sentinel", async () => {
    const { server, memory } = setup(true);
    for (const field of ["chunks_total", "chunks_done", "wait_ms", "exec_ms"]) {
      for (const bad of [null, undefined, "0"]) {
        const body: Record<string, unknown> = { ...queued, [field]: bad };
        if (bad === undefined) delete body[field];
        server.tools.set("memory_processing_job_status", () => body);
        await assert.rejects(
          memory.jobs.status({ jobId: JOB_ID }),
          ProtocolError,
          `${field}=${String(bad)}`,
        );
      }
    }
  });

  it("rejects bad arguments before any request", async () => {
    const { server, memory } = setup(true);
    await assert.rejects(memory.jobs.start({ sessionId: " ", kind: "consolidate" }), ArgumentError);
    await assert.rejects(
      memory.jobs.start({ sessionId: SESSION_ID, kind: "dream" as "consolidate" }),
      ArgumentError,
    );
    await assert.rejects(memory.jobs.status({ jobId: "" }), ArgumentError);
    assert.equal(server.requests.length, 0);
  });
});

describe("jobs against a server with chunking disabled", () => {
  it("discover reports both tools missing and the wrappers refuse before tools/call", async () => {
    const { server, memory } = setup(false);
    assert.deepEqual(await memory.jobs.discover(), { start: false, status: false });
    await assert.rejects(
      memory.jobs.start({ sessionId: SESSION_ID, kind: "consolidate" }),
      (error: unknown) => {
        assert.ok(error instanceof UnsupportedServerError);
        assert.equal(error.feature, "the memory_processing_job_start tool");
        assert.match(error.message, /\[chunking\] enabled = true/);
        assert.doesNotMatch(error.message, /or newer is required/);
        return true;
      },
    );
    await assert.rejects(memory.jobs.status({ jobId: JOB_ID }), UnsupportedServerError);
    assert.equal(server.toolCalls().length, 0);
  });

  it("discover reports each tool on its own: status can outlive start", async () => {
    const { server, memory } = setup(true);
    delete server.schemas["memory_processing_job_start"];
    server.tools.set("memory_processing_job_status", () => queued);
    assert.deepEqual(await memory.jobs.discover(), { start: false, status: true });
    await assert.rejects(
      memory.jobs.start({ sessionId: SESSION_ID, kind: "consolidate" }),
      UnsupportedServerError,
    );
    assert.equal((await memory.jobs.status({ jobId: JOB_ID })).jobId, JOB_ID);
  });

  it("a flag turned off after the tool list was cached surfaces the server's refusal", async () => {
    const { server, memory } = setup(true);
    assert.deepEqual(await memory.jobs.discover(), { start: true, status: true });
    server.tools.set("memory_processing_job_start", (_args, r) =>
      json({
        jsonrpc: "2.0",
        id: (r.body as { id: number }).id,
        result: {
          content: [{ type: "text", text: "chunked session processing is disabled" }],
          isError: true,
        },
      }),
    );
    await assert.rejects(
      memory.jobs.start({ sessionId: SESSION_ID, kind: "consolidate" }),
      (error: unknown) => {
        assert.ok(error instanceof ToolError);
        assert.equal(error.tool, "memory_processing_job_start");
        assert.match(error.detail, /disabled/);
        return true;
      },
    );
  });

  it("capabilities and refresh pick up a server restarted with the flag on", async () => {
    const { server, memory } = setup(false);
    assert.equal((await memory.capabilities()).tools["memory_processing_job_start"], undefined);
    server.enableChunking();
    assert.deepEqual(await memory.jobs.discover(), { start: false, status: false });
    const refreshed = await memory.capabilities({ refresh: true });
    assert.deepEqual([...(refreshed.tools["memory_processing_job_start"]?.properties ?? [])].sort(), [
      "kind",
      "project",
      "session_id",
      "workspace",
    ]);
    assert.deepEqual(await memory.jobs.discover(), { start: true, status: true });
  });
});

describe("conditional tool classification", () => {
  const [base, enabled, retained] = VARIANTS;
  assert.ok(base !== undefined && enabled !== undefined);

  it("gates exactly the two job tools behind chunking and keeps the 25 legacy tools", () => {
    const gated = Object.entries(TOOL_COVERAGE)
      .filter(([, entry]) => entry.feature === "chunking")
      .map(([name]) => name)
      .sort();
    assert.deepEqual(gated, ["memory_processing_job_start", "memory_processing_job_status"]);
    assert.equal(expectedTools([]).length, 25);
    assert.equal(expectedTools(["chunking"]).length, 27);
  });

  it("the committed default snapshot is the legacy 25 and passes without the job tools", () => {
    assert.equal(Object.keys(SNAPSHOT).length, 25);
    assert.deepEqual(variantProblems(base, SNAPSHOT), []);
  });

  it("the job tool schemas satisfy every argument the wrappers can send", () => {
    assert.deepEqual(variantProblems(enabled, { ...SNAPSHOT, ...CHUNKING_TOOL_SCHEMAS }), []);
  });

  it("an enabled snapshot missing a gated tool fails, an unclassified extra fails", () => {
    const missing = { ...SNAPSHOT, ...CHUNKING_TOOL_SCHEMAS };
    delete missing["memory_processing_job_status"];
    assert.deepEqual(variantProblems(enabled, missing), [
      "schema/tools.chunking.json (chunking): memory_processing_job_status is classified in src/coverage.ts but missing",
    ]);
    const extra = { ...SNAPSHOT, ...CHUNKING_TOOL_SCHEMAS, memory_new_tool: { properties: {}, required: [] } };
    assert.deepEqual(variantProblems(enabled, extra), [
      "schema/tools.chunking.json (chunking): memory_new_tool is listed but not classified in src/coverage.ts",
    ]);
  });

  it("a default snapshot that lists a gated tool fails", () => {
    const leaked = { ...SNAPSHOT, ...CHUNKING_TOOL_SCHEMAS };
    const problems = variantProblems(base, leaked);
    assert.equal(problems.length, 2);
    assert.match(problems[0] ?? "", /needs the chunking feature but is listed without it/);
  });

  it("a wire argument the schema lacks fails", () => {
    const broken = structuredClone({ ...SNAPSHOT, ...CHUNKING_TOOL_SCHEMAS });
    delete broken["memory_processing_job_start"]?.properties["session_id"];
    assert.deepEqual(variantProblems(enabled, broken), [
      'schema/tools.chunking.json (chunking): memory_processing_job_start: the SDK can send "session_id", which the schema lacks',
    ]);
  });

  it("the existing-jobs variant requires the read-only status and refuses start", () => {
    assert.ok(retained !== undefined);
    assert.equal(retained.existingJobs, true);
    assert.deepEqual(retained.features, []);
    assert.equal(expectedTools([], true).length, 26);
    assert.ok(expectedTools([], true).includes("memory_processing_job_status"));
    assert.ok(!expectedTools([], true).includes("memory_processing_job_start"));
    const { memory_processing_job_start: _start, ...statusOnly } = {
      ...SNAPSHOT,
      ...CHUNKING_TOOL_SCHEMAS,
    };
    assert.deepEqual(variantProblems(retained, statusOnly), []);
  });

  it("the 26-tool status-only result is not valid for a fresh flag-off server", () => {
    const { memory_processing_job_start: _start, ...statusOnly } = {
      ...SNAPSHOT,
      ...CHUNKING_TOOL_SCHEMAS,
    };
    assert.deepEqual(variantProblems(base, statusOnly), [
      "schema/tools.json (default): memory_processing_job_status needs the chunking feature but is listed without it",
    ]);
  });

  it("the existing-jobs variant fails when it lists start or drops status", () => {
    assert.ok(retained !== undefined);
    const both = { ...SNAPSHOT, ...CHUNKING_TOOL_SCHEMAS };
    assert.deepEqual(variantProblems(retained, both), [
      "schema/tools.chunking-retained.json (chunking-off-existing-jobs): memory_processing_job_start needs the chunking feature but is listed without it",
    ]);
    assert.deepEqual(variantProblems(retained, SNAPSHOT), [
      "schema/tools.chunking-retained.json (chunking-off-existing-jobs): memory_processing_job_status is classified in src/coverage.ts but missing",
    ]);
  });

  it("the generated availability wording says a retained tool also needs existing jobs", () => {
    const status = TOOL_COVERAGE["memory_processing_job_status"];
    const start = TOOL_COVERAGE["memory_processing_job_start"];
    assert.ok(status !== undefined && start !== undefined);
    assert.match(availability(status) ?? "", /earlier jobs from an enabled run are still stored/);
    assert.doesNotMatch(availability(start) ?? "", /earlier jobs/);
  });
});
