import assert from "node:assert/strict";
import { describe, it } from "node:test";

import { AiMemoryClient } from "../../src/client.js";
import {
  ArgumentError,
  NotFoundError,
  ProtocolError,
  UnsupportedServerError,
} from "../../src/errors.js";
import { FakeServer, json, text } from "../support/fake.js";
import * as payloads from "../support/payloads.js";

const BASE = "/api/v1/workspaces/demo/projects/app";

function setup(serverUrl = "http://127.0.0.1:49374") {
  const server = new FakeServer();
  const memory = new AiMemoryClient({
    serverUrl,
    scope: { workspace: "demo", project: "app" },
    fetch: server.fetch,
  });
  return { server, memory };
}

async function collect<T>(iterator: AsyncIterable<T>): Promise<T[]> {
  const out: T[] = [];
  for await (const item of iterator) out.push(item);
  return out;
}

describe("api.recent pagination", () => {
  it("follows next_cursor until null, sending the cursor unchanged", async () => {
    const { server, memory } = setup();
    server.routes.set(`GET ${BASE}/recent`, (request) => {
      const cursor = request.query.get("cursor");
      if (cursor === null) {
        assert.equal(request.query.get("updated_since"), "2026-01-01T00:00:00Z");
        return json({ pages: [payloads.pageSummary("a.md")], next_cursor: "c1" });
      }
      if (cursor === "c1") return json({ pages: [payloads.pageSummary("b.md")], next_cursor: "c2" });
      return json({ pages: [payloads.pageSummary("c.md")], next_cursor: null });
    });
    const pages = await collect(memory.api.recent({ updatedSince: "2026-01-01T00:00:00Z", limit: 1 }));
    assert.deepEqual(
      pages.map((p) => p.path),
      ["a.md", "b.md", "c.md"],
    );
    assert.equal(server.requests.length, 3);
    assert.ok(server.requests.every((r) => r.query.get("limit") === "1"));
  });

  it("stops on an immediate null cursor and defaults to the epoch", async () => {
    const { server, memory } = setup();
    server.routes.set(`GET ${BASE}/recent`, (request) => {
      assert.equal(request.query.get("updated_since"), "1970-01-01T00:00:00Z");
      return json({ pages: [], next_cursor: null });
    });
    assert.deepEqual(await collect(memory.api.recent()), []);
    assert.equal(server.requests.length, 1);
  });

  it("bounds the rounds a hostile server can force", async () => {
    const { server, memory } = setup();
    server.routes.set(`GET ${BASE}/recent`, () => json({ pages: [], next_cursor: "again" }));
    await assert.rejects(collect(memory.api.recent({ maxRounds: 5 })), ProtocolError);
    assert.equal(server.requests.length, 5);
  });

  it("refuses a malformed cursor", async () => {
    const { server, memory } = setup();
    server.routes.set(`GET ${BASE}/recent`, () => json({ pages: [], next_cursor: "bad\ncursor" }));
    await assert.rejects(collect(memory.api.recent()), ProtocolError);
  });

  it("treats a legacy array answer as an unsupported server", async () => {
    const { server, memory } = setup();
    server.routes.set(`GET ${BASE}/recent`, () => json([payloads.pageSummary("a.md")]));
    await assert.rejects(collect(memory.api.recent()), UnsupportedServerError);
  });
});

describe("api.page", () => {
  it("returns the page and its ETag, then notModified on a matching If-None-Match", async () => {
    const { server, memory } = setup();
    server.routes.set(`GET ${BASE}/pages/notes/retries.md`, (request) =>
      request.headers["if-none-match"] === '"e1"'
        ? new Response(null, { status: 304, headers: { etag: '"e1"' } })
        : json(payloads.apiPage, 200, { etag: '"e1"' }),
    );
    const first = await memory.api.page("notes/retries.md");
    assert.equal(first.notModified, false);
    if (!first.notModified) {
      assert.equal(first.page.id, payloads.PAGE_ID);
      assert.equal(first.page.body, "# Retry policy\nv2");
      assert.equal(first.etag, '"e1"');
    }
    const second = await memory.api.page("notes/retries.md", { ifNoneMatch: '"e1"' });
    assert.deepEqual(second, { notModified: true, etag: '"e1"' });
  });

  it("encodes each path segment and refuses dot segments before fetch", async () => {
    const { server, memory } = setup();
    server.routes.set(`GET ${BASE}/pages/notes/a%20b.md`, () => json(payloads.apiPage));
    server.routes.set(`GET ${BASE}/pages/notes/%252e/%252E%252E/a.md`, () => json(payloads.apiPage));
    await memory.api.page("notes/a b.md");
    await memory.api.page("notes/%2e/%2E%2E/a.md");
    const before = server.requests.length;
    await assert.rejects(memory.api.page("../../admin/checkpoints"), ArgumentError);
    await assert.rejects(memory.api.page("notes/./a.md"), ArgumentError);
    await assert.rejects(
      memory.api.page("a.md", { scope: { workspace: "..", project: "x" } }),
      ArgumentError,
    );
    assert.equal(server.requests.length, before);
  });

  it("raises NotFoundError with the --enable-api hint", async () => {
    const { memory } = setup();
    await assert.rejects(memory.api.page("missing.md"), (error: unknown) => {
      assert.ok(error instanceof NotFoundError);
      assert.match(error.hint ?? "", /--enable-api/);
      return true;
    });
  });
});

describe("api listings and search", () => {
  it("workspaces, projects, pages, briefing, handoffs, sessions, observations", async () => {
    const { server, memory } = setup();
    server.routes.set("GET /api/v1/workspaces", () =>
      json([{ workspace_name: "demo", project_count: 1, page_count: 2, last_updated: null }]),
    );
    server.routes.set("GET /api/v1/projects", (request) => {
      assert.equal(request.query.get("workspace"), "demo");
      return json([
        {
          workspace_id: "w1",
          workspace_name: "demo",
          project_id: "p1",
          project_name: "app",
          page_count: 2,
          last_updated: null,
        },
      ]);
    });
    server.routes.set(`GET ${BASE}/pages`, () => json([payloads.pageSummary("a.md")]));
    server.routes.set(`GET ${BASE}/briefing`, () => json(payloads.briefing));
    server.routes.set(`GET ${BASE}/handoffs`, (request) => {
      assert.equal(request.query.get("state"), "accepted");
      assert.equal(request.query.get("all_owners"), "true");
      return json({
        handoffs: [{ id: "h1", agent: "other", at: "t", state: "accepted", redacted: true }],
      });
    });
    server.routes.set(`GET ${BASE}/sessions`, (request) => {
      assert.equal(request.query.get("include_open"), "true");
      return json({ sessions: [payloads.sessionSummary] });
    });
    server.routes.set(`GET ${BASE}/sessions/${payloads.SESSION_ID}/observations`, (request) => {
      assert.equal(request.query.get("kinds"), "user-prompt,stop");
      assert.equal(request.query.get("q"), "parser");
      return json(payloads.observations);
    });

    assert.equal((await memory.api.workspaces())[0]?.name, "demo");
    assert.equal((await memory.api.projects({ workspace: "demo" }))[0]?.projectId, "p1");
    assert.equal((await memory.api.pages())[0]?.path, "a.md");
    assert.equal((await memory.api.briefing()).counts.pagesAll, 2);
    const handoffs = await memory.api.handoffs({ state: "accepted", allOwners: true });
    assert.equal(handoffs[0]?.redacted, true);
    assert.equal(handoffs[0]?.summary, undefined);
    assert.equal((await memory.api.sessions({ includeOpen: true }))[0]?.sessionId, payloads.SESSION_ID);
    const observed = await memory.api.observations(payloads.SESSION_ID, {
      kinds: ["user-prompt", "stop"],
      query: "parser",
    });
    assert.equal(observed.total, 1);
  });

  it("search uses GET for one project or global and POST for scopes", async () => {
    const { server, memory } = setup();
    const hit = {
      workspace: "demo",
      project: "app",
      path: "a.md",
      title: "A",
      kind: "note",
      snippet: "s",
      rank: -1,
    };
    server.routes.set("GET /api/v1/search", () => json([hit]));
    server.routes.set("POST /api/v1/search", () => json([hit]));
    await memory.api.search({ query: "retry", limit: 3 });
    let last = server.requests.at(-1);
    assert.equal(last?.query.get("workspace"), "demo");
    assert.equal(last?.query.get("project"), "app");
    assert.equal(last?.query.get("limit"), "3");
    await memory.api.search({ query: "retry", global: true });
    last = server.requests.at(-1);
    assert.equal(last?.query.get("workspace"), null);
    const scopes = [
      { workspace: "demo", project: "app" },
      { workspace: "demo", project: "other" },
    ];
    const hits = await memory.api.search({ query: "retry", scopes });
    last = server.requests.at(-1);
    assert.equal(last?.method, "POST");
    assert.deepEqual(last?.body, { q: "retry", scopes });
    assert.equal(hits[0]?.workspace, "demo");
  });

  it("raises ProtocolError on invalid JSON", async () => {
    const { server, memory } = setup();
    server.routes.set("GET /api/v1/workspaces", () => text("<html>"));
    await assert.rejects(memory.api.workspaces(), ProtocolError);
  });
});

describe("base path", () => {
  it("prefixes every route with the server URL's path", async () => {
    const { server, memory } = setup("https://memory.example.com/wiki/");
    server.routes.set("GET /wiki/api/v1/workspaces", () => json([]));
    server.tools.set("memory_status", () => payloads.status);
    await memory.api.workspaces();
    await memory.status();
    assert.ok(server.requests.every((r) => r.path.startsWith("/wiki/")));
    assert.equal(memory.serverUrl, "https://memory.example.com/wiki");
    assert.ok(server.requests.some((r) => r.url === "https://memory.example.com/wiki/mcp"));
  });
});
