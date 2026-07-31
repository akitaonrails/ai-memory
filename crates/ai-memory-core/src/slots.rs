//! Naming rules for memory slots (`_slots/…`).
//!
//! A slot is a small mutable page — "what I am working on", pending items,
//! project context — that the engine injects into every session start for its
//! project. On a server shared by several people that makes one operator's
//! working context everybody's, so slots can optionally be namespaced per
//! operator:
//!
//! * `_slots/current-focus.md` — **shared** with the whole project. Every
//!   pre-existing slot has this shape, and it stays visible to everyone.
//! * `_slots/<user>/current-focus.md` — belongs to `<user>`; only they see it
//!   in their brief.
//!
//! Shared-when-unprefixed mirrors the `NULL owner = shared` rule the handoff and
//! session tables already use, so turning the feature on cannot orphan or
//! reinterpret anything already stored.

/// Path prefix marking a slot page.
pub const SLOT_PREFIX: &str = "_slots/";

/// Is this a slot page (shared or personal)?
#[must_use]
pub fn is_slot_path(path: &str) -> bool {
    path.starts_with(SLOT_PREFIX)
}

/// The operator a slot belongs to, or `None` when it is shared.
///
/// Only the FIRST path segment after the prefix is considered, and only when
/// the slot has a nested shape: `_slots/alice/current-focus.md` is Alice's,
/// `_slots/current-focus.md` is shared.
#[must_use]
pub fn slot_owner(path: &str) -> Option<&str> {
    let rest = path.strip_prefix(SLOT_PREFIX)?;
    let (owner, remainder) = rest.split_once('/')?;
    if owner.is_empty() || remainder.is_empty() {
        return None;
    }
    Some(owner)
}

/// Can `user` be used as a slot namespace segment?
///
/// Stricter than [`crate::user::validate_username`] on purpose. That check
/// already rejects path separators, but the segment also ends up inside SQL
/// `GLOB` patterns, where `*`, `?` and `[` are wildcards — a user called `a*`
/// would otherwise match every other operator's slots. Rejecting them here
/// keeps one rule for both uses instead of escaping at each call site.
///
/// Also rejects `.` and `..` so a namespace can never resolve upwards.
#[must_use]
pub fn is_valid_slot_namespace(user: &str) -> bool {
    if user.is_empty() || user == "." || user == ".." {
        return false;
    }
    !user.chars().any(|c| {
        c.is_control()
            || c.is_whitespace()
            || matches!(
                c,
                '/' | '\\' | '*' | '?' | '[' | ']' | ':' | ';' | ',' | '"' | '\'' | '`'
            )
    })
}

/// Which slot pages a reader is allowed to see.
///
/// "Per-user slots are off" and "per-user slots are on but the viewer has no
/// name" are different rules, and an `Option<&str>` cannot tell them apart. The
/// difference is load-bearing: with the feature off a pre-existing
/// `_slots/backend/context.md` is an ordinary shared slot that everyone must
/// keep seeing, while with it on the same shape means "belongs to `backend`".
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum SlotVisibility {
    /// Every `_slots/…` page, personal namespaces included.
    ///
    /// The rule that predates per-user slots, hence the default: with
    /// `[slots] per_user` off a nested slot path carries no ownership meaning,
    /// so hiding it would silently drop a page from an existing brief. Also the
    /// right rule for views that deliberately show the whole wiki.
    #[default]
    All,
    /// Shared (un-namespaced) slots, plus `viewer`'s own namespace when they
    /// have a usable one. What `[slots] per_user` turns a session brief into.
    Owner {
        /// The operator the brief is being assembled for. `None` — an
        /// unattributed request — sees the shared slots only.
        viewer: Option<String>,
    },
}

impl SlotVisibility {
    /// The rule for `viewer` when `[slots] per_user` is `per_user`.
    ///
    /// A blank name is the same as no name at all: it identifies nobody, so it
    /// gets the shared slots and nothing else.
    #[must_use]
    pub fn for_viewer(per_user: bool, viewer: Option<&str>) -> Self {
        if !per_user {
            return Self::All;
        }
        Self::Owner {
            viewer: viewer
                .map(str::trim)
                .filter(|u| !u.is_empty())
                .map(ToOwned::to_owned),
        }
    }

    /// The viewer's own namespace, when the rule grants one and the name can
    /// actually be used as a segment.
    #[must_use]
    pub fn own_namespace(&self) -> Option<&str> {
        match self {
            Self::All => None,
            Self::Owner { viewer } => viewer.as_deref().filter(|u| is_valid_slot_namespace(u)),
        }
    }

    /// Does this rule hide the slots of namespaces other than the viewer's?
    #[must_use]
    pub fn hides_other_namespaces(&self) -> bool {
        matches!(self, Self::Owner { .. })
    }

    /// May the viewer see `path`?
    ///
    /// The in-memory twin of the SQL the store builds; non-slot paths are not
    /// this rule's business and always pass.
    #[must_use]
    pub fn allows(&self, path: &str) -> bool {
        let Some(owner) = slot_owner(path) else {
            return true;
        };
        match self {
            Self::All => true,
            Self::Owner { .. } => self.own_namespace() == Some(owner),
        }
    }
}

/// Where a slot write belongs once slots are namespaced per operator.
///
/// The three ways a write can fail to be "just store it" are genuinely
/// different, and collapsing any pair of them makes the guard fail open:
/// "leave it shared, by design" versus "this operator cannot have a namespace"
/// would send a user whose name is not a legal segment straight into the
/// project-wide slot every other operator reads, and either of those versus
/// "already namespaced to somebody else" would let a writer drop a page into a
/// namespace that is not theirs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlotPlacement {
    /// Not a slot, already the writer's own namespace, or a shared slot written
    /// by an unattributed actor — write the path unchanged.
    AsGiven,
    /// Write to this per-operator path instead.
    Personal(String),
    /// The path names a namespace that is not the writer's. With per-user slots
    /// on the caller must refuse: `_slots/<user>/…` bodies are injected verbatim
    /// into that operator's next session brief, so writing one under a name that
    /// is not yours puts chosen text into somebody else's agent context.
    ForeignNamespace,
    /// The operator's name cannot be a namespace segment (see
    /// [`is_valid_slot_namespace`]). With per-user slots on, the caller must
    /// refuse the write, whether the target is the shared path — somebody
    /// else's page — or a namespace spelled with that same unusable name, which
    /// [`SlotVisibility::own_namespace`] filters through the very same
    /// predicate, so nobody could ever read the page back.
    Unnamespaceable,
}

/// Decide where a slot write by `user` belongs.
///
/// `user` is `None` for an unattributed actor. That is not the same as "any
/// name will do": an unattributed writer keeps the shared path — the behaviour
/// that predates per-user slots — but owns no namespace, so every namespaced
/// slot is foreign to it.
#[must_use]
pub fn slot_placement(path: &str, user: Option<&str>) -> SlotPlacement {
    if !is_slot_path(path) {
        return SlotPlacement::AsGiven;
    }
    let user = user.map(str::trim).filter(|u| !u.is_empty());
    if let Some(owner) = slot_owner(path) {
        return match user {
            // A name that cannot BE a namespace cannot own one either, or the
            // predicate that refuses `a*` the shared slot would hand `a*` an
            // unrestricted `_slots/a*/…` prefix. The page would also be
            // unreadable: `SlotVisibility::own_namespace` runs the viewer
            // through `is_valid_slot_namespace`, so even its own writer would
            // never see it again.
            Some(u) if u == owner && !is_valid_slot_namespace(owner) => {
                SlotPlacement::Unnamespaceable
            }
            Some(u) if u == owner => SlotPlacement::AsGiven,
            _ => SlotPlacement::ForeignNamespace,
        };
    }
    let Some(user) = user else {
        return SlotPlacement::AsGiven;
    };
    if !is_valid_slot_namespace(user) {
        return SlotPlacement::Unnamespaceable;
    }
    match path.strip_prefix(SLOT_PREFIX) {
        Some(rest) if !rest.is_empty() => {
            SlotPlacement::Personal(format!("{SLOT_PREFIX}{user}/{rest}"))
        }
        _ => SlotPlacement::AsGiven,
    }
}

/// Where a page derived from ONE operator's session belongs once slots are
/// namespaced per operator. See [`staged_slot_target`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StagedSlotTarget {
    /// Record/write the path unchanged.
    AsGiven,
    /// Record this per-operator path instead of the one given.
    Rehomed(String),
    /// Do not record (or write) this page at all; the string says why, in terms
    /// a reviewer reading a run report can act on.
    Refused(String),
}

impl StagedSlotTarget {
    /// Why this target cannot be written AS GIVEN — `None` only for
    /// [`Self::AsGiven`].
    ///
    /// [`Self::Rehomed`] is an instruction to the door that is still choosing
    /// the path, and a refusal for every door after it: once a proposal exists,
    /// its target is bound to the stage-time snapshot of that exact page, so
    /// moving it is no longer possible — only declining it is.
    #[must_use]
    pub fn refusal(&self) -> Option<String> {
        match self {
            Self::AsGiven => None,
            Self::Rehomed(personal) => Some(format!(
                "it belongs in the owning operator's own slot namespace ('{personal}'), which \
                 has to be decided when the proposal is staged"
            )),
            Self::Refused(reason) => Some(reason.clone()),
        }
    }
}

/// Decide where an auto-improvement proposal's page belongs, from the operator
/// whose session produced it.
///
/// Auto-improve derives a proposal from ONE operator's session, and a slot body
/// is injected verbatim into a session brief. So under `[slots] per_user` the
/// project-wide `_slots/current-focus.md` is the wrong destination for one:
/// approving it there puts one operator's session output into EVERY operator's
/// brief. The right destination is the namespace of the operator the proposal
/// came from.
///
/// Which operator that is can only be settled while the proposal is being
/// staged (`staged_by_actor_user`), never at approval: the store binds an
/// approval to the proposal's recorded `target_path` and to the stage-time
/// snapshot of THAT page, so a target that should have been namespaced cannot be
/// corrected later. Hence one function for both doors — staging acts on
/// [`StagedSlotTarget::Rehomed`], approval can only refuse it (see
/// [`StagedSlotTarget::refusal`]) — and hence why neither door may key this on
/// whoever APPROVES: the unattended scheduler approves with no user at all, so
/// an approver-keyed guard waves the shared-slot write straight through.
///
/// `edit_mode` is the proposal's mode. A `patch` body was materialized against
/// the path as given, so its base does not exist under any other path and it
/// cannot be re-homed. Patch mode is confined to `_rules/` and `procedures/`
/// today, so this arm is a belt for a future that widens it.
///
/// With `per_user_slots` off — the default — every path is
/// [`StagedSlotTarget::AsGiven`]: a nested slot path carries no ownership
/// meaning, and nothing about auto-improve changes.
#[must_use]
pub fn staged_slot_target(
    per_user_slots: bool,
    path: &str,
    staged_by: Option<&str>,
    edit_mode: &str,
) -> StagedSlotTarget {
    if !per_user_slots || !is_slot_path(path) {
        return StagedSlotTarget::AsGiven;
    }
    match slot_placement(path, staged_by) {
        // The one place this rule is deliberately stricter than
        // [`slot_placement`], which leaves an unattributed writer on the SHARED
        // slot. That is right for `memory_write_page` and the consolidator: they
        // decide on a LIVE request, where "no actor" means the deployment names
        // nobody, so the shared slot is the pre-feature behaviour and nothing is
        // being taken from anyone. Here the decision is made from a STORED
        // record, and "no actor" means the session's owner was not recorded —
        // on a server with per-user slots on, that is one of several named
        // operators whose name went missing, not a statement about the project.
        // There is no owner to guess and the shared slot is read by everybody,
        // so refuse.
        SlotPlacement::AsGiven if slot_owner(path).is_none() => StagedSlotTarget::Refused(format!(
            "'{path}' is the project-wide slot, whose body reaches every operator's session \
             brief, and this proposal is attributed to no operator"
        )),
        SlotPlacement::AsGiven => StagedSlotTarget::AsGiven,
        SlotPlacement::Personal(_) if edit_mode == "patch" => StagedSlotTarget::Refused(format!(
            "'{path}' is the project-wide slot and this proposal is a patch materialized against \
             it, so it cannot be moved into the operator's own namespace"
        )),
        SlotPlacement::Personal(personal) => StagedSlotTarget::Rehomed(personal),
        SlotPlacement::ForeignNamespace => StagedSlotTarget::Refused(format!(
            "'{path}' belongs to another operator's slot namespace, whose body is injected \
             verbatim into their next session brief"
        )),
        SlotPlacement::Unnamespaceable => StagedSlotTarget::Refused(format!(
            "the operator this proposal belongs to cannot have a slot namespace, and '{path}' is \
             not theirs to write"
        )),
    }
}

/// Does this slot path end in `name` (e.g. `current-focus.md`), whether it is
/// shared or namespaced?
///
/// Code that recognises a specific slot by literal equality breaks the moment
/// slots can be namespaced — `_slots/alice/current-focus.md` stops matching
/// `_slots/current-focus.md` and gets treated as a brand-new page.
#[must_use]
pub fn is_slot_named(path: &str, name: &str) -> bool {
    match path.strip_prefix(SLOT_PREFIX) {
        Some(rest) => rest == name || rest.rsplit_once('/').is_some_and(|(_, tail)| tail == name),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unprefixed_slots_are_shared() {
        assert!(is_slot_path("_slots/current-focus.md"));
        assert_eq!(slot_owner("_slots/current-focus.md"), None);
    }

    #[test]
    fn nested_slots_belong_to_their_first_segment() {
        assert_eq!(slot_owner("_slots/alice/current-focus.md"), Some("alice"));
        // Deeper nesting still attributes to the first segment only.
        assert_eq!(slot_owner("_slots/alice/sub/x.md"), Some("alice"));
    }

    #[test]
    fn non_slot_paths_have_no_owner() {
        assert!(!is_slot_path("_rules/style.md"));
        assert_eq!(slot_owner("notes/x.md"), None);
    }

    /// GLOB metacharacters must be refused: the segment is interpolated into
    /// `GLOB` patterns, so a user named `a*` would otherwise read every other
    /// operator's slots.
    #[test]
    fn glob_metacharacters_are_not_valid_namespaces() {
        for bad in ["a*", "a?", "a[b]", "a/b", "a\\b", "", ".", "..", "a b"] {
            assert!(!is_valid_slot_namespace(bad), "{bad:?} must be rejected");
        }
        for good in ["alice", "alice-1", "alice_1", "Alice.Smith"] {
            assert!(is_valid_slot_namespace(good), "{good:?} must be accepted");
        }
    }

    #[test]
    fn personal_rewrite_is_idempotent_and_conservative() {
        assert_eq!(
            slot_placement("_slots/current-focus.md", Some("alice")),
            SlotPlacement::Personal("_slots/alice/current-focus.md".into())
        );
        // Already the writer's own namespace: leave alone.
        assert_eq!(
            slot_placement("_slots/alice/current-focus.md", Some("alice")),
            SlotPlacement::AsGiven
        );
        // Not a slot: leave alone.
        assert_eq!(
            slot_placement("notes/x.md", Some("alice")),
            SlotPlacement::AsGiven
        );
    }

    /// A path that already names an operator must be distinguishable from "no
    /// rewrite needed": the model picks these paths, so `AsGiven` there is a
    /// licence to write into anybody's namespace.
    #[test]
    fn another_operators_namespace_is_its_own_case() {
        assert_eq!(
            slot_placement("_slots/alice/current-focus.md", Some("bob")),
            SlotPlacement::ForeignNamespace
        );
        // Deeper nesting attributes to the first segment, so it is foreign too.
        assert_eq!(
            slot_placement("_slots/alice/sub/x.md", Some("bob")),
            SlotPlacement::ForeignNamespace
        );
        // An unattributed writer owns no namespace at all.
        assert_eq!(
            slot_placement("_slots/alice/current-focus.md", None),
            SlotPlacement::ForeignNamespace
        );
        // …but still keeps the shared path, the pre-feature behaviour.
        assert_eq!(
            slot_placement("_slots/current-focus.md", None),
            SlotPlacement::AsGiven
        );
        // A blank name identifies nobody, exactly like an absent one.
        assert_eq!(
            slot_placement("_slots/alice/x.md", Some("   ")),
            SlotPlacement::ForeignNamespace
        );
    }

    /// The guard must not fail open: a name that cannot be a namespace segment
    /// has to be distinguishable from "leave it shared", or the operator lands
    /// on the project-wide slot every other operator reads.
    #[test]
    fn unnamespaceable_user_is_not_reported_as_shared() {
        assert_eq!(
            slot_placement("_slots/current-focus.md", Some("a*")),
            SlotPlacement::Unnamespaceable
        );
        assert_eq!(
            slot_placement("_slots/current-focus.md", Some(".")),
            SlotPlacement::Unnamespaceable
        );
        // A non-slot page is never this rule's business, whatever the name.
        assert_eq!(
            slot_placement("notes/x.md", Some("a*")),
            SlotPlacement::AsGiven
        );
    }

    /// The write rule and the read rule must agree on which names can be a
    /// namespace. `own_namespace()` filters the viewer through
    /// `is_valid_slot_namespace`, so if placement let `a*` claim `_slots/a*/…`
    /// the page would be pinned into the wiki and visible to nobody at all —
    /// least of all the operator who wrote it.
    #[test]
    fn an_unnamespaceable_name_cannot_own_a_namespace_either() {
        for bad in ["a*", "a?", "a[b]", ".", ".."] {
            let path = format!("_slots/{bad}/current-focus.md");
            assert_eq!(
                slot_placement(&path, Some(bad)),
                SlotPlacement::Unnamespaceable,
                "{bad:?} must not be handed its own namespace"
            );
            // The read half, which is what makes the write half unusable.
            assert_eq!(
                SlotVisibility::for_viewer(true, Some(bad)).own_namespace(),
                None,
                "{bad:?}"
            );
            // Somebody else's name in that segment stays the foreign case: the
            // refusal is about the path, not about the writer's own name.
            assert_eq!(
                slot_placement(&path, Some("alice")),
                SlotPlacement::ForeignNamespace,
                "{bad:?}"
            );
        }
        // DEFAULT CONFIG (`[slots] per_user` off) never consults placement at
        // all, and a legal name keeps owning its namespace.
        assert_eq!(
            slot_placement("_slots/alice/current-focus.md", Some("alice")),
            SlotPlacement::AsGiven
        );
    }

    /// Feature OFF is not the same rule as "feature on, viewer unknown": with
    /// it off a nested slot path carries no ownership meaning and stays visible.
    #[test]
    fn visibility_off_shows_every_slot() {
        let off = SlotVisibility::for_viewer(false, None);
        assert_eq!(off, SlotVisibility::All);
        assert!(off.allows("_slots/backend/context.md"));
        assert!(!off.hides_other_namespaces());
        // Even a named viewer sees everything while the feature is off.
        assert!(SlotVisibility::for_viewer(false, Some("alice")).allows("_slots/bob/focus.md"));
    }

    #[test]
    fn visibility_on_admits_shared_and_own_only() {
        let alice = SlotVisibility::for_viewer(true, Some("alice"));
        assert!(alice.allows("_slots/current-focus.md"));
        assert!(alice.allows("_slots/alice/current-focus.md"));
        assert!(!alice.allows("_slots/bob/current-focus.md"));
        assert_eq!(alice.own_namespace(), Some("alice"));

        // Unattributed, blank, and unusable names all collapse to shared-only.
        for viewer in [None, Some(""), Some("   "), Some("a*")] {
            let v = SlotVisibility::for_viewer(true, viewer);
            assert_eq!(v.own_namespace(), None, "{viewer:?}");
            assert!(v.allows("_slots/current-focus.md"), "{viewer:?}");
            assert!(!v.allows("_slots/alice/current-focus.md"), "{viewer:?}");
        }
    }

    /// DEFAULT CONFIG: with `[slots] per_user` off, no auto-improve proposal is
    /// ever moved or refused, whatever the path and whoever staged it.
    #[test]
    fn staged_slot_target_is_inert_with_per_user_off() {
        for path in [
            "_slots/current-focus.md",
            "_slots/alice/current-focus.md",
            "_slots/bob/current-focus.md",
            "notes/x.md",
        ] {
            for staged_by in [None, Some("alice"), Some("a*")] {
                assert_eq!(
                    staged_slot_target(false, path, staged_by, "full_page"),
                    StagedSlotTarget::AsGiven,
                    "{path:?} / {staged_by:?}"
                );
            }
        }
    }

    /// The shared slot is where the prompt points every slot proposal, and it is
    /// the one destination a session-derived body may never take under per-user
    /// slots: it is injected verbatim into EVERY operator's brief. Named
    /// operator ⇒ re-home; nobody ⇒ refuse, never "leave it shared".
    #[test]
    fn staged_slot_target_never_leaves_a_proposal_on_the_shared_slot() {
        assert_eq!(
            staged_slot_target(true, "_slots/current-focus.md", Some("alice"), "full_page"),
            StagedSlotTarget::Rehomed("_slots/alice/current-focus.md".into())
        );
        // The unattended scheduler's own attribution: no operator at all.
        let refused = staged_slot_target(true, "_slots/current-focus.md", None, "full_page");
        assert!(
            matches!(refused, StagedSlotTarget::Refused(_)),
            "{refused:?}"
        );
        assert!(refused.refusal().is_some());
        // A blank name identifies nobody either.
        assert!(matches!(
            staged_slot_target(true, "_slots/current-focus.md", Some("   "), "full_page"),
            StagedSlotTarget::Refused(_)
        ));
        // And a name that cannot be a namespace must not fall back to shared.
        assert!(matches!(
            staged_slot_target(true, "_slots/current-focus.md", Some("a*"), "full_page"),
            StagedSlotTarget::Refused(_)
        ));
    }

    /// The staged target the approval door must accept: the proposal's own
    /// operator's namespace, and only that. Non-slot pages are never this
    /// rule's business.
    #[test]
    fn staged_slot_target_accepts_only_the_owning_operators_namespace() {
        assert_eq!(
            staged_slot_target(
                true,
                "_slots/alice/current-focus.md",
                Some("alice"),
                "full_page"
            ),
            StagedSlotTarget::AsGiven
        );
        assert!(matches!(
            staged_slot_target(
                true,
                "_slots/alice/current-focus.md",
                Some("bob"),
                "full_page"
            ),
            StagedSlotTarget::Refused(_)
        ));
        assert!(matches!(
            staged_slot_target(true, "_slots/alice/current-focus.md", None, "full_page"),
            StagedSlotTarget::Refused(_)
        ));
        assert_eq!(
            staged_slot_target(true, "notes/x.md", None, "full_page"),
            StagedSlotTarget::AsGiven
        );
    }

    /// A patch body was materialized against the shared slot, so its base does
    /// not exist under the operator's path: re-homing it would stage an update
    /// against a page that is not the one the patch was computed from.
    #[test]
    fn a_patch_against_the_shared_slot_is_refused_rather_than_rehomed() {
        assert!(matches!(
            staged_slot_target(true, "_slots/current-focus.md", Some("alice"), "patch"),
            StagedSlotTarget::Refused(_)
        ));
        // The operator's own slot needs no move, so patch mode is fine there.
        assert_eq!(
            staged_slot_target(
                true,
                "_slots/alice/current-focus.md",
                Some("alice"),
                "patch"
            ),
            StagedSlotTarget::AsGiven
        );
    }

    /// `Rehomed` is actionable only while the path is still being chosen; every
    /// later door has to be able to turn it into a refusal reason.
    #[test]
    fn only_as_given_has_no_refusal_reason() {
        assert_eq!(StagedSlotTarget::AsGiven.refusal(), None);
        assert!(
            StagedSlotTarget::Rehomed("_slots/alice/current-focus.md".into())
                .refusal()
                .is_some_and(|r| r.contains("_slots/alice/current-focus.md"))
        );
        assert_eq!(
            StagedSlotTarget::Refused("because".into()).refusal(),
            Some("because".to_string())
        );
    }

    #[test]
    fn named_slot_matches_shared_and_personal() {
        assert!(is_slot_named("_slots/current-focus.md", "current-focus.md"));
        assert!(is_slot_named(
            "_slots/alice/current-focus.md",
            "current-focus.md"
        ));
        assert!(!is_slot_named("_slots/alice/other.md", "current-focus.md"));
        assert!(!is_slot_named("notes/current-focus.md", "current-focus.md"));
    }
}
