//! Pure approval-policy decision for auto-improve proposals.
//!
//! `require_approval` holds every validated proposal; the optional
//! `require_approval_prefixes` list narrows that to proposals whose STORED
//! target path starts with one of the prefixes, leaving the rest
//! auto-approved. The decision is made on the stored path — the row an
//! operator can audit — never on the path the model claimed, which is why
//! this module takes a path string and no proposal body.

/// Whether at least one entry can hold a proposal: blank entries are
/// ignored, so a list of only blanks behaves as an empty list and no store
/// read is needed at the decision points.
pub fn auto_improve_prefixes_active(prefixes: &[String]) -> bool {
    prefixes.iter().any(|prefix| !prefix.trim().is_empty())
}

/// Whether one validated auto-improve proposal must wait for manual review.
///
/// `require_approval` holds everything. Otherwise a proposal is held only
/// when `target_path` starts with one of the configured prefixes — a
/// start-of-path match, so `_rules/` matches `_rules/x.md` but never
/// `_rulesx/z.md`. A blank entry matches nothing rather than everything.
pub fn auto_improve_requires_approval(
    require_approval: bool,
    prefixes: &[String],
    target_path: &str,
) -> bool {
    require_approval
        || prefixes
            .iter()
            .any(|prefix| !prefix.trim().is_empty() && target_path.starts_with(prefix.trim()))
}

/// The configured policy, for response and UI surfaces: `"manual"` when
/// every proposal is held, `"manual_for_prefixes"` when only matching paths
/// are held, `"auto_approve"` otherwise. A list of only blank entries
/// counts as empty.
pub fn auto_improve_approval_policy(require_approval: bool, prefixes: &[String]) -> &'static str {
    if require_approval {
        "manual"
    } else if auto_improve_prefixes_active(prefixes) {
        "manual_for_prefixes"
    } else {
        "auto_approve"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_matching_prefix_holds_the_proposal() {
        assert!(auto_improve_requires_approval(
            false,
            &list(&["_rules/"]),
            "_rules/x.md"
        ));
    }

    #[test]
    fn a_prefix_only_matches_at_the_start_of_the_path() {
        assert!(!auto_improve_requires_approval(
            false,
            &list(&["_rules/"]),
            "_rulesx/z.md"
        ));
        assert!(!auto_improve_requires_approval(
            false,
            &list(&["_rules/"]),
            "notes/_rules/x.md"
        ));
    }

    #[test]
    fn an_empty_or_blank_only_list_changes_nothing() {
        for prefixes in [list(&[]), list(&[""]), list(&["  "])] {
            assert!(
                !auto_improve_requires_approval(false, &prefixes, "_rules/x.md"),
                "{prefixes:?} must hold nothing"
            );
            assert!(!auto_improve_requires_approval(
                false,
                &prefixes,
                "gotchas/y.md"
            ));
        }
    }

    #[test]
    fn require_approval_true_holds_everything() {
        assert!(auto_improve_requires_approval(
            true,
            &list(&[]),
            "gotchas/y.md"
        ));
        assert!(auto_improve_requires_approval(
            true,
            &list(&["_rules/"]),
            "gotchas/y.md"
        ));
        assert!(auto_improve_requires_approval(
            true,
            &list(&["_rules/"]),
            "_rules/x.md"
        ));
    }

    #[test]
    fn prefixes_active_only_for_a_non_blank_entry() {
        assert!(!auto_improve_prefixes_active(&list(&[])));
        assert!(!auto_improve_prefixes_active(&list(&[""])));
        assert!(!auto_improve_prefixes_active(&list(&["  "])));
        assert!(auto_improve_prefixes_active(&list(&["_rules/"])));
        assert!(auto_improve_prefixes_active(&list(&["", "_rules/"])));
    }

    #[test]
    fn the_policy_names_each_configuration() {
        assert_eq!(
            auto_improve_approval_policy(false, &list(&[])),
            "auto_approve"
        );
        assert_eq!(
            auto_improve_approval_policy(false, &list(&[""])),
            "auto_approve",
            "a list of only blank entries counts as empty"
        );
        assert_eq!(
            auto_improve_approval_policy(false, &list(&["_rules/"])),
            "manual_for_prefixes"
        );
        assert_eq!(
            auto_improve_approval_policy(true, &list(&[])),
            "manual",
            "require_approval still holds everything"
        );
        assert_eq!(
            auto_improve_approval_policy(true, &list(&["_rules/"])),
            "manual"
        );
    }
}
