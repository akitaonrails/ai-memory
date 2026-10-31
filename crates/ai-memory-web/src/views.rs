//! Operational dashboard view models.

use crate::operations::AgentCard;
use crate::templates::SessionRow;
use askama::Template;

#[derive(Template)]
#[template(path = "agent.html")]
pub(crate) struct AgentView {
    pub i18n: ai_memory_core::i18n::Localizer,
    pub workspace: String,
    pub project: String,
    pub project_href: String,
    pub card: AgentCard,
    pub sessions: Vec<SessionRow>,
    pub previous: String,
    pub next: String,
}

pub(crate) struct ScopeOption {
    pub workspace: String,
    pub project: String,
    pub selected: bool,
    pub pending: u64,
    pub inbox_href: String,
}

pub(crate) struct RecipientOption {
    pub workspace: String,
    pub project: String,
    pub agent: String,
    pub label: String,
    pub selected: bool,
}

pub(crate) struct MessageRow {
    pub id: String,
    pub subject: String,
    pub raw_subject: String,
    pub body: String,
    pub from: String,
    pub to: String,
    pub agent: String,
    pub state: String,
    pub state_label: String,
    pub when: String,
}

#[derive(Template)]
#[template(path = "inbox.html")]
pub(crate) struct InboxView {
    pub i18n: ai_memory_core::i18n::Localizer,
    pub workspace: String,
    pub project: String,
    pub project_href: String,
    pub scopes: Vec<ScopeOption>,
    pub pending_total: u64,
    pub pending_projects: usize,
    pub recipients: Vec<RecipientOption>,
    pub rows: Vec<MessageRow>,
    pub inbox: bool,
    pub editing_count: usize,
    pub editing_href: String,
    pub state: String,
    pub inbox_href: String,
    pub outbox_href: String,
    pub previous: String,
    pub next: String,
    pub can_send: bool,
    pub needs_write_token: bool,
}
