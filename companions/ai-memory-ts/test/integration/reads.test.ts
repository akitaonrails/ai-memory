import assert from "node:assert/strict";
import { after, before, describe, it } from "node:test";

import { AiMemoryClient } from "../../src/client.js";
import { AuthError, InvalidParamsError, NotFoundError } from "../../src/errors.js";
import { startServer, type ServerHandle } from "../support/server.js";

let server: ServerHandle;
let memory: AiMemoryClient;
let sessionId: string;
const scope = { workspace: "it-reads", project: "app" };

before(async () => {
  server = await startServer();
  memory = new AiMemoryClient({ serverUrl: server.url, token: server.rootToken, scope });
  await memory.pages.write({ path: "notes/a.md", body: "# Alpha\nfirst page" });
  await memory.pages.write({ path: "_rules/style.md", body: "# Style\nuse tabs" });
  sessionId = await server.seedSession(scope.workspace, scope.project, "it-reads-session");
});
after(async () => server.stop());

describe("capabilities and identity", () => {
  it("reports the server, tools, identity and the mounted API", async () => {
    const caps = await memory.capabilities();
    assert.equal(caps.server.name, "ai-memory");
    assert.ok(caps.server.version.length > 0);
    assert.ok(["2025-03-26", "2024-11-05"].includes(caps.protocolVersion));
    assert.equal(caps.statefulSession, false);
    assert.ok(caps.tools["memory_write_page"]?.properties.includes("expected_page_id"));
    assert.equal(caps.meetsMinimumVersion, true);
    assert.equal(caps.identity?.level, "root");
    assert.equal(caps.apiMounted, true);
  });

  it("identity reflects root and user keys; a wrong token is AuthError", async () => {
    assert.equal((await memory.identity()).level, "root");
    const key = await server.addUser("reader");
    const user = new AiMemoryClient({ serverUrl: server.url, token: key });
    const identity = await user.identity();
    assert.equal(identity.level, "user");
    assert.equal(identity.operator, "user:reader");
    assert.equal(identity.distinguishesOperators, true);
    const wrong = new AiMemoryClient({ serverUrl: server.url, token: "FAKEsdkwrongtoken0000", scope });
    await assert.rejects(wrong.identity(), AuthError);
    await assert.rejects(wrong.status(), AuthError);
  });
});

describe("read tools", () => {
  it("status, recent, briefing and explore", async () => {
    const status = await memory.status();
    assert.ok(status.counts.pagesLatest >= 2);
    assert.deepEqual(
      { workspace: status.scope.workspace, project: status.scope.project, by: status.scope.resolvedBy },
      { ...scope, by: "explicit" },
    );
    const recent = await memory.recent({ limit: 5 });
    assert.ok(recent.hits.some((h) => h.path === "notes/a.md"));
    const briefing = await memory.briefing();
    assert.ok(briefing.rules.some((r) => r.path === "_rules/style.md"));
    assert.equal(briefing.activity7d.days, 7);
    assert.ok(briefing.recentPages.length > 0);
    const explored = await memory.explore({ focus: "rules" });
    assert.equal(explored.prose, null, "the test server has no LLM provider");
    assert.ok(explored.briefing !== undefined);
  });

  it("observations of a seeded session, then feedback", async () => {
    const page = await memory.observations({ sessionId });
    assert.equal(page.session.sessionId, sessionId);
    assert.equal(page.total, 3);
    assert.deepEqual(
      page.observations.map((o) => o.kind),
      ["session-start", "user-prompt", "session-end"],
    );
    const prompts = await memory.observations({ kinds: ["user-prompt"], order: "desc" });
    assert.equal(prompts.observations.length, 1);
    assert.match(prompts.observations[0]?.body ?? "", /Seeded prompt/);

    const feedback = await memory.feedback({ path: "notes/a.md", signal: "helpful" });
    assert.equal(feedback.recorded, true);
    assert.equal(feedback.signal, "helpful");
  });

  it("observations in a project with no session is InvalidParamsError", async () => {
    await memory.pages.write({ path: "notes/x.md", body: "# X", scope: { workspace: "it-reads", project: "empty" } });
    await assert.rejects(
      memory.observations({ scope: { workspace: "it-reads", project: "empty" } }),
      InvalidParamsError,
    );
  });

  it("callTool reaches an unwrapped tool", async () => {
    const result = (await memory.callTool(
      "memory_forget_sweep",
      { ...scope, dry_run: true },
      { readOnly: true },
    )) as Record<string, unknown>;
    assert.equal(typeof result, "object");
  });
});

describe("api namespace", () => {
  it("pages over more pages than one limit with the cursor", async () => {
    const pagingScope = { workspace: "it-reads", project: "paging" };
    for (let i = 0; i < 7; i += 1) {
      await memory.pages.write({ path: `notes/p${i}.md`, body: `# P${i}\nx`, scope: pagingScope });
    }
    const seen: string[] = [];
    for await (const page of memory.api.recent({ scope: pagingScope, limit: 2 })) seen.push(page.path);
    assert.equal(seen.length, 7);
    assert.equal(new Set(seen).size, 7);
  });

  it("page ETag round trip returns notModified", async () => {
    const first = await memory.api.page("notes/a.md");
    assert.equal(first.notModified, false);
    if (first.notModified) return;
    assert.equal(first.page.body, "# Alpha\nfirst page");
    const read = await memory.pages.read({ path: "notes/a.md" });
    assert.equal(first.page.id, read.pageId, "api id is the conditional-write token");
    assert.ok(first.etag !== undefined);
    const second = await memory.api.page("notes/a.md", { ifNoneMatch: first.etag });
    assert.equal(second.notModified, true);
  });

  it("reads a listed page whose path contains literal percent escapes", async () => {
    const path = "notes/%2e/%2E%2E/literal.md";
    const written = await memory.pages.write({ path, body: "# Literal percent path" });
    const listed = (await memory.api.pages()).find((page) => page.path === path);
    assert.ok(listed);
    const result = await memory.api.page(listed.path);
    assert.equal(result.notModified, false);
    assert.equal(result.page.id, written.pageId);
    assert.equal(result.page.path, path);
  });

  it("listings, search, handoffs and sessions", async () => {
    assert.ok((await memory.api.workspaces()).some((w) => w.name === "it-reads"));
    assert.ok((await memory.api.projects({ workspace: "it-reads" })).some((p) => p.project === "app"));
    assert.ok((await memory.api.pages()).some((p) => p.path === "notes/a.md"));
    const hits = await memory.api.search({ query: "alpha" });
    assert.equal(hits[0]?.path, "notes/a.md");
    const global = await memory.api.search({ query: "alpha", global: true });
    assert.ok(global.some((h) => h.project === "app"));
    const multi = await memory.api.search({ query: "alpha", scopes: [scope] });
    assert.ok(multi.length > 0);
    assert.ok((await memory.api.briefing()).counts.pagesLatest >= 2);
    await memory.handoffs.begin({ summary: "for the api listing" });
    assert.ok((await memory.api.handoffs({ state: "open" })).length > 0);
    const sessions = await memory.api.sessions();
    assert.ok(sessions.some((s) => s.sessionId === sessionId));
    const observed = await memory.api.observations(sessionId, { kinds: ["user-prompt"] });
    assert.equal(observed.total, 1);
    await assert.rejects(memory.api.page("notes/missing.md"), NotFoundError);
  });
});

describe("servers in other modes", () => {
  it("callTool works as the first MCP call in stateless and stateful modes", async () => {
    for (const stateful of [true, false]) {
      const fresh = await startServer({ stateful, basePath: "/wiki" });
      try {
        const methods: string[] = [];
        const client = new AiMemoryClient({
          serverUrl: fresh.url,
          token: fresh.rootToken,
          scope,
          fetch: async (url, init) => {
            const body = JSON.parse(init.body as string) as { method: string };
            methods.push(body.method);
            return fetch(url, init);
          },
        });
        await client.callTool("memory_write_page", {
          ...scope,
          path: "notes/raw-first.md",
          body: "# Raw first\ncreated through callTool",
        });
        assert.deepEqual(
          methods,
          stateful
            ? ["initialize", "notifications/initialized", "tools/call"]
            : ["initialize", "tools/call"],
        );
        assert.equal((await client.pages.read({ path: "notes/raw-first.md" })).title, "Raw first");
        assert.equal(methods.filter((method) => method === "initialize").length, 1);
        assert.equal(methods.filter((method) => method === "tools/list").length, 1);
      } finally {
        await fresh.stop();
      }
    }
  });

  it("every /api/v1 route is NotFoundError without --enable-api", async () => {
    const bare = await startServer({ api: false });
    try {
      const client = new AiMemoryClient({ serverUrl: bare.url, token: bare.rootToken, scope });
      assert.equal((await client.capabilities()).apiMounted, false);
      await assert.rejects(client.api.workspaces(), NotFoundError);
      // MCP still works.
      await client.pages.write({ path: "notes/a.md", body: "# A" });
    } finally {
      await bare.stop();
    }
  });

  it("a stateful server behind a base path", async () => {
    const prefixed = await startServer({ basePath: "/wiki", stateful: true });
    try {
      const client = new AiMemoryClient({ serverUrl: prefixed.url, token: prefixed.rootToken, scope });
      const caps = await client.capabilities();
      assert.equal(caps.statefulSession, true);
      assert.equal(caps.apiMounted, true);
      await client.pages.write({ path: "notes/a.md", body: "# Prefixed\nunder a base path" });
      assert.equal((await client.pages.read({ path: "notes/a.md" })).title, "Prefixed");
      assert.equal((await client.api.pages())[0]?.path, "notes/a.md");
      // Control: the same server without the prefix is not found.
      const unprefixed = new AiMemoryClient({
        serverUrl: prefixed.url.replace(/\/wiki$/, ""),
        token: prefixed.rootToken,
        scope,
      });
      await assert.rejects(unprefixed.identity(), NotFoundError);
    } finally {
      await prefixed.stop();
    }
  });
});
