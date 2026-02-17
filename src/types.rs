use serde::{Deserialize, Serialize};

/// A transaction submitted by a client to the mempool.
///
/// Modeled after CometBFT's transaction format with gas accounting fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transaction {
    pub id: u64,
    pub sender: String,
    pub payload: Vec<u8>,
    pub gas_limit: u64,
    pub gas_price: u64,
    pub nonce: u64,
}

impl Transaction {
    /// Estimated byte size for block packing. In a real system this would be
    /// the serialized wire size.
    pub fn size_bytes(&self) -> usize {
        // Fixed fields + payload length
        8 + self.sender.len() + self.payload.len() + 8 + 8 + 8
    }
}

/// Information about a proposer slot in the deterministic schedule.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct SlotInfo {
    pub slot: u64,
    pub proposer: String,
}

/// A block proposed by the current leader containing pending transactions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProposedBlock {
    pub height: u64,
    pub proposer: String,
    pub transactions: Vec<Transaction>,
    pub total_gas: u64,
    pub total_bytes: usize,
}

/// Result of CheckTx validation (analogous to CometBFT's ABCI CheckTx response).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckTxResult {
    pub valid: bool,
    pub reason: Option<String>,
}

/// Reverse inclusion confirmation stream: published by the proposer after
/// BuildBlock so that transaction sources can confirm which transactions
/// were included and remove them from their local pools.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SettlementEvent {
    pub block_height: u64,
    pub included_tx_ids: Vec<u64>,
}
