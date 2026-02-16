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
| `src/types.rs` | `Transaction`, `ProposedBlock`, `CheckTxResult`, and `SlotInfo` data types |
| `src/leader.rs` | Leader tracking: reactive Raft-based (`track_leader_raft`) and schedule-aware pre-tagging (`track_leader_scheduled`) |
| `src/schedule.rs` | Deterministic weighted round-robin proposer schedule (`ValidatorSchedule`) |
| `src/main.rs` | End-to-end demo with 6 phases exercising the full transaction lifecycle including pipeline pre-connection |

## Running the Demo

```sh
RUST_LOG=info cargo run
```

The demo runs through six phases:

1. **Phase 1** -- Submits 4 valid transactions from two senders (alice, bob) with sequential nonces and varying gas prices.
2. **Phase 2** -- Demonstrates CheckTx validation: duplicate rejection, stale nonce detection, and gas-exceeds-block-limit checks.
3. **Phase 3** -- Builds a block via PrepareProposal with gas-aware packing.
4. **Phase 4** -- Submits post-block transactions and runs RecheckPending to verify nonce revalidation.
5. **Phase 5** -- Verifies state replication to a follower node.
6. **Phase 6** -- Demonstrates pipeline pre-connection for fast BFT rotation using deterministic proposer scheduling and 3-tier tag pre-tagging.

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

## Pipeline Pre-Connection for Fast BFT

### The Problem

With reactive tag updates (Raft-based leader tracking), there is a 100-400ms delay between a leadership change and stream reconnection. In fast BFT protocols where leaders rotate every 500ms, this means stream connections may not be established until the proposer's slot is nearly over, wasting valuable block-building time.

### The Solution

CometBFT's proposer schedule is deterministic: given the validator set and their voting powers, every node can independently compute the proposer for any future slot using weighted round-robin. This predictability enables **pipeline pre-connection** -- tagging upcoming proposers before their slot starts so Mosaik establishes stream connections in advance.

### How It Works

The `ValidatorSchedule` in `src/schedule.rs` implements CometBFT's weighted round-robin algorithm:
1. Each round, every validator's accumulated priority increases by its voting power.
2. The validator with the highest priority is selected as proposer.
3. The selected validator's priority decreases by the total voting power of the set.

The `track_leader_scheduled` function in `src/leader.rs` uses this schedule to apply a 3-tier tag system on each slot tick:

| Tag | Meaning | Lead Time |
|-----|---------|-----------|
| `proposer` | Current slot's proposer, actively building blocks | 0 (current) |
| `proposer-next` | Next slot's proposer, pre-warming connections | 1 slot (500ms) |
| `proposer-soon` | Slot+2 proposer, establishing connections | 2 slots (1000ms) |

Transaction source consumers use a broader `subscribe_if` predicate that matches on any of these three tags:

```rust
subscribe_if(|peer| {
    peer.tags().contains(&Tag::from("proposer"))
    || peer.tags().contains(&Tag::from("proposer-next"))
    || peer.tags().contains(&Tag::from("proposer-soon"))
})
```

### The Benefit

By the time a validator's proposer slot starts, its stream connections have already been established during the 2 previous slots (up to 1000ms of lead time). The new proposer can immediately receive transactions without waiting for reactive tag updates and stream reconnection. This eliminates the connection establishment bottleneck that would otherwise consume a significant fraction of each 500ms slot.
