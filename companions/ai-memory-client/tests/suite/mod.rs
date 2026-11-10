use super::*;
use serde_json::json;

#[test]
fn bounded_body_schema_refuses_authority_depth_and_size_with_control() {
    let control = json!({"session_id":"sessão-界", "cwd":"/work", "_ai_memory_capture":{"actor":"untrusted provenance"}});
    check_body(&control).unwrap();
    assert!(check_body(&json!([control.clone()])).is_err());
    for field in ["workspace", "project", "actor", "author_id", "headers"] {
        let mut attack = control.clone();
        attack[field] = json!("forged");
        assert!(
            check_body(&attack).is_err(),
            "body authority must be refused"
        );
    }
    let mut deep = control.clone();
    for _ in 0..65 {
        deep = json!({"nested": deep});
    }
    assert!(check_body(&deep).is_err(), "deep bodies must be refused");
    let large = json!({"first":"x".repeat(140_000),"second":"x".repeat(140_000)});
    assert!(
        check_body(&large).is_err(),
        "body byte cap must be enforced"
    );
    let mut large_text = json!("x".repeat(256 * 1024 + 1));
    assert!(sanitize_external_value(&mut large_text).is_err());
    check_body(&control).unwrap();
}

#[test]
fn complete_literal_redaction_markers_survive_repeated_scrubbing() {
    for marker in [
        "[REDACTED]",
        "[REDACTED CREDENTIAL]",
        "[REDACTED PRIVATE KEY]",
    ] {
        let control = format!("界 token={marker} literal {marker}");
        assert_eq!(sanitize_external_text(&control), control);
        assert_eq!(
            sanitize_external_text(&sanitize_external_text(&control)),
            control
        );
    }
}

#[test]
fn secrets_are_rejected_in_all_envelope_positions_without_exposing_them() {
    for secret in [
        "Bearer abcdefghijklmnop",
        "github_pat_1234567890abcdefghijklmnop",
        "password=abcdefghi",
        "unsafe\u{1b}text",
    ] {
        assert_ne!(sanitize_external_text(secret), secret);
        for envelope in [
            json!(secret),
            json!({"session_id": secret}),
            json!({"cwd": secret}),
            json!({"source": secret}),
            json!({"metadata": [secret]}),
        ] {
            let error = reject_sensitive(&envelope).unwrap_err();
            assert!(!error.to_string().contains(secret));
        }
    }
    assert!(reject_sensitive(&json!({"nested": {"api_key": "short"}})).is_err());
    assert!(
        reject_sensitive(
            &json!({"session_id": "native", "cwd": "/work", "prompt": "ordinary text"})
        )
        .is_ok()
    );
}

#[test]
fn redacted_nested_values_validate_idempotently() {
    let body = json!({"session_id": "native", "cwd": "/work", "nested": {"token": "[REDACTED]", "api-key": "[REDACTED]"}});
    reject_sensitive(&body).unwrap();
    check_body(&body).unwrap();
    let mut again = body.clone();
    sanitize_external_value(&mut again).unwrap();
    assert_eq!(again, body);
}

#[test]
fn sanitation_keeps_native_identity_and_refuses_body_authority() {
    let mut body = json!({"session_id":"native", "cwd":"/work", "output":"\u{1b}[32m token=getToken() Bearer abcdefghijklmnop", "nested":{"password":"process.env.X"}});
    sanitize_external_value(&mut body).unwrap();
    assert_eq!(body["session_id"], "native");
    assert_eq!(body["cwd"], "/work");
    assert_eq!(body["nested"]["password"], "[REDACTED]");
    assert!(!body["output"].as_str().unwrap().contains(['\u{1b}']));
    check_body(&body).unwrap();
    for field in ["workspace", "project", "actor", "author_id", "headers"] {
        let mut forged = body.clone();
        forged[field] = "foreign".into();
        assert!(check_body(&forged).is_err());
    }
}

#[test]
fn sensitive_key_forms_redact_opaque_values_and_preserve_numeric_metrics() {
    let keys = [
        "accessToken",
        "refreshToken",
        "clientSecret",
        "privateKey",
        "x-api-key",
        "OPENAI_API_KEY",
        "accessKey",
        "HTTPAuthorization",
        "cookie",
        "sessionCookies",
        "credential",
        "credentials",
        "password",
        "token_count",
        "token_usage",
        "max_tokens",
        "input_tokens",
    ];
    for key in keys {
        let raw = json!({"nested": [{key: "fixture-value"}]});
        assert_eq!(sanitize_external_text("fixture-value"), "fixture-value");
        assert!(reject_sensitive(&raw).is_err());
        let mut safe = raw;
        sanitize_external_value(&mut safe).unwrap();
        assert_eq!(safe["nested"][0][key], "[REDACTED]");
        reject_sensitive(&safe).unwrap();
        let once = safe.clone();
        sanitize_external_value(&mut safe).unwrap();
        assert_eq!(safe, once);
    }
    let metrics = json!({"max_tokens": 12, "input_tokens": 123456789012345_u64, "token_count": 0, "token_usage": 1.25});
    let mut safe = metrics.clone();
    sanitize_external_value(&mut safe).unwrap();
    assert_eq!(safe, metrics);
    assert_eq!(
        serde_json::to_string(&safe).unwrap(),
        serde_json::to_string(&metrics).unwrap()
    );
    reject_sensitive(&safe).unwrap();
}

#[test]
fn unlisted_numeric_token_counts_survive_while_their_string_form_redacts() {
    let counts = json!({"cached_tokens": 7, "thinking_tokens": 1024, "cacheTokens": 3});
    let mut safe = counts.clone();
    sanitize_external_value(&mut safe).unwrap();
    assert_eq!(
        safe, counts,
        "numeric usage counts keep their value and type"
    );
    reject_sensitive(&safe).unwrap();

    let mut strings = json!({"cached_tokens": "fixture-value", "thinking_tokens": "fixture-value"});
    assert!(reject_sensitive(&strings).is_err());
    sanitize_external_value(&mut strings).unwrap();
    assert_eq!(strings["cached_tokens"], "[REDACTED]");
    assert_eq!(strings["thinking_tokens"], "[REDACTED]");
}

#[test]
fn numeric_token_metrics_preserve_representation_without_credential_bypass() {
    let metrics = json!({"max_tokens":12,"input_tokens":34,"token_count":0,"token_usage":1.25,"output_tokens":9007199254740993_u64,"total_tokens":89,"totalTokens":89,"cache_read_input_tokens":2,"cache_creation_input_tokens":3,"prompt_tokens":5,"completion_tokens":7,"reasoning_tokens":11});
    let raw = json!({"usage":metrics,"nested":[metrics.clone()]});
    let mut safe = raw.clone();
    sanitize_external_value(&mut safe).unwrap();
    assert!(
        safe == raw,
        "numeric metric values and types must survive sanitation"
    );
    assert!(serde_json::to_vec(&safe).unwrap() == serde_json::to_vec(&raw).unwrap());
    reject_sensitive(&safe).unwrap();
    let once = safe.clone();
    sanitize_external_value(&mut safe).unwrap();
    assert!(safe == once);
    for key in metrics.as_object().unwrap().keys() {
        let mut opaque = json!({key:"fixture-value"});
        sanitize_external_value(&mut opaque).unwrap();
        assert!(
            opaque[key] == "[REDACTED]",
            "opaque metric values must be redacted"
        );
    }
    for key in [
        "token",
        "access_tokens",
        "private_token_count",
        "auth_token_usage",
        "authorization_token_limit",
        "private_key",
        "accessKey",
    ] {
        let mut credential = json!({key:123456});
        sanitize_external_value(&mut credential).unwrap();
        assert!(
            credential[key] == "[REDACTED]",
            "numeric credential values must be redacted"
        );
    }
    let mut usage_object = json!({"token_usage":{"input_tokens":12,"output_tokens":34}});
    sanitize_external_value(&mut usage_object).unwrap();
    assert!(usage_object["token_usage"] == "[REDACTED]");
}

#[test]
fn common_credential_components_redact_opaque_values_with_author_controls() {
    for key in [
        "auth",
        "jwt",
        "pwd",
        "bearer",
        "session_key",
        "signature",
        "clientAuth",
        "HTTP_JWT",
        "HTTPJwt",
        "JWT",
        "client-pwd",
        "Bearer",
        "sessionKey",
        "Session-Key",
        "requestSignature",
    ] {
        for value in [json!("fixture-value"), json!(123456)] {
            let raw = json!({"nested":[{key:value}]});
            assert!(
                reject_sensitive(&raw).is_err(),
                "raw credential component must be refused"
            );
            let mut safe = raw;
            sanitize_external_value(&mut safe).unwrap();
            assert!(safe["nested"][0][key] == "[REDACTED]");
            reject_sensitive(&safe).unwrap();
            let once = safe.clone();
            sanitize_external_value(&mut safe).unwrap();
            assert!(safe == once);
        }
    }
    let control =
        json!({"author":"fixture-value","authority":"fixture-value","coauthor":"fixture-value"});
    assert!(sanitize_external_text("fixture-value") == "fixture-value");
    let mut safe = control.clone();
    sanitize_external_value(&mut safe).unwrap();
    assert!(
        safe == control,
        "author and authority are legitimate components"
    );
    reject_sensitive(&safe).unwrap();
}

#[test]
fn session_key_pairs_are_sensitive_with_native_session_metadata_control() {
    for key in [
        "session_key",
        "sessionKey",
        "Session-Key",
        "HTTP_SESSION_KEY",
    ] {
        for value in [json!("fixture-value"), json!(123456)] {
            let raw = json!({"nested":[{key:value}]});
            assert!(reject_sensitive(&raw).is_err());
            let mut safe = raw;
            sanitize_external_value(&mut safe).unwrap();
            assert!(safe["nested"][0][key] == "[REDACTED]");
            reject_sensitive(&safe).unwrap();
            let once = safe.clone();
            sanitize_external_value(&mut safe).unwrap();
            assert!(safe == once);
        }
    }
    let control = json!({"session":"fixture-value","session_id":"native","key":"fixture-value"});
    let mut safe = control.clone();
    sanitize_external_value(&mut safe).unwrap();
    assert!(safe == control);
    reject_sensitive(&safe).unwrap();
}

#[test]
fn plural_credential_key_pairs_redact_opaque_values_recursively() {
    assert!(sanitize_external_text("fixture-value") == "fixture-value");
    for key in [
        "api_keys",
        "access_keys",
        "private_keys",
        "session_keys",
        "apiKeys",
        "accessKeys",
        "privateKeys",
        "sessionKeys",
        "API_KEYS",
        "session-keys",
    ] {
        for value in [
            json!("fixture-value"),
            json!(["fixture-value", {"label":"opaque-value"}]),
        ] {
            let raw = json!({"nested":[{"details":{key:value}}]});
            assert!(
                reject_sensitive(&raw).is_err(),
                "plural key pair must be refused before sanitation"
            );
            let mut safe = raw;
            sanitize_external_value(&mut safe).unwrap();
            assert!(safe["nested"][0]["details"][key] == "[REDACTED]");
            check_body(&safe).unwrap();
            let once = safe.clone();
            sanitize_external_value(&mut safe).unwrap();
            assert!(safe == once);
        }
    }
}

#[test]
fn plural_key_guard_preserves_lone_keys_native_ids_and_metrics() {
    let control = json!({
        "key":"fixture-value", "keys":["fixture-value"],
        "session":"fixture-value", "session_id":"native-session", "event_id":"native-event",
        "author":"fixture-value", "authority":"fixture-value",
        "metrics":{"max_tokens":12,"input_tokens":34,"token_count":0,"token_usage":1.25,"output_tokens":9007199254740993_u64,"total_tokens":89,"totalTokens":89,"cache_read_input_tokens":2,"cache_creation_input_tokens":3,"prompt_tokens":5,"completion_tokens":7,"reasoning_tokens":11}
    });
    let bytes = serde_json::to_vec(&control).unwrap();
    check_body(&control).unwrap();
    let mut safe = control.clone();
    sanitize_external_value(&mut safe).unwrap();
    assert!(safe == control);
    assert!(serde_json::to_vec(&safe).unwrap() == bytes);
    sanitize_external_value(&mut safe).unwrap();
    assert!(safe == control);
}

#[test]
fn redaction_marker_suffix_never_exempts_a_credential() {
    for marker in [
        "[REDACTED]",
        "[REDACTED CREDENTIAL]",
        "[REDACTED PRIVATE KEY]",
    ] {
        let control = format!("token={marker}");
        assert_eq!(sanitize_external_text(&control), control);
        for attack in [
            format!("token=abcdefghijklmnop{marker}"),
            format!("token={marker}abcdefghijklmnop"),
        ] {
            assert!(reject_sensitive(&json!(attack)).is_err());
            assert_eq!(sanitize_external_text(&attack), "token=[REDACTED]");
            let safe = sanitize_external_text(&attack);
            assert_eq!(sanitize_external_text(&safe), safe);
        }
        assert_eq!(sanitize_external_text(&control), control);
    }
}

/// The vectors the server sanitizer runs too (`ai-memory-core::sanitize`), so
/// the two copies cannot drift: credentials split by an invisible character,
/// private-key blocks with and without an end line, and text that must pass
/// through unchanged.
#[test]
fn shared_sanitizer_vectors_hold() {
    let vectors: Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/sanitizer_vectors.json"
    ))
    .unwrap();
    let strings = |key: &str| -> Vec<String> {
        vectors[key]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap().to_string())
            .collect()
    };
    for secret in strings("credentials") {
        let (head, tail) = secret.split_at(secret.len() / 2);
        for invisible in strings("invisibles") {
            let out = sanitize_external_text(&format!("key {head}{invisible}{tail} end"));
            assert!(
                out.contains("[REDACTED") && !out.contains(head) && !out.contains(tail),
                "{invisible:?} inside {secret}: {out:?}"
            );
            assert!(out.starts_with("key ") && out.ends_with(" end"), "{out:?}");
        }
    }
    for field in vectors["credential_fields"].as_array().unwrap() {
        let input = field["input"].as_str().unwrap();
        let out = sanitize_external_text(input);
        assert!(
            out.contains("[REDACTED") && !out.contains(field["secret"].as_str().unwrap()),
            "{input:?} -> {out:?}"
        );
        for kept in field["kept"].as_array().into_iter().flatten() {
            assert!(out.contains(kept.as_str().unwrap()), "{input:?} -> {out:?}");
        }
    }
    let body_line = vectors["key_body_line"].as_str().unwrap();
    for block in vectors["key_blocks"].as_array().unwrap() {
        let input = block["input"].as_str().unwrap();
        let out = sanitize_external_text(input);
        assert!(
            out.contains("[REDACTED PRIVATE KEY]") && !out.contains(body_line),
            "{input:?} -> {out:?}"
        );
        for kept in block["kept"].as_array().unwrap() {
            assert!(out.contains(kept.as_str().unwrap()), "{input:?} -> {out:?}");
        }
    }
    for text in strings("unchanged") {
        assert_eq!(sanitize_external_text(&text), text);
    }
}

#[test]
fn characters_that_hide_text_or_split_a_credential_are_removed() {
    let hidden: String = "hidden note"
        .chars()
        .map(|c| char::from_u32(0xe0000 + u32::from(c)).unwrap())
        .collect();
    assert_eq!(
        sanitize_external_text(&format!("visible{hidden} text")),
        "visible text"
    );
    // A control character inside a credential no longer keeps it whole.
    let out = sanitize_external_text("sk-leak-012345\u{0}6789abcdef0123");
    assert_eq!(out, "[REDACTED CREDENTIAL]");
}

/// Control characters are stripped before matching, so a word glued onto a key
/// by a removed character used to keep the key whole: the `aim_`/`ams_` and
/// `AKIA` rules required a left word boundary the removal had destroyed, and
/// `amk_` had no rule (GHSA-6pmr-q79m-87vp, item 5, the client mirror). The
/// own-prefix and GitHub rules are now left-unanchored; AWS accepts a
/// percent-encoded byte.
#[test]
fn a_key_glued_onto_a_word_by_a_stripped_character_is_still_redacted() {
    let tail = "FAKEfake0123-_xy".repeat(3); // 48 base64url chars, > the 40 floor
    for prefix in ["aim_", "ams_", "amk_"] {
        let key = format!("{prefix}{tail}");
        for glued in [
            format!("title\u{7}{key}"),      // a removed BEL
            format!("title\u{1b}[32m{key}"), // a removed ANSI escape
            format!("word{key}"),            // a letter directly before it
        ] {
            let out = sanitize_external_text(&glued);
            assert!(
                out.contains("[REDACTED CREDENTIAL]") && !out.contains(&tail),
                "glued key must be redacted: {glued:?} -> {out:?}"
            );
        }
    }
    // A GitHub token glued onto a word by a removed BEL, and an AWS key behind
    // a percent-encoded byte.
    for (input, leaked) in [
        ("word\u{7}ghp_FAKE0123456789ABCDEFGHIJ0123", "ghp_FAKE"),
        ("url?k=%20AKIAFAKEFAKEFAKEFAKE", "FAKEFAKE"),
    ] {
        let out = sanitize_external_text(input);
        assert!(
            out.contains("[REDACTED CREDENTIAL]") && !out.contains(leaked),
            "credential must be redacted: {input:?} -> {out:?}"
        );
    }
}
