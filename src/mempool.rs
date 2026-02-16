use {
    crate::types::{CheckTxResult, ProposedBlock, Transaction},
    mosaik::{groups::StateMachine, primitives::UniqueId, unique_id},
    serde::{Deserialize, Serialize},
    std::collections::{BTreeMap, HashSet},
};

/// Commands that mutate the mempool state machine.
///
/// Follows the CometBFT transaction lifecycle:
/// CheckTx -> AddTransaction -> PrepareProposal (BuildBlock) -> RecheckPending
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MempoolCommand {
    /// Validates and adds a transaction to the mempool.
    /// Performs CheckTx inline: dedup check, nonce validation, gas limit check.
    AddTransaction(Transaction),

    /// PrepareProposal: the leader builds a candidate block from pending
    /// transactions, respecting max_block_size and max_block_gas limits.
    /// Transactions are reaped in FIFO order (arrival time).
    BuildBlock,

    /// RecheckTx: after a block is committed, re-validate remaining pending
    /// transactions against the updated nonce state. Evicts transactions with
    /// stale nonces (already consumed by included transactions).
    RecheckPending,

    /// Remove specific transactions by ID.
    ClearPending(Vec<u64>),
}

/// Queries against the mempool state machine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MempoolQuery {
    PendingCount,
    PendingTransactions(usize),
    /// CheckTx: validate a transaction without adding it to the mempool.
    /// Returns CheckTxResult indicating validity.
    CheckTx(Transaction),
}

/// Results returned from mempool queries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MempoolQueryResult {
    Count(usize),
    Transactions(Vec<Transaction>),
    Block(ProposedBlock),
    CheckTx(CheckTxResult),
}

/// The replicated mempool state machine.
///
/// NOTE: In real CometBFT, each node maintains its own independent mempool and
/// transactions are flooded via gossip. Our design replicates the mempool
/// through Raft consensus, which is a deliberate architectural choice that
/// provides stronger consistency guarantees at the cost of higher latency.
#[derive(Debug)]
pub struct MempoolStateMachine {
    /// Pending transactions in FIFO order (arrival time ordering).
    pending: Vec<Transaction>,
    /// Seen transaction IDs for deduplication.
    seen_txs: HashSet<u64>,
    /// Tracks the last committed nonce per sender for ordering validation.
    nonce_tracker: BTreeMap<String, u64>,
    /// Maximum number of transactions per block.
    max_block_size: usize,
    /// Maximum total gas per block.
    max_block_gas: u64,
    /// Current block height.
    block_height: u64,
}

impl MempoolStateMachine {
    pub fn new(max_block_size: usize, max_block_gas: u64) -> Self {
        Self {
            pending: Vec::new(),
            seen_txs: HashSet::new(),
            nonce_tracker: BTreeMap::new(),
            max_block_size,
            max_block_gas,
            block_height: 0,
        }
    }

    /// CheckTx: validates a transaction without modifying state.
    /// Mirrors CometBFT's ABCI CheckTx call.
    fn check_tx(&self, tx: &Transaction) -> CheckTxResult {
        // Dedup check
        if self.seen_txs.contains(&tx.id) {
            return CheckTxResult {
                valid: false,
                reason: Some(format!("duplicate transaction id={}", tx.id)),
            };
        }

        // Nonce ordering: must be >= expected next nonce for this sender
        let expected_nonce = self
            .nonce_tracker
            .get(&tx.sender)
            .map_or(0, |last| last + 1);
        if tx.nonce < expected_nonce {
            return CheckTxResult {
                valid: false,
                reason: Some(format!(
                    "stale nonce: tx nonce={} but expected >= {} for sender={}",
                    tx.nonce, expected_nonce, tx.sender
                )),
            };
        }

        // Gas limit sanity check
        if tx.gas_limit == 0 {
            return CheckTxResult {
                valid: false,
                reason: Some("gas_limit must be > 0".to_string()),
            };
        }

        // Transaction too large for any block
        if tx.gas_limit > self.max_block_gas {
            return CheckTxResult {
                valid: false,
                reason: Some(format!(
                    "gas_limit {} exceeds max_block_gas {}",
                    tx.gas_limit, self.max_block_gas
                )),
            };
        }

        CheckTxResult {
            valid: true,
            reason: None,
        }
    }
}

impl StateMachine for MempoolStateMachine {
    const ID: UniqueId =
        unique_id!("a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e3f4a5b6c7d8e9f0a1b2");

    type Command = MempoolCommand;
    type Query = MempoolQuery;
    type QueryResult = MempoolQueryResult;

    fn reset(&mut self) {
        self.pending.clear();
        self.seen_txs.clear();
        self.nonce_tracker.clear();
        self.block_height = 0;
    }

    fn apply(&mut self, command: Self::Command) {
        match command {
            MempoolCommand::AddTransaction(tx) => {
                // Inline CheckTx validation before accepting
                let result = self.check_tx(&tx);
                if !result.valid {
                    tracing::warn!(
                        id = tx.id,
                        sender = tx.sender,
                        reason = result.reason.as_deref().unwrap_or("unknown"),
                        "rejected transaction (CheckTx failed)"
                    );
                    return;
                }

                tracing::info!(
                    id = tx.id,
                    sender = tx.sender,
                    nonce = tx.nonce,
                    gas_limit = tx.gas_limit,
                    gas_price = tx.gas_price,
                    "accepted transaction into mempool"
                );
                self.seen_txs.insert(tx.id);
                self.pending.push(tx);
            }

            MempoolCommand::BuildBlock => {
                // PrepareProposal: reap transactions in FIFO order respecting
                // both max_block_size and max_block_gas limits.
                let mut block_txs = Vec::new();
                let mut total_gas: u64 = 0;
                let mut total_bytes: usize = 0;
                let mut included_indices = Vec::new();

                for (i, tx) in self.pending.iter().enumerate() {
                    if block_txs.len() >= self.max_block_size {
                        break;
                    }
                    if total_gas.saturating_add(tx.gas_limit) > self.max_block_gas {
                        continue; // skip tx that would exceed gas limit
                    }

                    total_gas = total_gas.saturating_add(tx.gas_limit);
                    total_bytes += tx.size_bytes();
                    block_txs.push(tx.clone());
                    included_indices.push(i);
                }

                // Update nonce tracker with included transactions
                for tx in &block_txs {
                    let entry = self.nonce_tracker.entry(tx.sender.clone()).or_insert(0);
                    if tx.nonce >= *entry {
                        *entry = tx.nonce;
                    }
                }

                // Remove included transactions (in reverse to preserve indices)
                for &i in included_indices.iter().rev() {
                    self.pending.remove(i);
                }

                self.block_height += 1;
                tracing::info!(
                    height = self.block_height,
                    tx_count = block_txs.len(),
                    total_gas,
                    total_bytes,
                    remaining = self.pending.len(),
                    "PrepareProposal: built block"
                );
            }

            MempoolCommand::RecheckPending => {
                // RecheckTx: re-validate remaining mempool transactions after
                // a block commit. Remove transactions with stale nonces.
                let before = self.pending.len();
                self.pending.retain(|tx| {
                    let expected = self
                        .nonce_tracker
                        .get(&tx.sender)
                        .map_or(0, |last| last + 1);
                    if tx.nonce < expected {
                        tracing::info!(
                            id = tx.id,
                            sender = tx.sender,
                            nonce = tx.nonce,
                            expected_nonce = expected,
                            "RecheckTx: evicting stale transaction"
                        );
                        false
                    } else {
                        true
                    }
                });
                let evicted = before - self.pending.len();
                if evicted > 0 {
                    tracing::info!(evicted, remaining = self.pending.len(), "RecheckTx complete");
                }
            }

            MempoolCommand::ClearPending(ids) => {
                self.pending.retain(|tx| !ids.contains(&tx.id));
            }
        }
    }

    fn query(&self, query: Self::Query) -> Self::QueryResult {
        match query {
            MempoolQuery::PendingCount => MempoolQueryResult::Count(self.pending.len()),
            MempoolQuery::PendingTransactions(n) => {
                let txs: Vec<Transaction> = self.pending.iter().take(n).cloned().collect();
                MempoolQueryResult::Transactions(txs)
            }
            MempoolQuery::CheckTx(tx) => MempoolQueryResult::CheckTx(self.check_tx(&tx)),
        }
    }
}
