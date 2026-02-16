#![allow(clippy::too_many_lines)]

mod cluster;
mod leader;
mod mempool;
mod schedule;
mod types;

use {
    cluster::ValidatorCluster,
    futures::{SinkExt, StreamExt},
    mempool::{MempoolCommand, MempoolQuery, MempoolQueryResult},
    mosaik::{discovery::PeerEntry, primitives::Tag, *},
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

    // Configuration: max 10 txs per block, max 200,000 gas per block
    let max_block_size: usize = 10;
    let max_block_gas: u64 = 200_000;

    // =========================================================================
    // Phase 1: Create 3 validator clusters (3 nodes each = 9 total)
    // =========================================================================
    tracing::info!("--- Phase 1: Creating validator clusters (3 clusters x 3 nodes = 9 nodes) ---");

    // Each cluster gets its own GroupKey (independent intra-cluster Raft)
    let cluster_a =
        ValidatorCluster::new("a", 3, network_id, GroupKey::random(), max_block_size, max_block_gas)
            .await?;
    tracing::info!("cluster A created (3 nodes, tag: validator-a)");

    let cluster_b =
        ValidatorCluster::new("b", 3, network_id, GroupKey::random(), max_block_size, max_block_gas)
            .await?;
    tracing::info!("cluster B created (3 nodes, tag: validator-b)");

    let cluster_c =
        ValidatorCluster::new("c", 3, network_id, GroupKey::random(), max_block_size, max_block_gas)
            .await?;
    tracing::info!("cluster C created (3 nodes, tag: validator-c)");

    // =========================================================================
    // Phase 2: Cross-discover all clusters
    // =========================================================================
    tracing::info!("--- Phase 2: Cross-discovering clusters ---");
    cluster_a.discover_cluster(&cluster_b).await?;
    cluster_a.discover_cluster(&cluster_c).await?;
    cluster_b.discover_cluster(&cluster_c).await?;
    tracing::info!("all clusters cross-discovered");

    // =========================================================================
    // Phase 3: Wait for all clusters to come online
    // =========================================================================
    tracing::info!("--- Phase 3: Waiting for all clusters to come online ---");
    cluster_a.wait_online().await;
    tracing::info!("cluster A online");
    cluster_b.wait_online().await;
    tracing::info!("cluster B online");
    cluster_c.wait_online().await;
    tracing::info!("cluster C online");
    tracing::info!("all 3 validator clusters online (9 nodes total)");

    // Log which node is leader in each cluster
    for (name, cluster) in [("A", &cluster_a), ("B", &cluster_b), ("C", &cluster_c)] {
        if let Some(idx) = cluster.leader_index() {
            tracing::info!("cluster {name}: node {idx} is the Raft leader");
        }
    }

    // =========================================================================
    // Phase 4: Set up proposer schedule and tag initial proposer cluster
    // =========================================================================
    tracing::info!("--- Phase 4: Setting up proposer schedule ---");

    let clusters = [&cluster_a, &cluster_b, &cluster_c];
    let cluster_names = ["a", "b", "c"];

    // Cluster A is the initial proposer (slot 0)
    let initial_proposer_idx = 0;
    clusters[initial_proposer_idx].add_tag_to_all(Tag::from(leader::TAG_PROPOSER))?;
    tracing::info!(
        "cluster {} is the initial proposer (all 3 nodes tagged 'proposer')",
        cluster_names[initial_proposer_idx].to_uppercase()
    );

    // Spawn cluster-aware proposer tracking
    let slot_duration = std::time::Duration::from_millis(500);
    let handles: Vec<Vec<_>> = clusters
        .iter()
        .map(|c| c.discovery_handles())
        .collect();
    let names: Vec<String> = cluster_names.iter().map(|n| n.to_string()).collect();
    tokio::spawn(async move {
        if let Err(e) =
            leader::track_proposer_clusters(handles, names, slot_duration, 3).await
        {
            tracing::error!("cluster proposer tracker failed: {e}");
        }
    });

    // Also spawn intra-cluster Raft leader tracking on each node.
    // This tracks the Raft leader WITHIN each cluster for the "raft-leader" tag,
    // independent of the proposer schedule.
    for (name, cluster) in [("a", &cluster_a), ("b", &cluster_b), ("c", &cluster_c)] {
        for (i, (net, group)) in cluster
            .networks
            .iter()
            .zip(cluster.groups.iter())
            .enumerate()
        {
            let discovery = net.discovery().clone();
            let secret_key = net.local().secret_key().clone();
            let when = group.when().clone();
            let cluster_name = name.to_string();
            tokio::spawn(async move {
                if let Err(e) = leader::track_leader_raft(discovery, secret_key, when).await {
                    tracing::error!("raft leader tracker {cluster_name}-{i} failed: {e}");
                }
            });
        }
    }

    // Give the leader tracker a moment to apply tags
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // =========================================================================
    // Phase 5: Create tx sources and submit transactions to proposer cluster
    // =========================================================================
    tracing::info!("--- Phase 5: Transaction submission to proposer cluster ---");

    let tx_source_a = Network::new(network_id).await?;
    let tx_source_b = Network::new(network_id).await?;

    // Set up stream producers BEFORE cross-discovery
    let mut producer_a = tx_source_a.streams().produce::<Transaction>();
    let mut producer_b = tx_source_b.streams().produce::<Transaction>();

    // Discover tx sources with all clusters
    cluster_a
        .discover_networks(&[&tx_source_a, &tx_source_b])
        .await?;
    cluster_b
        .discover_networks(&[&tx_source_a, &tx_source_b])
        .await?;
    cluster_c
        .discover_networks(&[&tx_source_a, &tx_source_b])
        .await?;
    tracing::info!("tx sources discovered by all clusters");

    // Create consumer on the proposer cluster's first node (any node works
    // since Raft forwards commands to the leader internally)
    let proposer_cluster = clusters[initial_proposer_idx];
    let mut consumer = proposer_cluster.networks[0]
        .streams()
        .consume::<Transaction>();

    // Wait for the consumer to subscribe to both tx-source producers
    tracing::info!("waiting for consumer to subscribe to tx-source producers...");
    consumer.when().subscribed().minimum_of(2).await;
    tracing::info!("proposer cluster subscribed to both tx-source streams");

    // Submit transactions
    tracing::info!("submitting transactions...");
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

    // Consume and add to the proposer cluster's mempool via Raft.
    // Note: execute() works from any node -- followers forward to the Raft leader.
    let proposer_group = &proposer_cluster.groups[0];
    tracing::info!("consuming transactions and adding to proposer cluster's mempool...");
    for _ in 0..4 {
        let tx = consumer
            .next()
            .await
            .expect("expected transaction from stream");
        tracing::info!(
            id = tx.id,
            sender = tx.sender,
            gas_price = tx.gas_price,
            "received transaction"
        );
        proposer_group
            .execute(MempoolCommand::AddTransaction(tx))
            .await?;
    }

    // Query pending count from the cluster leader
    let result = proposer_group
        .query(MempoolQuery::PendingCount, Consistency::Strong)
        .await?;
    if let MempoolQueryResult::Count(count) = result {
        tracing::info!("proposer cluster pending transactions: {count} (expected: 4)");
    }

    // =========================================================================
    // Phase 6: Build a block from the proposer cluster
    // =========================================================================
    tracing::info!("--- Phase 6: Block building from proposer cluster ---");
    proposer_group
        .execute(MempoolCommand::BuildBlock)
        .await?;

    let result = proposer_group
        .query(MempoolQuery::PendingCount, Consistency::Strong)
        .await?;
    if let MempoolQueryResult::Count(count) = result {
        tracing::info!("proposer cluster pending after block: {count} (expected: 0)");
    }

    // =========================================================================
    // Phase 7: Demonstrate intra-cluster replication and load balancing
    // =========================================================================
    tracing::info!("--- Phase 7: Intra-cluster replication and load balancing ---");

    // Submit new transactions for the post-block mempool
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

    for _ in 0..2 {
        let tx = consumer
            .next()
            .await
            .expect("expected transaction from stream");
        tracing::info!(id = tx.id, sender = tx.sender, "received post-block transaction");
        proposer_group
            .execute(MempoolCommand::AddTransaction(tx))
            .await?;
    }

    // Query the leader with Strong consistency
    let leader_result = proposer_group
        .query(MempoolQuery::PendingCount, Consistency::Strong)
        .await?;
    if let MempoolQueryResult::Count(count) = leader_result {
        tracing::info!("leader (Strong consistency): {count} pending (expected: 2)");
    }

    // Demonstrate load balancing: query a FOLLOWER node with Weak consistency.
    // Wait for replication to catch up first.
    let follower_idx = if proposer_cluster.leader_index() == Some(0) {
        1
    } else {
        0
    };
    let follower_group = &proposer_cluster.groups[follower_idx];

    // Wait for the follower to catch up to the leader's committed index
    follower_group
        .when()
        .committed()
        .reaches(proposer_group.committed())
        .await;

    let follower_result = follower_group
        .query(MempoolQuery::PendingCount, Consistency::Weak)
        .await?;
    if let MempoolQueryResult::Count(count) = follower_result {
        tracing::info!(
            "follower node {follower_idx} (Weak consistency): {count} pending (consistent with leader)"
        );
    }

    // Demonstrate command forwarding: submit a transaction via a follower node.
    // The follower automatically forwards to the Raft leader.
    tracing::info!("submitting transaction via follower node {follower_idx} (auto-forwarded to leader)...");
    producer_a
        .send(Transaction {
            id: 7,
            sender: "charlie".into(),
            payload: vec![17, 18, 19],
            gas_limit: 25_000,
            gas_price: 30,
            nonce: 0,
        })
        .await?;

    let tx = consumer
        .next()
        .await
        .expect("expected transaction from stream");
    // Execute via the follower -- Raft forwards to the cluster leader
    follower_group
        .execute(MempoolCommand::AddTransaction(tx))
        .await?;

    let result = proposer_group
        .query(MempoolQuery::PendingCount, Consistency::Strong)
        .await?;
    if let MempoolQueryResult::Count(count) = result {
        tracing::info!(
            "after follower-forwarded tx: {count} pending (expected: 3, confirms forwarding works)"
        );
    }

    // =========================================================================
    // Phase 8: Verify cross-cluster state isolation
    // =========================================================================
    tracing::info!("--- Phase 8: Cross-cluster state isolation ---");
    tracing::info!("each cluster has its own GroupKey = independent Raft group = isolated mempool");

    // Query cluster B's mempool -- should be empty since only cluster A received transactions
    let cluster_b_result = cluster_b.groups[0]
        .query(MempoolQuery::PendingCount, Consistency::Strong)
        .await?;
    if let MempoolQueryResult::Count(count) = cluster_b_result {
        tracing::info!(
            "cluster B pending: {count} (expected: 0, isolated from cluster A)"
        );
    }

    let cluster_c_result = cluster_c.groups[0]
        .query(MempoolQuery::PendingCount, Consistency::Strong)
        .await?;
    if let MempoolQueryResult::Count(count) = cluster_c_result {
        tracing::info!(
            "cluster C pending: {count} (expected: 0, isolated from cluster A)"
        );
    }

    // =========================================================================
    // Phase 9: Pipeline pre-connection with cluster-level tagging
    // =========================================================================
    tracing::info!("--- Phase 9: Pipeline pre-connection with cluster-level tags ---");
    tracing::info!(
        "waiting for 2 slot rotations to observe cluster-level tag pipeline..."
    );
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;

    // Verify tags across all clusters
    let tag_proposer = Tag::from(leader::TAG_PROPOSER);
    let tag_next = Tag::from(leader::TAG_PROPOSER_NEXT);
    let tag_soon = Tag::from(leader::TAG_PROPOSER_SOON);
    let tag_labels: [(Tag, &str); 3] = [
        (tag_proposer, "proposer"),
        (tag_next, "proposer-next"),
        (tag_soon, "proposer-soon"),
    ];

    for (cluster_name, cluster) in
        [("A", &cluster_a), ("B", &cluster_b), ("C", &cluster_c)]
    {
        for (node_idx, net) in cluster.networks.iter().enumerate() {
            let me: PeerEntry = net.discovery().me().into_unsigned();
            let tags: Vec<&str> = tag_labels
                .iter()
                .filter(|(t, _)| me.tags().contains(t))
                .map(|(_, label)| *label)
                .collect();
            if !tags.is_empty() {
                tracing::info!(
                    "  cluster {cluster_name} node {node_idx}: {}",
                    tags.join(", ")
                );
            }
        }
    }

    // Demonstrate how tx-source consumers use pipeline predicates
    // that match any node in any proposer-tagged cluster
    let proposer_tag_clone = Tag::from(leader::TAG_PROPOSER);
    let next_tag_clone = Tag::from(leader::TAG_PROPOSER_NEXT);
    let soon_tag_clone = Tag::from(leader::TAG_PROPOSER_SOON);
    let _pipeline_consumer = tx_source_a
        .streams()
        .consumer::<Transaction>()
        .subscribe_if(move |peer: &PeerEntry| {
            peer.tags().contains(&proposer_tag_clone)
                || peer.tags().contains(&next_tag_clone)
                || peer.tags().contains(&soon_tag_clone)
        })
        .build();
    tracing::info!("pipeline consumer created with cluster-aware 3-tier proposer predicate");
    tracing::info!(
        "benefit: all 3 nodes in the proposer cluster are tagged, so streams survive \
         intra-cluster Raft failover without reconnection"
    );

    tracing::info!("--- Demo complete ---");
    tracing::info!("summary:");
    tracing::info!("  - 3 validator clusters x 3 nodes = 9 total Mosaik nodes");
    tracing::info!("  - each cluster: independent Raft group with replicated mempool");
    tracing::info!("  - intra-cluster: failover via Raft, load balancing via Weak queries");
    tracing::info!("  - inter-cluster: proposer schedule with pipeline pre-connection");
    tracing::info!("  - stream stability: all cluster nodes share tags, surviving failover");

    Ok(())
}
