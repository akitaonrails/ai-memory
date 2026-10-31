/**
 * Tool payloads shaped like the server's real answers (captured from
 * ai-memory 3.0), for the unit suite. The integration suite pins the same
 * fields against a live server.
 */

export const PAGE_ID = "01a135d0-ede3-7073-b1c8-fc63f7a9614a";
export const NEXT_PAGE_ID = "01a135d1-07a9-7d70-8280-a70d32377145";
export const HANDOFF_ID = "01a135d1-22c2-72a0-a399-ff81f540d69f";
export const MESSAGE_ID = "01a135d1-3ecf-79d3-a65b-c0094a535f63";
export const SESSION_ID = "b2f3725b-265c-5da4-9866-35db4765d1b9";

export const write = { page_id: NEXT_PAGE_ID, path: "notes/retries.md", checkpoint: "b04977fb" };
export const patch = write;
export const resolvedWriteTarget = {
  workspace: "demo",
  project: "app",
  project_id: "01a135ce-c831-75e5-937e-1df03537fc28",
};
export const batchWrite = {
  pages: [
    { index: 0, path: "notes/a.md", page_id: PAGE_ID },
    { index: 1, path: "notes/b.md", page_id: NEXT_PAGE_ID },
  ],
  checkpoint: "c04d2f91",
};

export const read = {
  path: "notes/retries.md",
  page_id: PAGE_ID,
  title: "Retry policy",
  body: "# Retry policy\nKeep the same event ID.",
  frontmatter: {
    title: "Retry policy (explicit)",
    tags: ["a"],
    tier: "procedural",
    pinned: true,
    kind: "howto",
    entities: ["foo bar"],
    abstract: "short",
    relations: { fixes: ["notes/a.md"], bogus: ["x"] },
    expires_at: "2099-01-01",
    type: "Note",
    generated: { by: "process:ai-memory/3.0.0", at: "2026-10-13T15:10:47Z" },
  },
  sections: [{ anchor: "Retry policy", level: 1, sha256: "a".repeat(64) }],
  sections_truncated: false,
};

export const del = {
  path: "notes/gone.md",
  deleted: true,
  pre_checkpoint: null,
  checkpoint: "fa0a91e0",
};

export const hit = {
  id: PAGE_ID,
  path: "notes/retries.md",
  title: "Retry policy",
  snippet: "# <mark>Retry</mark> policy",
  rank: -0.018,
};

export const query = { hits: [hit] };

export const globalQuery = {
  hits: [],
  global_hits: [
    {
      workspace_name: "demo",
      project_name: "app",
      path: "notes/retries.md",
      title: "Retry policy",
      snippet: "# <mark>Retry</mark> policy",
      rank: -1.5e-6,
    },
  ],
};

export const counts = {
  pages_latest: 1,
  pages_all: 2,
  sessions: 0,
  observations: 0,
  evidence_rows: 0,
};

export const status = {
  counts,
  scope: { workspace: "demo", project: "app", resolved_by: "explicit" },
};

export const briefing = {
  counts,
  activity_7d: { days: 7, sessions: 0, observations: 0, pages_updated: 1 },
  activity_30d: { days: 30, sessions: 0, observations: 0, pages_updated: 1 },
  last_observation_at: null,
  pending_handoff_count: 0,
  pending_message_count: 0,
  rules: [],
  slots: [],
  recent_pages: [
    { path: "notes/retries.md", title: "Retry policy", kind: "note", updated_at: "2026-10-13T15:10:53Z" },
  ],
  recent_sessions: [
    {
      session_id: "09db5007-256a-5d22-970e-c9deb0081218",
      agent_kind: "open-code",
      cwd: "/work/app",
      last_activity_at: "2026-10-13T15:20:00Z",
      ended: false,
      handed_off: false,
      current: false,
    },
  ],
  scope: { workspace: "demo", project: "app", resolved_by: "explicit" },
};

export const handoffRow = {
  id: HANDOFF_ID,
  workspace_id: "w",
  project_id: "p",
  from_session_id: null,
  from_agent: "other",
  to_agent: null,
  cwd: null,
  owner_user: null,
  summary: "Left off here",
  open_questions: [],
  next_steps: ["a"],
  files_touched: [],
  state: "open",
  created_at: "2026-10-13T15:11:00.802246Z",
  accepted_by: null,
  accepted_at: null,
};

export const messageRow = {
  id: MESSAGE_ID,
  to_workspace_id: "w",
  to_project_id: "p2",
  from_workspace_id: "w",
  from_project_id: "p",
  from_agent: "other",
  from_owner_user: null,
  to_agent: null,
  subject: "hi",
  body: "please do X",
  nature: "reported",
  evidence: "source message 42",
  state: "pending",
  created_at: "2026-10-13T15:11:07.983842Z",
  claimed_at: null,
};

export const sessionSummary = {
  session_id: SESSION_ID,
  cwd: "/work/app",
  agent_kind: "claude-code",
  started_at: "2026-10-13T15:14:41.601313Z",
  ended_at: "2026-10-13T15:14:41.611666Z",
  observation_count: 3,
  actor_user: null,
  consolidation: null,
};

export const observations = {
  session: sessionSummary,
  observations: [
    {
      id: "01a135d4-8145-7850-8a36-c7d28db85c5f",
      session_id: SESSION_ID,
      kind: "user-prompt",
      title: "Continue the parser fix.",
      body: "Continue the parser fix.",
      importance: 8,
      created_at: "2026-10-13T15:14:41.605663Z",
      extension: null,
      source_event: null,
    },
  ],
  total: 1,
  offset: 0,
  limit: 50,
  order: "asc",
  elided_other_scope: 0,
  body_max_chars: 4000,
};

export const apiPage = {
  id: PAGE_ID,
  workspace: "demo",
  project: "app",
  path: "notes/retries.md",
  title: "Retry policy",
  kind: "note",
  tier: "semantic",
  pinned: false,
  created_at: "2026-10-13T15:10:47Z",
  updated_at: "2026-10-13T15:10:53Z",
  supersedes: null,
  frontmatter: { tier: "semantic" },
  body_markdown: "# Retry policy\nv2",
  links: [],
  backlinks: [],
};

export function pageSummary(path: string) {
  return { path, title: path, kind: "note", tier: "semantic", updated_at: "2026-10-13T15:10:53Z" };
}
