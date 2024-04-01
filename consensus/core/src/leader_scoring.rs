// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::{
    cmp::Ordering,
    collections::{BTreeMap, HashMap},
    fmt::Debug,
    ops::Bound::{Excluded, Included},
    sync::Arc,
};

use consensus_config::AuthorityIndex;

use crate::{
    base_committer::BaseCommitter,
    block::{BlockAPI, BlockDigest, BlockRef, Slot, VerifiedBlock},
    commit::CommitRange,
    context::Context,
    stake_aggregator::{QuorumThreshold, StakeAggregator},
    universal_committer::UniversalCommitter,
    CommittedSubDag, Round,
};

pub(crate) struct ReputationScoreCalculator<'a> {
    context: Arc<Context>,
    unscored_blocks: BTreeMap<BlockRef, VerifiedBlock>,
    committer: &'a UniversalCommitter,
    pub commit_range: CommitRange,
    pub scores_per_authority: Vec<u64>,
}

impl<'a> ReputationScoreCalculator<'a> {
    pub(crate) fn new(
        context: Arc<Context>,
        committer: &'a UniversalCommitter,
        unscored_subdags: &Vec<CommittedSubDag>,
    ) -> Self {
        let num_authorities = context.committee.size();
        let scores_per_authority = vec![0_u64; num_authorities];

        let unscored_blocks = unscored_subdags
            .iter()
            .flat_map(|subdag| subdag.blocks.iter())
            .map(|block| (block.reference(), block.clone()))
            .collect::<BTreeMap<_, _>>();

        assert!(
            !unscored_subdags.is_empty(),
            "Attempted to calculate scores with no unscored subdags"
        );
        let commit_indexes = unscored_subdags
            .iter()
            .map(|subdag| subdag.commit_index)
            .collect::<Vec<_>>();
        let min_commit_index = *commit_indexes.iter().min().unwrap();
        let max_commit_index = *commit_indexes.iter().max().unwrap();
        let commit_range = CommitRange::new(min_commit_index..max_commit_index);

        Self {
            context,
            unscored_blocks,
            committer,
            commit_range,
            scores_per_authority,
        }
    }

    pub(crate) fn calculate(&mut self) -> ReputationScores {
        assert!(
            !self.unscored_blocks.is_empty(),
            "Attempted to calculate scores with no blocks from unscored subdags"
        );
        let leader_rounds = self
            .unscored_blocks
            .keys()
            .map(|block_ref| block_ref.round)
            .filter(|round| *round != 0); // Skip genesis round
        let min_leader_round = leader_rounds.clone().min().unwrap();
        let max_leader_round = leader_rounds.clone().max().unwrap();

        // We will search for certificates for leaders up to R - 3.
        for leader_round in min_leader_round..=(max_leader_round - 3) {
            for committer in self.committer.committers.iter() {
                tracing::info!(
                    "Electing leader for round {leader_round} with committer {committer}"
                );
                if let Some(leader_slot) = committer.elect_leader(leader_round) {
                    tracing::info!("Calculating score for leader {leader_slot}");
                    self.calculate_scores_for_leader(leader_slot, committer);
                }
            }
        }

        ReputationScores::new(self.commit_range.clone(), self.scores_per_authority.clone())
    }

    pub(crate) fn calculate_scores_for_leader(
        &mut self,
        leader_slot: Slot,
        committer: &BaseCommitter,
    ) {
        let wave = committer.wave_number(leader_slot.round);
        let decision_round = committer.decision_round(wave);

        let leader_blocks = self.get_blocks_at_slot(leader_slot);

        if leader_blocks.is_empty() {
            tracing::info!("[{}] No block for leader slot {leader_slot} in this set of unscored committed subdags, skip scoring", self.context.own_index);
            return;
        }

        // At this point we are guaranteed that there is only one leader per slot
        // because we are operating on committed subdags.
        assert!(leader_blocks.len() == 1);

        let leader_block = leader_blocks.first().unwrap();

        // TODO(arun): move to a separate "scoring strategy" method. Will need to do points
        // for votes connected to certificates (certified vote). Can experiment with
        // point per certificate or 1 point per 2f+1 certs
        let decision_blocks = self.get_blocks_at_round(decision_round);
        let mut all_votes = HashMap::new();
        for potential_cert in decision_blocks {
            let authority = potential_cert.reference().author;
            if self.is_certificate(&potential_cert, leader_block, &mut all_votes) {
                tracing::info!(
                    "Found a certificate for leader {leader_block} from authority {authority}"
                );
                tracing::info!(
                    "[{}] scores +1 reputation for {authority}!",
                    self.context.own_index
                );
                self.add_score(authority, 1);
            }
        }
    }

    /// Adds the provided `score` to the existing score for the provided `authority`
    fn add_score(&mut self, authority_idx: AuthorityIndex, score: u64) {
        self.scores_per_authority[authority_idx] += score;
    }

    fn find_supported_block(&self, leader_slot: Slot, from: &VerifiedBlock) -> Option<BlockRef> {
        if from.round() < leader_slot.round {
            return None;
        }
        for ancestor in from.ancestors() {
            if Slot::from(*ancestor) == leader_slot {
                return Some(*ancestor);
            }
            // Weak links may point to blocks with lower round numbers than strong links.
            if ancestor.round <= leader_slot.round {
                continue;
            }
            if let Some(ancestor) = self.get_block(ancestor) {
                if let Some(support) = self.find_supported_block(leader_slot, &ancestor) {
                    return Some(support);
                }
            } else {
                // TODO(arun): Add unit test for this case.
                tracing::info!(
                    "Potential vote's ancestor block not found in unscored committed subdags: {:?}",
                    ancestor
                );
                return None;
            }
        }
        None
    }

    fn is_vote(&self, potential_vote: &VerifiedBlock, leader_block: &VerifiedBlock) -> bool {
        let reference = leader_block.reference();
        let leader_slot = Slot::from(reference);
        self.find_supported_block(leader_slot, potential_vote) == Some(reference)
    }

    fn is_certificate(
        &self,
        potential_certificate: &VerifiedBlock,
        leader_block: &VerifiedBlock,
        all_votes: &mut HashMap<BlockRef, bool>,
    ) -> bool {
        let mut votes_stake_aggregator = StakeAggregator::<QuorumThreshold>::new();
        for reference in potential_certificate.ancestors() {
            let is_vote = if let Some(is_vote) = all_votes.get(reference) {
                *is_vote
            } else if let Some(potential_vote) = self.get_block(reference) {
                let is_vote = self.is_vote(&potential_vote, leader_block);
                all_votes.insert(*reference, is_vote);
                is_vote
            } else {
                tracing::info!(
                    "Potential vote not found in unscored committed subdags: {:?}",
                    reference
                );
                false
            };

            if is_vote {
                tracing::trace!("{reference} is a vote for {leader_block}");
                if votes_stake_aggregator.add(reference.author, &self.context.committee) {
                    tracing::trace!(
                        "{potential_certificate} is a certificate for leader {leader_block}"
                    );
                    return true;
                }
            } else {
                tracing::trace!("{reference} is not a vote for {leader_block}",);
            }
        }
        tracing::trace!("{potential_certificate} is not a certificate for leader {leader_block}");
        false
    }

    fn get_blocks_at_slot(&self, slot: Slot) -> Vec<VerifiedBlock> {
        let mut blocks = vec![];
        for (_block_ref, block) in self.unscored_blocks.range((
            Included(BlockRef::new(slot.round, slot.authority, BlockDigest::MIN)),
            Included(BlockRef::new(slot.round, slot.authority, BlockDigest::MAX)),
        )) {
            blocks.push(block.clone())
        }
        blocks
    }

    fn get_blocks_at_round(&self, round: Round) -> Vec<VerifiedBlock> {
        let mut blocks = vec![];
        for (_block_ref, block) in self.unscored_blocks.range((
            Included(BlockRef::new(round, AuthorityIndex::ZERO, BlockDigest::MIN)),
            Excluded(BlockRef::new(
                round + 1,
                AuthorityIndex::ZERO,
                BlockDigest::MIN,
            )),
        )) {
            blocks.push(block.clone())
        }
        blocks
    }

    fn get_block(&self, block_ref: &BlockRef) -> Option<VerifiedBlock> {
        self.unscored_blocks.get(block_ref).cloned()
    }
}

#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub(crate) struct ReputationScores {
    /// Score per authority. Vec index is the AuthorityIndex.
    pub scores_per_authority: Vec<u64>,
    // The range of commits these scores were calculated from.
    pub commit_range: CommitRange,
}

impl ReputationScores {
    pub(crate) fn new(commit_range: CommitRange, scores_per_authority: Vec<u64>) -> Self {
        Self {
            scores_per_authority,
            commit_range,
        }
    }

    // Returns the authorities in score descending order.
    pub fn authorities_by_score_desc(&self, context: Arc<Context>) -> Vec<(AuthorityIndex, u64)> {
        let mut authorities: Vec<_> = self
            .scores_per_authority
            .iter()
            .enumerate()
            .map(|(index, score)| {
                (
                    context
                        .committee
                        .to_authority_index(index)
                        .expect("Should be a valid AuthorityIndex"),
                    *score,
                )
            })
            .collect();

        authorities.sort_by(|a1, a2| {
            match a2.1.cmp(&a1.1) {
                Ordering::Equal => {
                    // we resolve the score equality deterministically by ordering in authority
                    // identifier order descending.
                    a2.0.cmp(&a1.0)
                }
                result => result,
            }
        });

        authorities
    }

    pub(crate) fn update_metrics(&self, context: Arc<Context>) {
        let authorities = self.authorities_by_score_desc(context.clone());
        for (authority_index, score) in authorities {
            let authority = context.committee.authority(authority_index);
            if !authority.hostname.is_empty() {
                context
                    .metrics
                    .node_metrics
                    .reputation_scores
                    .with_label_values(&[&authority.hostname])
                    .set(score as i64);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use parking_lot::RwLock;

    use super::*;
    use crate::{
        block::{timestamp_utc_ms, BlockTimestampMs, TestBlock},
        dag_state::DagState,
        leader_schedule::{LeaderSchedule, LeaderSwapTable},
        storage::mem_store::MemStore,
        universal_committer::universal_committer_builder::UniversalCommitterBuilder,
    };

    #[test]
    fn test_reputation_scores_authorities_by_score_desc() {
        let context = Arc::new(Context::new_for_test(4).0);
        let scores = ReputationScores::new(CommitRange::new(1..300), vec![4, 1, 1, 3]);
        let authorities = scores.authorities_by_score_desc(context);
        assert_eq!(
            authorities,
            vec![
                (AuthorityIndex::new_for_test(0), 4),
                (AuthorityIndex::new_for_test(3), 3),
                (AuthorityIndex::new_for_test(2), 1),
                (AuthorityIndex::new_for_test(1), 1)
            ]
        );
    }

    #[test]
    fn test_reputation_scores_update_metrics() {
        let context = Arc::new(Context::new_for_test(4).0);
        let scores = ReputationScores::new(CommitRange::new(1..300), vec![1, 2, 4, 3]);
        scores.update_metrics(context.clone());
        let metrics = context.metrics.node_metrics.reputation_scores.clone();
        assert_eq!(
            metrics
                .get_metric_with_label_values(&["test_host_0"])
                .unwrap()
                .get(),
            1
        );
        assert_eq!(
            metrics
                .get_metric_with_label_values(&["test_host_1"])
                .unwrap()
                .get(),
            2
        );
        assert_eq!(
            metrics
                .get_metric_with_label_values(&["test_host_2"])
                .unwrap()
                .get(),
            4
        );
        assert_eq!(
            metrics
                .get_metric_with_label_values(&["test_host_3"])
                .unwrap()
                .get(),
            3
        );
    }

    #[test]
    fn test_reputation_score_calculator() {
        telemetry_subscribers::init_for_testing();
        let context = Arc::new(Context::new_for_test(4).0);
        let leader_schedule = Arc::new(LeaderSchedule::new(
            context.clone(),
            LeaderSwapTable::default(),
        ));
        let dag_state = Arc::new(RwLock::new(DagState::new(
            context.clone(),
            Arc::new(MemStore::new()),
        )));
        let committer = UniversalCommitterBuilder::new(
            context.clone(),
            leader_schedule.clone(),
            dag_state.clone(),
        )
        .with_pipeline(true)
        .build();

        // Populate fully connected test blocks for round 0 ~ 4, authorities 0 ~ 3.
        let max_round: u32 = 4;
        let num_authorities: u32 = 4;

        let mut blocks = Vec::new();
        let (genesis_references, genesis): (Vec<_>, Vec<_>) = context
            .committee
            .authorities()
            .map(|index| {
                let author_idx = index.0.value() as u32;
                let block = TestBlock::new(0, author_idx).build();
                VerifiedBlock::new_for_test(block)
            })
            .map(|block| (block.reference(), block))
            .unzip();
        blocks.extend(genesis);

        let mut ancestors = genesis_references;
        let mut leader = None;
        for round in 1..=max_round {
            let mut new_ancestors = vec![];
            for author in 0..num_authorities {
                let base_ts = round as BlockTimestampMs * 1000;
                let block = VerifiedBlock::new_for_test(
                    TestBlock::new(round, author)
                        .set_timestamp_ms(base_ts + (author + round) as u64)
                        .set_ancestors(ancestors.clone())
                        .build(),
                );
                new_ancestors.push(block.reference());
                blocks.push(block.clone());

                // only write one block for the final round, which is the leader
                // of the committed subdag.
                if round == max_round {
                    leader = Some(block.clone());
                    break;
                }
            }
            ancestors = new_ancestors;
        }

        let leader_block = leader.unwrap();
        let leader_ref = leader_block.reference();
        let commit_index = 1;

        let unscored_subdags = vec![CommittedSubDag::new(
            leader_ref,
            blocks,
            timestamp_utc_ms(),
            commit_index,
        )];
        let mut calculator =
            ReputationScoreCalculator::new(context.clone(), &committer, &unscored_subdags);
        let scores = calculator.calculate();
        assert_eq!(scores.scores_per_authority, vec![1, 1, 1, 1]);
        assert_eq!(scores.commit_range, CommitRange::new(1..1));
    }

    #[test]
    #[should_panic(expected = "Attempted to calculate scores with no unscored subdags")]
    fn test_reputation_score_calculator_no_subdags() {
        telemetry_subscribers::init_for_testing();
        let context = Arc::new(Context::new_for_test(4).0);
        let leader_schedule = Arc::new(LeaderSchedule::new(
            context.clone(),
            LeaderSwapTable::default(),
        ));
        let dag_state = Arc::new(RwLock::new(DagState::new(
            context.clone(),
            Arc::new(MemStore::new()),
        )));
        let committer = UniversalCommitterBuilder::new(
            context.clone(),
            leader_schedule.clone(),
            dag_state.clone(),
        )
        .with_pipeline(true)
        .build();

        let unscored_subdags = vec![];
        let mut calculator =
            ReputationScoreCalculator::new(context.clone(), &committer, &unscored_subdags);
        let scores = calculator.calculate();
        assert_eq!(scores.scores_per_authority, vec![0, 0, 0, 0]);
        assert_eq!(scores.commit_range, CommitRange::new(0..0));
    }

    #[test]
    #[should_panic(expected = "Attempted to calculate scores with no blocks from unscored subdags")]
    fn test_reputation_score_calculator_no_subdag_blocks() {
        telemetry_subscribers::init_for_testing();
        let context = Arc::new(Context::new_for_test(4).0);
        let leader_schedule = Arc::new(LeaderSchedule::new(
            context.clone(),
            LeaderSwapTable::default(),
        ));
        let dag_state = Arc::new(RwLock::new(DagState::new(
            context.clone(),
            Arc::new(MemStore::new()),
        )));
        let committer = UniversalCommitterBuilder::new(
            context.clone(),
            leader_schedule.clone(),
            dag_state.clone(),
        )
        .with_pipeline(true)
        .build();

        let blocks = vec![];
        let unscored_subdags = vec![CommittedSubDag::new(
            BlockRef::new(1, AuthorityIndex::ZERO, BlockDigest::MIN),
            blocks,
            timestamp_utc_ms(),
            1,
        )];
        let mut calculator =
            ReputationScoreCalculator::new(context.clone(), &committer, &unscored_subdags);
        calculator.calculate();
    }

    #[test]
    fn test_scoring_with_missing_block_in_subdag() {
        telemetry_subscribers::init_for_testing();
        let context = Arc::new(Context::new_for_test(4).0);
        let leader_schedule = Arc::new(LeaderSchedule::new(
            context.clone(),
            LeaderSwapTable::default(),
        ));
        let dag_state = Arc::new(RwLock::new(DagState::new(
            context.clone(),
            Arc::new(MemStore::new()),
        )));
        let committer = UniversalCommitterBuilder::new(
            context.clone(),
            leader_schedule.clone(),
            dag_state.clone(),
        )
        .with_pipeline(true)
        .build();

        let mut blocks = Vec::new();
        let (genesis_references, genesis): (Vec<_>, Vec<_>) = context
            .committee
            .authorities()
            .map(|index| {
                let author_idx = index.0.value() as u32;
                let block = TestBlock::new(0, author_idx).build();
                VerifiedBlock::new_for_test(block)
            })
            .map(|block| (block.reference(), block))
            .unzip();
        blocks.extend(genesis);

        let mut ancestors = genesis_references;
        let mut leader = None;
        for round in 1..=4 {
            let mut new_ancestors = vec![];
            for author in 0..4 {
                let base_ts = round as BlockTimestampMs * 1000;
                let block = VerifiedBlock::new_for_test(
                    TestBlock::new(round, author)
                        .set_timestamp_ms(base_ts + (author + round) as u64)
                        .set_ancestors(ancestors.clone())
                        .build(),
                );
                new_ancestors.push(block.reference());

                // Simulate referenced block which was part of another committed
                // subdag.
                if round == 1 && author == 0 {
                    tracing::info!("Skipping {block} in committed subdags blocks");
                    continue;
                }

                blocks.push(block.clone());

                if round == 4 && author == 0 {
                    leader = Some(block.clone());
                    break;
                }
            }
            ancestors = new_ancestors;
        }

        let leader_block = leader.unwrap();
        let leader_ref = leader_block.reference();
        let commit_index = 1;

        let unscored_subdags = vec![CommittedSubDag::new(
            leader_ref,
            blocks,
            timestamp_utc_ms(),
            commit_index,
        )];
        let mut calculator =
            ReputationScoreCalculator::new(context.clone(), &committer, &unscored_subdags);
        let scores = calculator.calculate();
        assert_eq!(scores.scores_per_authority, vec![1, 1, 1, 1]);
        assert_eq!(scores.commit_range, CommitRange::new(1..1));
    }
}
