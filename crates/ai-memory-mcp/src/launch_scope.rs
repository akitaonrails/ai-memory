//! The launch-directory scope a stdio bridge can supply for unscoped calls.
//!
//! The session-less bridge fills it into each project-scoped call that names
//! no scope (#1979). The session-aware bridge cannot: an explicit scope
//! outranks the caller's active-project pointer, so filling it would re-route
//! every session the hooks route correctly. It sends the same scope in two
//! headers instead, and the server fills it in only when the caller's own
//! keyed pointer did not resolve (#2005): after Claude Code's `/resume`, which
//! keeps the bridge's launch-time session id while the hooks publish under the
//! resumed one, after a restart, or before the session's first hook event.
//!
//! Both sides use the rules here, so they agree on which calls get the scope.
//! Filled arguments are ordinary explicit arguments: the tool resolves and
//! authorizes them exactly as if the client had passed them.

use rmcp::model::{JsonObject, Tool};

/// Header carrying the launch directory's workspace, percent-encoded.
pub const FALLBACK_WORKSPACE_HEADER: &str = "x-memory-actor-fallback-workspace";
/// Header carrying the launch directory's project, percent-encoded.
pub const FALLBACK_PROJECT_HEADER: &str = "x-memory-actor-fallback-project";

/// Whether a call's arguments already choose where it goes. A `null` key
/// counts as absent, the way the server's optional arguments read it. Any
/// other value is the caller's, even a malformed one, which the server then
/// refuses on its own terms rather than the scope replacing it.
#[must_use]
pub fn names_a_scope(arguments: &JsonObject) -> bool {
    let given = |key: &str| arguments.get(key).filter(|value| !value.is_null());
    ["workspace", "project", "project_id"]
        .into_iter()
        .any(|key| given(key).is_some())
        || given("scopes").is_some_and(|scopes| scopes.as_array().is_none_or(|s| !s.is_empty()))
        || given("global").is_some_and(|global| global.as_bool() != Some(false))
        || given("scope").is_some_and(|scope| scope.as_str() != Some("project"))
}

/// Whether `tool`'s input schema declares both `workspace` and `project`.
#[must_use]
pub fn takes_workspace_and_project(tool: &Tool) -> bool {
    tool.input_schema
        .get("properties")
        .and_then(serde_json::Value::as_object)
        .is_some_and(|properties| {
            properties.contains_key("workspace") && properties.contains_key("project")
        })
}

/// `arguments` with `workspace` and `project` filled in, unless they already
/// name a scope.
#[must_use]
pub fn fill(mut arguments: JsonObject, workspace: &str, project: &str) -> JsonObject {
    if !names_a_scope(&arguments) {
        arguments.insert("workspace".into(), workspace.into());
        arguments.insert("project".into(), project.into());
    }
    arguments
}

/// Percent-encode a scope name for a header: names may hold any UTF-8, and a
/// header value only visible ASCII.
#[must_use]
pub fn encode_header_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Reverse [`encode_header_value`]. `None` for a malformed escape, invalid
/// UTF-8, or a blank name, so a bad header is ignored rather than guessed at.
#[must_use]
pub fn decode_header_value(value: &str) -> Option<String> {
    let bytes = value.trim().as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = std::str::from_utf8(bytes.get(index + 1..index + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out)
        .ok()
        .filter(|name| !name.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_values_round_trip_any_name_and_refuse_malformed_ones() {
        for name in ["acme-api", "r\u{e9}po notes", "team/x", "%41"] {
            let encoded = encode_header_value(name);
            assert!(encoded.bytes().all(|b| b.is_ascii_graphic()), "{encoded}");
            assert_eq!(decode_header_value(&encoded).as_deref(), Some(name));
        }
        for bad in ["%", "%4", "%zz", "%FF", "", "%20"] {
            assert_eq!(decode_header_value(bad), None, "{bad}");
        }
    }

    #[test]
    fn fill_keeps_a_scope_the_call_names() {
        let args = |value: serde_json::Value| value.as_object().cloned().unwrap();
        let filled = fill(args(serde_json::json!({"query": "x"})), "ws", "p");
        assert_eq!(filled["workspace"], "ws");
        assert_eq!(filled["project"], "p");
        for named in [
            serde_json::json!({"project": "other"}),
            serde_json::json!({"global": true}),
            serde_json::json!({"scopes": [{"workspace": "a", "project": "b"}]}),
            serde_json::json!({"scope": "global"}),
        ] {
            let kept = fill(args(named.clone()), "ws", "p");
            assert_eq!(serde_json::Value::Object(kept), named);
        }
        let unscoped = fill(
            args(serde_json::json!({"workspace": null, "global": false, "scopes": []})),
            "ws",
            "p",
        );
        assert_eq!(unscoped["workspace"], "ws");
    }
}
