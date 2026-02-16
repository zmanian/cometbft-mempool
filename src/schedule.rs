use {mosaik::PeerId, std::time::Duration};

/// Information about a validator participating in the proposer schedule.
#[derive(Debug, Clone)]
struct ValidatorState {
    peer_id: PeerId,
    voting_power: u64,
}

/// Deterministic weighted round-robin proposer schedule.
///
/// Implements CometBFT's proposer selection algorithm: each round, all
/// validators increase their accumulated priority by their voting power, the
/// validator with the highest priority is selected as proposer, and their
/// priority is decreased by the total voting power of the set.
///
/// Because the algorithm is deterministic, every node can independently compute
/// the proposer for any slot without communication.
#[derive(Debug, Clone)]
pub struct ValidatorSchedule {
    validators: Vec<ValidatorState>,
    slot_duration: Duration,
    start_time: tokio::time::Instant,
}

impl ValidatorSchedule {
    /// Create a new schedule from a list of (PeerId, voting_power) pairs.
    ///
    /// `slot_duration` controls how long each proposer slot lasts. For fast BFT
    /// rotation this is typically 500ms.
    pub fn new(validators: Vec<(PeerId, u64)>, slot_duration: Duration) -> Self {
        let states = validators
            .into_iter()
            .map(|(peer_id, voting_power)| ValidatorState {
                peer_id,
                voting_power,
            })
            .collect();

        Self {
            validators: states,
            slot_duration,
            start_time: tokio::time::Instant::now(),
        }
    }

    /// Compute the proposer for a given slot using weighted round-robin.
    ///
    /// Algorithm (applied `slot + 1` times starting from priority-zero state):
    /// 1. Add `voting_power` to each validator's `accumulated_priority`
    /// 2. Select the validator with the highest `accumulated_priority`
    ///    (ties broken by lowest index)
    /// 3. Subtract `total_voting_power` from the selected validator's priority
    pub fn proposer_at(&self, slot: u64) -> PeerId {
        let total_power: i64 = self.validators.iter().map(|v| v.voting_power as i64).sum();

        let mut priorities: Vec<i64> = vec![0; self.validators.len()];

        let mut proposer_idx = 0;
        for _ in 0..=slot {
            // Step 1: add voting power to all priorities
            for (i, v) in self.validators.iter().enumerate() {
                priorities[i] += v.voting_power as i64;
            }

            // Step 2: find highest priority (lowest index breaks ties)
            proposer_idx = 0;
            let mut max_priority = priorities[0];
            for (i, &p) in priorities.iter().enumerate().skip(1) {
                if p > max_priority {
                    max_priority = p;
                    proposer_idx = i;
                }
            }

            // Step 3: subtract total power from selected
            priorities[proposer_idx] -= total_power;
        }

        self.validators[proposer_idx].peer_id
    }

    /// The duration of each proposer slot.
    pub fn slot_duration(&self) -> Duration {
        self.slot_duration
    }

    /// Current slot based on elapsed time since the schedule started.
    pub fn current_slot(&self) -> u64 {
        let elapsed = self.start_time.elapsed();
        (elapsed.as_millis() / self.slot_duration.as_millis()) as u64
    }

    /// Return the proposers for the next `n` slots starting from the current slot.
    ///
    /// Returns pairs of (slot_number, proposer_peer_id).
    pub fn upcoming_proposers(&self, n: usize) -> Vec<(u64, PeerId)> {
        let current = self.current_slot();
        (current..current + n as u64)
            .map(|slot| (slot, self.proposer_at(slot)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mosaik::SecretKey;

    fn make_peer_id() -> PeerId {
        SecretKey::generate(&mut rand::rng()).public()
    }

    #[test]
    fn equal_weight_round_robin() {
        let a = make_peer_id();
        let b = make_peer_id();
        let c = make_peer_id();

        let schedule =
            ValidatorSchedule::new(vec![(a, 1), (b, 1), (c, 1)], Duration::from_millis(500));

        // With equal weights, should cycle a, b, c, a, b, c, ...
        let first_three: Vec<PeerId> = (0..3).map(|s| schedule.proposer_at(s)).collect();
        // All three should be different
        assert_ne!(first_three[0], first_three[1]);
        assert_ne!(first_three[1], first_three[2]);
        assert_ne!(first_three[0], first_three[2]);

        // The cycle should repeat
        assert_eq!(schedule.proposer_at(0), schedule.proposer_at(3));
        assert_eq!(schedule.proposer_at(1), schedule.proposer_at(4));
        assert_eq!(schedule.proposer_at(2), schedule.proposer_at(5));
    }

    #[test]
    fn weighted_proposer_frequency() {
        let a = make_peer_id();
        let b = make_peer_id();

        // a has 2x the voting power of b
        let schedule =
            ValidatorSchedule::new(vec![(a, 2), (b, 1)], Duration::from_millis(500));

        // Over 3 slots, a should be proposer twice and b once
        let proposers: Vec<PeerId> = (0..3).map(|s| schedule.proposer_at(s)).collect();
        let a_count = proposers.iter().filter(|&&p| p == a).count();
        let b_count = proposers.iter().filter(|&&p| p == b).count();
        assert_eq!(a_count, 2);
        assert_eq!(b_count, 1);
    }

    #[test]
    fn deterministic_across_calls() {
        let a = make_peer_id();
        let b = make_peer_id();
        let c = make_peer_id();

        let schedule =
            ValidatorSchedule::new(vec![(a, 3), (b, 2), (c, 1)], Duration::from_millis(500));

        // Same slot always returns same proposer
        for slot in 0..20 {
            assert_eq!(schedule.proposer_at(slot), schedule.proposer_at(slot));
        }
    }
}
