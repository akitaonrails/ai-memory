//! Opt-in post-fusion ranking signals for `hybrid_search`.
//!
//! Two bounded, independently switchable signals, both off by default so a
//! store that never configures them ranks exactly as before:
//!
//! * **Session-recall routing** — a zero-LLM lexical check that recognises
//!   queries phrased as "find a past session / what did we do back then"
//!   ("上次 / 之前那次…的会话 / last time / yesterday …"). Session pages
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
//! Both signals are reported in `SearchExplain`, so an `explain=true`
//! query can account for the returned rank.

use serde::{Deserialize, Serialize};

/// Tunables for the opt-in ranking signals. `Default` disables both.
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
    fn default_tuning_is_inert() {
        let t = RetrievalTuning::default();
        assert!(!t.session_recall_routing);
        assert!(!t.abstract_vectors);
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
}
