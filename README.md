# CometBFT Mempool with Leader-Aware Transaction Routing

A CometBFT-style transaction mempool built on [Mosaik](https://github.com/zmanian/mosaik), demonstrating leader-aware transaction routing via dynamic stream predicates and Raft-replicated state.

## Architecture

The system consists of two node types:

**Validator nodes** (3 instances) form a Raft consensus group that replicates the mempool state machine. Each validator:
- Joins a shared Raft group with a `MempoolStateMachine` that handles transaction validation, block building, and post-commit recheck.
- Runs a leader-tracking task that watches for Raft leadership changes and updates the node's discovery entry with a `"proposer"` tag via `discovery.feed()`.
- Consumes transaction streams from tx-source nodes.

**Transaction source nodes** (2 instances) produce transactions via Mosaik streams. In a production deployment, validators would use `subscribe_if` predicates to consume only from sources whose discovery entries match the current proposer, enabling direct-to-leader routing.

### Transaction Lifecycle

The mempool implements the core CometBFT ABCI transaction lifecycle:

1. **CheckTx** -- Validates incoming transactions (deduplication, nonce ordering, gas limit checks) without modifying state.
2. **AddTransaction** -- Runs CheckTx inline, then appends valid transactions to the pending pool.
3. **BuildBlock (PrepareProposal)** -- Reaps pending transactions in FIFO order, respecting both `max_block_size` and `max_block_gas` limits. Updates the nonce tracker with included transactions.
4. **RecheckPending (RecheckTx)** -- After a block commits, re-validates remaining pending transactions against updated nonce state and evicts stale entries.

### Key Components

| File | Purpose |
|------|---------|
| `src/mempool.rs` | `MempoolStateMachine` implementing Mosaik's `StateMachine` trait with CheckTx, block building, and recheck logic |
| `src/types.rs` | `Transaction`, `ProposedBlock`, and `CheckTxResult` data types |
| `src/leader.rs` | Leader tracking task that maps Raft leadership to discovery tags |
| `src/main.rs` | End-to-end demo with 5 phases exercising the full transaction lifecycle |

## Running the Demo

```sh
RUST_LOG=info cargo run
```

The demo runs through five phases:

1. **Phase 1** -- Submits 4 valid transactions from two senders (alice, bob) with sequential nonces and varying gas prices.
2. **Phase 2** -- Demonstrates CheckTx validation: duplicate rejection, stale nonce detection, and gas-exceeds-block-limit checks.
3. **Phase 3** -- Builds a block via PrepareProposal with gas-aware packing.
4. **Phase 4** -- Submits post-block transactions and runs RecheckPending to verify nonce revalidation.
5. **Phase 5** -- Verifies state replication to a follower node.

## Improvements Over CometBFT's Mempool

### Eliminating Gossip Flood Overhead

Real CometBFT uses flood gossip for mempool propagation: every validator receives every transaction regardless of whether it is the current block proposer. This wastes bandwidth and creates redundant mempool state across all validators.

Mosaik's `subscribe_if` with dynamic predicate re-evaluation enables direct-to-proposer transaction routing. Validators consume transaction streams filtered by the `"proposer"` discovery tag. When leadership rotates, the leader tracking module updates the tag via `discovery.feed()`, Mosaik re-evaluates stream predicates, and transaction flows automatically re-route to the new leader -- without any application-level reconnection logic.

This reduces gossip bandwidth by eliminating redundant transaction dissemination to non-proposer validators.

### Better Block Building During Leader Transitions

Because the proposer receives transactions directly rather than waiting for gossip convergence, it always has the freshest and most complete mempool. This improves block building quality and reduces empty blocks during leader transitions, a known issue in CometBFT where a newly elected proposer may not yet have received all gossiped transactions.

### Stronger Consistency Guarantees

CometBFT's mempool is completely independent per node with no consistency guarantees. Transactions can be lost if a node crashes before gossiping them to peers. Our Raft-replicated mempool provides a consistent, ordered view of the transaction set across all validators. This is a deliberate architectural tradeoff: higher latency for transaction acceptance in exchange for stronger durability and ordering guarantees.

### Native Integration of Leadership and Routing

In CometBFT, the proposer schedule is determined by a weighted round-robin algorithm that all validators compute locally. Routing transactions preferentially to the proposer would require custom P2P protocol extensions.

Mosaik's discovery system provides this integration natively. The leader tracking module uses `When::is_leader()` / `When::is_follower()` to detect Raft leadership changes and `discovery.feed()` to publish tag updates. Stream consumers with `subscribe_if` predicates react to these tag changes automatically. No custom protocol work is needed -- leadership awareness and routing are composed from existing Mosaik primitives.
