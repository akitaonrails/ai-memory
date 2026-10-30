import assert from "node:assert/strict";
import { describe, it } from "node:test";

import { AiMemoryClient } from "../../src/client.js";
import { TOOL_COVERAGE } from "../../src/coverage.js";
import {
  AbortError,
  ArgumentError,
  PreconditionFailedError,
  ProtocolError,
  UnsupportedServerError,
} from "../../src/errors.js";
import { FakeServer, RpcFailure, json } from "../support/fake.js";
import * as payloads from "../support/payloads.js";

const SCOPE = { workspace: "demo", project: "app" };
const OTHER = { workspace: "demo", project: "other" };

function setup(): { server: FakeServer; memory: AiMemoryClient } {
  const server = new FakeServer();
  const memory = new AiMemoryClient({
    serverUrl: "http://127.0.0.1:49374",
    scope: SCOPE,
    fetch: server.fetch,
  });
  return { server, memory };
}

describe("pages", () => {
  it("write sends wire names, omits unset options and decodes the result", async () => {
    const { server, memory } = setup();
    server.tools.set("memory_write_page", () => payloads.write);
    const result = await memory.pages.write({
      path: "notes/retries.md",
      body: "# Retry policy",
      tags: ["a"],
      expiresAt: "2099-01-01",
      expectedPageId: payloads.PAGE_ID,
    });
    assert.deepEqual(server.lastArgs(), {
      workspace: "demo",
      project: "app",
      path: "notes/retries.md",
      body: "# Retry policy",
      tags: ["a"],
      expires_at: "2099-01-01",
      expected_page_id: payloads.PAGE_ID,
    });
    assert.equal(result.pageId, payloads.NEXT_PAGE_ID);
    assert.equal(result.path, "notes/retries.md");
    assert.equal(result.checkpoint, "b04977fb");
    assert.equal(result.raw["page_id"], payloads.NEXT_PAGE_ID);
  });

  it("write with createOnly sends create_only and refuses it with expectedPageId", async () => {
    const { server, memory } = setup();
    server.tools.set("memory_write_page", () => payloads.write);
    await memory.pages.write({ path: "notes/a.md", body: "x", createOnly: true, scope: OTHER });
    assert.deepEqual(server.lastArgs(), {
      workspace: "demo",
      project: "other",
      path: "notes/a.md",
      body: "x",
      create_only: true,
    });
    const before = server.requests.length;
    await assert.rejects(
      memory.pages.write({ path: "a.md", body: "x", createOnly: true, expectedPageId: "v" }),
      ArgumentError,
    );
    assert.equal(server.requests.length, before);
  });

  it("write surfaces a precondition failure with both ids", async () => {
    const { server, memory } = setup();
    server.tools.set(
      "memory_write_page",
      () =>
        new RpcFailure(-32600, "precondition failed", {
          reason: "precondition_failed",
          path: "notes/a.md",
          expected_page_id: "old",
          current_page_id: "new",
        }),
    );
    await assert.rejects(
      memory.pages.write({ path: "notes/a.md", body: "x", expectedPageId: "old" }),
      (error: unknown) => {
        assert.ok(error instanceof PreconditionFailedError);
        assert.equal(error.code, "precondition_failed");
        assert.equal(error.expectedPageId, "old");
        assert.equal(error.currentPageId, "new");
        return true;
      },
    );
  });

  it("patch sends typed anchored edits and decodes the new version", async () => {
    const { server, memory } = setup();
    server.tools.set("memory_patch_page", () => payloads.patch);
    const result = await memory.pages.patch({
      path: "notes/retries.md",
      expectedPageId: payloads.PAGE_ID,
      edits: [
        { op: "append", target: "section", anchor: "Log", content: "next" },
        {
          op: "replace_section",
          anchor: "Open",
          sectionSha256: "a".repeat(64),
          content: "none",
        },
      ],
    });
    assert.deepEqual(server.lastArgs(), {
      workspace: "demo",
      project: "app",
      path: "notes/retries.md",
      expected_page_id: payloads.PAGE_ID,
      edits: [
        { op: "append", target: "section", anchor: "Log", content: "next" },
        {
          op: "replace_section",
          anchor: "Open",
          section_sha256: "a".repeat(64),
          content: "none",
        },
      ],
    });
    assert.equal(result.pageId, payloads.NEXT_PAGE_ID);
  });

  it("read by path or query, never both, and decodes the page", async () => {
    const { server, memory } = setup();
    server.tools.set("memory_read_page", () => payloads.read);
    const page = await memory.pages.read({ path: "notes/retries.md" });
    assert.deepEqual(server.lastArgs(), { workspace: "demo", project: "app", path: "notes/retries.md" });
    assert.equal(page.pageId, payloads.PAGE_ID);
    assert.equal(page.body, payloads.read.body);
    assert.equal(page.sections[0]?.anchor, "Retry policy");
    assert.equal(page.sectionsTruncated, false);
    assert.equal(page.related, undefined);
    await memory.pages.read({ query: "retry", includeRelated: true, relatedDepth: 2 });
    assert.deepEqual(server.lastArgs(), {
      workspace: "demo",
      project: "app",
      query: "retry",
      include_related: true,
      related_depth: 2,
    });
    server.tools.set("memory_read_page", () => ({
      ...payloads.read,
      truncated: true,
      total_chars: 40_000,
    }));
    const capped = await memory.pages.read({ path: "notes/retries.md", maxChars: 500 });
    assert.deepEqual(server.lastArgs(), {
      workspace: "demo",
      project: "app",
      path: "notes/retries.md",
      max_chars: 500,
    });
    assert.equal(capped.truncated, true);
    assert.equal(capped.totalChars, 40_000);
    await memory.pages.read({ path: "preferences/style.md", global: true });
    assert.deepEqual(server.lastArgs(), {
      scope: "global",
      path: "preferences/style.md",
    });
    await assert.rejects(
      memory.pages.read({ path: "preferences/style.md", global: true, scope: OTHER }),
      ArgumentError,
    );
    server.tools.set("memory_read_page", () => payloads.read);
    const older = await memory.pages.read({ path: "notes/retries.md" });
    assert.equal(older.truncated, undefined);
    assert.equal(older.totalChars, undefined);
    const before = server.requests.length;
    await assert.rejects(
      memory.pages.read({ path: "a", query: "b" } as never),
      ArgumentError,
    );
    await assert.rejects(memory.pages.read({} as never), ArgumentError);
    assert.equal(server.requests.length, before);
  });

  it("delete decodes the checkpoints", async () => {
    const { server, memory } = setup();
    server.tools.set("memory_delete_page", () => payloads.del);
    const result = await memory.pages.delete({ path: "notes/gone.md", expectedPageId: "v1" });
    assert.deepEqual(server.lastArgs(), {
      workspace: "demo",
      project: "app",
      path: "notes/gone.md",
      expected_page_id: "v1",
    });
    assert.equal(result.deleted, true);
    assert.equal(result.preCheckpoint, null);
    assert.equal(result.checkpoint, "fa0a91e0");
  });

  it("update carries metadata over, applies the change and guards on the read version", async () => {
    const { server, memory } = setup();
    server.tools.set("memory_read_page", () => payloads.read);
    server.tools.set("memory_write_page", () => payloads.write);
    await memory.pages.update("notes/retries.md", (current) => ({
      body: `${current.body}\nmore`,
      tags: ["a", "b"],
      abstract: null,
    }));
    assert.deepEqual(server.lastArgs(), {
      workspace: "demo",
      project: "app",
      path: "notes/retries.md",
      body: "# Retry policy\nKeep the same event ID.\nmore",
      title: "Retry policy (explicit)",
      tier: "procedural",
      tags: ["a", "b"],
      pinned: true,
      kind: "howto",
      entities: ["foo bar"],
      relations: { fixes: ["notes/a.md"] },
      expires_at: "2099-01-01",
      expected_page_id: payloads.PAGE_ID,
    });
  });

  it("update writes nothing when the change function throws", async () => {
    const { server, memory } = setup();
    server.tools.set("memory_read_page", () => payloads.read);
    server.tools.set("memory_write_page", () => payloads.write);
    await assert.rejects(
      memory.pages.update("notes/retries.md", () => {
        throw new Error("caller bug");
      }),
      /caller bug/,
    );
    assert.deepEqual(
      server.toolCalls().map((c) => c.name),
      ["memory_read_page"],
    );
  });
});

describe("query", () => {
  it("project query sends the scope and decodes hits", async () => {
    const { server, memory } = setup();
    server.tools.set("memory_query", () => payloads.query);
    const result = await memory.query({ query: "retry", limit: 5, pinFirst: true, asOf: "2026-01-01T00:00:00Z" });
    assert.deepEqual(server.lastArgs(), {
      workspace: "demo",
      project: "app",
      query: "retry",
      limit: 5,
      pin_first: true,
      as_of: "2026-01-01T00:00:00Z",
    });
    assert.equal(result.hits.length, 1);
    assert.equal(result.hits[0]?.id, payloads.PAGE_ID);
    assert.equal(result.hits[0]?.superseded, false);
    assert.deepEqual(result.globalScopeHits, []);
  });

  it("global query omits every scope field and maps global hits", async () => {
    const { server, memory } = setup();
    server.tools.set("memory_query", () => payloads.globalQuery);
    const result = await memory.query({ query: "retry", global: true });
    assert.deepEqual(server.lastArgs(), { query: "retry", global: true });
    assert.equal(result.hits[0]?.workspace, "demo");
    assert.equal(result.hits[0]?.project, "app");
  });

  it("multi-scope query sends scopes and no workspace/project", async () => {
    const { server, memory } = setup();
    server.tools.set("memory_query", () => payloads.query);
    await memory.query({ query: "retry", scopes: [SCOPE, OTHER], includeSuperseded: true });
    assert.deepEqual(server.lastArgs(), {
      query: "retry",
      scopes: [SCOPE, OTHER],
      include_superseded: true,
    });
  });

  it("decodes superseded, pinned, raw hits and answers", async () => {
    const { server, memory } = setup();
    server.tools.set("memory_query", () => ({
      hits: [{ ...payloads.hit, superseded: true }, { ...payloads.hit, pinned: true }],
      raw_hits: [
        {
          id: "o1",
          session_id: payloads.SESSION_ID,
          kind: "user-prompt",
          title: "t",
          snippet: "s",
          rank: -1,
          created_at: "2026-10-13T00:00:00Z",
        },
      ],
      answer: { text: "The answer", citations: ["notes/retries.md"] },
      global_scope_hits: [payloads.hit],
    }));
    const result = await memory.query({ query: "retry", answer: true, reasoning: "low" });
    assert.equal(result.hits[0]?.superseded, true);
    assert.equal(result.hits[1]?.pinned, true);
    assert.equal(result.rawHits[0]?.sessionId, payloads.SESSION_ID);
    assert.equal(result.answer?.text, "The answer");
    assert.deepEqual(result.answer?.citations, ["notes/retries.md"]);
    assert.equal(result.globalScopeHits.length, 1);
  });

  it("decodes optional answer context accounting and accepts older responses", async () => {
    const { server, memory } = setup();
    server.tools.set("memory_query", () => ({ hits: [payloads.hit] }));
    const legacy = await memory.query({ query: "retry" });
    assert.equal(legacy.answerContext, undefined);
    assert.equal(Object.hasOwn(legacy, "answerContext"), false);
    server.tools.set("memory_query", () => ({
      hits: [payloads.hit],
      answer_unavailable: "no evidence space",
      answer_context: { admitted: 0, omitted: 1, clipped: 0, estimated_input_tokens: 2000,
        reserved_output_tokens: 2000, max_context_tokens: 1000 },
    }));
    const result = await memory.query({ query: "retry", answer: true });
    assert.deepEqual(result.answerContext, { admitted: 0, omitted: 1, clipped: 0,
      estimatedInputTokens: 2000, reservedOutputTokens: 2000, maxContextTokens: 1000 });
    assert.equal(result.hits.length, 1);
    assert.equal(result.answerUnavailable, "no evidence space");
  });

  it("raises ProtocolError when a promised field is missing", async () => {
    const { server, memory } = setup();
    server.tools.set("memory_query", () => ({ hits: [{ path: "notes/a.md" }] }));
    await assert.rejects(memory.query({ query: "retry" }), ProtocolError);
  });
});

describe("handoffs", () => {
  it("begin, list, accept and cancel send wire names and decode results", async () => {
    const { server, memory } = setup();
    server.tools.set("memory_handoff_begin", () => ({ handoff_id: payloads.HANDOFF_ID }));
    server.tools.set("memory_handoff_list", () => ({
      handoffs: [payloads.handoffRow],
      newer_unended_session: payloads.briefing.recent_sessions[0],
    }));
    server.tools.set("memory_handoff_accept", (args) =>
      args["handoff_id"] === "gone"
        ? { handoff: null, status: "none_pending" }
        : { handoff: payloads.handoffRow, status: "claimed" },
    );
    server.tools.set("memory_handoff_cancel", () => ({
      handoff_id: payloads.HANDOFF_ID,
      cancelled: true,
      state: "expired",
    }));

    const begun = await memory.handoffs.begin({
      summary: "Left off here",
      nextSteps: ["a"],
      openQuestions: ["q"],
      filesTouched: ["src/a.ts"],
      shared: true,
    });
    assert.deepEqual(server.lastArgs(), {
      workspace: "demo",
      project: "app",
      summary: "Left off here",
      next_steps: ["a"],
      open_questions: ["q"],
      files_touched: ["src/a.ts"],
      shared: true,
    });
    assert.equal(begun.handoffId, payloads.HANDOFF_ID);
    assert.ok(!("cwd" in server.lastArgs()));

    const listed = await memory.handoffs.list({ limit: 5 });
    assert.deepEqual(server.lastArgs(), { workspace: "demo", project: "app", limit: 5 });
    assert.equal(listed.handoffs[0]?.summary, "Left off here");
    assert.equal(listed.newerUnendedSession?.agentKind, "open-code");
    assert.equal(listed.newerUnendedSession?.ended, false);

    const claimed = await memory.handoffs.accept({ handoffId: payloads.HANDOFF_ID });
    assert.equal(claimed.status, "claimed");
    assert.equal(claimed.handoff?.id, payloads.HANDOFF_ID);
    const none = await memory.handoffs.accept({ handoffId: "gone" });
    assert.equal(none.status, "none_pending");
    assert.equal(none.handoff, null);

    const cancelled = await memory.handoffs.cancel({ handoffId: payloads.HANDOFF_ID, anyOwner: true });
    assert.deepEqual(server.lastArgs(), {
      workspace: "demo",
      project: "app",
      handoff_id: payloads.HANDOFF_ID,
      any_owner: true,
    });
    assert.equal(cancelled.cancelled, true);
  });

  it("accept refuses an unknown status", async () => {
    const { server, memory } = setup();
    server.tools.set("memory_handoff_accept", () => ({ handoff: null, status: "mystery" }));
    await assert.rejects(memory.handoffs.accept(), ProtocolError);
  });
});

describe("messages", () => {
  it("send uses the client scope as sender and names the recipient", async () => {
    const { server, memory } = setup();
    server.tools.set("memory_message_send", () => ({ message_id: payloads.MESSAGE_ID }));
    const sent = await memory.messages.send({
      to: OTHER,
      body: "please do X",
      subject: "hi",
      nature: "measured",
      evidence: "tests/export.test.ts:42",
    });
    assert.deepEqual(server.lastArgs(), {
      to_workspace: "demo",
      to_project: "other",
      from_workspace: "demo",
      from_project: "app",
      body: "please do X",
      subject: "hi",
      nature: "measured",
      evidence: "tests/export.test.ts:42",
    });
    assert.equal(sent.messageId, payloads.MESSAGE_ID);
  });

  it("list, pop and cancel decode their results", async () => {
    const { server, memory } = setup();
    server.tools.set("memory_message_list", () => ({ messages: [payloads.messageRow] }));
    server.tools.set("memory_message_pop", () => ({
      message: { ...payloads.messageRow, state: "claimed" },
      security_notice: "UNTRUSTED",
    }));
    server.tools.set("memory_message_cancel", () => ({ cancelled: 2 }));
    const listed = await memory.messages.list({ box: "outbox", limit: 3 });
    assert.deepEqual(server.lastArgs(), { workspace: "demo", project: "app", box: "outbox", limit: 3 });
    assert.equal(listed.messages[0]?.body, "please do X");
    assert.equal(listed.messages[0]?.nature, "reported");
    assert.equal(listed.messages[0]?.evidence, "source message 42");
    assert.equal(listed.messages[0]?.toAgent, null);
    const popped = await memory.messages.pop({ messageId: payloads.MESSAGE_ID, agent: "codex" });
    assert.deepEqual(server.lastArgs(), {
      workspace: "demo",
      project: "app",
      message_id: payloads.MESSAGE_ID,
      agent: "codex",
    });
    assert.equal(popped.message?.state, "claimed");
    assert.equal(popped.securityNotice, "UNTRUSTED");
    const cancelled = await memory.messages.cancel();
    assert.deepEqual(server.lastArgs(), { workspace: "demo", project: "app" });
    assert.equal(cancelled.cancelled, 2);
    await assert.rejects(memory.messages.list({ box: "spam" as never }), ArgumentError);
  });

  it("pop decodes an empty inbox", async () => {
    const { server, memory } = setup();
    server.tools.set("memory_message_pop", () => ({ message: null }));
    const popped = await memory.messages.pop();
    assert.equal(popped.message, null);
  });
});

describe("reads", () => {
  it("status, recent and briefing", async () => {
    const { server, memory } = setup();
    server.tools.set("memory_status", () => payloads.status);
    server.tools.set("memory_recent", () => ({ hits: [payloads.hit] }));
    server.tools.set("memory_briefing", () => payloads.briefing);
    const status = await memory.status();
    assert.equal(status.counts.pagesAll, 2);
    assert.equal(status.scope.resolvedBy, "explicit");
    const recent = await memory.recent({ limit: 3 });
    assert.deepEqual(server.lastArgs(), { workspace: "demo", project: "app", limit: 3 });
    assert.equal(recent.hits[0]?.path, "notes/retries.md");
    const briefing = await memory.briefing({ recentPagesLimit: 2, settledFirst: true });
    assert.deepEqual(server.lastArgs(), {
      workspace: "demo",
      project: "app",
      recent_pages_limit: 2,
      settled_first: true,
    });
    assert.equal(briefing.activity7d.pagesUpdated, 1);
    assert.equal(briefing.recentPages[0]?.kind, "note");
    assert.equal(briefing.recentSessions[0]?.sessionId, "09db5007-256a-5d22-970e-c9deb0081218");
    assert.equal(briefing.recentSessions[0]?.ended, false);
    assert.equal(briefing.recentSessions[0]?.current, false);
  });

  it("explore without a provider returns the briefing and a reason", async () => {
    const { server, memory } = setup();
    server.tools.set("memory_explore", () => ({
      prose: null,
      reason: "no LLM provider configured",
      briefing: payloads.briefing,
    }));
    const explored = await memory.explore({ focus: "rules", reasoning: "high" });
    assert.deepEqual(server.lastArgs(), {
      workspace: "demo",
      project: "app",
      focus: "rules",
      reasoning: "high",
    });
    assert.equal(explored.prose, null);
    assert.equal(explored.briefing?.counts.pagesLatest, 1);
  });

  it("observations and feedback", async () => {
    const { server, memory } = setup();
    server.tools.set("memory_read_session_observations", () => payloads.observations);
    server.tools.set("memory_feedback", () => ({
      recorded: true,
      path: "notes/retries.md",
      page_id: payloads.PAGE_ID,
      signal: "helpful",
    }));
    const page = await memory.observations({
      sessionId: payloads.SESSION_ID,
      kinds: ["user-prompt"],
      order: "desc",
      bodyMaxChars: 500,
    });
    assert.deepEqual(server.lastArgs(), {
      workspace: "demo",
      project: "app",
      session_id: payloads.SESSION_ID,
      kinds: ["user-prompt"],
      order: "desc",
      body_max_chars: 500,
    });
    assert.equal(page.observations[0]?.kind, "user-prompt");
    assert.equal(page.session.observationCount, 3);
    const fb = await memory.feedback({ path: "notes/retries.md", signal: "helpful", reason: "useful" });
    assert.equal(fb.recorded, true);
  });
});

describe("capability gating", () => {
  it("cancels concurrent capability waits independently", async () => {
    for (const heldMethod of ["initialize", "tools/list"] as const) {
      for (const cancelled of ["first", "second"] as const) {
      const server = new FakeServer();
      let announce!: () => void;
      let release!: () => void;
      const started = new Promise<void>((resolve) => { announce = resolve; });
      const held = new Promise<void>((resolve) => { release = resolve; });
      server.mcp = async (request) => {
        const body = request.body as { id?: number; method: string };
        const reply = (result: unknown) => json({ jsonrpc: "2.0", id: body.id, result });
        if (body.method === "initialize") {
          if (heldMethod === "initialize") {
            announce();
            await held;
          }
          return reply({
            protocolVersion: "2025-03-26",
            serverInfo: { name: "ai-memory", version: "3.2.10" },
            capabilities: {},
          });
        }
        if (body.method === "tools/list") {
          if (heldMethod === "tools/list") {
            announce();
            await held;
          }
          return reply({
            tools: [{
              name: "memory_status",
              inputSchema: { properties: { workspace: {}, project: {} } },
            }],
          });
        }
        if (body.method === "tools/call") {
          return reply({ content: [{ type: "text", text: JSON.stringify(payloads.status) }] });
        }
        throw new Error(`unexpected method ${body.method}`);
      };
      const memory = new AiMemoryClient({
        serverUrl: "http://127.0.0.1:49374",
        scope: SCOPE,
        fetch: server.fetch,
      });
      const firstController = new AbortController();
      const secondController = new AbortController();
      const first = memory.status({}, { signal: firstController.signal });
      await started;
      const second = memory.status({}, { signal: secondController.signal });

      (cancelled === "first" ? firstController : secondController).abort();
      const cancelledCall = cancelled === "first" ? first : second;
      await assert.rejects(cancelledCall, AbortError);
      release();
      const survivingCall = cancelled === "first" ? second : first;
      assert.equal((await survivingCall).scope.project, "app");
      }
    }
  });

  it("a server without expected_page_id refuses a conditional write before tools/call", async () => {
    const { server, memory } = setup();
    const write = server.schemas["memory_write_page"];
    assert.ok(write !== undefined);
    delete write.properties["expected_page_id"];
    server.serverVersion = "2.6.6";
    server.tools.set("memory_write_page", () => payloads.write);
    await assert.rejects(
      memory.pages.write({ path: "notes/a.md", body: "x", expectedPageId: "v1" }),
      (error: unknown) => {
        assert.ok(error instanceof UnsupportedServerError);
        assert.equal(error.feature, "memory_write_page.expected_page_id");
        assert.equal(error.minimumVersion, "3.0.0");
        assert.equal(error.serverVersion, "2.6.6");
        return true;
      },
    );
    assert.equal(server.toolCalls().length, 0);
  });

  it("a server without a tool refuses its wrapper", async () => {
    const { server, memory } = setup();
    delete server.schemas["memory_message_send"];
    await assert.rejects(memory.messages.send({ to: OTHER, body: "x" }), UnsupportedServerError);
    assert.equal(server.toolCalls().length, 0);
  });

  it("probes once per client and re-probes after refresh", async () => {
    const { server, memory } = setup();
    server.tools.set("memory_status", () => payloads.status);
    server.routes.set("GET /identity", () =>
      json({ version: "3.0.0", level: "root", operator: null, distinguishes_operators: false }),
    );
    server.routes.set("GET /api/v1/workspaces", () => json([]));
    await memory.status();
    await memory.status();
    const caps = await memory.capabilities();
    assert.equal(caps.meetsMinimumVersion, true);
    assert.equal(caps.apiMounted, true);
    assert.equal(caps.identity?.level, "root");
    const count = (method: string) =>
      server.requests.filter((r) => (r.body as { method?: string } | undefined)?.method === method)
        .length;
    assert.equal(count("initialize"), 1);
    await memory.capabilities({ refresh: true });
    assert.equal(count("initialize"), 2);
  });

  it("reports apiMounted false and identity null on 404", async () => {
    const { memory } = setup();
    const caps = await memory.capabilities();
    assert.equal(caps.apiMounted, false);
    assert.equal(caps.identity, null);
  });

  it("callTool initializes without schema probing and returns the decoded JSON", async () => {
    const { server, memory } = setup();
    delete server.schemas["memory_lint"];
    server.tools.set("memory_lint", () => ({ report: "ok" }));
    assert.deepEqual(await memory.callTool("memory_lint", { dry_run: true }), { report: "ok" });
    assert.deepEqual(
      server.requests.map((r) => (r.body as { method: string }).method),
      ["initialize", "tools/call"],
    );
  });
});

describe("wire names", () => {
  it("each wrapper sends only arguments its coverage entry declares", async () => {
    const { server, memory } = setup();
    server.enableChunking();
    for (const tool of Object.keys(TOOL_COVERAGE)) {
      server.tools.set(tool, () => ({}));
    }
    // Call every wrapper with every option set; decoding failures are
    // expected because the fake answers `{}`.
    const calls: Promise<unknown>[] = [
      memory.pages.write({
        path: "p",
        body: "b",
        title: "t",
        tier: "semantic",
        tags: ["t"],
        pinned: true,
        kind: "k",
        entities: ["e"],
        abstract: "a",
        relations: { fixes: ["x"] },
        expiresAt: "2099-01-01",
        expectedPageId: "v",
      }),
      memory.pages.write({ path: "p", body: "b", createOnly: true }),
      memory.pages.patch({
        path: "p",
        expectedPageId: "v",
        edits: [
          { op: "append", target: "section", anchor: "A", content: "a" },
          { op: "append", target: "end_of_page", content: "b" },
          { op: "add_section", anchor: "## B", content: "c" },
          { op: "replace_section", anchor: "C", sectionSha256: "h", content: "d" },
        ],
      }),
      memory.pages.read({ path: "p", includeRelated: true, relatedDepth: 2, maxChars: 500 }),
      memory.pages.read({ path: "global", global: true }),
      memory.pages.read({ query: "q" }),
      memory.pages.delete({ path: "p", expectedPageId: "v" }),
      memory.query({
        query: "q",
        limit: 1,
        includeExpired: true,
        includeSuperseded: true,
        pinFirst: true,
        explain: true,
        asOf: "2026-01-01T00:00:00Z",
        answer: true,
        reasoning: "low",
        kind: "decision",
        tier: "semantic",
        pathPrefix: "decisions/",
        tag: "t",
      }),
      memory.query({ query: "q", scopes: [SCOPE] }),
      memory.query({ query: "q", global: true }),
      memory.status(),
      memory.recent({ limit: 1 }),
      memory.briefing({ recentPagesLimit: 1, settledFirst: true }),
      memory.explore({ focus: "f", recentPagesLimit: 1, reasoning: "low" }),
      memory.observations({
        sessionId: "s",
        limit: 1,
        offset: 1,
        order: "asc",
        kinds: ["stop"],
        query: "q",
        bodyMaxChars: 300,
      }),
      memory.feedback({ path: "p", signal: "stale", reason: "r" }),
      memory.handoffs.begin({
        summary: "s",
        openQuestions: ["q"],
        nextSteps: ["n"],
        filesTouched: ["f"],
        shared: true,
      }),
      memory.handoffs.list({ anyOwner: true, limit: 1 }),
      memory.handoffs.accept({ handoffId: "h", anyOwner: true }),
      memory.handoffs.cancel({ handoffId: "h", anyOwner: true }),
      memory.messages.send({
        to: OTHER,
        from: SCOPE,
        body: "b",
        subject: "s",
        nature: "measured",
        evidence: "test:1",
      }),
      memory.messages.list({ box: "inbox", limit: 1 }),
      memory.messages.pop({ messageId: "m", agent: "codex" }),
      memory.messages.cancel({ messageId: "m" }),
      memory.jobs.start({ sessionId: "s", kind: "auto_improve" }),
      memory.jobs.status({ jobId: "j" }),
    ];
    await Promise.allSettled(calls);
    const seen = new Set<string>();
    for (const { name, args } of server.toolCalls()) {
      const entry = TOOL_COVERAGE[name];
      assert.ok(entry !== undefined && entry.status === "wrapped", `${name} is wrapped`);
      for (const key of Object.keys(args)) {
        assert.ok(entry.sends.includes(key), `${name} sent undeclared argument ${key}`);
        seen.add(`${name}.${key}`);
      }
    }
    // Every declared argument is reachable from some wrapper call above.
    for (const [name, entry] of Object.entries(TOOL_COVERAGE)) {
      if (entry.status !== "wrapped") continue;
      for (const key of entry.sends) {
        assert.ok(seen.has(`${name}.${key}`), `${name}.${key} is declared but never sent`);
      }
    }
  });
});
