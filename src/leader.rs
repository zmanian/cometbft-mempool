use {
    crate::schedule::ValidatorSchedule,
    mosaik::{
        *,
        discovery::{Discovery, PeerEntry},
        groups::When,
        primitives::Tag,
    },
};

/// Tags for pipeline pre-connection:
/// - "proposer"      = current slot's proposer (actively building blocks)
/// - "proposer-next" = next slot's proposer (pre-warming connections)
/// - "proposer-soon" = slot+2 proposer (establishing connections)
pub const TAG_PROPOSER: &str = "proposer";
pub const TAG_PROPOSER_NEXT: &str = "proposer-next";
pub const TAG_PROPOSER_SOON: &str = "proposer-soon";

/// All proposer-related tags, used for cleanup.
const PROPOSER_TAGS: [&str; 3] = [TAG_PROPOSER, TAG_PROPOSER_NEXT, TAG_PROPOSER_SOON];

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
/// next proposer is without any communication. Our Raft-based design uses
/// leader election with discovery tags for reactive leader-aware routing.
pub async fn track_leader_raft(
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

/// Schedule-aware leader tracking with pipeline pre-connection.
///
/// Instead of reacting to Raft leadership changes (which takes 100-400ms),
/// this function uses the deterministic validator schedule to pre-tag upcoming
/// proposers BEFORE their slot starts. This gives Mosaik's subscribe_if
/// predicates time to establish stream connections in advance.
///
/// On each slot tick, this node checks whether it is any of:
/// - Current proposer (tag: "proposer")
/// - Next proposer (tag: "proposer-next")
/// - Soon proposer, slot+2 (tag: "proposer-soon")
///
/// Tags are applied/removed accordingly, and stream consumers that match on
/// any of these tags will pre-connect to upcoming proposers.
pub async fn track_leader_scheduled(
    schedule: ValidatorSchedule,
    discovery: Discovery,
    secret_key: SecretKey,
    local_id: PeerId,
) -> anyhow::Result<()> {
    let mut interval = tokio::time::interval(schedule.slot_duration());
    let mut last_slot = u64::MAX; // sentinel: force update on first tick

    loop {
        interval.tick().await;

        let current_slot = schedule.current_slot();
        if current_slot == last_slot {
            continue;
        }
        last_slot = current_slot;

        let current_proposer = schedule.proposer_at(current_slot);
        let next_proposer = schedule.proposer_at(current_slot + 1);
        let soon_proposer = schedule.proposer_at(current_slot + 2);

        tracing::info!(
            slot = current_slot,
            current = %current_proposer,
            next = %next_proposer,
            soon = %soon_proposer,
            "slot transition"
        );

        // Determine which tags this node should have
        let should_be_proposer = current_proposer == local_id;
        let should_be_next = next_proposer == local_id;
        let should_be_soon = soon_proposer == local_id;

        // Start from current entry and remove all proposer tags, then add back
        // whichever ones apply.
        let entry: PeerEntry = discovery.me().into_unsigned();
        let mut updated = entry;

        // Remove all proposer-related tags first
        for tag_str in &PROPOSER_TAGS {
            updated = updated.remove_tags(Tag::from(*tag_str));
        }

        // Add back the tags this node should have
        if should_be_proposer {
            tracing::info!(slot = current_slot, "this node is the current proposer");
            updated = updated.add_tags(Tag::from(TAG_PROPOSER));
        }
        if should_be_next {
            tracing::info!(slot = current_slot, "this node is the next proposer");
            updated = updated.add_tags(Tag::from(TAG_PROPOSER_NEXT));
        }
        if should_be_soon {
            tracing::info!(slot = current_slot, "this node is the soon proposer (slot+2)");
            updated = updated.add_tags(Tag::from(TAG_PROPOSER_SOON));
        }

        let signed = updated.sign(&secret_key)?;
        discovery.feed(signed);
    }
}
