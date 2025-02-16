// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.
//
// Copyright (c) DUSK NETWORK. All rights reserved.

use std::sync::Arc;

use async_trait::async_trait;
use node_data::bls::PublicKeyBytes;
use node_data::ledger::to_str;
use node_data::message::payload::{Candidate, GetResource, Inv};
use node_data::message::{
    ConsensusHeader, Message, Payload, SignedStepMessage, StepMessage,
    WireMessage,
};
use tokio::sync::Mutex;
use tracing::info;

use crate::commons::{Database, RoundUpdate};
use crate::config::{
    is_emergency_iter, MAX_BLOCK_SIZE, MAX_NUMBER_OF_FAULTS,
    MAX_NUMBER_OF_TRANSACTIONS,
};
use crate::errors::ConsensusError;
use crate::iteration_ctx::RoundCommittees;
use crate::merkle::merkle_root;
use crate::msg_handler::{MsgHandler, StepOutcome};
use crate::user::committee::Committee;

pub struct ProposalHandler<D: Database> {
    pub(crate) db: Arc<Mutex<D>>,
}

#[async_trait]
impl<D: Database> MsgHandler for ProposalHandler<D> {
    /// Verifies if msg is a valid new_block message.
    fn verify(
        &self,
        msg: &Message,
        round_committees: &RoundCommittees,
    ) -> Result<(), ConsensusError> {
        let p = Self::unwrap_msg(msg)?;
        let iteration = p.header().iteration;
        let generator = round_committees
            .get_generator(iteration)
            .expect("committee to be created before run");
        super::handler::verify_candidate_msg(p, &generator)?;

        Ok(())
    }

    /// Collects а Candidate message.
    async fn collect(
        &mut self,
        msg: Message,
        _ru: &RoundUpdate,
        _committee: &Committee,
        _generator: Option<PublicKeyBytes>,
        _round_committees: &RoundCommittees,
    ) -> Result<StepOutcome, ConsensusError> {
        // store candidate block
        let p = Self::unwrap_msg(&msg)?;
        self.db
            .lock()
            .await
            .store_candidate_block(p.candidate.clone())
            .await;

        info!(
            event = "New Candidate",
            hash = &to_str(&p.candidate.header().hash),
            round = p.candidate.header().height,
            iter = p.candidate.header().iteration,
            prev_block = &to_str(&p.candidate.header().prev_block_hash)
        );

        Ok(StepOutcome::Ready(msg))
    }

    async fn collect_from_past(
        &mut self,
        msg: Message,
        _committee: &Committee,
        _generator: Option<PublicKeyBytes>,
    ) -> Result<StepOutcome, ConsensusError> {
        let p = Self::unwrap_msg(&msg)?;

        self.db
            .lock()
            .await
            .store_candidate_block(p.candidate.clone())
            .await;

        info!(
            event = "New Candidate",
            hash = &to_str(&p.candidate.header().hash),
            round = p.candidate.header().height,
            iter = p.candidate.header().iteration,
            prev_block = &to_str(&p.candidate.header().prev_block_hash)
        );

        Ok(StepOutcome::Ready(msg))
    }

    /// Handles of an event of step execution timeout
    fn handle_timeout(
        &self,
        ru: &RoundUpdate,
        curr_iteration: u8,
    ) -> Option<Message> {
        if is_emergency_iter(curr_iteration) {
            // In Emergency Mode we request the Candidate from our peers
            // in case we arrived late and missed the votes

            let prev_block_hash = ru.hash();
            let round = ru.round;

            info!(
                event = "request candidate block",
                src = "emergency_iter",
                iteration = curr_iteration,
                prev_block_hash = to_str(&ru.hash())
            );

            let mut inv = Inv::new(1);
            inv.add_candidate_from_iteration(ConsensusHeader {
                prev_block_hash,
                round,
                iteration: curr_iteration,
            });
            let msg = GetResource::new(inv, None, u64::MAX, 0);
            return Some(msg.into());
        }

        None
    }
}

impl<D: Database> ProposalHandler<D> {
    pub(crate) fn new(db: Arc<Mutex<D>>) -> Self {
        Self { db }
    }

    fn unwrap_msg(msg: &Message) -> Result<&Candidate, ConsensusError> {
        match &msg.payload {
            Payload::Candidate(c) => Ok(c),
            _ => Err(ConsensusError::InvalidMsgType),
        }
    }
}

fn verify_candidate_msg(
    p: &Candidate,
    expected_generator: &PublicKeyBytes,
) -> Result<(), ConsensusError> {
    if expected_generator != p.sign_info().signer.bytes() {
        return Err(ConsensusError::NotCommitteeMember);
    }

    let candidate_size = p
        .candidate
        .size()
        .map_err(|_| ConsensusError::UnknownBlockSize)?;
    if candidate_size > MAX_BLOCK_SIZE {
        return Err(ConsensusError::InvalidBlockSize(candidate_size));
    }

    // Verify msg signature
    p.verify_signature()?;

    if p.consensus_header().prev_block_hash
        != p.candidate.header().prev_block_hash
    {
        return Err(ConsensusError::InvalidBlockHash);
    }

    // INFO: we verify the transaction number and the merkle roots here because
    // the signature only includes the header's hash, making 'txs' and 'faults'
    // fields malleable from an adversary. We then discard blocks with errors
    // related to these fields rather than propagating the message and vote
    // Invalid

    // Check number of transactions
    if p.candidate.txs().len() > MAX_NUMBER_OF_TRANSACTIONS {
        return Err(ConsensusError::TooManyTransactions(
            p.candidate.txs().len(),
        ));
    }

    // Verify tx_root
    let tx_digests: Vec<_> =
        p.candidate.txs().iter().map(|t| t.digest()).collect();
    let tx_root = merkle_root(&tx_digests[..]);
    if tx_root != p.candidate.header().txroot {
        return Err(ConsensusError::InvalidBlock);
    }

    // Check number of faults
    if p.candidate.faults().len() > MAX_NUMBER_OF_FAULTS {
        return Err(ConsensusError::TooManyFaults(p.candidate.faults().len()));
    }

    // Verify fault_root
    let fault_digests: Vec<_> =
        p.candidate.faults().iter().map(|t| t.digest()).collect();
    let fault_root = merkle_root(&fault_digests[..]);
    if fault_root != p.candidate.header().faultroot {
        return Err(ConsensusError::InvalidBlock);
    }

    Ok(())
}

pub fn verify_stateless(
    c: &Candidate,
    round_committees: &RoundCommittees,
) -> Result<(), ConsensusError> {
    let iteration = c.header().iteration;
    let generator = round_committees
        .get_generator(iteration)
        .expect("committee to be created before run");
    verify_candidate_msg(c, &generator)?;

    Ok(())
}
