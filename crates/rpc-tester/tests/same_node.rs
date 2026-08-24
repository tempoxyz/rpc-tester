//! Self-test running the tester against a single anvil node.
//!
//! Pointing `rpc1` and `rpc2` at the same node must always succeed, so this catches bugs in the
//! tester itself (request construction, comparison, pagination, reporting) without needing two
//! live nodes. Requires `anvil` to be installed.

use alloy_node_bindings::{Anvil, AnvilInstance};
use alloy_primitives::B256;
use alloy_provider::{network::AnyNetwork, Provider, ProviderBuilder};
use rpc_tester::RpcTester;
use serde_json::json;
use std::time::Duration;

/// First two anvil dev accounts.
const ALICE: &str = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";
const BOB: &str = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8";

/// Init code that emits a `LOG1` with a fixed topic and deploys an empty contract:
/// `PUSH32 topic PUSH1 0 PUSH1 0 LOG1 STOP`.
///
/// This gives us a receipt with a log so the receipt-derived `eth_getLogs` filters are exercised.
const LOG_EMITTER_INIT_CODE: &str =
    "0x7faaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa60006000a100";

fn provider(anvil: &AnvilInstance) -> impl Provider<AnyNetwork> + Clone {
    ProviderBuilder::new()
        .disable_recommended_fillers()
        .network::<AnyNetwork>()
        .on_http(anvil.endpoint_url())
}

/// Sends a transaction from an unlocked dev account and waits until it is mined.
async fn send_and_mine<P: Provider<AnyNetwork>>(provider: &P, tx: serde_json::Value) {
    let hash: B256 =
        provider.raw_request("eth_sendTransaction".into(), [tx]).await.expect("send tx");
    for _ in 0..50 {
        if provider.get_transaction_receipt(hash).await.expect("get receipt").is_some() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("transaction {hash} was not mined");
}

/// Populates the chain with a value transfer, a log-emitting contract deployment, and an empty
/// block, and returns the chain head.
async fn populate_chain<P: Provider<AnyNetwork>>(provider: &P) -> u64 {
    send_and_mine(provider, json!({ "from": ALICE, "to": BOB, "value": "0x2540be400" })).await;
    send_and_mine(provider, json!({ "from": ALICE, "data": LOG_EMITTER_INIT_CODE })).await;
    let _: serde_json::Value = provider.raw_request("evm_mine".into(), ()).await.expect("evm_mine");

    let head = provider.get_block_number().await.expect("get head");
    assert!(head >= 3, "expected at least 3 blocks, got {head}");
    head
}

#[tokio::test(flavor = "multi_thread")]
async fn same_node_passes() {
    let anvil = Anvil::new().try_spawn().expect("anvil must be installed");
    let head = populate_chain(&provider(&anvil)).await;

    let tester = RpcTester::builder(provider(&anvil), provider(&anvil))
        .with_tracing(true)
        .with_all_txes(true)
        .build();

    // Full suite over the whole chain, including the empty genesis block.
    tester.run(0..=head).await.expect("same node must be a superset of itself");

    // Sampled non-contiguous blocks, as used by historical mode.
    tester.run_blocks([0, head]).await.expect("same node must be a superset of itself");
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_block_errors() {
    let anvil = Anvil::new().try_spawn().expect("anvil must be installed");
    let head = populate_chain(&provider(&anvil)).await;

    let tester = RpcTester::builder(provider(&anvil), provider(&anvil)).build();
    let result = tester.run_blocks([head + 1000]).await;
    assert!(result.is_err(), "requesting a block beyond the tip must error, not panic");
}
