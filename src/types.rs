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
