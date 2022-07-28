use crate::metrics;
use beacon_chain::validator_monitor::{get_block_delay_ms, timestamp_now};
use beacon_chain::{BeaconChain, BeaconChainTypes, CountUnrealized};
use lighthouse_network::PubsubMessage;
use network::NetworkMessage;
use slog::{crit, error, info, Logger};
use slot_clock::SlotClock;
use std::marker::PhantomData;
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedSender;
use tree_hash::TreeHash;
use types::{
    BeaconBlockAltair, BeaconBlockBase, BeaconBlockBodyAltair, BeaconBlockBodyBase,
    BeaconBlockBodyMerge, BeaconBlockMerge, BlindedPayload, ExecutionBlockHash, ExecutionPayload,
    ExecutionPayloadHeader, FullPayload, SignedBeaconBlock, SignedBeaconBlockAltair,
    SignedBeaconBlockBase, SignedBeaconBlockMerge,
};
use warp::Rejection;

/// Handles a request from the HTTP API for full blocks.
pub async fn publish_block<T: BeaconChainTypes>(
    block: Arc<SignedBeaconBlock<T::EthSpec>>,
    chain: Arc<BeaconChain<T>>,
    network_tx: &UnboundedSender<NetworkMessage<T::EthSpec>>,
    log: Logger,
) -> Result<(), Rejection> {
    let seen_timestamp = timestamp_now();

    // Send the block, regardless of whether or not it is valid. The API
    // specification is very clear that this is the desired behaviour.
    crate::publish_pubsub_message(network_tx, PubsubMessage::BeaconBlock(block.clone()))?;

    // Determine the delay after the start of the slot, register it with metrics.
    let delay = get_block_delay_ms(seen_timestamp, block.message(), &chain.slot_clock);
    metrics::observe_duration(&metrics::HTTP_API_BLOCK_BROADCAST_DELAY_TIMES, delay);

    match chain
        .process_block(block.clone(), CountUnrealized::True)
        .await
    {
        Ok(root) => {
            info!(
                log,
                "Valid block from HTTP API";
                "block_delay" => ?delay,
                "root" => format!("{}", root),
                "proposer_index" => block.message().proposer_index(),
                "slot" => block.slot(),
            );

            // Notify the validator monitor.
            chain.validator_monitor.read().register_api_block(
                seen_timestamp,
                block.message(),
                root,
                &chain.slot_clock,
            );

            // Update the head since it's likely this block will become the new
            // head.
            chain.recompute_head_at_current_slot().await;

            // Perform some logging to inform users if their blocks are being produced
            // late.
            //
            // Check to see the thresholds are non-zero to avoid logging errors with small
            // slot times (e.g., during testing)
            let crit_threshold = chain.slot_clock.unagg_attestation_production_delay();
            let error_threshold = crit_threshold / 2;
            if delay >= crit_threshold {
                crit!(
                    log,
                    "Block was broadcast too late";
                    "msg" => "system may be overloaded, block likely to be orphaned",
                    "delay_ms" => delay.as_millis(),
                    "slot" => block.slot(),
                    "root" => ?root,
                )
            } else if delay >= error_threshold {
                error!(
                    log,
                    "Block broadcast was delayed";
                    "msg" => "system may be overloaded, block may be orphaned",
                    "delay_ms" => delay.as_millis(),
                    "slot" => block.slot(),
                    "root" => ?root,
                )
            }

            Ok(())
        }
        Err(e) => {
            let msg = format!("{:?}", e);
            error!(
                log,
                "Invalid block provided to HTTP API";
                "reason" => &msg
            );
            Err(warp_utils::reject::broadcast_without_import(msg))
        }
    }
}

/// Handles a request from the HTTP API for blinded blocks. This converts blinded blocks into full
/// blocks before publishing.
pub async fn publish_blinded_block<T: BeaconChainTypes>(
    block: SignedBeaconBlock<T::EthSpec, BlindedPayload<T::EthSpec>>,
    chain: Arc<BeaconChain<T>>,
    network_tx: &UnboundedSender<NetworkMessage<T::EthSpec>>,
    log: Logger,
) -> Result<(), Rejection> {
    let full_block = reconstruct_block(chain.clone(), block, log.clone()).await?;
    publish_block::<T>(Arc::new(full_block), chain, network_tx, log).await
}

/// Deconstruct the given blinded block, and construct a full block. This attempts to use the
/// execution layer's payload cache, and if that misses, attempts a blind block proposal to retrieve
/// the full payload.
async fn reconstruct_block<T: BeaconChainTypes>(
    chain: Arc<BeaconChain<T>>,
    block: SignedBeaconBlock<T::EthSpec, BlindedPayload<T::EthSpec>>,
    log: Logger,
) -> Result<SignedBeaconBlock<T::EthSpec, FullPayload<T::EthSpec>>, Rejection> {
    let full_block = match block {
        SignedBeaconBlock::Base(b) => {
            let SignedBeaconBlockBase { message, signature } = b;

            let BeaconBlockBase {
                slot,
                proposer_index,
                parent_root,
                state_root,
                body,
            } = message;

            let BeaconBlockBodyBase {
                randao_reveal,
                eth1_data,
                graffiti,
                proposer_slashings,
                attester_slashings,
                attestations,
                deposits,
                voluntary_exits,
                _phantom,
            } = body;

            SignedBeaconBlock::Base(SignedBeaconBlockBase {
                message: BeaconBlockBase {
                    slot,
                    proposer_index,
                    parent_root,
                    state_root,
                    body: BeaconBlockBodyBase {
                        randao_reveal,
                        eth1_data,
                        graffiti,
                        proposer_slashings,
                        attester_slashings,
                        attestations,
                        deposits,
                        voluntary_exits,
                        _phantom: PhantomData::default(),
                    },
                },
                signature,
            })
        }
        SignedBeaconBlock::Altair(b) => {
            let SignedBeaconBlockAltair { message, signature } = b;

            let BeaconBlockAltair {
                slot,
                proposer_index,
                parent_root,
                state_root,
                body,
            } = message;

            let BeaconBlockBodyAltair {
                randao_reveal,
                eth1_data,
                graffiti,
                proposer_slashings,
                attester_slashings,
                attestations,
                deposits,
                voluntary_exits,
                sync_aggregate,
                _phantom,
            } = body;

            let full_body = BeaconBlockBodyAltair {
                randao_reveal,
                eth1_data,
                graffiti,
                proposer_slashings,
                attester_slashings,
                attestations,
                deposits,
                voluntary_exits,
                sync_aggregate,
                _phantom: PhantomData::default(),
            };

            SignedBeaconBlock::Altair(SignedBeaconBlockAltair {
                message: BeaconBlockAltair {
                    slot,
                    proposer_index,
                    parent_root,
                    state_root,
                    body: full_body,
                },
                signature,
            })
        }
        SignedBeaconBlock::Merge(ref b) => {
            let SignedBeaconBlockMerge { message, signature } = b;

            let BeaconBlockMerge {
                slot,
                proposer_index,
                parent_root,
                state_root,
                body,
            } = message;

            let BeaconBlockBodyMerge {
                randao_reveal,
                eth1_data,
                graffiti,
                proposer_slashings,
                attester_slashings,
                attestations,
                deposits,
                voluntary_exits,
                sync_aggregate,
                execution_payload,
            } = body;

            let payload_root = execution_payload.tree_hash_root();

            let BlindedPayload {
                execution_payload_header,
            } = execution_payload;

            let ExecutionPayloadHeader {
                parent_hash,
                fee_recipient,
                state_root: payload_state_root,
                receipts_root,
                logs_bloom,
                prev_randao,
                block_number,
                gas_limit,
                gas_used,
                timestamp,
                extra_data,
                base_fee_per_gas,
                block_hash,
                transactions_root: _transactions_root,
            } = execution_payload_header;

            let el = chain.execution_layer.as_ref().ok_or_else(|| {
                warp_utils::reject::custom_server_error("Missing execution layer".to_string())
            })?;

            // If the execution block hash is zero, use an empty payload.
            let full_payload = if *block_hash == ExecutionBlockHash::zero() {
                ExecutionPayload::default()
            // If we already have an execution payload with this transactions root cached, use it.
            } else if let Some(cached_payload) = el.get_payload_by_root(&payload_root) {
                info!(log, "Reconstructing a full block using a local payload"; "block_hash" => ?cached_payload.block_hash);
                cached_payload
            // Otherwise, this means we are attempting a blind block proposal.
            } else {
                let full_payload = el.propose_blinded_beacon_block(&block).await.map_err(|e| {
                    warp_utils::reject::custom_server_error(format!(
                        "Blind block proposal failed: {:?}",
                        e
                    ))
                })?;
                info!(log, "Successfully published a block to the builder network"; "block_hash" => ?full_payload.block_hash);
                full_payload
            };

            SignedBeaconBlock::Merge(SignedBeaconBlockMerge {
                message: BeaconBlockMerge {
                    slot: *slot,
                    proposer_index: *proposer_index,
                    parent_root: *parent_root,
                    state_root: *state_root,
                    body: BeaconBlockBodyMerge {
                        randao_reveal: randao_reveal.clone(),
                        eth1_data: eth1_data.clone(),
                        graffiti: *graffiti,
                        proposer_slashings: proposer_slashings.clone(),
                        attester_slashings: attester_slashings.clone(),
                        attestations: attestations.clone(),
                        deposits: deposits.clone(),
                        voluntary_exits: voluntary_exits.clone(),
                        sync_aggregate: sync_aggregate.clone(),
                        execution_payload: FullPayload {
                            execution_payload: ExecutionPayload {
                                parent_hash: *parent_hash,
                                fee_recipient: *fee_recipient,
                                state_root: *payload_state_root,
                                receipts_root: *receipts_root,
                                logs_bloom: logs_bloom.clone(),
                                prev_randao: *prev_randao,
                                block_number: *block_number,
                                gas_limit: *gas_limit,
                                gas_used: *gas_used,
                                timestamp: *timestamp,
                                extra_data: extra_data.clone(),
                                base_fee_per_gas: *base_fee_per_gas,
                                block_hash: *block_hash,
                                transactions: full_payload.transactions,
                            },
                        },
                    },
                },
                signature: signature.clone(),
            })
        }
    };
    Ok(full_block)
}
