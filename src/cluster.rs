use {
    crate::mempool::MempoolStateMachine,
    mosaik::{
        discovery::{Discovery, PeerEntry},
        groups::When,
        primitives::Tag,
        *,
    },
};

/// A validator cluster is a set of Mosaik nodes sharing a Raft group.
///
/// Each validator in the CometBFT-style system is not a single node but a
/// multi-node Raft cluster. This provides:
/// - **Failover**: If the cluster's Raft leader dies, a follower takes over
///   automatically via Raft leader election.
/// - **Load balancing**: Read queries can be served by followers using
///   `Consistency::Weak`.
/// - **Stream stability**: All nodes in the cluster share the same tags
///   (e.g., "proposer"), so transaction source streams remain connected
///   even during intra-cluster failover.
#[allow(dead_code)]
pub struct ValidatorCluster {
    pub name: String,
    pub networks: Vec<Network>,
    pub groups: Vec<Group<MempoolStateMachine>>,
    pub validator_tag: Tag,
}

#[allow(dead_code)]
impl ValidatorCluster {
    /// Create a new validator cluster with `size` nodes.
    ///
    /// All nodes join the same Raft group (same `group_key`) and share a
    /// common validator tag (e.g., "validator-a"). Each node gets its own
    /// `MempoolStateMachine` instance that Raft keeps in sync.
    pub async fn new(
        name: &str,
        size: usize,
        network_id: NetworkId,
        group_key: GroupKey,
        max_block_size: usize,
        max_block_gas: u64,
    ) -> anyhow::Result<Self> {
        let validator_tag = Tag::from(format!("validator-{name}").as_str());

        let mut networks = Vec::new();
        for _ in 0..size {
            let net = Network::builder(network_id)
                .with_discovery(
                    mosaik::discovery::Config::builder().with_tags(validator_tag),
                )
                .build()
                .await?;
            networks.push(net);
        }

        // Cross-discover within the cluster so all nodes find each other
        for i in 0..networks.len() {
            for j in 0..networks.len() {
                if i != j {
                    networks[i]
                        .discovery()
                        .sync_with(networks[j].local().addr())
                        .await?;
                }
            }
        }

        // All nodes join the same Raft group
        let mut groups = Vec::new();
        for net in &networks {
            let group = net
                .groups()
                .with_key(group_key)
                .with_state_machine(MempoolStateMachine::new(max_block_size, max_block_gas))
                .join();
            groups.push(group);
        }

        Ok(Self {
            name: name.to_string(),
            networks,
            groups,
            validator_tag,
        })
    }

    /// Wait for the cluster's Raft group to come online on all nodes.
    pub async fn wait_online(&self) {
        for group in &self.groups {
            group.when().online().await;
        }
    }

    /// Get the cluster leader's group handle, if a leader is elected.
    pub fn leader_group(&self) -> Option<&Group<MempoolStateMachine>> {
        self.groups.iter().find(|g| g.is_leader())
    }

    /// Get the index of the node that is currently the Raft leader.
    pub fn leader_index(&self) -> Option<usize> {
        self.groups.iter().position(|g| g.is_leader())
    }

    /// Add a tag to ALL nodes in the cluster (e.g., "proposer").
    ///
    /// This is the key mechanism for stream stability: since all nodes share
    /// the tag, tx source `subscribe_if` predicates match any node in the
    /// cluster. If the Raft leader fails, streams remain connected to the
    /// surviving nodes.
    pub fn add_tag_to_all(&self, tag: Tag) -> anyhow::Result<()> {
        for net in &self.networks {
            let entry: PeerEntry = net.discovery().me().into_unsigned();
            let updated = entry.add_tags(tag);
            let signed = updated.sign(net.local().secret_key())?;
            net.discovery().feed(signed);
        }
        Ok(())
    }

    /// Remove a tag from ALL nodes in the cluster.
    pub fn remove_tag_from_all(&self, tag: Tag) -> anyhow::Result<()> {
        for net in &self.networks {
            let entry: PeerEntry = net.discovery().me().into_unsigned();
            let updated = entry.remove_tags(tag);
            let signed = updated.sign(net.local().secret_key())?;
            net.discovery().feed(signed);
        }
        Ok(())
    }

    /// Cross-discover with another cluster (inter-cluster discovery).
    pub async fn discover_cluster(&self, other: &ValidatorCluster) -> anyhow::Result<()> {
        for my_net in &self.networks {
            for other_net in &other.networks {
                my_net
                    .discovery()
                    .sync_with(other_net.local().addr())
                    .await?;
            }
        }
        Ok(())
    }

    /// Discover individual networks (e.g., tx source nodes).
    pub async fn discover_networks(&self, others: &[&Network]) -> anyhow::Result<()> {
        for my_net in &self.networks {
            for other_net in others {
                my_net
                    .discovery()
                    .sync_with(other_net.local().addr())
                    .await?;
            }
        }
        Ok(())
    }

    /// Extract cloneable handles for all nodes in the cluster.
    ///
    /// Returns (Discovery, SecretKey) pairs that can be moved into spawned
    /// tasks for tag management.
    pub fn discovery_handles(&self) -> Vec<(Discovery, SecretKey)> {
        self.networks
            .iter()
            .map(|net| (net.discovery().clone(), net.local().secret_key().clone()))
            .collect()
    }

    /// Extract When handles for spawning intra-cluster leader tracking tasks.
    pub fn when_handles(&self) -> Vec<When> {
        self.groups.iter().map(|g| g.when().clone()).collect()
    }
}
