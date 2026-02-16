# CometBFT Mempool with Per-Validator Raft Clusters

A CometBFT-style transaction mempool built on [Mosaik](https://github.com/zmanian/mosaik), demonstrating per-validator Raft clusters with intra-cluster failover, load balancing, and inter-cluster proposer rotation via dynamic stream predicates.

## Architecture

The system uses a two-level architecture:

### Level 1: Intra-Validator Raft Clusters

Each "validator" is a 3-node Mosaik Raft cluster, not a single node. The cluster provides:
- **Failover**: If the cluster's Raft leader dies, a follower takes over automatically.
- **Load balancing**: Read queries (e.g., CheckTx) can be served by followers using `Consistency::Weak`.
- **Command forwarding**: Transactions submitted to any node in the cluster are automatically forwarded to the Raft leader.
- **State replication**: The `MempoolStateMachine` is replicated across all 3 cluster nodes.

### Level 2: Inter-Validator Proposer Schedule

A deterministic round-robin schedule determines which validator cluster is the current block proposer. The proposer schedule operates at the cluster level:
- When cluster A is the proposer, ALL 3 nodes in cluster A get the `"proposer"` tag.
- Transaction source streams use `subscribe_if` predicates that match on `"proposer"` tags.
- Stream connections target any node in the proposer cluster, surviving intra-cluster failover.

```
Validator A Cluster (3 nodes, 1 Raft group):
  [A0 leader] [A1 follower] [A2 follower]
  All tagged: "validator-a"
  When proposer: also tagged "proposer"

Validator B Cluster (3 nodes, 1 Raft group):
  [B0 leader] [B1 follower] [B2 follower]
  All tagged: "validator-b"

Validator C Cluster (3 nodes, 1 Raft group):
  [C0 leader] [C1 follower] [C2 follower]
  All tagged: "validator-c"

Tx Sources (2 nodes):
  Produce Stream<Transaction>
  subscribe_if matches "proposer" | "proposer-next" | "proposer-soon"
```

### Transaction Lifecycle

The mempool implements the core CometBFT ABCI transaction lifecycle:

1. **CheckTx** -- Validates incoming transactions (deduplication, nonce ordering, gas limit checks) without modifying state.
2. **AddTransaction** -- Runs CheckTx inline, then appends valid transactions to the pending pool.
3. **BuildBlock (PrepareProposal)** -- Reaps pending transactions in FIFO order, respecting both `max_block_size` and `max_block_gas` limits. Updates the nonce tracker with included transactions.
4. **RecheckPending (RecheckTx)** -- After a block commits, re-validates remaining pending transactions against updated nonce state and evicts stale entries.

### Key Components

| File | Purpose |
|------|---------|
| `src/cluster.rs` | `ValidatorCluster` encapsulating a multi-node Raft cluster with tag management and cross-discovery |
| `src/mempool.rs` | `MempoolStateMachine` implementing Mosaik's `StateMachine` trait with CheckTx, block building, and recheck logic |
| `src/types.rs` | `Transaction`, `ProposedBlock`, `CheckTxResult`, and `SlotInfo` data types |
| `src/leader.rs` | Leader tracking: reactive Raft-based (`track_leader_raft`), schedule-aware single-node (`track_leader_scheduled`), and cluster-aware (`track_proposer_clusters`) |
| `src/schedule.rs` | Deterministic weighted round-robin proposer schedule (`ValidatorSchedule`) |
| `src/main.rs` | End-to-end demo with 9 phases exercising the full two-level cluster architecture |

## Running the Demo

```sh
RUST_LOG=info cargo run
```

The demo runs through nine phases:

1. **Phase 1** -- Creates 3 validator clusters (3 nodes each = 9 total Mosaik nodes), each with its own `GroupKey` for independent Raft consensus.
2. **Phase 2** -- Cross-discovers all clusters so every node knows about every other node.
3. **Phase 3** -- Waits for all clusters to elect Raft leaders and come online.
4. **Phase 4** -- Sets up the proposer schedule and tags the initial proposer cluster. Spawns cluster-aware proposer tracking and intra-cluster Raft leader tracking.
5. **Phase 5** -- Creates tx sources, submits 4 transactions, consumes them on the proposer cluster, and adds them to the replicated mempool via Raft.
6. **Phase 6** -- Builds a block from the proposer cluster using PrepareProposal with gas-aware packing.
7. **Phase 7** -- Demonstrates intra-cluster capabilities: Strong vs. Weak consistency queries (load balancing), follower-to-leader command forwarding, and state replication verification.
8. **Phase 8** -- Verifies cross-cluster state isolation: each cluster's mempool is independent (different GroupKeys = different Raft groups).
9. **Phase 9** -- Demonstrates pipeline pre-connection with cluster-level 3-tier tag propagation and stream stability.

## Per-Validator Raft Clusters

### Why Clusters?

In production CometBFT deployments, validators already use **sentry node topologies** for DDoS protection: the validator sits behind multiple sentry nodes that handle P2P connections. However, if the validator process crashes, the entire validator is offline until manual intervention.

Per-validator Raft clusters extend this pattern with:
- **Automatic failover**: Raft leader election within the cluster means a follower takes over in seconds, not minutes.
- **State replication**: The mempool state is replicated to all cluster nodes, so no transactions are lost during failover.
- **Load balancing**: Followers can serve read queries (e.g., CheckTx validation) at `Consistency::Weak`, offloading the leader.

### Stream Stability

The key insight: all nodes in a validator cluster share the same tags (e.g., `"proposer"`). When tx source consumers use `subscribe_if` predicates that match on tags:
- The predicate matches ANY node in the proposer cluster.
- If the cluster's Raft leader fails, Raft elects a new leader, but all nodes still carry the `"proposer"` tag.
- Transaction source streams remain connected to surviving cluster nodes -- no stream reconnection needed.

This contrasts with single-node validators where a node failure requires reactive stream reconnection (100-400ms delay).

### Command Forwarding

Mosaik's `Group::execute()` works from any node in the Raft group. If called on a follower, the command is automatically forwarded to the current Raft leader. This means:
- Transaction sources can submit to any node in the proposer cluster.
- The cluster handles leader routing internally.
- Application code doesn't need to track which cluster node is the leader.

## Improvements Over CometBFT's Mempool

### Eliminating Gossip Flood Overhead

Real CometBFT uses flood gossip for mempool propagation: every validator receives every transaction regardless of whether it is the current block proposer. This wastes bandwidth and creates redundant mempool state across all validators.

Mosaik's `subscribe_if` with dynamic predicate re-evaluation enables direct-to-proposer transaction routing. Validators consume transaction streams filtered by the `"proposer"` discovery tag. When the proposer rotates, the cluster-aware tracker updates tags on the entire cluster via `discovery.feed()`, Mosaik re-evaluates stream predicates, and transaction flows automatically re-route to the new proposer cluster -- without any application-level reconnection logic.

### Better Block Building During Leader Transitions

Because the proposer cluster receives transactions directly rather than waiting for gossip convergence, it always has the freshest and most complete mempool. With Raft replication within the cluster, the mempool state survives individual node failures, further improving block building reliability during transitions.

### Stronger Consistency Guarantees

CometBFT's mempool is completely independent per node with no consistency guarantees. Transactions can be lost if a node crashes before gossiping them to peers. Our Raft-replicated mempool provides a consistent, ordered view of the transaction set within each validator cluster. This is a deliberate architectural tradeoff: higher latency for transaction acceptance in exchange for stronger durability and ordering guarantees.

### Native Integration of Leadership and Routing

In CometBFT, the proposer schedule is determined by a weighted round-robin algorithm that all validators compute locally. Routing transactions preferentially to the proposer would require custom P2P protocol extensions.

Mosaik's discovery system provides this integration natively. The cluster-aware tracker uses `add_tag_to_all` / `remove_tag_from_all` to tag entire clusters at once. Stream consumers with `subscribe_if` predicates react to these tag changes automatically. No custom protocol work is needed -- leadership awareness and routing are composed from existing Mosaik primitives.

## Pipeline Pre-Connection for Fast BFT

### The Problem

With reactive tag updates, there is a 100-400ms delay between a proposer rotation and stream reconnection. In fast BFT protocols where proposers rotate every 500ms, this means stream connections may not be established until the proposer's slot is nearly over.

### The Solution

The deterministic proposer schedule enables **pipeline pre-connection** -- tagging upcoming proposer clusters before their slot starts so Mosaik establishes stream connections in advance.

The `track_proposer_clusters` function in `src/leader.rs` applies a 3-tier tag system to entire clusters on each slot tick:

| Tag | Meaning | Lead Time |
|-----|---------|-----------|
| `proposer` | Current slot's proposer cluster | 0 (current) |
| `proposer-next` | Next slot's proposer cluster | 1 slot (500ms) |
| `proposer-soon` | Slot+2 proposer cluster | 2 slots (1000ms) |

All nodes in each tagged cluster receive the same tags, so the pipeline predicate can match any of the 3 nodes:

```rust
subscribe_if(|peer| {
    peer.tags().contains(&Tag::from("proposer"))
    || peer.tags().contains(&Tag::from("proposer-next"))
    || peer.tags().contains(&Tag::from("proposer-soon"))
})
```

### The Benefit

By the time a cluster's proposer slot starts, stream connections have already been established to its nodes during the 2 previous slots (up to 1000ms lead time). Combined with cluster-level tagging, this means:
- The new proposer cluster can immediately receive transactions.
- Intra-cluster failover doesn't disrupt stream connections.
- The connection establishment bottleneck is eliminated.
