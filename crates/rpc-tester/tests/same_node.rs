//! Self-test running the tester against a single anvil node.
//!
//! Pointing `rpc1` and `rpc2` at the same node must always succeed, so this catches bugs in the
//! tester itself (request construction, comparison, pagination, reporting) without needing two
//! live nodes. Requires `anvil` to be installed.

use alloy_node_bindings::{Anvil, AnvilInstance};
use alloy_primitives::B256;
use alloy_provider::{network::AnyNetwork, Provider, ProviderBuilder};
use rpc_tester::{filters, RpcTester};
use serde_json::json;
use std::time::Duration;

/// First two anvil dev accounts.
const ALICE: &str = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";
const BOB: &str = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8";

/// Init code that stores `1` at slot 0, emits a `LOG2` with two fixed topics, and deploys an
/// empty contract:
/// `PUSH1 1 PUSH1 0 SSTORE PUSH32 topic2 PUSH32 topic1 PUSH1 0 PUSH1 0 LOG2 STOP`.
///
/// This gives us a receipt with a two-topic log so the receipt-derived `eth_getLogs` filters
/// (signature, positional topic, and multi-topic) are exercised, and non-empty contract storage
/// for the state calls.
const LOG_EMITTER_INIT_CODE: &str =
    "0x60016000557fbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb7faaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa60006000a200";

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
        .with_finality_tags(true)
        .with_execution_witness(true)
        .build();

    // Full suite over the whole chain, including the empty genesis block.
    tester.run(0..=head).await.expect("same node must be a superset of itself");

    // Sampled non-contiguous blocks, as used by historical mode.
    tester.run_blocks([0, head]).await.expect("same node must be a superset of itself");
}

/// The pending transaction filter scenario has to see a transaction that enters the pool while
/// block production is held, so its delivery cannot be attributed to a new block.
#[tokio::test(flavor = "multi_thread")]
async fn pending_transaction_filter_drains_without_new_block() {
    let anvil = Anvil::new().arg("--no-mining").try_spawn().expect("anvil must be installed");
    let provider = provider(&anvil);

    let submit = {
        let provider = provider.clone();
        tokio::spawn(async move {
            // land between the scenario's polls
            tokio::time::sleep(Duration::from_secs(3)).await;
            let hash: B256 = provider
                .raw_request(
                    "eth_sendTransaction".into(),
                    [json!({ "from": ALICE, "to": BOB, "value": "0x2540be400" })],
                )
                .await
                .expect("send tx");
            let tx = provider
                .get_transaction_by_hash(hash)
                .await
                .expect("get tx")
                .expect("transaction must have been admitted to the pool");
            assert!(tx.block_number.is_none(), "block production must be held");
            hash
        })
    };

    let (polls, _hash) =
        tokio::join!(filters::poll_pending_transaction_filter(&provider, "anvil"), submit);
    let polls = polls.expect("scenario must succeed");

    assert_eq!(polls.poll_error, None);
    assert_eq!(polls.repeated_hashes, 0);
    assert!(
        polls.polls.iter().any(|poll| poll.hashes > 0 && !poll.after_new_block),
        "the transaction must be delivered by a poll no new block precedes, got {:?}",
        polls.polls
    );
    assert!(!filters::suggests_head_gating(&polls.polls));
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_block_errors() {
    let anvil = Anvil::new().try_spawn().expect("anvil must be installed");
    let head = populate_chain(&provider(&anvil)).await;

    let tester = RpcTester::builder(provider(&anvil), provider(&anvil)).build();
    let result = tester.run_blocks([head + 1000]).await;
    assert!(result.is_err(), "requesting a block beyond the tip must error, not panic");
}
