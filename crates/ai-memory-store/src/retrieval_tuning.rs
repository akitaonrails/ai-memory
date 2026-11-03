//! Post-fusion ranking signals for `hybrid_search`.
//!
//! Bounded, independently switchable signals. All but the observation stream
//! are off by default; with every one of them off a store ranks exactly as it
//! did before any of them existed:
//!
//! * **Session-recall routing** — a zero-LLM lexical check that recognises
//!   queries phrased as "find a past session / what did we do back then"
//!   ("上次 / 之前那次…的会话 / last time / yesterday / última sessão …"). Session pages
//!   otherwise carry a bounded authority penalty (kind `session` −0.15,
//!   tier `episodic` −0.08, ×0.77 combined) because they are rarely the
//!   right answer to a fact query — but a query that is *about* a past
//!   session needs one. When the routing fires, the penalty is handed
//!   back plus a small bonus, still inside the authority bounds every
//!   other page lives in.
//! * **Abstract vectors** — a fifth RRF stream over
//!   `page_abstract_embeddings`, the embedding of a page's frontmatter
//!   `abstract:` line (the L0 layer). A one-line summary embeds far more
//!   sharply than a multi-thousand-character body, giving the vector side
//!   a precise second vote per page. Contributes nothing while the table
//!   is empty.
//!
//! * **Observation stream** — a further RRF stream over `observations_fts`.
//!   The zero-LLM session page keeps only a short excerpt of each turn, so
//!   the evidence a question needs often lives only in the session's
//!   sanitized observation bodies. Each session's best-ranked matching
//!   observation votes once for that session's latest page; a session with
//!   no page contributes nothing (the raw fallback still covers it). On by
//!   default since it met its measured success criteria.
//!
//! * **Feedback-informed authority** (both OFF by default, #1941) — two
//!   bounded adjustments folded inside the existing authority clamp rather
//!   than new multipliers. `feedback_demotion` subtracts its weight from
//!   the authority factor of a *current* page version carrying open
//!   `stale`/`wrong` feedback, until a rewrite supersedes that version
//!   (feedback attaches to the version, so the penalty clears with it).
//!   `query_signal_weight` boosts a page whose current version carries a
//!   (normalized query → page) association recorded from an earlier
//!   read-after-query or `helpful` signal; the boost scales with the
//!   lexical similarity between the stored and current queries, so it is
//!   capped by similarity and never fires for unrelated queries.
//!
//! Every signal is reported in `SearchExplain`, so an `explain=true`
//! query can account for the returned rank.

use serde::{Deserialize, Serialize};

/// Tunables for the ranking signals. `Default` enables only the observation
/// stream.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RetrievalTuning {
    /// Lexical session-recall routing (see [`is_session_recall_query`]).
    pub session_recall_routing: bool,
    /// Extra authority added to session pages when the routing fires, on
    /// top of cancelling their kind/tier penalty.
    pub session_recall_bonus: f64,
    /// Add the L0 abstract-embedding stream to the RRF fusion. Reads
    /// `page_abstract_embeddings`; contributes nothing while it is empty.
    pub abstract_vectors: bool,
    /// Weight of the belief-strength confidence factor folded into
    /// [`crate::belief`] page authority (P2,
    /// `docs/design-hindsight-borrowings.md` §3). `0.0` (the default) leaves
    /// ranking byte-identical to a store that never heard of belief strength:
    /// no belief query runs and the authority factor is untouched. When
    /// positive, a page's derived `confidence` adds up to `weight * confidence`
    /// to its authority factor, still clamped inside the existing `[0.55,
    /// 1.50]` bounds — one more bounded factor, never a new multiplier tower.
    ///
    /// Ships OFF: this changes retrieval ranking, so per the design it must not
    /// default on without a positive R2 delta (retrieval-triple / QA), not yet
    /// performed.
    pub belief_authority_weight: f64,
    /// Add the observation-evidence stream to the RRF fusion (R1): search
    /// `observations_fts` in the same project and let each matching
    /// observation vote, once per session, for its session's latest
    /// `sessions/<id>.md` page. Nothing is written. ON by default: on the
    /// retrieval eval it lifted zero-LLM hit@5 without breaking the latency or
    /// token budgets. `false` restores the previous ranking byte for byte.
    pub observation_stream: bool,
    /// Weight subtracted from the authority factor of a page whose *current*
    /// version carries open `stale`/`wrong` feedback (#1941). `0.0` (the
    /// default) is inert — no feedback lookup runs and ranking is
    /// byte-identical. When positive, the weight is subtracted inside the
    /// existing `[0.55, 1.50]` authority clamp, and only while the feedback
    /// points at the current version: rewriting the page supersedes that
    /// version, which clears the penalty exactly as it clears the lint
    /// finding. `helpful`/`not_helpful` never demote.
    ///
    /// Ships OFF: this changes retrieval ranking, so per the design it must
    /// not default on without a positive measured delta (`ai-memory-eval`).
    pub feedback_demotion: f64,
    /// Weight of the query-scoped implicit-signal boost (#1941): a page whose
    /// current version carries a (normalized query → page) association —
    /// recorded when a `memory_query` hit was followed in the same session by
    /// a `memory_read_page` of that path or a `helpful` feedback signal —
    /// gains up to `weight * similarity * count_saturation` added to its
    /// authority factor, inside the existing `[0.55, 1.50]` clamp. The
    /// similarity between the stored and current queries (a Dice coefficient
    /// over their token sets) caps the boost, so an unrelated query gains
    /// nothing and an identical one gains the full amount; the count saturates
    /// after [`crate::query_signals::QUERY_SIGNAL_COUNT_CAP`] distinct
    /// sessions, which bounds the read-more→boost-more loop.
    ///
    /// `0.0` (the default) is inert: no association lookup runs, ranking is
    /// byte-identical, and the server-side capture writes nothing. Ships OFF
    /// for the same measured-delta reason as `feedback_demotion`.
    pub query_signal_weight: f64,
}

impl Default for RetrievalTuning {
    fn default() -> Self {
        Self {
            session_recall_routing: false,
            // Measured on a 138-query golden set: 0.25 recovers most
            // session-recall hit@1 (0.14 → 0.71 alongside the abstract
            // stream) while unrouted fact queries keep their exact baseline
            // ranking. Lower it (e.g. 0.15) if "之前/上次"-prefixed fact
            // queries are common in your traffic and rank drift on them
            // matters more than session recall.
            session_recall_bonus: 0.25,
            abstract_vectors: false,
            // OFF by default: folding belief confidence into ranking is
            // R2-gated (see the field doc).
            belief_authority_weight: 0.0,
            observation_stream: true,
            // OFF by default (#1941): both feedback-informed signals must
            // earn their default with a measured eval before they can ship
            // on.
            feedback_demotion: 0.0,
            query_signal_weight: 0.0,
        }
    }
}

// Markers are matched on the lower-cased query. CJK phrases match as plain
// substrings; Latin phrases match on word boundaries so "now" never fires
// inside "known" or "snow". "会话" (session) only counts in a recall frame
// ("的会话 / 次会话 / 会话里"), so a query *about* sessions as a topic —
// "user_sessions 异常会话" — is not routed to past-session recall.
const SESSION_RECALL_ZH: &[&str] = &[
    "之前",
    "上次",
    "上回",
    "上一次",
    "那次",
    "那回",
    "前几天",
    "前天",
    "昨天",
    "那天",
    "上周",
    "上星期",
    "上个月",
    "的会话",
    "次会话",
    "个会话",
    "会话里",
    "会话中",
    "历史会话",
    "当时我们",
    "我们当时",
];
const SESSION_RECALL_EN: &[&str] = &[
    "last time",
    "last session",
    "previous session",
    "earlier session",
    "that session",
    "the session where",
    "yesterday",
    "the other day",
    "last week",
    "when we",
    "we did",
    "did we",
    "what did we",
    "how did we",
    "back then",
    "earlier we",
    "previously",
];
// Require an explicit historical phrase: "sessão", "antes", or "ontem"
// alone also occur in technical queries unrelated to recalling our work.
const SESSION_RECALL_PT_BR: &[&str] = &[
    "última sessão",
    "ultima sessao",
    "última sessao",
    "ultima sessão",
    "sessão anterior",
    "sessao anterior",
    "onde paramos ontem",
    "decisão anterior",
    "decisao anterior",
];

/// Route a query to session-recall retrieval by lexical markers. Deliberately
/// zero-LLM: it gates a bounded ranking nudge, not a retrieval mode, and a
/// false positive costs a few rank positions rather than a wrong answer.
#[must_use]
pub fn is_session_recall_query(query: &str) -> bool {
    let lowered = query.to_lowercase();
    let padded = latin_word_padded(&lowered);
    SESSION_RECALL_ZH.iter().any(|m| lowered.contains(m))
        || SESSION_RECALL_EN
            .iter()
            .chain(SESSION_RECALL_PT_BR.iter())
            .any(|m| padded.contains(&format!(" {m} ")))
}

/// Replace every non-alphanumeric Latin character with a space and pad the
/// ends, so multi-word markers can be matched as ` word word `.
fn latin_word_padded(lowered: &str) -> String {
    let mut out = String::with_capacity(lowered.len() + 2);
    out.push(' ');
    for ch in lowered.chars() {
        if ch.is_ascii_alphanumeric() || (!ch.is_ascii() && ch.is_alphabetic()) {
            out.push(ch);
        } else {
            out.push(' ');
        }
    }
    out.push(' ');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_tuning_enables_only_the_observation_stream() {
        let t = RetrievalTuning::default();
        assert!(!t.session_recall_routing);
        assert!(!t.abstract_vectors);
        assert_eq!(t.belief_authority_weight, 0.0);
        assert!(t.observation_stream);
        assert_eq!(t.feedback_demotion, 0.0);
        assert_eq!(t.query_signal_weight, 0.0);
    }

    #[test]
    fn detects_session_recall_queries() {
        for q in [
            "上次排查 Caddy 配置的会话完成了吗",
            "之前那次我们把端口改成了什么",
            "之前 agent-memory 方案推到 GitHub 的首次提交哈希是什么",
            "what did we decide last time about the reranker",
            "yesterday's session on the spool bug",
        ] {
            assert!(is_session_recall_query(q), "{q}");
        }
    }

    #[test]
    fn plain_fact_queries_are_not_session_recall() {
        for q in [
            "PowerShell 没有 head 命令",
            "lark-cli docs update 报错 degrade_code 1011",
            "known issue with snow rendering",
            "git commit unable to auto-detect email",
            // "会话" as a topic, not a recall frame.
            "user_sessions 异常会话 登录失败",
        ] {
            assert!(!is_session_recall_query(q), "{q}");
        }
    }

    #[test]
    fn latin_markers_respect_word_boundaries() {
        assert!(!is_session_recall_query("unknown snowfall"));
        assert!(is_session_recall_query("what did we ship yesterday"));
    }

    #[test]
    fn detects_portuguese_session_recall_queries() {
        for q in [
            "o que fizemos na última sessão",
            "o que fizemos na ultima sessao",
            "o que fizemos na última sessao",
            "o que fizemos na ultima sessão",
            "onde paramos ontem",
            "lembre a decisão anterior",
            "lembre a decisao anterior",
            "o que discutimos na sessão anterior",
            "o que discutimos na sessao anterior",
            "ONDE PARAMOS ONTEM?",
            "lembre a (DECISÃO ANTERIOR), sobre SQLite",
        ] {
            assert!(is_session_recall_query(q), "{q}");
        }
    }

    #[test]
    fn portuguese_markers_require_history_and_word_boundaries() {
        for q in [
            "erro na sessão do usuário",
            "erro na sessao do usuario",
            "a sessão passada para o middleware é nula",
            "sessao passada como parametro",
            "sessão expira",
            "sessao expira",
            "antes de salvar",
            "sessão",
            "sessao",
            "antes",
            "ontem",
            "decisão",
            "anterior",
            "o que fizemos agora",
            "onde paramos o serviço",
            "penúltima sessão",
            "penultima sessao",
            "decisão anteriormente tomada",
            "decisao anteriormente tomada",
            "xonde paramos ontem",
            "onde paramos ontemx",
        ] {
            assert!(!is_session_recall_query(q), "{q}");
        }
    }
}
