//! Escaped, bounded view models built from the engine's authenticated responses.
use super::{Selection, backend::Client};
use askama::Template;
use axum::response::Response;
use serde_json::Value;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Screen {
    Login,
    Password,
    Sessions,
    Session,
    Users,
    Keys,
    Secret,
    Purge,
}
impl Screen {
    fn name(self) -> &'static str {
        match self {
            Self::Login => "login",
            Self::Password => "password",
            Self::Sessions => "sessions",
            Self::Session => "session",
            Self::Users => "users",
            Self::Keys => "keys",
            Self::Secret => "secret",
            Self::Purge => "purge",
        }
    }
}
pub(super) struct Identity {
    pub username: String,
    pub admin: bool,
    pub must_change_password: bool,
}
pub(super) struct Project {
    pub name: String,
    pub href: String,
}
pub(super) struct Session {
    pub id: String,
    pub short_id: String,
    pub agent: String,
    pub owner: String,
    pub started: String,
    pub state: &'static str,
    pub count: u64,
    pub can_delete: bool,
    pub href: String,
}
pub(super) struct Observation {
    pub kind: String,
    pub title: String,
    pub body: String,
}
pub(super) struct Key {
    pub id: String,
    pub label: String,
    pub preview: String,
    pub active: bool,
}
pub(super) struct User {
    pub username: String,
    pub name: String,
    pub role: &'static str,
    pub next_role: &'static str,
    pub status: &'static str,
    pub enabled: bool,
    pub keys: Vec<Key>,
}
pub(super) struct Count {
    pub label: &'static str,
    pub value: u64,
}

#[derive(Template)]
#[template(path = "management/console.html")]
pub(super) struct ConsoleView {
    pub i18n: ai_memory_core::i18n::Localizer,
    pub mcp_path: String,
    pub min_password: usize,
    pub root: String,
    pub screen: &'static str,
    pub script: &'static str,
    pub csrf: String,
    pub username: String,
    pub admin: bool,
    pub projects: Vec<Project>,
    pub workspace: String,
    pub project: String,
    pub sessions: Vec<Session>,
    pub observations: Vec<Observation>,
    pub users: Vec<User>,
    pub keys: Vec<Key>,
    pub session_id: String,
    pub previous: String,
    pub next: String,
    pub secret: String,
    pub secret_title: String,
    pub counts: Vec<Count>,
}
impl ConsoleView {
    pub fn new(root: &str, screen: Screen, csrf: String) -> Self {
        Self {
            i18n: ai_memory_core::i18n::Localizer::default(),
            mcp_path: format!("{}/mcp", root.strip_suffix("/manage").unwrap_or(root)),
            min_password: super::MIN_PASSWORD_CHARACTERS,
            root: root.into(),
            screen: screen.name(),
            script: super::SCRIPT,
            csrf,
            username: String::new(),
            admin: false,
            projects: vec![],
            workspace: String::new(),
            project: String::new(),
            sessions: vec![],
            observations: vec![],
            users: vec![],
            keys: vec![],
            session_id: String::new(),
            previous: String::new(),
            next: String::new(),
            secret: String::new(),
            secret_title: String::new(),
            counts: vec![],
        }
    }
    pub async fn load(
        &mut self,
        client: &Client,
        me: &Identity,
        selection: Selection,
    ) -> Result<(), Response> {
        match self.screen {
            "sessions" | "session" => self.load_sessions(client, me, selection).await?,
            "users" => {
                let (users, credentials) = tokio::try_join!(
                    client.get("/admin/users"),
                    client.get("/admin/api-credentials")
                )?;
                self.users = rows(&users, "users")
                    .iter()
                    .map(|u| User {
                        username: text(u, "username"),
                        name: text(u, "name"),
                        role: if u["role"] == "root" { "Admin" } else { "User" },
                        next_role: if u["role"] == "root" { "user" } else { "root" },
                        enabled: u["disabled_at"].is_null(),
                        status: if !u["disabled_at"].is_null() {
                            "Disabled"
                        } else if u["must_change_password"] == true {
                            "Password change required"
                        } else {
                            "Active"
                        },
                        keys: rows(&credentials, "credentials")
                            .iter()
                            .filter(|key| key["user_id"] == u["id"])
                            .map(Key::from)
                            .collect(),
                    })
                    .collect();
            }
            "keys" => {
                self.keys = rows(
                    &client.get("/management/v1/credentials").await?,
                    "credentials",
                )
                .iter()
                .map(Key::from)
                .collect();
            }
            _ => {}
        }
        Ok(())
    }
    async fn load_sessions(
        &mut self,
        client: &Client,
        me: &Identity,
        selection: Selection,
    ) -> Result<(), Response> {
        let data = client.get("/management/v1/projects").await?;
        let projects = rows(&data, "projects");
        for p in projects {
            let workspace = text(p, "workspace_name");
            let project = text(p, "project_name");
            self.projects.push(Project {
                name: format!("{workspace} / {project}"),
                href: format!("{}/sessions?{}", self.root, query(&workspace, &project, 0)),
            });
        }
        let (workspace, project) = match (selection.workspace, selection.project) {
            (Some(w), Some(p)) => (w, p),
            (None, None) if self.screen == "sessions" => match projects.first() {
                Some(p) => (text(p, "workspace_name"), text(p, "project_name")),
                None => return Ok(()),
            },
            _ => {
                return Err(super::failure(
                    &self.i18n,
                    axum::http::StatusCode::BAD_REQUEST,
                    "Both workspace and project are required".into(),
                ));
            }
        };
        self.workspace = workspace;
        self.project = project;
        let q = query(&self.workspace, &self.project, selection.offset);
        let (data, leaf, suffix, count) = if self.screen == "session" {
            let sid = selection.session.ok_or_else(|| {
                super::failure(
                    &self.i18n,
                    axum::http::StatusCode::BAD_REQUEST,
                    "Session ID required".into(),
                )
            })?;
            self.session_id = sid.to_string();
            let data = client
                .get(&format!("/management/v1/sessions/{sid}/observations?{q}"))
                .await?;
            self.observations = rows(&data, "records")
                .iter()
                .map(|r| Observation {
                    kind: text(r, "kind"),
                    title: text(r, "title"),
                    body: text(r, "body"),
                })
                .collect();
            let count = self.observations.len();
            (data, "session", format!("&session={sid}"), count)
        } else {
            let data = client.get(&format!("/management/v1/sessions?{q}")).await?;
            self.sessions = rows(&data, "sessions")
                .iter()
                .map(|s| {
                    let id = text(s, "session_id");
                    let owner = text(s, "actor_user");
                    Session {
                        short_id: crate::templates::short_id(&id),
                        agent: text(s, "agent_kind"),
                        started: text(s, "started_at"),
                        state: if s["ended_at"].is_null() {
                            "Open"
                        } else {
                            "Ended"
                        },
                        count: s["observation_count"].as_u64().unwrap_or(0),
                        can_delete: me.admin || owner == format!("user:{}", me.username),
                        owner: if owner.is_empty() {
                            "Shared".into()
                        } else {
                            owner
                        },
                        href: format!("{}/session?{q}&session={id}", self.root),
                        id,
                    }
                })
                .collect();
            let count = self.sessions.len();
            (data, "sessions", String::new(), count)
        };
        let size = data["page_size"]
            .as_u64()
            .and_then(|s| usize::try_from(s).ok())
            .filter(|s| *s > 0)
            .ok_or_else(|| {
                super::failure(
                    &self.i18n,
                    axum::http::StatusCode::BAD_GATEWAY,
                    "Missing pagination contract".into(),
                )
            })?;
        if selection.offset > 0 {
            self.previous = format!(
                "{}/{leaf}?{}{suffix}",
                self.root,
                query(
                    &self.workspace,
                    &self.project,
                    selection.offset.saturating_sub(size)
                )
            );
        }
        let more = if leaf == "session" {
            (selection.offset.saturating_add(count) as u64) < data["total"].as_u64().unwrap_or(0)
        } else {
            count == size
        };
        if more {
            self.next = format!(
                "{}/{leaf}?{}{suffix}",
                self.root,
                query(
                    &self.workspace,
                    &self.project,
                    selection.offset.saturating_add(size)
                )
            );
        }
        Ok(())
    }
    pub fn preview(
        &mut self,
        report: &Value,
        session_id: String,
        workspace: String,
        project: String,
    ) {
        self.screen = Screen::Purge.name();
        self.session_id = session_id;
        self.workspace = workspace;
        self.project = project;
        self.counts = [
            ("Observations", "observations_deleted"),
            ("Handoffs", "handoffs_deleted"),
            ("Page versions", "pages_deleted"),
            ("Review runs", "auto_improve_runs_deleted"),
            (
                "Events in other projects",
                "collateral_observations_deleted",
            ),
            (
                "Handoffs unlinked in other projects",
                "collateral_handoffs_denulled",
            ),
        ]
        .into_iter()
        .map(|(label, key)| Count {
            label,
            value: report[key].as_u64().unwrap_or(0),
        })
        .collect();
    }
}
impl From<&Value> for Key {
    fn from(k: &Value) -> Self {
        Self {
            id: text(k, "id"),
            label: text(k, "label"),
            preview: text(k, "preview"),
            active: k["revoked_at"].is_null(),
        }
    }
}
pub(super) fn text(v: &Value, key: &str) -> String {
    v[key].as_str().unwrap_or("").to_string()
}
fn rows<'a>(v: &'a Value, key: &str) -> &'a [Value] {
    v[key].as_array().map(Vec::as_slice).unwrap_or(&[])
}
pub(super) fn query(workspace: &str, project: &str, offset: usize) -> String {
    format!(
        "workspace={}&project={}&offset={offset}",
        crate::templates::encode_segment(workspace),
        crate::templates::encode_segment(project)
    )
}
