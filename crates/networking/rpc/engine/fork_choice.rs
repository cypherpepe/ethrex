use ethrex_blockchain::{
    error::{ChainError, InvalidForkChoice},
    fork_choice::apply_fork_choice,
    latest_canonical_block_hash,
    payload::{create_payload, BuildPayloadArgs},
};
use ethrex_common::types::BlockHeader;
use ethrex_p2p::sync_manager::SyncStatus;
use serde_json::Value;
use tracing::{debug, info, warn};

use crate::{
    rpc::{RpcApiContext, RpcHandler},
    types::{
        fork_choice::{ForkChoiceResponse, ForkChoiceState, PayloadAttributesV3},
        payload::PayloadStatus,
    },
    utils::RpcErr,
    utils::RpcRequest,
};

#[derive(Debug)]
pub struct ForkChoiceUpdatedV1 {
    pub fork_choice_state: ForkChoiceState,
    pub payload_attributes: Option<PayloadAttributesV3>,
}

impl RpcHandler for ForkChoiceUpdatedV1 {
    fn parse(params: &Option<Vec<Value>>) -> Result<Self, RpcErr> {
        let (fork_choice_state, payload_attributes) = parse(params, false)?;
        Ok(ForkChoiceUpdatedV1 {
            fork_choice_state,
            payload_attributes,
        })
    }

    async fn handle(&self, context: RpcApiContext) -> Result<Value, RpcErr> {
        let (head_block_opt, mut response) =
            handle_forkchoice(&self.fork_choice_state, context.clone(), 1).await?;
        if let (Some(head_block), Some(attributes)) = (head_block_opt, &self.payload_attributes) {
            let chain_config = context.storage.get_chain_config()?;
            if chain_config.is_cancun_activated(attributes.timestamp) {
                return Err(RpcErr::UnsuportedFork(
                    "forkChoiceV1 used to build Cancun payload".to_string(),
                ));
            }
            validate_attributes_v1(attributes, &head_block)?;
            let payload_id = build_payload(attributes, context, &self.fork_choice_state, 1).await?;
            response.set_id(payload_id);
        }
        serde_json::to_value(response).map_err(|error| RpcErr::Internal(error.to_string()))
    }
}

#[derive(Debug)]
pub struct ForkChoiceUpdatedV2 {
    pub fork_choice_state: ForkChoiceState,
    pub payload_attributes: Option<PayloadAttributesV3>,
}

impl RpcHandler for ForkChoiceUpdatedV2 {
    fn parse(params: &Option<Vec<Value>>) -> Result<Self, RpcErr> {
        let (fork_choice_state, payload_attributes) = parse(params, false)?;
        Ok(ForkChoiceUpdatedV2 {
            fork_choice_state,
            payload_attributes,
        })
    }

    async fn handle(&self, context: RpcApiContext) -> Result<Value, RpcErr> {
        let (head_block_opt, mut response) =
            handle_forkchoice(&self.fork_choice_state, context.clone(), 2).await?;
        if let (Some(head_block), Some(attributes)) = (head_block_opt, &self.payload_attributes) {
            let chain_config = context.storage.get_chain_config()?;
            if chain_config.is_cancun_activated(attributes.timestamp) {
                return Err(RpcErr::UnsuportedFork(
                    "forkChoiceV2 used to build Cancun payload".to_string(),
                ));
            } else if chain_config.is_shanghai_activated(attributes.timestamp) {
                validate_attributes_v2(attributes, &head_block)?;
            } else {
                // Behave as a v1
                validate_attributes_v1(attributes, &head_block)?;
            }
            let payload_id = build_payload(attributes, context, &self.fork_choice_state, 2).await?;
            response.set_id(payload_id);
        }
        serde_json::to_value(response).map_err(|error| RpcErr::Internal(error.to_string()))
    }
}

#[derive(Debug)]
pub struct ForkChoiceUpdatedV3 {
    pub fork_choice_state: ForkChoiceState,
    pub payload_attributes: Option<PayloadAttributesV3>,
}

impl From<ForkChoiceUpdatedV3> for RpcRequest {
    fn from(val: ForkChoiceUpdatedV3) -> Self {
        RpcRequest {
            method: "engine_forkchoiceUpdatedV3".to_string(),
            params: Some(vec![
                serde_json::json!(val.fork_choice_state),
                serde_json::json!(val.payload_attributes),
            ]),
            ..Default::default()
        }
    }
}

impl RpcHandler for ForkChoiceUpdatedV3 {
    fn parse(params: &Option<Vec<Value>>) -> Result<Self, RpcErr> {
        let (fork_choice_state, payload_attributes) = parse(params, true)?;
        Ok(ForkChoiceUpdatedV3 {
            fork_choice_state,
            payload_attributes,
        })
    }

    #[cfg(feature = "based")]
    async fn relay_to_gateway_or_fallback(
        req: &RpcRequest,
        context: RpcApiContext,
    ) -> Result<Value, RpcErr> {
        info!("Relaying engine_forkchoiceUpdatedV3 to gateway");

        let request = Self::parse(&req.params)?;

        let gateway_auth_client = context.gateway_auth_client.clone();

        let gateway_request = gateway_auth_client
            .engine_forkchoice_updated_v3(request.fork_choice_state, request.payload_attributes);

        // Parse it again as it was consumed for gateway_response and it is the same as cloning it.
        let request = Self::parse(&req.params)?;
        let client_response = request.handle(context).await;

        let gateway_response = gateway_request
            .await
            .map_err(|err| {
                RpcErr::Internal(format!(
                    "Could not relay engine_forkchoiceUpdatedV3 to gateway: {err}",
                ))
            })
            .and_then(|response| {
                serde_json::to_value(response).map_err(|error| RpcErr::Internal(error.to_string()))
            });

        if gateway_response.is_err() {
            warn!(error = ?gateway_response, "Gateway engine_forkchoiceUpdatedV3 failed, falling back to local node");
        } else {
            info!("Successfully relayed engine_forkchoiceUpdatedV3 to gateway");
        }

        gateway_response.or(client_response)
    }

    async fn handle(&self, context: RpcApiContext) -> Result<Value, RpcErr> {
        let (head_block_opt, mut response) =
            handle_forkchoice(&self.fork_choice_state, context.clone(), 3).await?;
        if let (Some(head_block), Some(attributes)) = (head_block_opt, &self.payload_attributes) {
            validate_attributes_v3(attributes, &head_block, &context)?;
            let payload_id = build_payload(attributes, context, &self.fork_choice_state, 3).await?;
            response.set_id(payload_id);
        }
        serde_json::to_value(response).map_err(|error| RpcErr::Internal(error.to_string()))
    }
}

fn parse(
    params: &Option<Vec<Value>>,
    is_v3: bool,
) -> Result<(ForkChoiceState, Option<PayloadAttributesV3>), RpcErr> {
    let params = params
        .as_ref()
        .ok_or(RpcErr::BadParams("No params provided".to_owned()))?;
    if params.len() != 2 {
        return Err(RpcErr::BadParams("Expected 2 params".to_owned()));
    }

    let forkchoice_state: ForkChoiceState = serde_json::from_value(params[0].clone())?;
    // if there is an error when parsing, set to None
    let payload_attributes: Option<PayloadAttributesV3> =
        match serde_json::from_value::<Option<PayloadAttributesV3>>(params[1].clone()) {
            Ok(attributes) => attributes,
            Err(error) => {
                info!("Could not parse params {}", error);
                None
            }
        };
    if let Some(attr) = &payload_attributes {
        if !is_v3 && attr.parent_beacon_block_root.is_some() {
            return Err(RpcErr::InvalidPayloadAttributes(
                "Attribute parent_beacon_block_root is non-null".to_string(),
            ));
        }
    }
    Ok((forkchoice_state, payload_attributes))
}

async fn handle_forkchoice(
    fork_choice_state: &ForkChoiceState,
    context: RpcApiContext,
    version: usize,
) -> Result<(Option<BlockHeader>, ForkChoiceResponse), RpcErr> {
    debug!(
        "New fork choice request v{} with head: {:#x}, safe: {:#x}, finalized: {:#x}.",
        version,
        fork_choice_state.head_block_hash,
        fork_choice_state.safe_block_hash,
        fork_choice_state.finalized_block_hash
    );

    // Update fcu head in syncer
    context.syncer.set_head(fork_choice_state.head_block_hash);

    let fork_choice_res = if let Some(latest_valid_hash) = context
        .storage
        .get_latest_valid_ancestor(fork_choice_state.head_block_hash)
        .await?
    {
        warn!(
            "Invalid fork choice state. Reason: Invalid ancestor {:#x}",
            latest_valid_hash
        );
        Err(InvalidForkChoice::InvalidAncestor(latest_valid_hash))
    } else {
        // Check parent block hash in invalid_ancestors (if head block exists)
        let head_block = context
            .storage
            .get_block_header_by_hash(fork_choice_state.head_block_hash)?;
        let check_parent = if let Some(head_block) = head_block {
            debug!(
                "Checking parent for invalid ancestor {}",
                head_block.parent_hash
            );
            context
                .storage
                .get_latest_valid_ancestor(head_block.parent_hash)
                .await
                .ok()
                .flatten()
        } else {
            None
        };

        // Check head block hash in invalid_ancestors
        if let Some(latest_valid_hash) = check_parent {
            Err(InvalidForkChoice::InvalidAncestor(latest_valid_hash))
        } else {
            // Check if there is an ongoing sync before applying the forkchoice
            match context.syncer.status()? {
                // Apply current fork choice
                SyncStatus::Inactive => {
                    // All checks passed, apply fork choice
                    apply_fork_choice(
                        &context.storage,
                        fork_choice_state.head_block_hash,
                        fork_choice_state.safe_block_hash,
                        fork_choice_state.finalized_block_hash,
                    )
                    .await
                }
                // Restart sync if needed
                _ => Err(InvalidForkChoice::Syncing),
            }
        }
    };

    match fork_choice_res {
        Ok(head) => {
            // Remove included transactions from the mempool after we accept the fork choice
            // TODO(#797): The remove of transactions from the mempool could be incomplete (i.e. REORGS)
            match context
                .storage
                .get_block_by_hash(head.compute_block_hash())
                .await
            {
                Ok(Some(block)) => {
                    for tx in &block.body.transactions {
                        context
                            .blockchain
                            .remove_transaction_from_pool(&tx.compute_hash())
                            .map_err(|err| RpcErr::Internal(err.to_string()))?;
                    }
                }
                Ok(None) => {
                    warn!("Couldn't get block by hash to remove transactions from the mempool. This is expected in a reconstruted network")
                }
                Err(_) => {
                    return Err(RpcErr::Internal(
                        "Failed to get block by hash to remove transactions from the mempool"
                            .to_string(),
                    ))
                }
            };

            Ok((
                Some(head),
                ForkChoiceResponse::from(PayloadStatus::valid_with_hash(
                    fork_choice_state.head_block_hash,
                )),
            ))
        }
        Err(forkchoice_error) => {
            let forkchoice_response = match forkchoice_error {
                InvalidForkChoice::NewHeadAlreadyCanonical => {
                    ForkChoiceResponse::from(PayloadStatus::valid_with_hash(
                        latest_canonical_block_hash(&context.storage)
                            .await
                            .map_err(|e| RpcErr::Internal(e.to_string()))?,
                    ))
                }
                InvalidForkChoice::Syncing => {
                    // Start sync
                    context
                        .storage
                        .update_sync_status(false)
                        .await
                        .map_err(|e| RpcErr::Internal(e.to_string()))?;
                    context.syncer.start_sync().await;
                    ForkChoiceResponse::from(PayloadStatus::syncing())
                }
                InvalidForkChoice::Disconnected(_, _) | InvalidForkChoice::ElementNotFound(_) => {
                    warn!("Invalid fork choice state. Reason: {:?}", forkchoice_error);
                    return Err(RpcErr::InvalidForkChoiceState(forkchoice_error.to_string()));
                }
                InvalidForkChoice::InvalidAncestor(last_valid_hash) => {
                    ForkChoiceResponse::from(PayloadStatus::invalid_with(
                        last_valid_hash,
                        InvalidForkChoice::InvalidAncestor(last_valid_hash).to_string(),
                    ))
                }
                reason => {
                    warn!(
                        "Invalid fork choice payload. Reason: {}",
                        reason.to_string()
                    );
                    let latest_valid_hash = context
                        .storage
                        .get_latest_canonical_block_hash()
                        .await?
                        .ok_or(RpcErr::Internal(
                            "Missing latest canonical block".to_owned(),
                        ))?;
                    ForkChoiceResponse::from(PayloadStatus::invalid_with(
                        latest_valid_hash,
                        reason.to_string(),
                    ))
                }
            };
            Ok((None, forkchoice_response))
        }
    }
}

fn validate_attributes_v1(
    attributes: &PayloadAttributesV3,
    head_block: &BlockHeader,
) -> Result<(), RpcErr> {
    if attributes.withdrawals.is_some() {
        return Err(RpcErr::WrongParam("withdrawals".to_string()));
    }
    validate_timestamp(attributes, head_block)
}

fn validate_attributes_v2(
    attributes: &PayloadAttributesV3,
    head_block: &BlockHeader,
) -> Result<(), RpcErr> {
    if attributes.withdrawals.is_none() {
        return Err(RpcErr::WrongParam("withdrawals".to_string()));
    }
    validate_timestamp(attributes, head_block)
}

fn validate_attributes_v3(
    attributes: &PayloadAttributesV3,
    head_block: &BlockHeader,
    context: &RpcApiContext,
) -> Result<(), RpcErr> {
    let chain_config = context.storage.get_chain_config()?;
    // Specification indicates this order of validations:
    // https://github.com/ethereum/execution-apis/blob/main/src/engine/cancun.md#specification-1
    if attributes.withdrawals.is_none() {
        return Err(RpcErr::InvalidPayloadAttributes("withdrawals".to_string()));
    }
    if attributes.parent_beacon_block_root.is_none() {
        return Err(RpcErr::InvalidPayloadAttributes(
            "Attribute parent_beacon_block_root is null".to_string(),
        ));
    }
    if !chain_config.is_cancun_activated(attributes.timestamp) {
        return Err(RpcErr::UnsuportedFork(
            "forkChoiceV3 used to build pre-Cancun payload".to_string(),
        ));
    }
    validate_timestamp(attributes, head_block)
}

fn validate_timestamp(
    attributes: &PayloadAttributesV3,
    head_block: &BlockHeader,
) -> Result<(), RpcErr> {
    if attributes.timestamp <= head_block.timestamp {
        return Err(RpcErr::InvalidPayloadAttributes(
            "invalid timestamp".to_string(),
        ));
    }
    Ok(())
}

async fn build_payload(
    attributes: &PayloadAttributesV3,
    context: RpcApiContext,
    fork_choice_state: &ForkChoiceState,
    version: u8,
) -> Result<u64, RpcErr> {
    info!("Fork choice updated includes payload attributes. Creating a new payload.");
    let args = BuildPayloadArgs {
        parent: fork_choice_state.head_block_hash,
        timestamp: attributes.timestamp,
        fee_recipient: attributes.suggested_fee_recipient,
        random: attributes.prev_randao,
        withdrawals: attributes.withdrawals.clone(),
        beacon_root: attributes.parent_beacon_block_root,
        version,
    };
    let payload_id = args.id();
    let payload = match create_payload(&args, &context.storage) {
        Ok(payload) => payload,
        Err(ChainError::EvmError(error)) => return Err(error.into()),
        // Parent block is guaranteed to be present at this point,
        // so the only errors that may be returned are internal storage errors
        Err(error) => return Err(RpcErr::Internal(error.to_string())),
    };
    context.storage.add_payload(payload_id, payload).await?;

    Ok(payload_id)
}
