// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.
//
// Copyright (c) DUSK NETWORK. All rights reserved.

use crate::commons::{ConsensusError, Database, QuorumMsgSender, RoundUpdate};
use crate::operations::Operations;
use crate::phase::Phase;

use node_data::message::{AsyncQueue, Message, Topics};

use crate::execution_ctx::ExecutionCtx;
use crate::proposal;
use crate::queue::MsgRegistry;
use crate::user::provisioners::Provisioners;
use crate::{ratification, validation};
use tracing::{debug, info, Instrument};

use crate::iteration_ctx::IterationCtx;
use crate::step_votes_reg::AttInfoRegistry;
use std::env;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{oneshot, Mutex};
use tokio::task::JoinHandle;

pub struct Consensus<T: Operations, D: Database> {
    /// inbound is a queue of messages that comes from outside world
    inbound: AsyncQueue<Message>,
    /// outbound_msgs is a queue of messages, this consensus instance shares
    /// with the outside world.
    outbound: AsyncQueue<Message>,

    /// future_msgs is a queue of messages read from inbound_msgs queue. These
    /// msgs are pending to be handled in a future round/step.
    future_msgs: Arc<Mutex<MsgRegistry<Message>>>,

    /// Reference to the executor of any EST-related call
    executor: Arc<T>,

    // Database
    db: Arc<Mutex<D>>,
}

impl<T: Operations + 'static, D: Database + 'static> Consensus<T, D> {
    /// Creates an instance of Consensus.
    ///
    /// # Arguments
    ///
    /// * `inbound` - a queue of input messages consumed by main loop
    /// * `outbound` - a queue of output messages that  main loop broadcasts to
    ///   the outside world
    ///
    ///   broadcasts to the outside world
    pub fn new(
        inbound: AsyncQueue<Message>,
        outbound: AsyncQueue<Message>,
        future_msgs: Arc<Mutex<MsgRegistry<Message>>>,
        executor: Arc<T>,
        db: Arc<Mutex<D>>,
    ) -> Self {
        Self {
            inbound,
            outbound,
            future_msgs,
            executor,
            db,
        }
    }

    /// Spins the consensus state machine. The consensus runs for the whole
    /// round until either a new round is produced or the node needs to re-sync.
    ///
    /// # Arguments
    ///
    /// * `provisioner` - a list of the provisioners based on the most recent
    ///   contract state.
    ///
    /// * `cancel_rx` - a chan that allows the client to drop consensus
    ///   execution on demand.
    pub async fn spin(
        &self,
        ru: RoundUpdate,
        provisioners: Arc<Provisioners>,
        cancel_rx: oneshot::Receiver<i32>,
    ) -> Result<(), ConsensusError> {
        let round = ru.round;
        debug!(event = "consensus started", round);
        let sender = QuorumMsgSender::new(self.outbound.clone());

        // proposal-validation-ratification loop
        let mut handle = self.spawn_consensus(ru, provisioners, sender);

        // Usually this select will be terminated due to cancel signal however
        // it may also be terminated due to unrecoverable error in the main loop
        let result;
        tokio::select! {
            recv = &mut handle => {
                result = recv.map_err(|_| ConsensusError::Canceled(round))?;
                if let Err(ref err) = result {
                    tracing::error!(event = "consensus failed", ?err);
                }
            },
            _ = cancel_rx => {
                result = Err(ConsensusError::Canceled(round));
                tracing::debug!(event = "consensus canceled", round);
            }
        }

        // Tear-down procedure
        abort(&mut handle).await;

        result
    }

    /// Executes Consensus algorithm
    ///
    /// Consensus loop terminates on any of these conditions:
    ///
    /// * A fully valid block for current round is accepted
    /// * Unrecoverable error is returned by a step execution
    fn spawn_consensus(
        &self,
        ru: RoundUpdate,
        provisioners: Arc<Provisioners>,
        sender: QuorumMsgSender,
    ) -> JoinHandle<Result<(), ConsensusError>> {
        let inbound = self.inbound.clone();
        let outbound = self.outbound.clone();
        let future_msgs = self.future_msgs.clone();
        let executor = self.executor.clone();
        let db = self.db.clone();

        tokio::spawn(async move {
            if ru.round > 0 {
                future_msgs.lock().await.remove_msgs_by_round(ru.round - 1);
            }

            let sv_registry =
                Arc::new(Mutex::new(AttInfoRegistry::new(ru.clone())));

            let proposal_handler = Arc::new(Mutex::new(
                proposal::handler::ProposalHandler::new(db.clone()),
            ));

            let validation_handler = Arc::new(Mutex::new(
                validation::handler::ValidationHandler::new(
                    sv_registry.clone(),
                ),
            ));

            let ratification_handler = Arc::new(Mutex::new(
                ratification::handler::RatificationHandler::new(
                    sv_registry.clone(),
                ),
            ));

            let mut phases = [
                Phase::Proposal(proposal::step::ProposalStep::new(
                    executor.clone(),
                    db.clone(),
                    proposal_handler.clone(),
                )),
                Phase::Validation(validation::step::ValidationStep::new(
                    executor.clone(),
                    validation_handler.clone(),
                )),
                Phase::Ratification(ratification::step::RatificationStep::new(
                    ratification_handler.clone(),
                )),
            ];

            // Consensus loop
            // Initialize and run consensus loop

            let mut iter: u8 = 0;
            let mut iter_ctx = IterationCtx::new(
                ru.round,
                iter,
                proposal_handler,
                validation_handler,
                ratification_handler,
                ru.base_timeouts.clone(),
            );

            loop {
                Self::consensus_delay().await;

                iter_ctx.on_begin(iter);

                let mut msg = Message::empty();
                // Execute a single iteration
                for phase in phases.iter_mut() {
                    let step_name = phase.to_step_name();
                    // Initialize new phase with message returned by previous
                    // phase.
                    phase.reinitialize(msg, ru.round, iter).await;

                    // Construct phase execution context
                    let ctx = ExecutionCtx::new(
                        &mut iter_ctx,
                        inbound.clone(),
                        outbound.clone(),
                        future_msgs.clone(),
                        provisioners.as_ref(),
                        ru.clone(),
                        iter,
                        step_name,
                        executor.clone(),
                        sv_registry.clone(),
                        sender.clone(),
                    );

                    // Execute a phase
                    msg = phase
                        .run(ctx)
                        .instrument(tracing::info_span!(
                            "main",
                            round = ru.round,
                            iter = iter,
                            name = ?step_name,
                            pk = ru.pubkey_bls.to_bs58(),
                        ))
                        .await?;

                    // During execution of any step we may encounter that an
                    // quorum is generated for a former or current iteration.
                    if msg.topic() == Topics::Quorum {
                        sender.send_quorum(msg.clone()).await;
                    }
                }

                iter_ctx.on_close();
                iter += 1;
            }
        })
    }

    async fn consensus_delay() {
        let spin_time: u64 = env::var("RUSK_CONSENSUS_SPIN_TIME")
            .unwrap_or_default()
            .parse()
            .unwrap_or_default();

        if spin_time == 0 {
            return;
        }

        info!("RUSK_CONSENSUS_SPIN_TIME is {spin_time}");

        let spin_time = UNIX_EPOCH + Duration::from_secs(spin_time);
        let mut now = SystemTime::now();
        while spin_time > now {
            let to_wait =
                spin_time.duration_since(now).expect("When the hell am I?");

            info!(
                "Waiting {to_wait:?} for consensus to be triggered at {}",
                time_util::print_system_time_to_rfc3339(&spin_time)
            );

            let chunk = match to_wait {
                // More than 1h print every 15min
                secs if secs > Duration::from_secs(60 * 60) => {
                    Duration::from_secs(15 * 60)
                }
                // More than 30min print every 10in
                secs if secs > Duration::from_secs(30 * 60) => {
                    Duration::from_secs(10 * 60)
                }
                // More than 5min print every 5min
                secs if secs > Duration::from_secs(5 * 60) => {
                    Duration::from_secs(5 * 60)
                }
                // More than 1min print every 30secs
                secs if secs > Duration::from_secs(60) => {
                    Duration::from_secs(30)
                }
                // Countdown last minute
                _ => Duration::from_secs(1),
            };

            tokio::time::sleep(chunk).await;
            now = SystemTime::now();
        }
        env::remove_var("RUSK_CONSENSUS_SPIN_TIME");
    }
}

#[inline]
async fn abort<T>(h: &mut JoinHandle<T>) {
    if h.is_finished() {
        return;
    }

    h.abort();

    let _ = h.await;
}
