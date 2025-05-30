use crate::eth::fee_calculator::estimate_gas_tip;

use crate::rpc::{RpcApiContext, RpcHandler};
use crate::utils::RpcErr;
use serde_json::Value;

// TODO: This does not need a struct,
// but I'm leaving it like this for consistency
// with the other RPC endpoints.
// The handle function could simply be
// a function called 'estimate'.
#[derive(Debug, Clone)]
pub struct MaxPriorityFee;

impl RpcHandler for MaxPriorityFee {
    fn parse(_: &Option<Vec<Value>>) -> Result<Self, RpcErr> {
        Ok(MaxPriorityFee {})
    }

    async fn handle(&self, context: RpcApiContext) -> Result<Value, RpcErr> {
        let estimated_gas_tip = estimate_gas_tip(&context.storage).await?;

        let gas_tip = match estimated_gas_tip {
            Some(gas_tip) => gas_tip,
            None => return Ok(serde_json::Value::Null),
        };

        let gas_as_hex = format!("0x{:x}", gas_tip);
        Ok(serde_json::Value::String(gas_as_hex))
    }
}

#[cfg(test)]
mod tests {
    use super::MaxPriorityFee;
    use crate::eth::test_utils::{
        add_eip1559_tx_blocks, add_legacy_tx_blocks, add_mixed_tx_blocks, setup_store,
        BASE_PRICE_IN_WEI,
    };

    use crate::utils::test_utils::example_local_node_record;
    use crate::{
        rpc::{map_http_requests, RpcApiContext, RpcHandler},
        utils::{parse_json_hex, test_utils::example_p2p_node, RpcRequest},
    };
    #[cfg(feature = "based")]
    use crate::{EngineClient, EthClient};
    #[cfg(feature = "based")]
    use bytes::Bytes;
    use ethrex_blockchain::Blockchain;
    use ethrex_p2p::sync_manager::SyncManager;
    #[cfg(feature = "l2")]
    use secp256k1::{rand, SecretKey};
    use serde_json::{json, Value};
    use std::sync::Arc;

    async fn default_context() -> RpcApiContext {
        let storage = setup_store().await;
        let blockchain = Arc::new(Blockchain::default_with_store(storage.clone()));
        RpcApiContext {
            storage,
            blockchain,
            jwt_secret: Default::default(),
            local_p2p_node: example_p2p_node(),
            local_node_record: example_local_node_record(),
            active_filters: Default::default(),
            syncer: Arc::new(SyncManager::dummy()),
            #[cfg(feature = "based")]
            gateway_eth_client: EthClient::new(""),
            #[cfg(feature = "based")]
            gateway_auth_client: EngineClient::new("", Bytes::default()),
            #[cfg(feature = "based")]
            gateway_pubkey: Default::default(),
            #[cfg(feature = "l2")]
            valid_delegation_addresses: Vec::new(),
            #[cfg(feature = "l2")]
            sponsor_pk: SecretKey::new(&mut rand::thread_rng()),
        }
    }

    #[tokio::test]
    async fn test_for_legacy_txs() {
        let context = default_context().await;

        add_legacy_tx_blocks(&context.storage, 100, 10).await;

        let gas_price = MaxPriorityFee {};
        let response = gas_price.handle(context).await.unwrap();
        let parsed_result = parse_json_hex(&response).unwrap();
        assert_eq!(parsed_result, BASE_PRICE_IN_WEI);
    }

    #[tokio::test]
    async fn test_for_eip_1559_txs() {
        let context = default_context().await;

        add_eip1559_tx_blocks(&context.storage, 100, 10).await;

        let gas_price = MaxPriorityFee {};
        let response = gas_price.handle(context).await.unwrap();
        let parsed_result = parse_json_hex(&response).unwrap();
        assert_eq!(parsed_result, BASE_PRICE_IN_WEI);
    }
    #[tokio::test]
    async fn test_with_mixed_transactions() {
        let context = default_context().await;

        add_mixed_tx_blocks(&context.storage, 100, 10).await;

        let gas_price = MaxPriorityFee {};
        let response = gas_price.handle(context).await.unwrap();
        let parsed_result = parse_json_hex(&response).unwrap();
        assert_eq!(parsed_result, BASE_PRICE_IN_WEI);
    }
    #[tokio::test]
    async fn test_with_not_enough_blocks_or_transactions() {
        let context = default_context().await;

        add_mixed_tx_blocks(&context.storage, 100, 0).await;

        let gas_price = MaxPriorityFee {};
        let response = gas_price.handle(context).await.unwrap();
        assert_eq!(response, Value::Null);
    }
    #[tokio::test]
    async fn test_with_no_blocks_but_genesis() {
        let context = default_context().await;
        let gas_price = MaxPriorityFee {};

        let response = gas_price.handle(context).await.unwrap();
        assert_eq!(response, Value::Null);
    }
    #[tokio::test]
    async fn request_smoke_test() {
        let raw_json = json!(
        {
            "jsonrpc":"2.0",
            "method":"eth_maxPriorityFeePerGas",
            "id":1
        });
        let expected_response = json!("0x3b9aca00");
        let request: RpcRequest = serde_json::from_value(raw_json).expect("Test json is not valid");
        let mut context = default_context().await;
        context.local_p2p_node = example_p2p_node();

        add_eip1559_tx_blocks(&context.storage, 100, 3).await;

        let response = map_http_requests(&request, context).await.unwrap();
        assert_eq!(response, expected_response)
    }
}
