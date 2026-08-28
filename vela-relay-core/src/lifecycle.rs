//! The single authoritative UserOperation lifecycle state machine.
//!
//! Every durable status write flows through the decisions here. The shell's
//! Redis scripts perform only mechanical guarded writes: they verify that the
//! stored state still equals the state a decision was computed against and
//! apply the merge; whether a transition is legal is decided in this module,
//! nowhere else.
//!
//! Behavior is lifted 1:1 from the pre-split Lua tables
//! (`PATCH_RECORD_SCRIPT`, `MARK_BUNDLE_SUBMITTED_SCRIPT`); see
//! `specs/001-crux-core-split/data-model.md` §2.

use crate::task::UserOperationStatus;

/// The transition table. Same-status writes are always legal field merges;
/// terminal states (and the API-only `NotFound`) have no outgoing transitions.
pub fn transition_is_allowed(current: UserOperationStatus, next: UserOperationStatus) -> bool {
    use UserOperationStatus::{Failed, Included, NotSubmitted, Queued, Rejected, Submitted};

    current == next
        || matches!(
            (current, next),
            (Queued, NotSubmitted | Submitted | Rejected | Failed)
                | (NotSubmitted, Submitted | Rejected | Failed)
                | (Submitted, Included | Rejected | Failed)
        )
}

/// Outcome of judging one status patch against the table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PatchDecision {
    /// The patch may be merged into the record (guarded on `current`).
    Apply,
    /// The requested transition is illegal; the record must stay untouched.
    RefuseIllegalTransition,
}

/// Judge a record patch. `requested` is the patch's `status` field, if any; a
/// patch without a status change (or restating the current status) is always a
/// legal field merge.
pub fn decide_patch(
    current: UserOperationStatus,
    requested: Option<UserOperationStatus>,
) -> PatchDecision {
    match requested {
        Some(next) if !transition_is_allowed(current, next) => {
            PatchDecision::RefuseIllegalTransition
        }
        _ => PatchDecision::Apply,
    }
}

/// Outcome of judging one bundle member when a handleOps transaction is
/// submitted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BundleSubmissionDecision {
    /// `queued`/`not_submitted` on the right chain: becomes `submitted` with
    /// the bundle's transaction hash and `admitted = true`, and joins the
    /// bundle index.
    Transition,
    /// Already `submitted` with the same transaction hash: joins the bundle
    /// index again without mutation (idempotent producer retry).
    IndexOnly,
    /// Wrong chain, terminal, or submitted under a different transaction:
    /// left untouched and unindexed.
    Skip,
}

/// Lua's `%.14g` `tostring` renders integers with at most 14 significant
/// digits exactly; beyond that the pre-split script deliberately failed
/// closed instead of aliasing chains. Preserved so legacy records (no
/// `chainIdText`) keep refusing outside that range.
const LEGACY_CHAIN_ID_EXACT_LIMIT: u64 = 100_000_000_000_000;

/// Judge one stored record against a submitted bundle. `chain_id_text` is the
/// record's decimal-text chain id (empty for legacy records, which fall back
/// to the numeric field within Lua's canonical-render range, fail-closed
/// beyond it).
pub fn decide_bundle_submission(
    status: UserOperationStatus,
    record_transaction_hash: Option<&str>,
    record_chain_id: u64,
    record_chain_id_text: &str,
    bundle_chain_id: u64,
    bundle_transaction_hash: &str,
) -> BundleSubmissionDecision {
    let same_chain = if record_chain_id_text.is_empty() {
        record_chain_id == bundle_chain_id && record_chain_id < LEGACY_CHAIN_ID_EXACT_LIMIT
    } else {
        record_chain_id_text == bundle_chain_id.to_string()
    };
    if !same_chain {
        return BundleSubmissionDecision::Skip;
    }
    match status {
        UserOperationStatus::Queued | UserOperationStatus::NotSubmitted => {
            BundleSubmissionDecision::Transition
        }
        UserOperationStatus::Submitted
            if record_transaction_hash == Some(bundle_transaction_hash) =>
        {
            BundleSubmissionDecision::IndexOnly
        }
        _ => BundleSubmissionDecision::Skip,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BundleSubmissionDecision, PatchDecision, decide_bundle_submission, decide_patch,
        transition_is_allowed,
    };
    use crate::task::UserOperationStatus::{
        self, Failed, Included, NotFound, NotSubmitted, Queued, Rejected, Submitted,
    };

    const EVERY_STATUS: [UserOperationStatus; 7] = [
        NotFound,
        Queued,
        NotSubmitted,
        Submitted,
        Rejected,
        Included,
        Failed,
    ];

    #[test]
    fn status_transition_matrix_is_monotonic() {
        assert!(transition_is_allowed(Queued, NotSubmitted));
        assert!(transition_is_allowed(Queued, Submitted));
        assert!(transition_is_allowed(Queued, Rejected));
        assert!(transition_is_allowed(Queued, Failed));
        assert!(transition_is_allowed(NotSubmitted, Submitted));
        assert!(transition_is_allowed(NotSubmitted, Rejected));
        assert!(transition_is_allowed(NotSubmitted, Failed));
        assert!(transition_is_allowed(Submitted, Included));
        assert!(transition_is_allowed(Submitted, Rejected));
        assert!(transition_is_allowed(Submitted, Failed));

        for terminal in [Rejected, Included, Failed] {
            for next in EVERY_STATUS {
                assert_eq!(
                    transition_is_allowed(terminal, next),
                    terminal == next,
                    "terminal {terminal:?} must not transition to {next:?}"
                );
            }
        }
        assert!(!transition_is_allowed(Submitted, Queued));
        assert!(!transition_is_allowed(NotSubmitted, Queued));
        assert!(!transition_is_allowed(Queued, Included));
        assert!(!transition_is_allowed(NotFound, Queued));
    }

    #[test]
    fn same_status_patches_are_always_field_merges() {
        for status in EVERY_STATUS {
            assert!(transition_is_allowed(status, status));
            assert_eq!(decide_patch(status, Some(status)), PatchDecision::Apply);
            assert_eq!(decide_patch(status, None), PatchDecision::Apply);
        }
    }

    #[test]
    fn illegal_patches_are_refused() {
        for terminal in [Rejected, Included, Failed] {
            for next in EVERY_STATUS {
                if next != terminal {
                    assert_eq!(
                        decide_patch(terminal, Some(next)),
                        PatchDecision::RefuseIllegalTransition
                    );
                }
            }
        }
        assert_eq!(
            decide_patch(Submitted, Some(Queued)),
            PatchDecision::RefuseIllegalTransition
        );
    }

    #[test]
    fn terminal_and_durable_predicates_stay_distinct() {
        for status in EVERY_STATUS {
            assert_eq!(
                status.is_terminal(),
                matches!(status, Rejected | Included | Failed)
            );
            assert_eq!(
                status.is_durable(),
                matches!(status, Submitted | Rejected | Included | Failed)
            );
        }
    }

    #[test]
    fn bundle_submission_transitions_only_pre_submission_members_on_the_same_chain() {
        for status in [Queued, NotSubmitted] {
            assert_eq!(
                decide_bundle_submission(status, None, 42161, "42161", 42161, "0xbundle"),
                BundleSubmissionDecision::Transition
            );
        }
        assert_eq!(
            decide_bundle_submission(
                Submitted,
                Some("0xbundle"),
                42161,
                "42161",
                42161,
                "0xbundle"
            ),
            BundleSubmissionDecision::IndexOnly
        );
        assert_eq!(
            decide_bundle_submission(
                Submitted,
                Some("0xother"),
                42161,
                "42161",
                42161,
                "0xbundle"
            ),
            BundleSubmissionDecision::Skip
        );
        for status in [Rejected, Included, Failed, NotFound] {
            assert_eq!(
                decide_bundle_submission(
                    status,
                    Some("0xbundle"),
                    42161,
                    "42161",
                    42161,
                    "0xbundle"
                ),
                BundleSubmissionDecision::Skip
            );
        }
    }

    #[test]
    fn bundle_submission_chain_comparison_uses_decimal_text_with_fail_closed_legacy_fallback() {
        assert_eq!(
            decide_bundle_submission(Queued, None, 42161, "42161", 1, "0xbundle"),
            BundleSubmissionDecision::Skip,
            "text chain mismatch must skip"
        );
        assert_eq!(
            decide_bundle_submission(Queued, None, 42161, "", 42161, "0xbundle"),
            BundleSubmissionDecision::Transition,
            "legacy numeric fallback matches exact small chain ids"
        );
        assert_eq!(
            decide_bundle_submission(Queued, None, 42161, "", 1, "0xbundle"),
            BundleSubmissionDecision::Skip
        );
        let beyond_canonical = 100_000_000_000_000;
        assert_eq!(
            decide_bundle_submission(
                Queued,
                None,
                beyond_canonical,
                "",
                beyond_canonical,
                "0xbundle"
            ),
            BundleSubmissionDecision::Skip,
            "legacy records beyond Lua's canonical render range fail closed"
        );
        assert_eq!(
            decide_bundle_submission(
                Queued,
                None,
                beyond_canonical,
                "100000000000000",
                beyond_canonical,
                "0xbundle"
            ),
            BundleSubmissionDecision::Transition,
            "text-carrying records are exact at any magnitude"
        );
    }
}
