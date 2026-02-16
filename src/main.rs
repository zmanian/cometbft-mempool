#![allow(clippy::too_many_lines)]

mod leader;
mod mempool;
mod types;

use {
    futures::{SinkExt, StreamExt},
    mempool::{MempoolCommand, MempoolQuery, MempoolQueryResult, MempoolStateMachine},
    mosaik::{*, discovery::PeerEntry, primitives::Tag},
    types::Transaction,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,mosaik=debug".parse().unwrap()),
        )
        .init();

    let network_id = NetworkId::random();
    let group_key = GroupKey::random();

    // Configuration: max 10 txs per block, max 200,000 gas per block
    let max_block_size: usize = 10;
    let max_block_gas: u64 = 200_000;

    // --- Create 3 validator nodes with "validator" tag ---
    tracing::info!("creating validator nodes...");
    let validator0 = Network::builder(network_id)
        .with_discovery(
            mosaik::discovery::Config::builder().with_tags(Tag::from("validator")),
        )
        .build()
        .await?;

    let validator1 = Network::builder(network_id)
        .with_discovery(
            mosaik::discovery::Config::builder().with_tags(Tag::from("validator")),
        )
        .build()
        .await?;

    let validator2 = Network::builder(network_id)
        .with_discovery(
            mosaik::discovery::Config::builder().with_tags(Tag::from("validator")),
        )
        .build()
        .await?;

    // Cross-discover all validators
    tracing::info!("cross-discovering validators...");
    discover_all([&validator0, &validator1, &validator2]).await?;

    // --- All validators join the same Raft group ---
    // NOTE: In real CometBFT, each node maintains its own independent mempool
    // and transactions propagate via gossip flooding. Our Raft-replicated mempool
    // is a deliberate design choice that trades higher latency for stronger
    // consistency: all validators see the same ordered transaction set.
    let g0 = validator0
        .groups()
        .with_key(group_key)
        .with_state_machine(MempoolStateMachine::new(max_block_size, max_block_gas))
        .join();

    let g1 = validator1
        .groups()
        .with_key(group_key)
        .with_state_machine(MempoolStateMachine::new(max_block_size, max_block_gas))
        .join();

    let g2 = validator2
        .groups()
        .with_key(group_key)
        .with_state_machine(MempoolStateMachine::new(max_block_size, max_block_gas))
        .join();

    // Wait for the group to come online on all nodes
    tracing::info!("waiting for validator group to come online...");
    g0.when().online().await;
    g1.when().online().await;
    g2.when().online().await;

    let leader_id = g0
        .leader()
        .expect("leader should be elected after online");
    tracing::info!("validator group online, leader: {leader_id}");

    // --- Spawn leader-tracking tasks on each validator ---
    // These tasks update the "proposer" tag when leadership changes.
    tokio::spawn({
        let discovery = validator0.discovery().clone();
        let secret_key = validator0.local().secret_key().clone();
        let when = g0.when().clone();
        async move {
            if let Err(e) = leader::track_leader(discovery, secret_key, when).await {
                tracing::error!("leader tracker 0 failed: {e}");
            }
        }
    });

    tokio::spawn({
        let discovery = validator1.discovery().clone();
        let secret_key = validator1.local().secret_key().clone();
        let when = g1.when().clone();
        async move {
            if let Err(e) = leader::track_leader(discovery, secret_key, when).await {
                tracing::error!("leader tracker 1 failed: {e}");
            }
        }
    });

    tokio::spawn({
        let discovery = validator2.discovery().clone();
        let secret_key = validator2.local().secret_key().clone();
        let when = g2.when().clone();
        async move {
            if let Err(e) = leader::track_leader(discovery, secret_key, when).await {
                tracing::error!("leader tracker 2 failed: {e}");
            }
        }
    });

    // Give the leader tracker a moment to apply the proposer tag
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Verify that the leader node has the "proposer" tag
    let proposer_tag = Tag::from("proposer");
    for (i, validator) in [&validator0, &validator1, &validator2]
        .iter()
        .enumerate()
    {
        let me: PeerEntry = validator.discovery().me().into_unsigned();
        if me.tags().contains(&proposer_tag) {
            tracing::info!("validator{i} has proposer tag (is the leader)");
        }
    }

    // --- Create 2 tx-source nodes ---
    tracing::info!("creating tx-source nodes...");
    let tx_source_a = Network::new(network_id).await?;
    let tx_source_b = Network::new(network_id).await?;

    // --- Set up stream producers BEFORE cross-discovery ---
    // Producers must be created before sync so their stream IDs appear in the
    // catalog entries that validators receive during discovery.
    tracing::info!("creating transaction stream producers...");
    let mut producer_a = tx_source_a.streams().produce::<Transaction>();
    let mut producer_b = tx_source_b.streams().produce::<Transaction>();

    // Cross-discover tx sources with validators (after producers exist)
    tracing::info!("cross-discovering tx sources with validators...");
    discover_all([
        &tx_source_a,
        &tx_source_b,
        &validator0,
        &validator1,
        &validator2,
    ])
    .await?;

    // All validators consume transactions from tx sources
    tracing::info!("creating transaction consumer on validator0...");
    let mut consumer0 = validator0.streams().consume::<Transaction>();

    // Wait for the consumer to subscribe to both tx-source producers
    tracing::info!("waiting for consumer to subscribe to tx-source producers...");
    consumer0.when().subscribed().minimum_of(2).await;
    tracing::info!("validator0 subscribed to both tx-source streams");

    // =========================================================================
    // Phase 1: Submit valid transactions with varying gas prices
    // =========================================================================
    tracing::info!("--- Phase 1: Submitting valid transactions ---");

    // Alice sends two transactions with sequential nonces
    producer_a
        .send(Transaction {
            id: 1,
            sender: "alice".into(),
            payload: vec![1, 2, 3],
            gas_limit: 21_000,
            gas_price: 10,
            nonce: 0,
        })
        .await?;

    producer_a
        .send(Transaction {
            id: 2,
            sender: "alice".into(),
            payload: vec![4, 5, 6],
            gas_limit: 50_000,
            gas_price: 20,
            nonce: 1,
        })
        .await?;

    // Bob sends two transactions with sequential nonces
    producer_b
        .send(Transaction {
            id: 3,
            sender: "bob".into(),
            payload: vec![7, 8, 9],
            gas_limit: 21_000,
            gas_price: 15,
            nonce: 0,
        })
        .await?;

    producer_b
        .send(Transaction {
            id: 4,
            sender: "bob".into(),
            payload: vec![10, 11, 12],
            gas_limit: 100_000,
            gas_price: 5,
            nonce: 1,
        })
        .await?;

    // Consume and add all 4 valid transactions to the mempool via Raft
    tracing::info!("consuming transactions and adding to mempool...");
    for _ in 0..4 {
        let tx = consumer0
            .next()
            .await
            .expect("expected transaction from stream");

        tracing::info!(
            id = tx.id,
            sender = tx.sender,
            gas_limit = tx.gas_limit,
            gas_price = tx.gas_price,
            nonce = tx.nonce,
            "received transaction"
        );

        g0.execute(MempoolCommand::AddTransaction(tx)).await?;
    }

    // Query pending count: should be 4
    let result = g0
        .query(MempoolQuery::PendingCount, Consistency::Strong)
        .await?;
    if let MempoolQueryResult::Count(count) = result {
        tracing::info!("pending transactions after Phase 1: {count} (expected: 4)");
    }

    // =========================================================================
    // Phase 2: Demonstrate CheckTx validation - dedup and nonce checks
    // =========================================================================
    tracing::info!("--- Phase 2: CheckTx validation ---");

    // CheckTx: duplicate transaction (id=1 already in mempool)
    let dup_check = g0
        .query(
            MempoolQuery::CheckTx(Transaction {
                id: 1,
                sender: "alice".into(),
                payload: vec![1, 2, 3],
                gas_limit: 21_000,
                gas_price: 10,
                nonce: 0,
            }),
            Consistency::Strong,
        )
        .await?;
    if let MempoolQueryResult::CheckTx(result) = &dup_check {
        tracing::info!(
            valid = result.valid,
            reason = result.reason.as_deref().unwrap_or("none"),
            "CheckTx duplicate tx id=1"
        );
    }

    // Submit the duplicate via AddTransaction to show it gets rejected
    tracing::info!("submitting duplicate transaction id=1 (should be rejected)...");
    producer_a
        .send(Transaction {
            id: 1,
            sender: "alice".into(),
            payload: vec![1, 2, 3],
            gas_limit: 21_000,
            gas_price: 10,
            nonce: 0,
        })
        .await?;

    let dup_tx = consumer0
        .next()
        .await
        .expect("expected duplicate transaction from stream");
    g0.execute(MempoolCommand::AddTransaction(dup_tx)).await?;

    // CheckTx: stale nonce (alice nonce=0 already accepted)
    let stale_check = g0
        .query(
            MempoolQuery::CheckTx(Transaction {
                id: 99,
                sender: "alice".into(),
                payload: vec![],
                gas_limit: 21_000,
                gas_price: 10,
                nonce: 0, // stale: alice already has nonce 0 and 1 pending
            }),
            Consistency::Strong,
        )
        .await?;
    if let MempoolQueryResult::CheckTx(result) = &stale_check {
        tracing::info!(
            valid = result.valid,
            reason = result.reason.as_deref().unwrap_or("none"),
            "CheckTx stale nonce for alice (nonce=0)"
        );
    }

    // CheckTx: gas exceeds block limit
    let gas_check = g0
        .query(
            MempoolQuery::CheckTx(Transaction {
                id: 100,
                sender: "charlie".into(),
                payload: vec![],
                gas_limit: max_block_gas + 1, // exceeds max_block_gas
                gas_price: 10,
                nonce: 0,
            }),
            Consistency::Strong,
        )
        .await?;
    if let MempoolQueryResult::CheckTx(result) = &gas_check {
        tracing::info!(
            valid = result.valid,
            reason = result.reason.as_deref().unwrap_or("none"),
            "CheckTx gas exceeds block limit"
        );
    }

    // Pending count should still be 4 (duplicate was rejected)
    let result = g0
        .query(MempoolQuery::PendingCount, Consistency::Strong)
        .await?;
    if let MempoolQueryResult::Count(count) = result {
        tracing::info!("pending transactions after Phase 2: {count} (expected: 4)");
    }

    // =========================================================================
    // Phase 3: PrepareProposal - gas-aware block building
    // =========================================================================
    tracing::info!("--- Phase 3: PrepareProposal (BuildBlock) ---");

    // List pending transactions before block
    let result = g0
        .query(MempoolQuery::PendingTransactions(10), Consistency::Strong)
        .await?;
    if let MempoolQueryResult::Transactions(txs) = &result {
        tracing::info!("pending transactions before block:");
        for tx in txs {
            tracing::info!(
                "  tx {} from {} (gas: {}, gas_price: {}, nonce: {})",
                tx.id,
                tx.sender,
                tx.gas_limit,
                tx.gas_price,
                tx.nonce
            );
        }
    }

    // Build a block. With max_block_gas=200,000:
    // tx1 (21k) + tx2 (50k) + tx3 (21k) + tx4 (100k) = 192k < 200k
    // All 4 should fit in one block.
    tracing::info!(
        "building block (max_block_size={}, max_block_gas={})...",
        max_block_size,
        max_block_gas
    );
    g0.execute(MempoolCommand::BuildBlock).await?;

    // Pending count after block: should be 0
    let result = g0
        .query(MempoolQuery::PendingCount, Consistency::Strong)
        .await?;
    if let MempoolQueryResult::Count(count) = result {
        tracing::info!("pending transactions after block 1: {count} (expected: 0)");
    }

    // =========================================================================
    // Phase 4: Post-block - submit more txs and demonstrate RecheckPending
    // =========================================================================
    tracing::info!("--- Phase 4: RecheckPending after block commit ---");

    // Submit new transactions for alice and bob (nonce 2 for both)
    // Also submit a tx with a stale nonce that should be evicted by recheck
    producer_a
        .send(Transaction {
            id: 5,
            sender: "alice".into(),
            payload: vec![13, 14],
            gas_limit: 30_000,
            gas_price: 25,
            nonce: 2,
        })
        .await?;

    producer_b
        .send(Transaction {
            id: 6,
            sender: "bob".into(),
            payload: vec![15, 16],
            gas_limit: 40_000,
            gas_price: 12,
            nonce: 2,
        })
        .await?;

    // Consume and add to mempool
    for _ in 0..2 {
        let tx = consumer0
            .next()
            .await
            .expect("expected transaction from stream");
        tracing::info!(
            id = tx.id,
            sender = tx.sender,
            nonce = tx.nonce,
            "received post-block transaction"
        );
        g0.execute(MempoolCommand::AddTransaction(tx)).await?;
    }

    let result = g0
        .query(MempoolQuery::PendingCount, Consistency::Strong)
        .await?;
    if let MempoolQueryResult::Count(count) = result {
        tracing::info!("pending before recheck: {count} (expected: 2)");
    }

    // RecheckPending: re-validate all pending transactions against updated
    // nonce state. After block 1 committed alice nonce=1 and bob nonce=1,
    // so alice nonce=2 and bob nonce=2 are still valid.
    tracing::info!("executing RecheckPending...");
    g0.execute(MempoolCommand::RecheckPending).await?;

    let result = g0
        .query(MempoolQuery::PendingCount, Consistency::Strong)
        .await?;
    if let MempoolQueryResult::Count(count) = result {
        tracing::info!("pending after recheck: {count} (expected: 2, no evictions)");
    }

    // =========================================================================
    // Phase 5: Verify state replication to a follower
    // =========================================================================
    tracing::info!("--- Phase 5: Verifying replication to follower ---");
    g1.when().committed().reaches(g0.committed()).await;
    let follower_result = g1
        .query(MempoolQuery::PendingCount, Consistency::Weak)
        .await?;
    if let MempoolQueryResult::Count(count) = follower_result {
        tracing::info!("follower g1 sees {count} pending transactions (consistent with leader)");
    }

    // In a production system, tx sources would use subscribe_if with the
    // "proposer" tag to route transactions only to the current leader:
    //
    //   let proposer_tag = Tag::from("proposer");
    //   let consumer = network.streams()
    //       .consumer::<Transaction>()
    //       .subscribe_if(move |peer: &PeerEntry| peer.tags().contains(&proposer_tag))
    //       .build();
    //
    // Mosaik's dynamic predicate re-evaluation would automatically re-route
    // when leadership changes and the "proposer" tag moves to a new node.

    tracing::info!("cometbft mempool example complete");
    Ok(())
}

/// Utility: cross-discover all networks with each other.
async fn discover_all(
    networks: impl IntoIterator<Item = &Network>,
) -> anyhow::Result<()> {
    let networks = networks.into_iter().collect::<Vec<_>>();
    for (i, net_i) in networks.iter().enumerate() {
        for (j, net_j) in networks.iter().enumerate() {
            if i != j {
                net_i.discovery().sync_with(net_j.local().addr()).await?;
            }
        }
    }
    Ok(())
}
