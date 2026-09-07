//! Opt-in post-fusion ranking signals for `hybrid_search`.
//!
//! Three bounded, independently switchable signals, all off by default so a
//! store that never configures them ranks exactly as before:
//!
//! * **Hotness** — `1 + alpha * hotness(page)`, where `hotness` blends how
//!   often a page has been retrieved with how recently it was updated
//!   (sigmoid(ln(1 + access_count)) × exp(−ln2 · age_days / half_life)).
//!   The shape follows OpenViking's cold/hot memory lifecycle score; the
//!   multiplicative form is what keeps it commensurate with RRF sums, which
//!   live in `[1/(k+n), streams/(k+1)]` rather than `[0, 1]`.
//! * **Query intent** — a zero-LLM lexical routing of the query into one of
//!   two intents the fused ranking is known to underserve:
//!   * `recency` ("现在 / 最新 / currently / still …") multiplies each hit
//!     by `1 + beta * exp(−ln2 · age_days / half_life)` so the page most
//!     recently updated on the topic outranks a stale twin;
//!   * `session_recall` ("上次 / 那次会话 / last time / yesterday …") lifts
//!     the authority penalty session pages otherwise carry (kind `session`
//!     −0.15, tier `episodic` −0.08) and adds a small boost, so a query that
//!     is *about* a past session can actually reach one.
//! * **Abstract vectors** — a fifth RRF stream over `page_abstract_embeddings`,
//!   the embedding of a page's frontmatter `abstract:` line. OpenViking's L0
//!   layer: a one-line summary embeds far more sharply than a long body, so
//!   it gives the vector side a precise second vote per page.
//!
//! Every multiplier and stream is reported in `SearchExplain`, so an
//! `explain=true` query can account for the returned rank.

use serde::{Deserialize, Serialize};

/// Microseconds in one day.
const DAY_US: f64 = 86_400.0 * 1_000_000.0;

/// Tunables for the opt-in ranking signals. `Default` disables both.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RetrievalTuning {
    /// Weight of the hotness boost (`1 + alpha * hotness`). `0` disables it.
    pub hotness_alpha: f64,
    /// Half-life, in days, of the recency term inside the hotness score.
    pub hotness_half_life_days: f64,
    /// Enable lexical query-intent routing (`recency` / `session_recall`).
    pub query_intent: bool,
    /// Weight of the recency boost applied under the `recency` intent.
    pub recency_beta: f64,
    /// Half-life, in days, of the recency boost under the `recency` intent.
    pub recency_half_life_days: f64,
    /// Extra authority added to session pages under `session_recall`, on
    /// top of cancelling their kind/tier penalties.
    pub session_recall_bonus: f64,
    /// Add the L0 abstract-embedding stream to the RRF fusion. Reads
    /// `page_abstract_embeddings`; contributes nothing while it is empty.
    pub abstract_vectors: bool,
}

impl Default for RetrievalTuning {
    fn default() -> Self {
        Self {
            hotness_alpha: 0.0,
            hotness_half_life_days: 7.0,
            query_intent: false,
            recency_beta: 0.5,
            recency_half_life_days: 30.0,
            session_recall_bonus: 0.10,
            abstract_vectors: false,
        }
    }
}

impl RetrievalTuning {
    /// True when any signal needs per-page `access_count` / `updated_at`.
    #[must_use]
    pub fn needs_page_signals(&self, intent: Option<QueryIntent>) -> bool {
        self.hotness_alpha > 0.0 || matches!(intent, Some(QueryIntent::Recency))
    }

    /// Detect the query intent, or `None` when routing is disabled or the
    /// query carries no intent marker.
    #[must_use]
    pub fn intent_for(&self, query: &str) -> Option<QueryIntent> {
        if self.query_intent {
            detect_query_intent(query)
        } else {
            None
        }
    }

    /// `1 + alpha * hotness`, or exactly `1.0` when hotness is disabled.
    #[must_use]
    pub fn hotness_boost(&self, access_count: i64, updated_at_us: i64, now_us: i64) -> (f64, f64) {
        if self.hotness_alpha <= 0.0 {
            return (0.0, 1.0);
        }
        let h = hotness_score(
            access_count,
            updated_at_us,
            now_us,
            self.hotness_half_life_days,
        );
        (h, 1.0 + self.hotness_alpha * h)
    }

    /// Multiplier for one hit under the detected intent. Session-page
    /// handling lives on `PageAuthority`; this covers the recency intent.
    #[must_use]
    pub fn recency_boost(&self, updated_at_us: i64, now_us: i64) -> f64 {
        if self.recency_beta <= 0.0 {
            return 1.0;
        }
        1.0 + self.recency_beta * time_decay(updated_at_us, now_us, self.recency_half_life_days)
    }
}

/// Lexically detected query intent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryIntent {
    /// The caller wants the *current* state of something.
    Recency,
    /// The caller wants to find a past session / what happened then.
    SessionRecall,
}

impl QueryIntent {
    /// Stable wire name (matches the serde representation).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Recency => "recency",
            Self::SessionRecall => "session_recall",
        }
    }
}

/// `sigmoid(ln(1 + access_count)) * exp(-ln2 * age_days / half_life)` in `[0, 1]`.
#[must_use]
pub fn hotness_score(
    access_count: i64,
    updated_at_us: i64,
    now_us: i64,
    half_life_days: f64,
) -> f64 {
    let n = access_count.max(0) as f64;
    let freq = 1.0 / (1.0 + (-(1.0 + n).ln()).exp());
    freq * time_decay(updated_at_us, now_us, half_life_days)
}

/// Exponential recency in `(0, 1]`: `1` for a page updated right now, `0.5`
/// after one half-life. A non-positive half-life disables the decay (`1`).
#[must_use]
pub fn time_decay(updated_at_us: i64, now_us: i64, half_life_days: f64) -> f64 {
    if half_life_days <= 0.0 {
        return 1.0;
    }
    let age_days = (now_us.saturating_sub(updated_at_us).max(0) as f64) / DAY_US;
    (-(std::f64::consts::LN_2 / half_life_days) * age_days).exp()
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
const RECENCY_ZH: &[&str] = &[
    "现在",
    "目前",
    "当前",
    "最新",
    "最近",
    "如今",
    "现状",
    "现行",
    "现有",
    "现用",
    "改成",
    "改为",
    "换成",
    "切换",
    "切到",
    "迁到",
    "还在",
    "仍然",
    "仍在",
    "是否还",
    "还吃",
    "还用",
    "还加",
    "新版",
    "升级后",
    "以后",
    "今后",
];
const RECENCY_EN: &[&str] = &[
    "now",
    "current",
    "currently",
    "latest",
    "recent",
    "recently",
    "still",
    "anymore",
    "these days",
    "at the moment",
    "switched",
    "changed to",
    "moved to",
    "up to date",
    "newest",
    "today",
    "nowadays",
];

/// Route a query to an intent by lexical markers. Session-recall markers win
/// over recency markers because "上次我们把 X 改成什么" is about *where* the
/// answer lives (a session), not about which version is current.
#[must_use]
pub fn detect_query_intent(query: &str) -> Option<QueryIntent> {
    let lowered = query.to_lowercase();
    let padded = latin_word_padded(&lowered);
    let hits = |zh: &[&str], en: &[&str]| {
        zh.iter().any(|m| lowered.contains(m))
            || en.iter().any(|m| padded.contains(&format!(" {m} ")))
    };
    if hits(SESSION_RECALL_ZH, SESSION_RECALL_EN) {
        Some(QueryIntent::SessionRecall)
    } else if hits(RECENCY_ZH, RECENCY_EN) {
        Some(QueryIntent::Recency)
    } else {
        None
    }
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

    const DAY: i64 = 86_400 * 1_000_000;

    #[test]
    fn default_tuning_is_inert() {
        let t = RetrievalTuning::default();
        assert_eq!(t.hotness_boost(50, 0, 10 * DAY), (0.0, 1.0));
        assert_eq!(t.intent_for("现在 X 用什么"), None);
        assert!(!t.needs_page_signals(None));
    }

    #[test]
    fn hotness_is_bounded_and_monotone() {
        let now = 100 * DAY;
        let fresh_hot = hotness_score(100, now, now, 7.0);
        let fresh_cold = hotness_score(0, now, now, 7.0);
        let stale_hot = hotness_score(100, now - 70 * DAY, now, 7.0);
        assert!(fresh_hot > fresh_cold && fresh_cold > stale_hot);
        assert!((0.0..=1.0).contains(&fresh_hot) && (0.0..=1.0).contains(&stale_hot));
        // sigmoid(ln 1) = 0.5: a never-read page updated now scores 0.5.
        assert!((fresh_cold - 0.5).abs() < 1e-9);
        // one half-life halves the recency term.
        let half = hotness_score(0, now - 7 * DAY, now, 7.0);
        assert!((half - 0.25).abs() < 1e-9);
    }

    #[test]
    fn hotness_boost_uses_alpha() {
        let t = RetrievalTuning {
            hotness_alpha: 0.4,
            ..RetrievalTuning::default()
        };
        let (h, boost) = t.hotness_boost(0, 10 * DAY, 10 * DAY);
        assert!((h - 0.5).abs() < 1e-9);
        assert!((boost - 1.2).abs() < 1e-9);
        // future timestamps never produce a boost above 1 + alpha.
        let (_, capped) = t.hotness_boost(i64::MAX, 20 * DAY, 10 * DAY);
        assert!(capped <= 1.0 + t.hotness_alpha + 1e-9);
    }

    #[test]
    fn recency_boost_decays_with_age() {
        let t = RetrievalTuning {
            query_intent: true,
            ..RetrievalTuning::default()
        };
        let now = 400 * DAY;
        let fresh = t.recency_boost(now, now);
        let month = t.recency_boost(now - 30 * DAY, now);
        let year = t.recency_boost(now - 365 * DAY, now);
        assert!((fresh - 1.5).abs() < 1e-9);
        assert!((month - 1.25).abs() < 1e-9);
        assert!(year < 1.001 && year >= 1.0);
    }

    #[test]
    fn detects_recency_intent() {
        for q in [
            "现在 new-api 网关映射到哪个端口",
            "目前补剂方案里还吃 Omega-3 吗",
            "What is the current embedding model?",
            "is the qwen container still running",
            "latest deployment steps for h3",
        ] {
            assert_eq!(detect_query_intent(q), Some(QueryIntent::Recency), "{q}");
        }
    }

    #[test]
    fn detects_session_recall_intent_and_wins_over_recency() {
        for q in [
            "上次排查 Caddy 配置的会话完成了吗",
            "之前那次我们把端口改成了什么",
            "之前 agent-memory 方案推到 GitHub 的首次提交哈希是什么",
            "what did we decide last time about the reranker",
            "yesterday's session on the spool bug",
        ] {
            assert_eq!(
                detect_query_intent(q),
                Some(QueryIntent::SessionRecall),
                "{q}"
            );
        }
    }

    #[test]
    fn plain_fact_queries_have_no_intent() {
        for q in [
            "PowerShell 没有 head 命令",
            "lark-cli docs update 报错 degrade_code 1011",
            "known issue with snow rendering",
            "git commit unable to auto-detect email",
            // "会话" as a topic, not a recall frame.
            "user_sessions 异常会话 登录失败",
        ] {
            assert_eq!(detect_query_intent(q), None, "{q}");
        }
    }

    #[test]
    fn latin_markers_respect_word_boundaries() {
        assert_eq!(detect_query_intent("unknown snowfall"), None);
        assert_eq!(
            detect_query_intent("Now: unknown"),
            Some(QueryIntent::Recency)
        );
    }
}
