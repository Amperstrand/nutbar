//! Saga/operation compaction — cdk-increment-2, unit 1.
//!
//! cdk 0.18's design: a `wallet_sagas` row exists only while an operation
//! is in flight; completion deletes the row. A `Send/TokenCreated` row
//! deliberately outlives `confirm()` — it keeps the revoke window open
//! until the recipient redeems the proofs at the mint. Two gaps make old
//! wallets drag (S7 evidence: the 4321-saga wallet booted cashud in
//! over 10 min, ~3s per saga, because boot-time recovery re-resumes every
//! row sequentially with a network round-trip each):
//!
//! 1. Nothing in a *running* wallet finalizes send sagas whose proofs the
//!    recipient has already redeemed — every `/send`, stash token and
//!    renewal leaves a row that only the next boot can drain.
//! 2. Rows whose tokens are never redeemed are re-checked at every boot,
//!    forever.
//!
//! This module is the missing drain: a periodic pass that mirrors cdk's
//! own `recover_or_complete_send` spent-branch semantics exactly — one
//! batched NUT-07 check per mint, proofs kept as `Spent` (local replay
//! detection), transaction flipped `Pending → Completed` (terminal states
//! respected), saga row deleted. Unspent rows are NEVER touched: the
//! revoke window is money. All state transitions go through cdk's own
//! `WalletDatabase` trait — no hand-rolled SQL.
//!
//! Unit tests drive the pass through a local fake mint connector that
//! answers NUT-07 check-state (the harness shape of cdk's crate-private
//! `MockMintConnector`); the wire path itself is locked by the
//! `s_saga_compaction` E2E scenario against the real fake mint.

use cdk::nuts::{CheckStateRequest, State};
use cdk::wallet::types::{
    SendSagaState, TransactionId, TransactionStatus, WalletSaga, WalletSagaState,
};
use cdk::wallet::Wallet;
use tracing::warn;
use uuid::Uuid;

/// Upper bound on send sagas examined per wallet per pass — bounds both
/// the DB work and the NUT-07 payload (one batched request per pass).
pub const COMPACTION_BATCH: usize = 256;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PassOutcome {
    /// Saga rows remaining in the repository after the pass.
    pub open_sagas: usize,
    /// Completed send operations pruned this pass.
    pub compacted: usize,
    /// Send sagas kept because their proofs are still unspent (revoke
    /// window preserved) or the mint could not answer.
    pub retained: usize,
    /// Open rows in the in-flight phase of the money-state partition
    /// (melt/receive/swap/issue — recovery territory, never compacted).
    pub in_flight_open: usize,
    /// Open rows in the rollbackable phase (all send leaf states).
    pub rollbackable_open: usize,
}

/// One money-safe compaction pass across the given wallets (one pass per
/// mint wallet; all share the same localstore in the repository layout).
///
/// Only `Send/TokenCreated` sagas for the wallet's own mint+unit are
/// candidates. A saga is pruned when the mint confirms ALL its reserved
/// proofs spent (the recipient redeemed the token — the operation is
/// definitively complete) or when it has no reserved proofs at all
/// (orphaned row, nothing at stake — mirrors cdk's recovery cleanup).
/// Everything else is retained untouched.
pub async fn compaction_pass(wallets: &[Wallet], batch: usize) -> PassOutcome {
    let mut outcome = PassOutcome::default();
    let mut remaining: usize = 0;

    for wallet in wallets {
        let sagas = match wallet.localstore.get_incomplete_sagas().await {
            Ok(sagas) => sagas,
            Err(e) => {
                warn!(mint = %wallet.mint_url, error = %e, "compaction: could not read sagas");
                continue;
            }
        };
        remaining += sagas.len();
        for saga in &sagas {
            if saga.mint_url == wallet.mint_url && saga.unit == wallet.unit {
                match crate::state::saga_phase(&saga.state) {
                    crate::state::MoneyPhase::InFlight => outcome.in_flight_open += 1,
                    crate::state::MoneyPhase::Rollbackable => outcome.rollbackable_open += 1,
                    crate::state::MoneyPhase::Terminal => {
                        // cdk's design deletes terminal rows; a Terminal
                        // classification here means residue the pass below
                        // or recovery drains.
                    }
                }
            }
        }

        let candidates: Vec<WalletSaga> = sagas
            .into_iter()
            .filter(|s| s.mint_url == wallet.mint_url && s.unit == wallet.unit)
            .filter(|s| matches!(s.state, WalletSagaState::Send(SendSagaState::TokenCreated)))
            .take(batch)
            .collect();
        if candidates.is_empty() {
            continue;
        }

        // Split orphans (no reserved proofs — nothing at stake, no network
        // needed) from live candidates.
        let mut with_proofs: Vec<(WalletSaga, Vec<cdk::wallet::types::ProofInfo>)> = Vec::new();
        let mut orphans: Vec<WalletSaga> = Vec::new();
        for saga in candidates {
            match wallet.localstore.get_reserved_proofs(&saga.id).await {
                Ok(proofs) if proofs.is_empty() => orphans.push(saga),
                Ok(proofs) => with_proofs.push((saga, proofs)),
                Err(e) => {
                    warn!(saga_id = %saga.id, error = %e, "compaction: reserved-proof lookup failed");
                }
            }
        }

        for saga in orphans {
            if prune_saga(wallet, &saga.id, TransactionStatus::Completed).await {
                outcome.compacted += 1;
                remaining -= 1;
            }
        }

        if with_proofs.is_empty() {
            continue;
        }

        // One batched NUT-07 check for every candidate y of this mint.
        let ys: Vec<_> = with_proofs
            .iter()
            .flat_map(|(_, proofs)| proofs.iter().map(|p| p.y))
            .collect();
        match wallet
            .mint_connector()
            .post_check_state(CheckStateRequest { ys })
            .await
        {
            Ok(response) => {
                let spent: std::collections::HashSet<_> = response
                    .states
                    .into_iter()
                    .filter(|s| s.state == State::Spent)
                    .map(|s| s.y)
                    .collect();
                for (saga, proofs) in with_proofs {
                    let saga_ys: Vec<_> = proofs.iter().map(|p| p.y).collect();
                    if saga_ys.iter().all(|y| spent.contains(y)) {
                        // Mirror of cdk's `recover_or_complete_send` spent
                        // branch: proofs stay recorded as Spent (replay
                        // detection), transaction completes, row goes away.
                        if let Err(e) = wallet
                            .localstore
                            .update_proofs_state(saga_ys, State::Spent)
                            .await
                        {
                            warn!(saga_id = %saga.id, error = %e, "compaction: marking spent proofs failed");
                            outcome.retained += 1;
                            continue;
                        }
                        if prune_saga(wallet, &saga.id, TransactionStatus::Completed).await {
                            outcome.compacted += 1;
                            remaining -= 1;
                        } else {
                            outcome.retained += 1;
                        }
                    } else {
                        outcome.retained += 1;
                    }
                }
            }
            Err(e) => {
                // Mint unreachable: retain everything this pass — the revoke
                // window must survive a dead mint (S10 lesson).
                tracing::debug!(
                    mint = %wallet.mint_url,
                    error = %e,
                    "compaction: mint unreachable, retaining candidates this pass"
                );
                outcome.retained += with_proofs.len();
            }
        }
    }

    outcome.open_sagas = remaining;
    outcome
}

/// Delete a saga row and complete its transaction. Returns success only
/// when both steps landed (a row whose transaction update failed is kept
/// so the history stays honest).
async fn prune_saga(wallet: &Wallet, saga_id: &Uuid, status: TransactionStatus) -> bool {
    if let Err(e) = complete_saga_transaction(wallet, *saga_id, status).await {
        warn!(saga_id = %saga_id, error = %e, "compaction: transaction completion failed");
        return false;
    }
    if let Err(e) = wallet.localstore.delete_saga(saga_id).await {
        warn!(saga_id = %saga_id, error = %e, "compaction: saga delete failed");
        return false;
    }
    true
}

/// Port of cdk's `pub(crate) update_transaction_status_by_saga_id`: resolve
/// the transaction by `TransactionId::from_saga_id` with a saga_id scan
/// fallback, then flip `Pending → status` only. Terminal states are never
/// overwritten; the original timestamp is preserved by writing back the
/// read row.
async fn complete_saga_transaction(
    wallet: &Wallet,
    saga_id: Uuid,
    status: TransactionStatus,
) -> anyhow::Result<()> {
    let mut transactions = Vec::new();
    match wallet
        .localstore
        .get_transaction(TransactionId::from_saga_id(saga_id))
        .await?
    {
        Some(transaction) => transactions.push(transaction),
        None => {
            transactions = wallet
                .localstore
                .list_transactions(
                    Some(wallet.mint_url.clone()),
                    None,
                    Some(wallet.unit.clone()),
                )
                .await?
                .into_iter()
                .filter(|t| t.saga_id == Some(saga_id))
                .collect();
        }
    }
    for mut transaction in transactions {
        if transaction.status == status {
            continue;
        }
        if transaction.status != TransactionStatus::Pending {
            warn!(
                saga_id = %saga_id,
                current = ?transaction.status,
                requested = ?status,
                "compaction: refusing to overwrite terminal transaction status"
            );
            continue;
        }
        transaction.status = status;
        wallet.localstore.add_transaction(transaction).await?;
    }
    Ok(())
}


#[cfg(test)]
mod tests {
    use super::*;
    use cdk::mint_url::MintUrl;
    use cdk::nuts::{CheckStateResponse, CurrencyUnit, Id, ProofState, PublicKey, State};
    use cdk::secret::Secret;
    use cdk::wallet::types::{
        OperationData, ReceiveOperationData, SendOperationData, Transaction,
        TransactionDirection,
    };
    use cdk::wallet::{MintConnector, WalletBuilder};
    use cdk::Amount;
    use std::collections::HashMap;
    use std::str::FromStr;
    use std::sync::{Arc, Mutex};

    const KEYSET_ID: &str = "0094d5a774c40a32";

    /// A fake mint answering NUT-07 check-state from a settable response
    /// and refusing every other endpoint — the harness shape of cdk's
    /// crate-private `MockMintConnector`, rebuilt locally because upstream
    /// gates `test_utils` behind `#![cfg(test)]`.
    #[derive(Debug, Default)]
    struct FakeCheckMint {
        check_state: Mutex<Option<Result<Vec<ProofState>, String>>>,
    }

    impl FakeCheckMint {
        fn set_check_state(&self, response: Result<CheckStateResponse, cdk::Error>) {
            let stored = match response {
                Ok(response) => Ok(response.states),
                Err(e) => Err(e.to_string()),
            };
            *self.check_state.lock().unwrap() = Some(stored);
        }
    }

    #[async_trait::async_trait]
    impl MintConnector for FakeCheckMint {
        async fn post_check_state(
            &self,
            _request: CheckStateRequest,
        ) -> Result<CheckStateResponse, cdk::Error> {
            match self.check_state.lock().unwrap().clone() {
                Some(Ok(states)) => Ok(CheckStateResponse { states }),
                Some(Err(message)) => Err(cdk::Error::Custom(message)),
                None => Err(cdk::Error::Custom(
                    "fake mint: no check-state response staged".to_string(),
                )),
            }
        }
        async fn get_mint_keys(&self) -> Result<Vec<cdk::nuts::KeySet>, cdk::Error> {
            unimplemented!("fake mint: only check-state is staged")
        }
        async fn get_mint_keyset(&self, _keyset_id: Id) -> Result<cdk::nuts::KeySet, cdk::Error> {
            unimplemented!("fake mint: only check-state is staged")
        }
        async fn get_mint_keysets(&self) -> Result<cdk::nuts::KeysetResponse, cdk::Error> {
            unimplemented!("fake mint: only check-state is staged")
        }
        async fn post_mint_quote(
            &self,
            _request: cdk::MintQuoteRequest,
        ) -> Result<cdk::MintQuoteResponse<String>, cdk::Error> {
            unimplemented!("fake mint: only check-state is staged")
        }
        async fn post_mint(
            &self,
            _method: &cdk::nuts::PaymentMethod,
            _request: cdk::nuts::MintRequest<String>,
        ) -> Result<cdk::nuts::MintResponse, cdk::Error> {
            unimplemented!("fake mint: only check-state is staged")
        }
        async fn post_batch_check_mint_quote_status(
            &self,
            _method: &cdk::nuts::PaymentMethod,
            _request: cdk::nuts::BatchCheckMintQuoteRequest<String>,
        ) -> Result<Vec<cdk::MintQuoteResponse<String>>, cdk::Error> {
            unimplemented!("fake mint: only check-state is staged")
        }
        async fn post_batch_mint(
            &self,
            _method: &cdk::nuts::PaymentMethod,
            _request: cdk::nuts::BatchMintRequest<String>,
        ) -> Result<cdk::nuts::MintResponse, cdk::Error> {
            unimplemented!("fake mint: only check-state is staged")
        }
        async fn post_melt_quote(
            &self,
            _request: cdk::MeltQuoteRequest,
        ) -> Result<cdk::MeltQuoteCreateResponse<String>, cdk::Error> {
            unimplemented!("fake mint: only check-state is staged")
        }
        async fn get_mint_quote_status(
            &self,
            _method: cdk::nuts::PaymentMethod,
            _quote_id: &str,
        ) -> Result<cdk::MintQuoteResponse<String>, cdk::Error> {
            unimplemented!("fake mint: only check-state is staged")
        }
        async fn get_melt_quote_status(
            &self,
            _method: cdk::nuts::PaymentMethod,
            _quote_id: &str,
        ) -> Result<cdk::MeltQuoteResponse<String>, cdk::Error> {
            unimplemented!("fake mint: only check-state is staged")
        }
        async fn post_melt(
            &self,
            _method: &cdk::nuts::PaymentMethod,
            _request: cdk::nuts::MeltRequest<String>,
        ) -> Result<cdk::MeltQuoteResponse<String>, cdk::Error> {
            unimplemented!("fake mint: only check-state is staged")
        }
        async fn post_swap(
            &self,
            _request: cdk::nuts::SwapRequest,
        ) -> Result<cdk::nuts::SwapResponse, cdk::Error> {
            unimplemented!("fake mint: only check-state is staged")
        }
        async fn get_mint_info(&self) -> Result<cdk::nuts::MintInfo, cdk::Error> {
            unimplemented!("fake mint: only check-state is staged")
        }
        async fn post_restore(
            &self,
            _request: cdk::nuts::RestoreRequest,
        ) -> Result<cdk::nuts::RestoreResponse, cdk::Error> {
            unimplemented!("fake mint: only check-state is staged")
        }
        async fn get_auth_wallet(&self) -> Option<cdk::wallet::AuthWallet> {
            None
        }
        async fn set_auth_wallet(&self, _wallet: Option<cdk::wallet::AuthWallet>) {}
        async fn fetch_lnurl_pay_request(
            &self,
            _url: &str,
        ) -> Result<cdk::wallet::LnurlPayResponse, cdk::Error> {
            unimplemented!("fake mint: only check-state is staged")
        }
        async fn fetch_lnurl_invoice(
            &self,
            _url: &str,
        ) -> Result<cdk::wallet::LnurlPayInvoiceResponse, cdk::Error> {
            unimplemented!("fake mint: only check-state is staged")
        }
    }

    // ---- fixtures ---------------------------------------------------------

    fn mint_url() -> MintUrl {
        MintUrl::from_str("https://test-mint.example.com").unwrap()
    }

    fn fixture_proof(amount: u64) -> cdk::nuts::Proof {
        cdk::nuts::Proof {
            amount: Amount::from(amount),
            keyset_id: Id::from_str(KEYSET_ID).unwrap(),
            secret: Secret::generate(),
            // Any valid curve point works — the compaction path never
            // verifies signatures.
            c: cdk::dhke::hash_to_curve(b"compact-fixture").unwrap(),
            witness: None,
            dleq: None,
            p2pk_e: None,
        }
    }

    fn fixture_proof_info(amount: u64) -> cdk::wallet::types::ProofInfo {
        cdk::wallet::types::ProofInfo::new(
            fixture_proof(amount),
            mint_url(),
            State::Unspent,
            CurrencyUnit::Sat,
        )
        .unwrap()
    }

    fn send_saga(id: Uuid, amount: u64) -> WalletSaga {
        WalletSaga::new(
            id,
            WalletSagaState::Send(SendSagaState::TokenCreated),
            Amount::from(amount),
            mint_url(),
            CurrencyUnit::Sat,
            OperationData::Send(SendOperationData {
                amount: Amount::from(amount),
                memo: None,
                counter_start: None,
                counter_end: None,
                token: None,
                proofs: None,
            }),
        )
    }

    fn other_saga(id: Uuid) -> WalletSaga {
        WalletSaga::new(
            id,
            WalletSagaState::Receive(cdk::wallet::types::ReceiveSagaState::ProofsPending),
            Amount::from(2u64),
            mint_url(),
            CurrencyUnit::Sat,
            OperationData::Receive(ReceiveOperationData {
                token: Some("token".to_string()),
                counter_start: None,
                counter_end: None,
                amount: Some(Amount::from(2u64)),
                blinded_messages: None,
            }),
        )
    }

    async fn new_wallet() -> (Wallet, Arc<FakeCheckMint>) {
        let db = cdk_sqlite::wallet::memory::empty().await.unwrap();
        let connector = Arc::new(FakeCheckMint::default());
        let wallet = WalletBuilder::new()
            .mint_url(mint_url())
            .unit(CurrencyUnit::Sat)
            .localstore(Arc::new(db))
            .seed([7u8; 64])
            .shared_client(connector.clone())
            .build()
            .unwrap();
        (wallet, connector)
    }

    /// Insert a send saga with `n` reserved proofs bound to it, plus a
    /// Pending outgoing transaction. Returns the reserved proofs' ys.
    async fn seed_send_saga(wallet: &Wallet, saga_id: Uuid, n_proofs: usize) -> Vec<PublicKey> {
        wallet
            .localstore
            .add_saga(send_saga(saga_id, n_proofs as u64))
            .await
            .unwrap();

        let mut ys = Vec::new();
        let mut proofs = Vec::new();
        for _ in 0..n_proofs {
            let info = fixture_proof_info(1);
            ys.push(info.y);
            proofs.push(info);
        }
        wallet
            .localstore
            .update_proofs(proofs, vec![])
            .await
            .unwrap();
        wallet
            .localstore
            .reserve_proofs(ys.clone(), &saga_id)
            .await
            .unwrap();

        let transaction = Transaction {
            mint_url: mint_url(),
            direction: TransactionDirection::Outgoing,
            amount: Amount::from(n_proofs as u64),
            fee: Amount::ZERO,
            unit: CurrencyUnit::Sat,
            ys: ys.clone(),
            timestamp: 1_700_000_000,
            memo: None,
            metadata: HashMap::new(),
            quote_id: None,
            payment_request: None,
            payment_proof: None,
            payment_method: None,
            saga_id: Some(saga_id),
            status: TransactionStatus::Pending,
        };
        wallet.localstore.add_transaction(transaction).await.unwrap();
        ys
    }

    fn spent_response(ys: &[PublicKey]) -> CheckStateResponse {
        CheckStateResponse {
            states: ys
                .iter()
                .map(|y| ProofState {
                    y: *y,
                    state: State::Spent,
                    witness: None,
                })
                .collect(),
        }
    }

    fn unspent_response(ys: &[PublicKey]) -> CheckStateResponse {
        CheckStateResponse {
            states: ys
                .iter()
                .map(|y| ProofState {
                    y: *y,
                    state: State::Unspent,
                    witness: None,
                })
                .collect(),
        }
    }

    // ---- the fake-mint matrix (adapted from cdk's recovery tests) -------

    #[tokio::test]
    async fn spent_send_saga_is_compacted() {
        let (wallet, mint) = new_wallet().await;
        let saga_id = Uuid::new_v4();
        let ys = seed_send_saga(&wallet, saga_id, 2).await;
        mint.set_check_state(Ok(spent_response(&ys)));

        let outcome = compaction_pass(std::slice::from_ref(&wallet), COMPACTION_BATCH).await;
        assert_eq!(outcome.compacted, 1, "the redeemed send is pruned: {outcome:?}");
        assert_eq!(outcome.retained, 0);
        assert_eq!(outcome.open_sagas, 0, "row deleted: {outcome:?}");

        // Proofs are kept as Spent (replay detection), not removed.
        let spent = wallet
            .localstore
            .get_proofs(
                Some(mint_url()),
                Some(CurrencyUnit::Sat),
                Some(vec![State::Spent]),
                None,
            )
            .await
            .unwrap();
        assert_eq!(spent.len(), 2, "spent proofs stay recorded as Spent");
        // Balance no longer counts them.
        let balance: u64 = wallet.total_balance().await.unwrap().into();
        assert_eq!(balance, 0, "compacted proofs cannot be double-spent");
        // Transaction completed.
        let tx = wallet
            .localstore
            .get_transaction(TransactionId::from_saga_id(saga_id))
            .await
            .unwrap()
            .expect("transaction kept");
        assert_eq!(tx.status, TransactionStatus::Completed);
        assert_eq!(tx.timestamp, 1_700_000_000, "original timestamp preserved");
    }

    #[tokio::test]
    async fn unspent_send_saga_is_retained() {
        let (wallet, mint) = new_wallet().await;
        let saga_id = Uuid::new_v4();
        let ys = seed_send_saga(&wallet, saga_id, 1).await;
        mint.set_check_state(Ok(unspent_response(&ys)));

        let outcome = compaction_pass(std::slice::from_ref(&wallet), COMPACTION_BATCH).await;
        assert_eq!(outcome.compacted, 0, "revoke window is money: {outcome:?}");
        assert_eq!(outcome.retained, 1);
        assert_eq!(outcome.open_sagas, 1, "row kept");

        let saga = wallet.localstore.get_saga(&saga_id).await.unwrap();
        assert!(saga.is_some(), "unspent send saga row survives");
        let tx = wallet
            .localstore
            .get_transaction(TransactionId::from_saga_id(saga_id))
            .await
            .unwrap()
            .expect("transaction kept");
        assert_eq!(
            tx.status,
            TransactionStatus::Pending,
            "not completed while redeemable"
        );
    }

    #[tokio::test]
    async fn receive_and_swap_sagas_are_never_touched() {
        // Even an all-spent answer must not touch non-send rows.
        let (wallet, mint) = new_wallet().await;
        let send_id = Uuid::new_v4();
        let ys = seed_send_saga(&wallet, send_id, 1).await;
        let receive_id = Uuid::new_v4();
        wallet.localstore.add_saga(other_saga(receive_id)).await.unwrap();
        mint.set_check_state(Ok(spent_response(&ys)));

        let outcome = compaction_pass(std::slice::from_ref(&wallet), COMPACTION_BATCH).await;
        assert_eq!(outcome.compacted, 1, "only the send saga is pruned");
        assert_eq!(outcome.open_sagas, 1, "the receive saga remains open");
        assert!(
            wallet.localstore.get_saga(&receive_id).await.unwrap().is_some(),
            "receive saga untouched"
        );
    }

    #[tokio::test]
    async fn orphaned_send_saga_without_proofs_is_cleaned_without_the_mint() {
        // No check-state response is staged on the fake mint: reaching out
        // for an orphan would fail this test loudly.
        let (wallet, _mint) = new_wallet().await;
        let saga_id = Uuid::new_v4();
        wallet.localstore.add_saga(send_saga(saga_id, 3)).await.unwrap();

        let outcome = compaction_pass(std::slice::from_ref(&wallet), COMPACTION_BATCH).await;
        assert_eq!(
            outcome.compacted, 1,
            "orphaned rows have nothing at stake: {outcome:?}"
        );
        assert_eq!(outcome.open_sagas, 0);
        assert!(wallet.localstore.get_saga(&saga_id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn mixed_batch_compacts_only_fully_spent_sagas() {
        let (wallet, mint) = new_wallet().await;
        let spent_id = Uuid::new_v4();
        let spent_ys = seed_send_saga(&wallet, spent_id, 2).await;
        let half_id = Uuid::new_v4();
        let half_ys = seed_send_saga(&wallet, half_id, 2).await;
        // The mint redeemed only one of the second saga's proofs.
        let mut states = spent_response(&spent_ys).states;
        states.push(ProofState {
            y: half_ys[0],
            state: State::Spent,
            witness: None,
        });
        states.push(ProofState {
            y: half_ys[1],
            state: State::Unspent,
            witness: None,
        });
        mint.set_check_state(Ok(CheckStateResponse { states }));

        let outcome = compaction_pass(std::slice::from_ref(&wallet), COMPACTION_BATCH).await;
        assert_eq!(
            outcome.compacted, 1,
            "only the fully-spent saga goes: {outcome:?}"
        );
        assert_eq!(outcome.retained, 1, "partially spent = still money in flight");
        assert!(wallet.localstore.get_saga(&spent_id).await.unwrap().is_none());
        assert!(wallet.localstore.get_saga(&half_id).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn terminal_transaction_status_is_never_overwritten() {
        let (wallet, mint) = new_wallet().await;
        let saga_id = Uuid::new_v4();
        let ys = seed_send_saga(&wallet, saga_id, 1).await;
        // A revoked-then-failed transaction shape: already terminal.
        let mut tx = wallet
            .localstore
            .get_transaction(TransactionId::from_saga_id(saga_id))
            .await
            .unwrap()
            .unwrap();
        tx.status = TransactionStatus::Failed;
        wallet.localstore.add_transaction(tx).await.unwrap();
        mint.set_check_state(Ok(spent_response(&ys)));

        let outcome = compaction_pass(std::slice::from_ref(&wallet), COMPACTION_BATCH).await;
        assert_eq!(
            outcome.compacted, 1,
            "the saga row still goes (proofs confirmed spent)"
        );
        let tx = wallet
            .localstore
            .get_transaction(TransactionId::from_saga_id(saga_id))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(tx.status, TransactionStatus::Failed, "terminal status preserved");
    }

    #[tokio::test]
    async fn batch_limit_bounds_the_pass() {
        let (wallet, _mint) = new_wallet().await;
        for _ in 0..5 {
            seed_send_saga(&wallet, Uuid::new_v4(), 1).await;
        }
        // No check-state response staged: examined candidates are retained.

        let outcome = compaction_pass(std::slice::from_ref(&wallet), 2).await;
        assert_eq!(outcome.open_sagas, 5, "all rows still present");
        assert_eq!(outcome.retained, 2, "only the batch was examined");
        assert_eq!(outcome.compacted, 0);
    }

    #[tokio::test]
    async fn unreachable_mint_retains_everything() {
        let (wallet, mint) = new_wallet().await;
        seed_send_saga(&wallet, Uuid::new_v4(), 1).await;
        mint.set_check_state(Err(cdk::Error::Custom("connection closed".to_string())));

        let outcome = compaction_pass(std::slice::from_ref(&wallet), COMPACTION_BATCH).await;
        assert_eq!(outcome.compacted, 0, "dead mint never prunes: {outcome:?}");
        assert_eq!(outcome.retained, 1);
        assert_eq!(outcome.open_sagas, 1);
    }
}
