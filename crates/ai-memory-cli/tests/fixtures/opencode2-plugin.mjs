// Execute the generated adapter, rather than only checking template substrings.
// Run by `cargo test` (opencode2_plugin_passes_the_node_host_fixture), or by hand:
// AI_MEMORY_DATA_DIR=<empty dir> node --experimental-strip-types opencode2-plugin.mjs /absolute/plugin.ts
import assert from "node:assert/strict";
import { pathToFileURL } from "node:url";
import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";

const requests = [];
globalThis.fetch = async (input, options = {}) => {
  // Like a real fetch, a request cancelled before it is sent never arrives.
  options.signal?.throwIfAborted();
  const url = new URL(input);
  requests.push({ url, payload: options.body ? JSON.parse(options.body) : undefined });
  // A server slower than the plugin's timeout: the fetch rejects like an abort.
  if (url.pathname === "/handoff" && url.searchParams.get("session_id") === "root-slow") {
    throw new DOMException("The operation timed out.", "TimeoutError");
  }
  if (url.pathname === "/recap" && url.searchParams.get("mode") === "anchor") {
    // `root-slow` stands for a server that predates `mode=anchor`: its
    // answer is the full recap, which must never be injected automatically.
    const id = url.searchParams.get("session_id");
    return new Response(id === "root-slow" ? "> 🧭 ai-memory: compaction recap for this session"
      : `> 🧭 ai-memory: post-compaction anchor for this session (${id})`);
  }
  return new Response(url.pathname === "/handoff" ? "Remember the verified handoff." : "{}");
};
const { default: plugin } = await import(pathToFileURL(process.argv[2]).href);

function host(directory, records) {
  const hooks = new Map();
  const events = [];
  let wake;
  return {
    hooks,
    emit(event) {
      events.push(event);
      wake?.();
    },
    ctx: {
      location: { directory },
      session: {
        async get({ sessionID }) {
          const record = records.get(sessionID);
          assert.ok(record, `unknown session ${sessionID}`);
          return record;
        },
        async hook(name, callback) {
          hooks.set(`session.${name}`, callback);
          return { async dispose() {} };
        },
      },
      tool: {
        async hook(name, callback) {
          hooks.set(`tool.${name}`, callback);
          return { async dispose() {} };
        },
      },
      event: {
        async *subscribe({ signal }) {
          signal.addEventListener("abort", () => wake?.(), { once: true });
          while (!signal.aborted) {
            if (!events.length) await new Promise((resolve) => { wake = resolve; });
            while (events.length) yield events.shift();
          }
        },
      },
    },
  };
}

async function until(predicate, message) {
  const deadline = performance.now() + 5000;
  while (!predicate()) {
    assert.ok(performance.now() < deadline, message);
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
}

const root = process.cwd();
const info = (id, parentID) => ({ id, title: id, location: { directory: root }, parentID });
const records = new Map([["root-a", info("root-a")], ["root-b", { ...info("root-b"), location: { directory: root + "/beta" } }],
  ["child", info("child", "root-a")], ["root-slow", info("root-slow")], ["root-c", info("root-c")]]);
const a = host(root, records);
const b = host(root + "/beta", records);
const disposeA = await plugin.setup(a.ctx);
const disposeB = await plugin.setup(b.ctx);
try {
  // Existing/resumed sessions may never emit session.created after plugin load.
  await a.hooks.get("session.prompt")({ sessionID: "root-a", messageID: "m1", prompt: { text: "remember alpha" } });
  const first = { sessionID: "root-a", system: [] };
  const next = { sessionID: "root-a", system: [] };
  await a.hooks.get("session.context")(first);
  await a.hooks.get("session.context")(next);
  assert.deepEqual(first.system, next.system);
  assert.equal(first.system.length, 1);
  assert.equal(requests.filter((r) => r.url.pathname === "/handoff").length, 1, "claim once, inject repeatedly");

  await a.hooks.get("session.prompt")({ sessionID: "child", messageID: "c1", prompt: { text: "child work" } });
  const child = { sessionID: "child", system: [] };
  await a.hooks.get("session.context")(child);
  assert.equal(child.system.length, 0);
  assert.equal(requests.filter((r) => r.url.pathname === "/handoff").length, 1, "child must not claim");

  // A failed startup-context fetch is retried on a later model request, but a
  // bounded number of times: the context hook fires on every model call.
  const slowHandoffs = () => requests.filter((r) => r.url.pathname === "/handoff" && r.url.searchParams.get("session_id") === "root-slow").length;
  for (let i = 0; i < 6; i++) {
    const slow = { sessionID: "root-slow", system: [] };
    await a.hooks.get("session.context")(slow);
    assert.equal(slow.system.length, 0);
  }
  assert.equal(slowHandoffs(), 3, "a failing handoff fetch must stop retrying after its attempt budget");

  await a.hooks.get("tool.execute.after")({ sessionID: "root-a", tool: "example", id: "t1", input: {},
    status: "completed", result: { content: "content-only tool evidence" } });
  for (const type of ["session.execution.succeeded", "session.execution.failed", "session.execution.interrupted"]) {
    a.emit({ type, data: { sessionID: "root-a" } });
    b.emit({ type, data: { sessionID: "root-a" } });
  }
  a.emit({ type: "session.execution.succeeded", data: { sessionID: "child" } });
  await until(() => requests.filter((r) => r.url.searchParams.get("event") === "stop").length === 4, "completion events delivered");
  const stops = requests.filter((r) => r.url.searchParams.get("event") === "stop");
  assert.equal(stops.filter((r) => r.payload.turn_checkpoint).length, 3);
  assert.equal(stops.find((r) => r.payload.sessionID === "child").payload.agent_id, "root-a");
  assert.ok(requests.some((r) => r.payload?.output === "content-only tool evidence"));
  assert.ok(!requests.some((r) => r.url.searchParams.get("event") === "session-end"));

  // #1983: the compaction request gets one line naming the session, its end
  // arms the anchor for every later model call, and the next compaction's
  // start forgets it. A subagent's compaction gets neither.
  const anchors = () => requests.filter((r) => r.url.pathname === "/recap" && r.url.searchParams.get("mode") === "anchor");
  const stopCount = () => requests.filter((r) => r.url.searchParams.get("event") === "stop").length;
  a.emit({ type: "session.compaction.started", data: { sessionID: "root-a", reason: "auto" } });
  await until(() => requests.some((r) => r.url.searchParams.get("event") === "pre-compact"), "compaction start captured");
  const summaryRequest = { sessionID: "root-a", system: [] };
  await a.hooks.get("session.compaction")(summaryRequest);
  assert.equal(summaryRequest.system.length, 1);
  assert.equal(summaryRequest.system[0].type, "text");
  assert.ok(summaryRequest.system[0].text.includes("(root-a)"), summaryRequest.system[0].text);
  const childSummary = { sessionID: "child", system: [] };
  await a.hooks.get("session.compaction")(childSummary);
  assert.equal(childSummary.system.length, 0, "a subagent compaction gets no note");
  a.emit({ type: "session.compaction.ended", data: { sessionID: "root-a", reason: "auto", text: "summary" } });
  a.emit({ type: "session.compaction.ended", data: { sessionID: "child", reason: "auto", text: "summary" } });
  await until(() => anchors().length === 1, "compaction end arms the anchor");
  assert.equal(anchors()[0].url.searchParams.get("session_id"), "root-a");
  assert.equal(anchors()[0].url.searchParams.get("budget"), "2000");
  for (let i = 0; i < 2; i++) {
    const call = { sessionID: "root-a", system: [] };
    await a.hooks.get("session.context")(call);
    assert.ok(call.system.some((part) => part.text === "> 🧭 ai-memory: post-compaction anchor for this session (root-a)"), "anchor rides every model call until the next compaction");
  }
  assert.equal(anchors().length, 1, "the anchor is fetched once per compaction");
  const stopsBefore = stopCount();
  a.emit({ type: "session.compaction.started", data: { sessionID: "root-a", reason: "auto" } });
  a.emit({ type: "session.execution.succeeded", data: { sessionID: "root-a" } });
  await until(() => stopCount() > stopsBefore, "events after the next compaction start processed");
  const afterNext = { sessionID: "root-a", system: [] };
  await a.hooks.get("session.context")(afterNext);
  assert.ok(!afterNext.system.some((part) => part.text === "> 🧭 ai-memory: post-compaction anchor for this session (root-a)"), "the next compaction forgets the anchor");
  assert.equal(anchors().length, 1, "a subagent compaction never fetches an anchor");
  a.emit({ type: "session.compaction.ended", data: { sessionID: "root-slow", reason: "auto", text: "summary" } });
  await until(() => anchors().length === 2, "an older server is still asked");
  const oldServer = { sessionID: "root-slow", system: [] };
  await a.hooks.get("session.context")(oldServer);
  assert.ok(!oldServer.system.some((part) => part.text.includes("compaction recap")), "an older server's recap is never injected");
  // Deleting a session forgets its armed anchor along with the rest of its state.
  const anchorC = "> 🧭 ai-memory: post-compaction anchor for this session (root-c)";
  a.emit({ type: "session.compaction.ended", data: { sessionID: "root-c", reason: "auto", text: "summary" } });
  await until(() => anchors().length === 3, "compaction end arms root-c's anchor");
  const armed = { sessionID: "root-c", system: [] };
  await a.hooks.get("session.context")(armed);
  assert.ok(armed.system.some((part) => part.text === anchorC), "root-c's anchor is armed");
  a.emit({ type: "session.deleted", data: { sessionID: "root-c" } });
  await until(() => requests.some((r) => r.url.searchParams.get("event") === "session-end" && r.payload.sessionID === "root-c"), "deleted root-c must end");
  const afterDelete = { sessionID: "root-c", system: [] };
  await a.hooks.get("session.context")(afterDelete);
  assert.ok(!afterDelete.system.some((part) => part.text === anchorC), "a deleted session's anchor is forgotten");
  assert.equal(anchors().length, 3, "a deleted session never refetches its anchor");

  // #2486: OpenCode 2 token usage reporting via _ai_memory_usage_calls.
  // 1. Streaming updates to the same message ID + step-finish part for that
  //    message ID must deduplicate by call_id using MAX (no double-counting).
  const stopsBeforeUsage = stopCount();
  a.emit({
    type: "message.updated",
    data: {
      info: {
        id: "msg_1",
        sessionID: "root-a",
        role: "assistant",
        modelID: "claude-sonnet-4",
        tokens: { input: 100, output: 10, cache: { read: 40, write: 5 } },
      },
    },
  });
  a.emit({
    type: "message.updated",
    data: {
      info: {
        id: "msg_1",
        sessionID: "root-a",
        role: "assistant",
        modelID: "claude-sonnet-4",
        tokens: { input: 100, output: 35, cache: { read: 40, write: 5 } },
      },
    },
  });
  a.emit({
    type: "message.part.updated",
    data: {
      part: {
        id: "prt_1",
        messageID: "msg_1",
        sessionID: "root-a",
        type: "step-finish",
        modelID: "claude-sonnet-4",
        tokens: { input: 100, output: 35, cache: { read: 40, write: 5 } },
      },
    },
  });
  // Invalid / zero / negative / float counters must be ignored (omitted).
  a.emit({
    type: "message.updated",
    data: {
      info: {
        id: "msg_zero",
        sessionID: "root-a",
        role: "assistant",
        tokens: { input: 0, output: 0, cache: { read: 0, write: 0 } },
      },
    },
  });
  a.emit({
    type: "message.updated",
    data: {
      info: {
        id: "msg_neg",
        sessionID: "root-a",
        role: "assistant",
        tokens: { input: -5, output: 10 },
      },
    },
  });
  a.emit({
    type: "message.updated",
    data: {
      info: {
        id: "msg_float",
        sessionID: "root-a",
        role: "assistant",
        tokens: { input: 12.5, output: 10 },
      },
    },
  });
  a.emit({ type: "session.execution.succeeded", data: { sessionID: "root-a" } });
  await until(() => stopCount() > stopsBeforeUsage, "usage stop delivered");
  const usageStop = requests.filter((r) => r.url.searchParams.get("event") === "stop").at(-1);
  assert.deepEqual(usageStop.payload._ai_memory_usage_calls, [
    {
      call_id: "msg_1",
      input_tokens: 100,
      output_tokens: 35,
      cache_write_tokens: 5,
      cache_read_tokens: 40,
      model: "claude-sonnet-4",
    },
  ], "repeated updates and step-finish for the same message must merge into one call_id");

  // 2. A stop with no new measured usage (or only zero/invalid counters) must
  //    omit _ai_memory_usage_calls completely.
  const stopsBeforeEmptyUsage = stopCount();
  a.emit({
    type: "message.updated",
    data: { info: { id: "msg_absent", sessionID: "root-a", role: "assistant" } },
  });
  a.emit({ type: "session.execution.succeeded", data: { sessionID: "root-a" } });
  await until(() => stopCount() > stopsBeforeEmptyUsage, "empty usage stop delivered");
  const emptyUsageStop = requests.filter((r) => r.url.searchParams.get("event") === "stop").at(-1);
  assert.equal(emptyUsageStop.payload._ai_memory_usage_calls, undefined, "absent/zero usage must omit _ai_memory_usage_calls");

  // 3. Trailing usage before session.deleted is flushed on session-end.
  a.emit({
    type: "message.updated",
    data: {
      info: {
        id: "msg_child_final",
        sessionID: "child",
        role: "assistant",
        modelID: "claude-haiku-4",
        tokens: { input: 50, output: 20, cache: { read: 0, write: 0 } },
      },
    },
  });

  await b.hooks.get("session.prompt")({ sessionID: "root-b", messageID: "b1", prompt: { text: "beta" } });
  await b.hooks.get("session.context")({ sessionID: "root-b", system: [] });
  // Deletion, not unload, is the end signal: a deleted child keeps ancestry
  // so its close does not hand the next session a sub-task baton.
  a.emit({ type: "session.deleted", data: { sessionID: "child" } });
  await until(() => requests.some((r) => r.url.searchParams.get("event") === "session-end" && r.payload.sessionID === "child"), "deleted session must end");
  const childEnd = requests.find((r) => r.url.searchParams.get("event") === "session-end" && r.payload.sessionID === "child");
  assert.equal(childEnd.payload.agent_id, "root-a", "child close must retain ancestry to suppress automatic handoffs");
  assert.deepEqual(childEnd.payload._ai_memory_usage_calls, [
    {
      call_id: "msg_child_final",
      input_tokens: 50,
      output_tokens: 20,
      cache_write_tokens: 0,
      cache_read_tokens: 0,
      model: "claude-haiku-4",
    },
  ], "session-end must flush trailing measured usage");
  // #1074: OpenCode unloads the plugin when it evicts idle location services;
  // the host and its sessions stay alive, so unload must not end them.
  await disposeA();
  assert.ok(!requests.some((r) => r.url.searchParams.get("event") === "session-end" && r.payload.sessionID === "root-a"), "eviction unload must not end a live session");
  assert.ok(!requests.some((r) => r.url.searchParams.get("event") === "session-end" && r.payload.sessionID === "root-b"), "location cleanup must not close another instance");

  // 4. Plugin reload: replaying the same message ID after reload preserves the
  //    same native call_id so server deduplication never double-counts.
  const reloadedA = host(root, records);
  const disposeReloadedA = await plugin.setup(reloadedA.ctx);
  try {
    const stopsBeforeReload = stopCount();
    reloadedA.emit({
      type: "message.updated",
      data: {
        info: {
          id: "msg_1",
          sessionID: "root-a",
          role: "assistant",
          modelID: "claude-sonnet-4",
          tokens: { input: 100, output: 35, cache: { read: 40, write: 5 } },
        },
      },
    });
    reloadedA.emit({ type: "session.execution.succeeded", data: { sessionID: "root-a" } });
    await until(() => stopCount() > stopsBeforeReload, "reloaded stop delivered");
    const reloadStop = requests.filter((r) => r.url.searchParams.get("event") === "stop").at(-1);
    assert.deepEqual(reloadStop.payload._ai_memory_usage_calls, [
      {
        call_id: "msg_1",
        input_tokens: 100,
        output_tokens: 35,
        cache_write_tokens: 5,
        cache_read_tokens: 40,
        model: "claude-sonnet-4",
      },
    ], "reload must preserve native call_id for idempotent server deduplication");
  } finally {
    await disposeReloadedA();
  }
  console.log("PASS: resumed sessions, retained handoff, child isolation, bounded handoff retries, terminal events, post-compaction anchor, deletion ends, unload does not");
} finally {
  await disposeB();
}

// #2485: deleting a session after plugin reload still sends session-end once,
// preserves subagent ancestry, ignores never-captured IDs, and tracks moves.
{
  const sessionEnds = (id) =>
    requests.filter((r) => r.url.searchParams.get("event") === "session-end" && r.payload.sessionID === id);
  records.set("reload-root", info("reload-root"));
  records.set("reload-child", info("reload-child", "reload-root"));
  records.set("move-root", info("move-root"));

  const instA = host(root, records);
  const disposeInstA = await plugin.setup(instA.ctx);
  await instA.hooks.get("session.prompt")({ sessionID: "reload-root", messageID: "rr1", prompt: { text: "reload root" } });
  await instA.hooks.get("session.prompt")({ sessionID: "reload-child", messageID: "rc1", prompt: { text: "reload child" } });
  await disposeInstA();
  assert.equal(sessionEnds("reload-root").length, 0, "dispose must not end reload-root");
  assert.equal(sessionEnds("reload-child").length, 0, "dispose must not end reload-child");

  // Simulate deleted sessions returning 404 from ctx.session.get.
  records.delete("reload-root");
  records.delete("reload-child");

  const instB = host(root, records);
  const instC = host(root + "/beta", records);
  const disposeInstB = await plugin.setup(instB.ctx);
  const disposeInstC = await plugin.setup(instC.ctx);
  try {
    for (const id of ["reload-root", "reload-child"]) {
      instB.emit({ type: "session.deleted", data: { sessionID: id } });
      instC.emit({ type: "session.deleted", data: { sessionID: id } });
    }
    await until(() => sessionEnds("reload-root").length === 1 && sessionEnds("reload-child").length === 1,
      "reloaded location instance must end persisted sessions");

    // Re-delivering session.deleted and deleting an unknown/never-captured session
    // must not emit duplicates or fabricate rows.
    for (const id of ["reload-root", "reload-child", "never-captured"]) {
      instB.emit({ type: "session.deleted", data: { sessionID: id } });
      instC.emit({ type: "session.deleted", data: { sessionID: id } });
    }
  } finally {
    await Promise.all([disposeInstB(), disposeInstC()]);
  }
  assert.equal(sessionEnds("reload-root").length, 1, "reload-root must end exactly once");
  assert.equal(sessionEnds("reload-root")[0].payload.cwd, root);
  assert.equal(sessionEnds("reload-root")[0].payload.agent_id, undefined);
  assert.equal(sessionEnds("reload-child").length, 1, "reload-child must end exactly once");
  assert.equal(sessionEnds("reload-child")[0].payload.cwd, root);
  assert.equal(sessionEnds("reload-child")[0].payload.agent_id, "reload-root", "subagent must retain agent_id after reload");
  assert.equal(sessionEnds("never-captured").length, 0, "unknown session must not emit session-end");

  // Moving a session from dir A to dir B, reloading, then deleting: only B ends it with B's cwd.
  const moveA = host(root, records);
  const moveB = host(root + "/beta", records);
  const disposeMoveA = await plugin.setup(moveA.ctx);
  const disposeMoveB = await plugin.setup(moveB.ctx);
  try {
    await moveA.hooks.get("session.prompt")({ sessionID: "move-root", messageID: "mr1", prompt: { text: "before move" } });
    records.set("move-root", { ...info("move-root"), location: { directory: root + "/beta" } });
    const moveEvent = { type: "session.moved", data: { sessionID: "move-root", location: { directory: root + "/beta" } } };
    moveA.emit(moveEvent);
    moveB.emit(moveEvent);
    await until(() => requests.some((r) => r.url.searchParams.get("event") === "session-start" && r.payload.sessionID === "move-root" && r.payload.session_moved === true),
      "session move transition must be emitted");
  } finally {
    await Promise.all([disposeMoveA(), disposeMoveB()]);
  }
  records.delete("move-root");
  const moveA2 = host(root, records);
  const moveB2 = host(root + "/beta", records);
  const disposeMoveA2 = await plugin.setup(moveA2.ctx);
  const disposeMoveB2 = await plugin.setup(moveB2.ctx);
  try {
    moveA2.emit({ type: "session.deleted", data: { sessionID: "move-root" } });
    moveB2.emit({ type: "session.deleted", data: { sessionID: "move-root" } });
    await until(() => sessionEnds("move-root").length === 1, "moved session must end after reload");
  } finally {
    await Promise.all([disposeMoveA2(), disposeMoveB2()]);
  }
  assert.equal(sessionEnds("move-root").length, 1, "moved session must end exactly once");
  assert.equal(sessionEnds("move-root")[0].payload.cwd, root + "/beta", "moved session must end with destination cwd");
  console.log("PASS: reloaded plugin ends deleted sessions once, preserves subagent ancestry, ignores unknown IDs, and tracks moves");
}

const spool = join(process.env.AI_MEMORY_DATA_DIR, "hook-spool");
const realNow = Date.now;
globalThis.fetch = async () => { throw new Error("offline fixture"); };
Date.now = () => 1800000000000;
try {
  const offlineA = host(root, records);
  const offlineB = host(root + "/beta", records);
  const closeA = await plugin.setup(offlineA.ctx);
  const closeB = await plugin.setup(offlineB.ctx);
  await Promise.all([
    offlineA.hooks.get("session.prompt")({ sessionID: "root-a", messageID: "offline-a", prompt: { text: "offline alpha" } }),
    offlineB.hooks.get("session.prompt")({ sessionID: "root-b", messageID: "offline-b", prompt: { text: "offline beta" } }),
  ]);
  await Promise.all([closeA(), closeB()]);
  const entries = readdirSync(spool).filter((name) => name.endsWith(".json")).map((name) => JSON.parse(readFileSync(join(spool, name), "utf8")));
  for (const id of ["root-a", "root-b"]) {
    assert.ok(entries.some((entry) => new URL(entry.url).searchParams.get("event") === "session-start" && JSON.parse(entry.body).sessionID === id), `same-millisecond spool must retain ${id}`);
  }
  assert.ok(entries.every((entry) => new URL(entry.url).searchParams.has("ingest_key")), "stable delivery keys survive spooling");
  console.log("PASS: concurrent location spools do not overwrite same-millisecond events");
} finally {
  Date.now = realNow;
}

// A server which accepts requests but never answers must not hold plugin unload
// behind every queued request's individual timeout.
globalThis.fetch = (_url, { signal } = {}) => new Promise((_resolve, reject) => {
  if (signal.aborted) reject(signal.reason);
  else signal.addEventListener("abort", () => reject(signal.reason), { once: true });
});
const stalled = host(root, records);
const closeStalled = await plugin.setup(stalled.ctx);
await stalled.hooks.get("session.prompt")({ sessionID: "root-a", messageID: "stalled", prompt: { text: "bounded shutdown" } });
for (let n = 0; n < 60; n++) {
  await stalled.hooks.get("tool.execute.after")({ sessionID: "root-a", tool: "example", id: `pending-${n}`, input: {}, status: "completed", result: { content: "queued evidence" } });
}
stalled.emit({ type: "session.text.ended", data: { sessionID: "root-a", text: "last response" } });
stalled.emit({ type: "session.execution.succeeded", data: { sessionID: "root-a" } });
await new Promise((resolve) => setImmediate(resolve));
const shutdown = performance.now();
// The plugin's drain timers are unref'd; a real host process stays alive.
const host_alive = setInterval(() => {}, 1000);
await closeStalled();
clearInterval(host_alive);
assert.ok(performance.now() - shutdown < 5000, "unload must not wait for the entire network timeout backlog");
console.log("PASS: unload cancels stalled capture and remains bounded");
