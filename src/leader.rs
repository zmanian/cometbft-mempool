use mosaik::{
    *,
    discovery::{Discovery, PeerEntry},
    groups::When,
    primitives::Tag,
};

/// Tracks leader transitions for a Raft group and updates the "proposer" tag
/// on the local node's discovery entry accordingly.
///
/// This function runs forever and is intended to be spawned as a tokio task.
/// When the local node becomes leader, it adds the "proposer" tag.
/// When the local node becomes a follower, it removes the "proposer" tag.
///
/// Takes cloneable handles so it can be spawned as an independent task.
///
/// NOTE: In real CometBFT, the proposer rotates deterministically each block
/// height using a weighted round-robin algorithm. All validators know who the
/// next proposer is without any communication. Our design instead uses Raft
/// leader election with discovery tags, which is a novel optimization that
/// enables dynamic leader-aware routing: tx-source nodes can use subscribe_if
/// predicates to stream transactions directly to the current leader, avoiding
/// the gossip flooding that CometBFT uses for mempool propagation.
pub async fn track_leader(
    discovery: Discovery,
    secret_key: SecretKey,
    when: When,
) -> anyhow::Result<()> {
    let proposer_tag = Tag::from("proposer");

    loop {
        // Wait until this node becomes leader
        when.is_leader().await;

        tracing::info!("local node became leader, adding proposer tag");
        let entry: PeerEntry = discovery.me().into_unsigned();
        let updated = entry.add_tags(proposer_tag);
        let signed = updated.sign(&secret_key)?;
        discovery.feed(signed);

        // Wait until this node loses leadership
        when.is_follower().await;

        tracing::info!("local node lost leadership, removing proposer tag");
        let entry: PeerEntry = discovery.me().into_unsigned();
        let updated = entry.remove_tags(proposer_tag);
        let signed = updated.sign(&secret_key)?;
        discovery.feed(signed);
    }
}
