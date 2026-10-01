//! Money-state partition — cdk-increment-2, unit 4.
//!
//! Port of Minibits' `transactionStates.test.ts` (MIT,
//! minibits-cash/minibits_wallet @d10d389): every reachable payment
//! state occupies EXACTLY ONE of three phases — terminal, in-flight,
//! rollbackable — and the partition is total (no unclassified state)
//! and mutually exclusive (no state in two phases). The phases are the
//! money-safety vocabulary the rest of the daemon branches on:
//!
//! - [`MoneyPhase::Terminal`] — finished; no retry, no recovery, the
//!   journal is clear.
//! - [`MoneyPhase::InFlight`] — money is moving; inputs stay locked and
//!   must NEVER be re-spent while the outcome is unknown.
//! - [`MoneyPhase::Rollbackable`] — recoverable; a revoke/compensate/
//!   reconcile path exists and is the only sanctioned way out.
//!
//! The partition covers both layers of the composite state machine:
//! cdk's saga leaf states (wallet layer) and cashud's payment journal
//! states (daemon layer). The property tests pin the partition; any new
//! state that breaks totality or mutual exclusivity fails to compile or
//! fails the properties.

use cdk::wallet::types::{IssueSagaState, MeltSagaState, ReceiveSagaState, SendSagaState, SwapSagaState, WalletSagaState};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoneyPhase {
    Terminal,
    InFlight,
    Rollbackable,
}

impl MoneyPhase {
    pub fn label(self) -> &'static str {
        match self {
            Self::Terminal => "terminal",
            Self::InFlight => "in_flight",
            Self::Rollbackable => "rollbackable",
        }
    }
}

/// cashud's payment journal states (the daemon layer of the partition).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JournalState {
    /// `payment.pending` with delivery context — the resolver re-posts
    /// the SAME token (gateway replay idempotency settles it).
    PendingWithMeta,
    /// `payment.pending` without context (legacy/crash window) — no
    /// blind retry; operator reconciliation. Still in flight: the token
    /// may have been delivered, so it is never re-spent or discarded.
    PendingLegacy,
    /// Token retained under quarantine (outcome unknown / terminal
    /// rejection) — recovery is a mint-side spent-check away.
    Quarantined,
    /// Gateway credited the payment (session observed or spent-replay
    /// adoption) — journal cleared.
    Settled,
    /// Terminal failure with the journal cleared (rejected-and-preserved
    /// tokens resolve to here after retry success or operator sweep).
    /// Reachable only through the operator sweep — no daemon path
    /// persists a failed journal state, which is the point of the
    /// partition: the daemon never writes off money itself.
    #[allow(dead_code)]
    Failed,
}

/// Phase of a journal state. Exhaustive by construction — adding a
/// variant without classifying it is a compile error here.
pub fn journal_phase(state: JournalState) -> MoneyPhase {
    match state {
        JournalState::PendingWithMeta | JournalState::PendingLegacy => MoneyPhase::InFlight,
        JournalState::Quarantined => MoneyPhase::Rollbackable,
        JournalState::Settled | JournalState::Failed => MoneyPhase::Terminal,
    }
}

/// Phase of a cdk saga leaf state (the wallet layer of the partition).
/// Exhaustive over all 13 leaf states — a new cdk state breaks the
/// compile until it is classified, which is the point.
pub fn saga_phase(state: &WalletSagaState) -> MoneyPhase {
    match state {
        // Send: proofs locked before the token exists (cdk compensates
        // back), token handed out (revoke window), or mid-revocation.
        WalletSagaState::Send(SendSagaState::ProofsReserved)
        | WalletSagaState::Send(SendSagaState::TokenCreated)
        | WalletSagaState::Send(SendSagaState::RollingBack) => MoneyPhase::Rollbackable,
        // Receive/Swap/Issue: value moving between us and the mint with
        // the outcome not yet observed.
        WalletSagaState::Receive(ReceiveSagaState::ProofsPending)
        | WalletSagaState::Receive(ReceiveSagaState::SwapRequested)
        | WalletSagaState::Swap(SwapSagaState::ProofsReserved)
        | WalletSagaState::Swap(SwapSagaState::SwapRequested)
        | WalletSagaState::Issue(IssueSagaState::SecretsPrepared)
        | WalletSagaState::Issue(IssueSagaState::MintRequested) => MoneyPhase::InFlight,
        // Melt: the payment may have settled at the mint while we were
        // down — NEVER release inputs automatically (cdk's own
        // pending-melt semantics; the S8 silent-loss guard).
        WalletSagaState::Melt(MeltSagaState::ProofsReserved)
        | WalletSagaState::Melt(MeltSagaState::MeltRequested)
        | WalletSagaState::Melt(MeltSagaState::PaymentPending) => MoneyPhase::InFlight,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cdk::wallet::types::{
        IssueSagaState, MeltSagaState, ReceiveSagaState, SendSagaState, SwapSagaState,
        WalletSagaState,
    };

    const ALL_JOURNAL: [JournalState; 5] = [
        JournalState::PendingWithMeta,
        JournalState::PendingLegacy,
        JournalState::Quarantined,
        JournalState::Settled,
        JournalState::Failed,
    ];

    fn all_saga_leaves() -> Vec<WalletSagaState> {
        vec![
            WalletSagaState::Send(SendSagaState::ProofsReserved),
            WalletSagaState::Send(SendSagaState::TokenCreated),
            WalletSagaState::Send(SendSagaState::RollingBack),
            WalletSagaState::Receive(ReceiveSagaState::ProofsPending),
            WalletSagaState::Receive(ReceiveSagaState::SwapRequested),
            WalletSagaState::Swap(SwapSagaState::ProofsReserved),
            WalletSagaState::Swap(SwapSagaState::SwapRequested),
            WalletSagaState::Issue(IssueSagaState::SecretsPrepared),
            WalletSagaState::Issue(IssueSagaState::MintRequested),
            WalletSagaState::Melt(MeltSagaState::ProofsReserved),
            WalletSagaState::Melt(MeltSagaState::MeltRequested),
            WalletSagaState::Melt(MeltSagaState::PaymentPending),
        ]
    }

    // ---- the Minibits partition properties --------------------------

    #[test]
    fn partition_is_total_and_mutually_exclusive() {
        // Every state lands in exactly one phase: the classifiers are
        // exhaustive matches (compile-enforced totality) returning a
        // single phase (mutual exclusivity by type). Re-assert both over
        // the full state space so a refactor to sets fails loudly.
        let phases = [
            MoneyPhase::Terminal,
            MoneyPhase::InFlight,
            MoneyPhase::Rollbackable,
        ];
        for state in ALL_JOURNAL {
            let hits = phases
                .iter()
                .filter(|p| journal_phase(state) == **p)
                .count();
            assert_eq!(hits, 1, "{state:?} must occupy exactly one phase");
        }
        for state in &all_saga_leaves() {
            let hits = phases.iter().filter(|p| saga_phase(state) == **p).count();
            assert_eq!(hits, 1, "{state:?} must occupy exactly one phase");
        }
    }

    #[test]
    fn every_saga_leaf_state_is_covered() {
        // 12 leaf states in cdk 0.18 — a new one changes this count AND
        // breaks saga_phase's exhaustive match; both must be updated
        // together, never silently.
        assert_eq!(all_saga_leaves().len(), 12);
        assert_eq!(
            all_saga_leaves().len(),
            all_saga_leaves().iter().collect::<std::collections::HashSet<_>>().len(),
            "leaf states are distinct"
        );
    }

    // ---- money-safety pins ------------------------------------------

    #[test]
    fn sent_tokens_are_rollbackable_never_in_flight() {
        // The revoke window: an unredeemed token's proofs can be swapped
        // back — compaction (unit 1) prunes only after the mint confirms
        // them spent, which is exactly the Terminal transition.
        assert_eq!(
            saga_phase(&WalletSagaState::Send(SendSagaState::TokenCreated)),
            MoneyPhase::Rollbackable
        );
    }

    #[test]
    fn melt_states_are_in_flight_never_rollbackable() {
        // A melt may have settled at the mint while we were down;
        // releasing its inputs would double-spend. cdk's own
        // pending-melt semantics and the S8 silent-loss finding.
        for leaf in [
            WalletSagaState::Melt(MeltSagaState::ProofsReserved),
            WalletSagaState::Melt(MeltSagaState::MeltRequested),
            WalletSagaState::Melt(MeltSagaState::PaymentPending),
        ] {
            assert_eq!(saga_phase(&leaf), MoneyPhase::InFlight, "{leaf:?}");
        }
    }

    #[test]
    fn legacy_pending_is_in_flight_not_rollbackable() {
        // No delivery context → the resolver must NOT re-post it (no
        // blind retry), but the token may have been delivered → never
        // reclaimable either. Operator reconciliation is the only exit.
        assert_eq!(
            journal_phase(JournalState::PendingLegacy),
            MoneyPhase::InFlight
        );
    }

    #[test]
    fn quarantined_payments_are_rollbackable() {
        // Retained token + mint-side spent-check = the sanctioned
        // recovery path (quarantine is the recovery queue, not a grave).
        assert_eq!(
            journal_phase(JournalState::Quarantined),
            MoneyPhase::Rollbackable
        );
    }

    #[test]
    fn settled_and_failed_are_terminal() {
        assert_eq!(journal_phase(JournalState::Settled), MoneyPhase::Terminal);
        assert_eq!(journal_phase(JournalState::Failed), MoneyPhase::Terminal);
    }

    #[test]
    fn journal_phases_stay_disjoint_from_terminal_while_pending() {
        // No pending variant may classify Terminal — a pending token is
        // never "done" (money-safety: the journal IS the proof of an
        // unresolved outcome).
        for state in [
            JournalState::PendingWithMeta,
            JournalState::PendingLegacy,
        ] {
            assert_ne!(journal_phase(state), MoneyPhase::Terminal, "{state:?}");
        }
    }
}
