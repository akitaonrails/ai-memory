/**
 * Processing jobs against a real server with chunking on and a provider whose
 * endpoint refuses connections: the server accepts a start (the job is
 * created and stored durably) while no model ever answers. This is the
 * successful-status decode check the drift test's lookups cannot give.
 */

import assert from "node:assert/strict";
import { after, before, describe, it } from "node:test";

import { AiMemoryClient } from "../../src/client.js";
import { AiMemoryError } from "../../src/errors.js";
import { startServer, type ServerHandle } from "../support/server.js";

const scope = { workspace: "it-jobs", project: "app" };
let server: ServerHandle;
let memory: AiMemoryClient;
let sessionId: string;

before(async () => {
  server = await startServer({ chunking: true, unreachableLlm: true });
  memory = new AiMemoryClient({ serverUrl: server.url, token: server.rootToken, scope });
  sessionId = await server.seedSession(scope.workspace, scope.project, "it-jobs-session");
});
after(async () => server.stop());

describe("jobs against a chunking-enabled server", () => {
  it("start returns a decoded status, a retry joins the same job, and status reads it back", async () => {
    assert.deepEqual(await memory.jobs.discover(), { start: true, status: true });

    const started = await memory.jobs.start({ sessionId, kind: "consolidate" });
    assert.equal(started.sessionId, sessionId);
    assert.equal(started.kind, "consolidate");
    assert.equal(typeof started.state, "string");
    for (const count of [
      started.chunksTotal,
      started.chunksDone,
      started.callsUsed,
      started.waitMs,
      started.execMs,
    ]) {
      assert.ok(Number.isInteger(count) && count >= 0, `count ${String(count)}`);
    }
    assert.ok(started.maxCalls > 0);

    const again = await memory.jobs.start({ sessionId, kind: "consolidate" });
    assert.equal(again.jobId, started.jobId, "a compatible start joins the existing job");

    const status = await memory.jobs.status({ jobId: started.jobId });
    assert.equal(status.jobId, started.jobId);
    assert.equal(status.sessionId, sessionId);
    assert.equal(status.kind, "consolidate");
    assert.ok(Number.isInteger(status.chunksTotal) && Number.isInteger(status.chunksDone));
    assert.ok(status.callsUsed <= status.maxCalls);
    assert.equal(status.raw["job_id"], started.jobId);
  });

  it("a job id is not readable through another project's scope", async () => {
    const started = await memory.jobs.start({ sessionId, kind: "consolidate" });
    await assert.rejects(
      memory.jobs.status({ jobId: started.jobId, scope: { ...scope, project: "other" } }),
      AiMemoryError,
    );
  });

  it("an unknown session or an unknown kind is refused, never started", async () => {
    await assert.rejects(
      memory.jobs.start({ sessionId: "00000000-0000-0000-0000-000000000000", kind: "consolidate" }),
      AiMemoryError,
    );
    await assert.rejects(
      memory.callTool("memory_processing_job_start", {
        ...scope,
        session_id: sessionId,
        kind: "dream",
      }),
      AiMemoryError,
    );
  });
});
