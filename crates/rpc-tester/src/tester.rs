//! [`RpcTester`] implementation.

use super::{MethodName, TestError};
use crate::{get_logs, report::report, rpc, rpc_raw, rpc_with_block};
use alloy_primitives::{Address, BlockHash, BlockNumber, Bytes, B256, U256};
use alloy_provider::{
    ext::{DebugApi, TraceApi},
    network::{AnyNetwork, AnyRpcBlock, TransactionResponse},
    Provider,
};
use alloy_rpc_types::{AccessListResult, BlockId, BlockNumberOrTag, Filter};
use alloy_rpc_types_trace::{
    filter::TraceFilter,
    geth::{
        CallConfig, FlatCallConfig, GethDebugBuiltInTracerType, GethDebugTracerType,
        GethDebugTracingOptions, GethDefaultTracingOptions, PreStateConfig,
    },
    parity::{LocalizedTransactionTrace, TraceType},
};
use alloy_transport::TransportError;
use eyre::Result;
use futures::Future;
use serde::Serialize;
use std::{
    collections::{BTreeMap, HashMap},
    fmt::Debug,
    future::IntoFuture,
    ops::RangeInclusive,
    pin::Pin,
};
use tracing::{debug, info, trace, warn};

// Alias type
type BlockTestResults = BTreeMap<BlockNumber, Vec<(MethodName, Result<(), TestError>)>>;

// Alias type for BalanceChanges
type BalanceChanges = HashMap<Address, U256>;

// Alias type for raw JSON responses compared structurally, e.g. the execution witness, whose
// schema differs across clients.
type JsonValue = serde_json::Value;
/// Type that runs queries two nodes rpc queries and ensures that the first is at least a superset
/// of the second.
#[derive(Debug)]
pub struct RpcTester<P: Provider<AnyNetwork>> {
    /// First RPC node.
    rpc1: P,
    /// Second RPC node.
    rpc2: P,
    /// Whether to query tracing methods.
    use_tracing: bool,
    /// Whether to query reth namespace.
    use_reth: bool,
    /// Whether to call rpc transaction methods for every transaction. Otherwise, the first
    /// transaction of each distinct type in the block.
    use_all_txes: bool,
    /// Whether to skip extended eth methods not supported by all clients (e.g.,
    /// `eth_getRawTransactionByBlockNumberAndIndex`).
    skip_extended_eth: bool,
    /// Whether to compare the moving `safe` and `finalized` tags. Requires both nodes to share a
    /// consensus view.
    use_finality_tags: bool,
    /// Whether to compare `debug_executionWitness` for the newest tested block. Witness
    /// generation re-executes the block, so this is opt-in.
    use_execution_witness: bool,
    /// Whether to query Otterscan's `ots` namespace, which only some clients serve.
    use_ots: bool,
    /// Whether to query Tempo's `tempo` namespace.
    use_tempo: bool,
    /// Oldest block both nodes still serve. Anything below it was pruned on at least one node, so
    /// the genesis-pinned `earliest` tag is only compared when this is zero.
    history_floor: BlockNumber,
    /// Maximum requests per second for rate limiting.
    rate_limit_rps: Option<u32>,
    /// Last timestamp for rate limiting.
    last_request_time: tokio::sync::Mutex<std::time::Instant>,
}

impl<P: Provider<AnyNetwork>> RpcTester<P> {
    /// Returns [`RpcTesterBuilder`].
    pub const fn builder(rpc1: P, rpc2: P) -> RpcTesterBuilder<P> {
        RpcTesterBuilder::new(rpc1, rpc2)
    }
}

impl<P> RpcTester<P>
where
    P: Provider<AnyNetwork> + Clone + Send + Sync,
{
    /// Verifies that results from `rpc1` are at least a superset of `rpc2`.
    ///
    /// All suites run to completion before failing so a diff in one does not hide findings in
    /// another; the first error is returned after every report has been printed.
    pub async fn run(&self, block_range: RangeInclusive<BlockNumber>) -> Result<()> {
        let per_block = self.test_per_block(block_range.clone()).await;
        let block_range_result = self.test_block_range(block_range.clone()).await;
        let negative = self.test_negative(*block_range.end()).await;
        let tags = self.test_tags().await;
        let witness = self.test_execution_witness(*block_range.end()).await;
        per_block.and(block_range_result).and(negative).and(tags).and(witness)
    }

    /// Verifies RPC calls applicable to single blocks for each of the given blocks.
    ///
    /// Unlike [`Self::run`], this runs only the per-block suite — no block-range, negative-probe,
    /// or block-tag tests — so the blocks do not need to be contiguous. This is intended for
    /// sampled historical blocks, see [`historical_blocks`](crate::historical_blocks).
    pub async fn run_blocks(&self, blocks: impl IntoIterator<Item = BlockNumber>) -> Result<()> {
        self.test_per_block(blocks).await
    }

    /// Verifies RPC calls applicable to single blocks.
    ///
    /// If a block cannot be fetched, the results collected so far are still reported before the
    /// fetch error is returned.
    async fn test_per_block(
        &self,
        blocks: impl IntoIterator<Item = BlockNumber>,
    ) -> Result<(), eyre::Error> {
        let mut results = BlockTestResults::new();
        let mut fetch_err = None;

        for block_number in blocks {
            info!(block_number, "testing rpc");

            let mut tests = vec![];

            let (block, block_hash, block_tag, block_id) =
                match self.fetch_block(block_number).await {
                    Ok(fetched) => fetched,
                    Err(err) => {
                        fetch_err = Some(err);
                        break;
                    }
                };

            // EIP-1898 block hash object form with require canonical semantics.
            let canonical_args = (
                Address::ZERO,
                serde_json::json!({ "blockHash": block_hash, "requireCanonical": true }),
            );

            #[rustfmt::skip]
            let block_calls = vec![
                rpc!(
                    self,
                    get_block_by_hash,
                    block_hash;
                    kind(alloy_rpc_types::BlockTransactionsKind::Full)
                ),
                rpc!(
                    self,
                    get_block_by_number,
                    block_tag;
                    kind(alloy_rpc_types::BlockTransactionsKind::Full)
                ),
                rpc!(self, get_block_transaction_count_by_hash, block_hash),
                rpc!(self, get_block_transaction_count_by_number, block_tag),
                rpc!(self, get_uncle_count, BlockId::Hash(block_hash.into())),
                rpc!(self, get_uncle_count, BlockId::Number(block_tag)),
                rpc!(self, get_uncle, BlockId::Hash(block_hash.into()), 0u64),
                rpc!(self, get_uncle, BlockId::Number(block_tag), 0u64),
                rpc!(self, get_block_receipts, block_id),
                // Raw-encoding parity catches divergence that JSON normalization hides.
                rpc!(self, debug_get_raw_header, block_id),
                rpc!(self, debug_get_raw_block, block_id),
                rpc!(self, debug_get_raw_receipts, block_id),
                rpc_raw!(self, reth_getBalanceChangesInBlock, BalanceChanges, (block_id,)),
                rpc_raw!(self, eth_getBalance, U256, canonical_args),
                rpc!(self, trace_block, block_id),
                rpc!(self, trace_replay_block_transactions, block_id; trace_types([TraceType::StateDiff])),
                rpc!(self, debug_trace_block_by_hash, block_hash, call_tracer_opts()),
                rpc!(self, debug_trace_block_by_number, block_tag, call_tracer_opts()),
                rpc!(self, debug_trace_block_by_number, block_tag, call_tracer_with_log_opts()),
                rpc!(self, debug_trace_block_by_number, block_tag, prestate_tracer_opts()),
                get_logs!(self, &Filter::new().select(block_number)),
                get_logs!(self, &Filter::new().at_block_hash(block_hash)),
                get_logs!(self, &Filter::new().select(block_number).address(vec![
                    "0x6b175474e89094c44da98b954eedeac495271d0f".parse::<Address>().unwrap(), // dai
                    "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48".parse::<Address>().unwrap(), // usdc
                ]))
            ];

            tests.extend(block_calls);

            #[rustfmt::skip]
            let ots_block_calls = vec![
                rpc_raw!(self, ots_getBlockDetails, JsonValue, (block_number,)),
                rpc_raw!(self, ots_getBlockTransactions, JsonValue, (block_number, 0, 10)),
            ];
            tests.extend(ots_block_calls);

            // // Transaction/Receipt based RPCs
            let mut seen_sample_keys = Vec::new();
            for (index, tx) in block.transactions.txns().enumerate() {
                let tx_json = serde_json::to_value(tx).expect("should json");

                // Without --use-all-txes, sample the first transaction of each distinct type
                // instead of just the first of the block: the first transaction is often the same
                // kind of searcher activity, while rarer types (blob, 7702, legacy) exercise
                // different serialization and execution paths. Tempo transactions are further
                // split by the features that take their own paths, see `sample_key`.
                if !self.use_all_txes {
                    let key = sample_key(&tx_json);
                    if seen_sample_keys.contains(&key) {
                        continue;
                    }
                    seen_sample_keys.push(key);
                }

                let (tx_hash, tx_from) = (tx.tx_hash(), tx.from());

                // Replaying the transaction as a call at the parent block exercises EVM execution
                // on historical state, which no data-retrieval method touches. Both nodes get the
                // identical request, so results (or revert errors) must agree. Note that gas
                // estimates and access lists are implementation-defined, so those two comparisons
                // are meaningful primarily for same-client pairs.
                let (request, parent) =
                    (call_request_json(&tx_json), BlockId::number(block_number.saturating_sub(1)));
                let call_args = (request.clone(), parent);
                let estimate_args = call_args.clone();
                let access_list_args = call_args.clone();
                // Tempo's simulation adds the metadata of every TIP-20 token in the logs.
                let tempo_simulate_args = (
                    serde_json::json!({ "blockStateCalls": [{ "calls": [request.clone()] }] }),
                    parent,
                );
                // The traced replays cover the tracers on a call, which has no receipt to fill
                // the root frame from.
                let trace_call_args = (request.clone(), parent, call_tracer_with_log_opts());
                let parity_call_args =
                    (request.clone(), [TraceType::Trace, TraceType::StateDiff], parent);
                #[rustfmt::skip]
                let exec_calls = vec![
                    rpc_raw!(self, eth_call, Bytes, call_args),
                    rpc_raw!(self, eth_estimateGas, U256, estimate_args),
                    rpc_raw!(self, eth_createAccessList, AccessListResult, access_list_args),
                    rpc_raw!(self, debug_traceCall, JsonValue, trace_call_args),
                    rpc_raw!(self, trace_call, JsonValue, parity_call_args),
                    rpc_raw!(self, tempo_simulateV1, JsonValue, tempo_simulate_args),
                ];
                tests.extend(exec_calls);

                if self.use_tempo {
                    // Replays with the transaction's own fees, fee token and authorizations, which
                    // the fee-free replays drop. Calls, estimates and traced calls still resolve
                    // and validate the fee token but skip fee collection, so only the simulation
                    // charges and refunds it, emitting logs outside any call frame.
                    let fee_call_args = (fee_call_request_json(&tx_json), parent);
                    let fee_estimate_args = fee_call_args.clone();
                    let fee_prestate_args = (
                        fee_call_args.0.clone(),
                        parent,
                        serde_json::json!({ "tracer": "prestateTracer", "tracerConfig": { "diffMode": true } }),
                    );
                    let fee_simulate_args = (
                        serde_json::json!({
                            "blockStateCalls": [{ "calls": [fee_call_args.0] }],
                            "traceTransfers": true,
                        }),
                        parent,
                    );
                    #[rustfmt::skip]
                    let fee_calls = vec![
                        rpc_raw!(self, eth_call, Bytes, fee_call_args),
                        rpc_raw!(self, eth_estimateGas, U256, fee_estimate_args),
                        rpc_raw!(self, debug_traceCall, JsonValue, fee_prestate_args),
                        rpc_raw!(self, tempo_simulateV1, JsonValue, fee_simulate_args),
                    ];
                    tests.extend(fee_calls);
                }

                if !self.skip_extended_eth {
                    // Simulation builds a block around the call, with receipts, logs and
                    // synthetic transfer logs that `eth_call` does not return.
                    let simulate_args = (
                        serde_json::json!({
                            "blockStateCalls": [{ "calls": [request] }],
                            "traceTransfers": true,
                        }),
                        parent,
                    );
                    tests.push(rpc_raw!(self, eth_simulateV1, JsonValue, simulate_args));
                }

                if let Some(receipt) = self.rpc2.get_transaction_receipt(tx_hash).await? {
                    if let Some(log) = receipt.inner.inner.logs().first().map(|l| l.address()) {
                        #[rustfmt::skip]
                        tests.push(get_logs!(self, Filter::new().select(block_number).address(log)));

                        // State of the contract that emitted the log at this block. These hit the
                        // bytecode and (historical) storage paths, which no other call exercises.
                        #[rustfmt::skip]
                        let state_calls = vec![
                            rpc_with_block!(self, get_code_at, log; block_id),
                            rpc_with_block!(self, get_storage_at, log, U256::ZERO; block_id),
                        ];
                        tests.extend(state_calls);
                    }

                    if let Some(topic) = receipt
                        .inner
                        .inner
                        .logs()
                        .last()
                        .and_then(|log| log.topics().first())
                        .copied()
                    {
                        #[rustfmt::skip]
                        tests.push(
                            get_logs!(self, Filter::new().select(block_number).event_signature(topic))
                        );
                    }

                    // Multi-topic filter combining topic0 and topic1 of the same log, which
                    // exercises positional topic matching instead of just the signature index.
                    if let Some(log) =
                        receipt.inner.inner.logs().iter().find(|log| log.topics().len() >= 2)
                    {
                        let (topic0, topic1) = (log.topics()[0], log.topics()[1]);
                        #[rustfmt::skip]
                        tests.push(get_logs!(
                            self,
                            Filter::new().select(block_number).event_signature(topic0).topic1(topic1)
                        ));
                    }

                    // OR-list filter over multiple event signatures.
                    let mut signatures: Vec<B256> = receipt
                        .inner
                        .inner
                        .logs()
                        .iter()
                        .filter_map(|log| log.topics().first().copied())
                        .collect();
                    signatures.sort_unstable();
                    signatures.dedup();
                    signatures.truncate(4);
                    if signatures.len() >= 2 {
                        let or_filter =
                            Filter::new().select(block_number).event_signature(signatures);
                        tests.push(get_logs!(self, or_filter));
                    }
                }

                #[rustfmt::skip]
                let tx_calls = vec![
                    rpc!(self, get_raw_transaction_by_hash, tx_hash),
                    rpc!(self, debug_get_raw_transaction, tx_hash),
                    rpc!(self, get_transaction_by_hash, tx_hash),
                    rpc!(self, get_transaction_by_block_hash_and_index, block_hash, index),
                    rpc!(self, get_transaction_by_block_number_and_index, block_tag, index),
                    rpc!(self, get_transaction_receipt, tx_hash),
                    rpc_with_block!(self, get_transaction_count, tx_from; block_id),
                    rpc_with_block!(self, get_balance, tx_from; block_id),
                    // Senders are usually EOAs, but EIP-7702 delegations make sender code
                    // observable.
                    rpc_with_block!(self, get_code_at, tx_from; block_id),
                    rpc!(self, trace_transaction, tx_hash),
                    rpc!(self, debug_trace_transaction, tx_hash, call_tracer_opts()),
                    rpc!(self, debug_trace_transaction, tx_hash, call_tracer_with_log_opts()),
                    rpc!(self, debug_trace_transaction, tx_hash, prestate_tracer_opts()),
                    rpc!(self, debug_trace_transaction, tx_hash, prestate_diff_tracer_opts()),
                    rpc!(self, debug_trace_transaction, tx_hash, flat_call_tracer_opts()),
                    rpc!(self, debug_trace_transaction, tx_hash, flat_call_tracer_with_precompiles_opts()),
                    rpc!(self, debug_trace_transaction, tx_hash, four_byte_tracer_opts()),
                    rpc!(self, debug_trace_transaction, tx_hash, struct_log_opts()),
                    rpc_raw!(self, trace_replayTransaction, JsonValue, (tx_hash, [TraceType::Trace, TraceType::StateDiff])),
                ];
                tests.extend(tx_calls);

                // Otterscan builds its own call tree and internal operations from separate
                // inspectors, which can diverge from the other tracers.
                #[rustfmt::skip]
                let ots_tx_calls = vec![
                    rpc_raw!(self, ots_traceTransaction, JsonValue, (tx_hash,)),
                    rpc_raw!(self, ots_getInternalOperations, JsonValue, (tx_hash,)),
                    rpc_raw!(self, ots_getTransactionError, JsonValue, (tx_hash,)),
                ];
                tests.extend(ots_tx_calls);

                if !self.skip_extended_eth {
                    #[rustfmt::skip]
                    let extended_calls = vec![
                        rpc!(self, get_raw_transaction_by_block_hash_and_index, block_hash, index),
                        rpc!(self, get_raw_transaction_by_block_number_and_index, block_tag, index),
                    ];
                    tests.extend(extended_calls);
                }
            }
            let block_results = futures::future::join_all(tests).await;
            results.insert(block_number, block_results);
        }
        let report_result =
            report(results.into_iter().map(|(k, v)| (format!("Block Number {k}"), v)).collect());
        if let Some(err) = fetch_err {
            return Err(err);
        }
        report_result
    }

    /// Verifies RPC calls applicable to block ranges.
    async fn test_block_range(&self, block_range: RangeInclusive<u64>) -> Result<(), eyre::Error> {
        let start = *block_range.start();
        let end = *block_range.end();

        // ERC-20 Transfer event signature: Transfer(address,address,uint256)
        let transfer_event_signature =
            "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef"
                .parse::<B256>()
                .unwrap();

        // Range-indexed counterpart to `eth_getLogs`, backed by the trace index instead of the
        // log index.
        let trace_filter = TraceFilter {
            from_block: Some(start),
            to_block: Some(end),
            from_address: vec![],
            to_address: vec![],
            mode: Default::default(),
            after: None,
            count: None,
        };
        let trace_filter_test = Box::pin(self.test_rpc_call(
            "trace_filter",
            Some(format!("{trace_filter:?}")),
            |provider: &P| provider.trace_filter(&trace_filter),
        ))
            as Pin<Box<dyn Future<Output = (MethodName, Result<(), TestError>)> + Send>>;

        #[rustfmt::skip]
        report(vec![(
            format!("{start}..={end}"),
            futures::future::join_all([
                rpc!(self, get_chain_id),
                // Fully deterministic for a historical range anchored to a numeric block.
                rpc!(self, get_fee_history, end - start + 1, BlockNumberOrTag::Number(end), &[25.0, 50.0, 75.0][..]),
                get_logs!(self, Filter::new().from_block(start).to_block(end)),
                get_logs!(self, Filter::new().from_block(start).to_block(end).address(vec![
                    "0x6b175474e89094c44da98b954eedeac495271d0f".parse::<Address>().unwrap(), // dai
                    "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48".parse::<Address>().unwrap(), // usdc
                ])),
                get_logs!(self, Filter::new().from_block(start).to_block(end).event_signature(transfer_event_signature)),
                trace_filter_test
            ])
            .await,
        )])?;

        Ok(())
    }

    /// Verifies that both nodes agree on requests for data that does not exist.
    ///
    /// Clients diverge a lot on the miss path: null vs error responses, and error codes for
    /// invalid requests. All probes use inputs that cannot exist, so both nodes must return the
    /// same null response or the same error.
    async fn test_negative(&self, head: BlockNumber) -> Result<(), eyre::Error> {
        let missing_hash = B256::repeat_byte(0xff);
        let future_tag = BlockNumberOrTag::Number(head.saturating_add(1_000_000));
        let head_tag = BlockNumberOrTag::Number(head);

        #[rustfmt::skip]
        let tests = vec![
            rpc!(self, get_block_by_hash, missing_hash; kind(alloy_rpc_types::BlockTransactionsKind::Full)),
            rpc!(self, get_block_by_number, future_tag; kind(alloy_rpc_types::BlockTransactionsKind::Full)),
            rpc!(self, get_block_transaction_count_by_number, future_tag),
            rpc!(self, get_block_receipts, BlockId::Number(future_tag)),
            rpc!(self, get_transaction_by_hash, missing_hash),
            rpc!(self, get_raw_transaction_by_hash, missing_hash),
            rpc!(self, get_transaction_receipt, missing_hash),
            rpc!(self, get_transaction_by_block_number_and_index, head_tag, 100_000usize),
            // Missing transactions may return null; Alloy's typed helper requires an array.
            rpc_raw!(self, trace_transaction, Option<Vec<LocalizedTransactionTrace>>, (missing_hash,)),
            rpc!(self, debug_trace_transaction, missing_hash, call_tracer_opts()),
        ];

        report(vec![("Negative probes".to_string(), futures::future::join_all(tests).await)])
    }

    /// Verifies RPC calls addressed by block tag rather than number or hash.
    ///
    /// `earliest` is pinned to genesis, so it is compared only when neither node pruned it (see
    /// [`RpcTesterBuilder::with_history_floor`]). `safe` and
    /// `finalized` move with the chain and require both nodes to share a consensus view, so they
    /// are gated behind the finality tags option.
    async fn test_tags(&self) -> Result<(), eyre::Error> {
        let mut tests = Vec::new();

        if self.history_floor == 0 {
            #[rustfmt::skip]
            tests.extend([
                rpc!(self, get_block_by_number, BlockNumberOrTag::Earliest; kind(alloy_rpc_types::BlockTransactionsKind::Full)),
                rpc!(self, get_block_transaction_count_by_number, BlockNumberOrTag::Earliest),
                rpc!(self, get_block_receipts, BlockId::Number(BlockNumberOrTag::Earliest)),
                rpc!(self, get_uncle_count, BlockId::Number(BlockNumberOrTag::Earliest)),
            ]);
        } else {
            info!(history_floor = self.history_floor, "skipping `earliest` tag, genesis is pruned");
        }

        tests.push(rpc_raw!(self, tempo_forkSchedule, JsonValue, ()));

        if self.use_finality_tags {
            for tag in [BlockNumberOrTag::Safe, BlockNumberOrTag::Finalized] {
                #[rustfmt::skip]
                tests.push(rpc!(self, get_block_by_number, tag; kind(alloy_rpc_types::BlockTransactionsKind::Hashes)));
            }
        }

        if tests.is_empty() {
            return Ok(());
        }

        report(vec![("Block tags".to_string(), futures::future::join_all(tests).await)])
    }

    /// Verifies `debug_executionWitness` for the newest tested block.
    ///
    /// The witness contains every piece of state touched while re-executing the block, making it
    /// a dense probe of execution and state-read parity. Generation re-executes the block and is
    /// expensive, so this is opt-in and runs only for the single newest block.
    async fn test_execution_witness(&self, head: BlockNumber) -> Result<(), eyre::Error> {
        if !self.use_execution_witness {
            return Ok(());
        }

        let witness_args = (BlockNumberOrTag::Number(head),);
        #[rustfmt::skip]
        let tests = vec![
            rpc_raw!(self, debug_executionWitness, JsonValue, witness_args),
        ];

        report(vec![("Execution witness".to_string(), futures::future::join_all(tests).await)])
    }

    /// Fetches the block and its identifiers from `rpc2`, verifying both nodes agree on the
    /// canonical hash.
    ///
    /// Retries briefly to ride out transient tip reorgs (this narrows the reorg window, it does
    /// not close it) and errors on persistent divergence or when either node is missing the
    /// block.
    async fn fetch_block(
        &self,
        block_number: u64,
    ) -> Result<(AnyRpcBlock, BlockHash, BlockNumberOrTag, BlockId), eyre::Error> {
        let block_tag = BlockNumberOrTag::Number(block_number);
        let block_id = BlockId::Number(block_tag);

        // Reorg guard: when the nodes disagree on the canonical hash mid-run, every downstream
        // comparison diffs confusingly. Transient reorgs at the tip settle quickly, so retry a
        // few times and fail with a clear error only on persistent divergence.
        let mut last_hashes = (None, None);
        for attempt in 0..4 {
            if attempt > 0 {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }

            let block = self
                .rpc2
                .get_block_by_number(block_number.into())
                .full()
                .await?
                .ok_or_else(|| eyre::eyre!("block {block_number} not found on rpc2"))?;
            eyre::ensure!(
                block.header.number == block_number,
                "rpc2 returned block {} for requested block {block_number}",
                block.header.number
            );
            let block_hash = block.header.hash;

            let rpc1_hash = self
                .rpc1
                .get_block_by_number(block_number.into())
                .await?
                .map(|block| block.header.hash);
            if rpc1_hash == Some(block_hash) {
                return Ok((block, block_hash, block_tag, block_id));
            }

            warn!(block_number, ?rpc1_hash, rpc2_hash = %block_hash, "nodes disagree on canonical block hash, retrying");
            last_hashes = (rpc1_hash, Some(block_hash));
        }

        // A missing block on rpc1 is a coverage gap (pruned or unsynced history), not a reorg;
        // report it as such instead of a hash disagreement.
        match last_hashes {
            (None, _) => Err(eyre::eyre!("block {block_number} not found on rpc1")),
            (Some(rpc1_hash), rpc2_hash) => Err(eyre::eyre!(
                "rpc1 and rpc2 disagree on the canonical hash of block {block_number}: rpc1 {rpc1_hash} vs rpc2 {rpc2_hash:?}",
            )),
        }
    }

    /// Apply rate limiting if configured.
    /// Sleeps if necessary to maintain the configured rate limit.
    ///
    /// The limit is applied per test, and each test issues one request per node, so an endpoint
    /// serving both roles receives up to twice the configured rate. `eth_getLogs` pagination
    /// retries and block/receipt prefetches are not throttled.
    async fn apply_rate_limit(&self) {
        if let Some(rps) = self.rate_limit_rps {
            let min_interval = std::time::Duration::from_secs_f64(1.0 / rps as f64);
            let mut last_time = self.last_request_time.lock().await;
            let now = std::time::Instant::now();
            let elapsed = now.duration_since(*last_time);
            if elapsed < min_interval {
                let sleep_time = min_interval - elapsed;
                debug!("Rate limiting: sleeping for {:?}", sleep_time);
                tokio::time::sleep(sleep_time).await;
            }
            *last_time = std::time::Instant::now();
        }
    }

    /// Compares the response to a specific method between both rpcs. Only collects differences.
    ///
    /// If any namespace is disabled skip it.
    async fn test_rpc_call<'a, F, Fut, T>(
        &'a self,
        name: &str,
        args: Option<String>,
        method_call: F,
    ) -> (MethodName, Result<(), TestError>)
    where
        F: Fn(&'a P) -> Fut + 'a,
        Fut: std::future::Future<Output = Result<T, TransportError>> + 'a + Send,
        T: PartialEq + Debug + Serialize,
    {
        // The debug namespace is gated together with tracing: `debug_getRaw*` are typically
        // enabled alongside the trace methods, and running them against nodes without the
        // namespace would fail every block. `debug_executionWitness` has its own opt-in,
        // independent of the tracing gate, and so does the `ots` namespace.
        let skip = if name == "debug_executionWitness" {
            !self.use_execution_witness
        } else if name.starts_with("ots") {
            !self.use_ots
        } else {
            name.starts_with("reth") && !self.use_reth ||
                name.starts_with("tempo") && !self.use_tempo ||
                (name.contains("trace") || name.starts_with("debug")) && !self.use_tracing
        };
        if skip {
            return (name.to_string(), Ok(()));
        }

        // Apply rate limiting if configured
        self.apply_rate_limit().await;

        trace!("## {name}");
        let t = std::time::Instant::now();
        let (rpc1_result, rpc2_result) =
            tokio::join!(method_call(&self.rpc1), method_call(&self.rpc2));
        debug!(elapsed = t.elapsed().as_millis(), ?rpc1_result, ?rpc2_result, "{name}");

        let result = match (rpc1_result, rpc2_result) {
            (Ok(rpc1), Ok(rpc2)) => {
                if rpc1 == rpc2 {
                    Ok(())
                } else {
                    let mut rpc1 = serde_json::to_value(&rpc1).expect("should json");
                    let mut rpc2 = serde_json::to_value(&rpc2).expect("should json");

                    if name == "eth_createAccessList" {
                        normalize_access_list_result(&mut rpc1);
                        normalize_access_list_result(&mut rpc2);
                    }

                    if rpc1 == rpc2 {
                        Ok(())
                    } else {
                        Err(TestError::Diff { rpc1, rpc2, args })
                    }
                }
            }
            // Both nodes rejecting the call with the same error response is agreement, e.g. a
            // deliberate miss-path probe or a namespace disabled on both nodes. Warn on each
            // such pass: a run where entire suites "agree on errors" compares no data and would
            // otherwise be indistinguishable from a verified run.
            (Err(e1), Err(e2)) => {
                if errors_match(&e1, &e2) {
                    warn!(name, error = ?e1, "passed via matching errors, no data compared");
                    Ok(())
                } else {
                    Err(TestError::ErrDiff {
                        rpc1: format!("{e1:?}"),
                        rpc2: format!("{e2:?}"),
                        args,
                    })
                }
            }
            (Err(e), _) => Err(TestError::Rpc1Err(format!("rpc1: {e:?}"))),
            (Ok(_), Err(e)) => Err(TestError::Rpc2Err(format!("rpc2: {e:?}"))),
        };

        (name.to_string(), result)
    }
}

/// Canonicalizes the set-like arrays in an `eth_createAccessList` result.
///
/// Access-list item and storage-key ordering does not affect execution, and clients may return
/// equivalent lists in different orders.
fn normalize_access_list_result(value: &mut JsonValue) {
    let Some(access_list) = value.get_mut("accessList").and_then(JsonValue::as_array_mut) else {
        return
    };

    for item in access_list.iter_mut() {
        if let Some(storage_keys) = item.get_mut("storageKeys").and_then(JsonValue::as_array_mut) {
            storage_keys.sort_unstable_by(|a, b| a.as_str().cmp(&b.as_str()));
        }
    }

    access_list.sort_unstable_by(|a, b| {
        a.get("address")
            .and_then(JsonValue::as_str)
            .cmp(&b.get("address").and_then(JsonValue::as_str))
    });
}

/// Builds an `eth_call` request object from a transaction response's JSON, keeping a minimal
/// subset of fields (`from`, `to`, `gas`, `value`, `input`, and a Tempo transaction's `calls`,
/// nonce key and validity window).
///
/// Access and authorization lists are dropped, so the call is not a faithful replay — but both
/// nodes receive the identical request, so parity still holds. The gas price is pinned to zero
/// rather than omitted: the effective price of a fee-less call is client-defined, and a contract
/// branching on `GASPRICE` would otherwise legitimately diverge across implementations. A missing
/// `to` naturally maps to a create call.
fn call_request_json(tx: &serde_json::Value) -> serde_json::Value {
    let mut request = serde_json::Map::new();
    for key in ["from", "to", "gas", "value", "input"] {
        if let Some(value) = tx.get(key).filter(|value| !value.is_null()) {
            request.insert(key.to_string(), value.clone());
        }
    }
    // A Tempo transaction runs its calls instead of `to` and `input`.
    if let Some(calls) = tx.get("calls").and_then(JsonValue::as_array).filter(|c| !c.is_empty()) {
        let calls = calls
            .iter()
            .map(|call| {
                let fields = ["to", "value", "input"].into_iter().filter_map(|key| {
                    let value = call.get(key).filter(|value| !value.is_null())?;
                    Some((key.to_string(), value.clone()))
                });
                JsonValue::Object(fields.collect())
            })
            .collect();
        request.insert("calls".to_string(), JsonValue::Array(calls));
        // Without its nonce key, a transaction on an expiring or 2D nonce replays on the protocol
        // nonce, which charges new-account gas while that nonce is zero. An expiring nonce also
        // requires its validity window.
        for key in ["nonceKey", "validBefore", "validAfter"] {
            if let Some(value) = tx.get(key).filter(|value| !value.is_null()) {
                request.insert(key.to_string(), value.clone());
            }
        }
        // Tempo transactions only take EIP-1559 fee fields.
        for key in ["maxFeePerGas", "maxPriorityFeePerGas"] {
            request.insert(key.to_string(), JsonValue::String("0x0".to_string()));
        }
        return JsonValue::Object(request);
    }
    request.insert("gasPrice".to_string(), serde_json::Value::String("0x0".to_string()));
    serde_json::Value::Object(request)
}

/// Builds a call request that pays the transaction's own fees, unlike [`call_request_json`], and
/// carries a Tempo transaction's fee token, nonce and authorizations.
///
/// A sponsored transaction is replayed with its sender paying, since a fee payer signature does
/// not cover the request.
fn fee_call_request_json(tx: &JsonValue) -> JsonValue {
    let mut request = call_request_json(tx);
    let request_fields = request.as_object_mut().expect("call request is an object");
    for key in ["gasPrice", "maxFeePerGas", "maxPriorityFeePerGas"] {
        request_fields.remove(key);
    }
    // Responses report the effective `gasPrice` of EIP-1559 transactions too, and a request may
    // not carry both kinds of fee fields.
    let fee_fields: &[&str] = if tx.get("maxFeePerGas").is_some_and(|fee| !fee.is_null()) {
        &["maxFeePerGas", "maxPriorityFeePerGas"]
    } else {
        &["gasPrice"]
    };
    let tempo_fields = ["feeToken", "nonce", "keyAuthorization"];
    let list_fields = ["accessList", "authorizationList", "aaAuthorizationList"];
    for key in fee_fields.iter().chain(&tempo_fields).chain(&list_fields) {
        let Some(value) = tx.get(*key).filter(|value| !value.is_null()) else { continue };
        if value.as_array().is_some_and(Vec::is_empty) {
            continue;
        }
        request_fields.insert((*key).to_string(), value.clone());
    }
    request
}

/// Returns the key that groups transactions for sampling: the type, and for a Tempo transaction
/// also whether it batches calls, whether it creates a contract, its nonce kind, sponsorship, key
/// authorization and signature type, which each take their own execution or tracing path.
fn sample_key(tx: &JsonValue) -> JsonValue {
    let tx_type = tx.get("type").cloned().unwrap_or_default();
    let Some(calls) = tx.get("calls").and_then(JsonValue::as_array) else { return tx_type };
    let is_set = |key: &str| tx.get(key).is_some_and(|value| !value.is_null());
    let nonce = match tx.get("nonceKey").and_then(JsonValue::as_str) {
        None | Some("0x0") => "protocol",
        // An expiring nonce uses the maximum nonce key.
        Some(key) if key.len() == 66 && key[2..].bytes().all(|byte| byte == b'f') => "expiring",
        Some(_) => "2d",
    };
    serde_json::json!([
        tx_type,
        calls.len() > 1,
        // Only the first call may create a contract.
        calls.first().is_some_and(|call| call.get("to").is_none_or(JsonValue::is_null)),
        nonce,
        is_set("feePayerSignature"),
        is_set("keyAuthorization"),
        tx.pointer("/signature/type"),
    ])
}

/// Returns whether two RPC errors are the same error response.
///
/// Only JSON-RPC error responses are compared (by code and message); transport-level failures
/// such as timeouts are never treated as agreement.
fn errors_match(e1: &TransportError, e2: &TransportError) -> bool {
    match (e1.as_error_resp(), e2.as_error_resp()) {
        // The data field must match too: reverts surface as identical code/message pairs with
        // the revert bytes in data, and differing revert data is genuine execution divergence.
        (Some(r1), Some(r2)) => {
            r1.code == r2.code &&
                r1.message == r2.message &&
                error_data(r1.data.as_deref()) == error_data(r2.data.as_deref())
        }
        _ => false,
    }
}

/// Returns an error response's data, treating empty revert data (`"0x"`) as absent.
///
/// Clients differ on how they encode a revert without output: some omit `data`, others (geth,
/// reth since paradigmxyz/reth#28243) return `"0x"`. Both mean the same revert.
fn error_data(data: Option<&serde_json::value::RawValue>) -> Option<&str> {
    data.map(|data| data.get()).filter(|data| *data != "\"0x\"")
}

/// Returns the oldest block `provider` still serves, at most `head`.
///
/// Probes genesis first. A pruned reth node rejects it with an error naming the earliest
/// available block, which is used directly; otherwise the lowest block the node returns is found
/// by binary search, assuming pruning only removes a contiguous prefix of history.
///
/// Only a missing block or a pruned-history error counts as pruned. Any other error (a timeout,
/// a rate limit, a node bug) is returned, so it fails the run instead of silently raising the
/// floor. With `rate_limit_rps`, probes are spaced to stay within it.
pub async fn earliest_available_block<P: Provider<AnyNetwork>>(
    provider: &P,
    head: BlockNumber,
    rate_limit_rps: Option<u32>,
) -> Result<BlockNumber> {
    let interval =
        rate_limit_rps.map(|rps| std::time::Duration::from_secs_f64(1.0 / rps.max(1) as f64));
    let probe = |block: BlockNumber| async move {
        if let Some(interval) = interval {
            tokio::time::sleep(interval).await;
        }
        let response = provider.get_block_by_number(BlockNumberOrTag::Number(block)).await;
        classify_probe(response.map(|block| block.is_some()))
    };

    match probe(0).await? {
        Probe::Available => return Ok(0),
        Probe::Pruned(Some(earliest)) => return Ok(earliest.min(head)),
        Probe::Pruned(None) => {
            debug!("genesis unavailable, searching for the earliest available block")
        }
    }

    let (mut lo, mut hi) = (1, head);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        match probe(mid).await? {
            Probe::Available => hi = mid,
            Probe::Pruned(Some(earliest)) => return Ok(earliest.min(head)),
            Probe::Pruned(None) => lo = mid + 1,
        }
    }
    Ok(lo)
}

/// Outcome of probing whether a node serves a block.
#[derive(Debug, PartialEq, Eq)]
enum Probe {
    /// The node returned the block.
    Available,
    /// The node pruned the block, with the earliest available block when its error names one.
    Pruned(Option<BlockNumber>),
}

/// Reth's JSON-RPC error code for history that pruning removed.
const PRUNED_HISTORY_ERROR_CODE: i64 = 4444;

/// Classifies a block probe: a missing block or a pruned-history error is pruned, any other error
/// is returned.
fn classify_probe(response: Result<bool, TransportError>) -> Result<Probe> {
    match response {
        Ok(true) => Ok(Probe::Available),
        Ok(false) => Ok(Probe::Pruned(None)),
        Err(err) => {
            if let Some(resp) = err.as_error_resp() {
                let earliest = parse_earliest_available(&resp.message);
                if resp.code == PRUNED_HISTORY_ERROR_CODE || earliest.is_some() {
                    return Ok(Probe::Pruned(earliest));
                }
            }
            Err(eyre::eyre!("probing for the earliest available block failed: {err}"))
        }
    }
}

/// Extracts `N` from an error message containing `earliest available N`, as reth's
/// pruned-history error (code 4444) does:
/// `pruned history unavailable: requested 1, earliest available 26350000`.
fn parse_earliest_available(message: &str) -> Option<BlockNumber> {
    let (_, rest) = message.split_once("earliest available")?;
    let digits: String =
        rest.trim_start_matches([' ', ':']).chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// Returns tracing options for the geth `callTracer`.
fn call_tracer_opts() -> GethDebugTracingOptions {
    GethDebugTracingOptions::default()
        .with_tracer(GethDebugTracerType::BuiltInTracer(GethDebugBuiltInTracerType::CallTracer))
}

/// Returns tracing options for the geth `prestateTracer`.
///
/// The prestate tracer surfaces state-read divergence that the call tracer hides.
fn prestate_tracer_opts() -> GethDebugTracingOptions {
    GethDebugTracingOptions::default()
        .with_tracer(GethDebugTracerType::BuiltInTracer(GethDebugBuiltInTracerType::PreStateTracer))
}

/// Returns tracing options for the geth `callTracer` with logs.
///
/// Logs are attached to their frames and carry log indices, a separate code path from the plain
/// call tracer.
fn call_tracer_with_log_opts() -> GethDebugTracingOptions {
    GethDebugTracingOptions::call_tracer(CallConfig::default().with_log())
}

/// Returns tracing options for the geth `prestateTracer` in diff mode, which reports the
/// post-state of every changed account.
fn prestate_diff_tracer_opts() -> GethDebugTracingOptions {
    GethDebugTracingOptions::prestate_tracer(PreStateConfig {
        diff_mode: Some(true),
        ..Default::default()
    })
}

/// Returns tracing options for the geth `flatCallTracer` with its defaults, which leave out
/// precompile calls, so a node that counts a contract as a precompile drops its frames.
fn flat_call_tracer_opts() -> GethDebugTracingOptions {
    GethDebugTracingOptions::flat_call_tracer(FlatCallConfig::default())
}

/// Returns tracing options for the geth `flatCallTracer` that keep precompile calls and use
/// Parity's error labels, so missing precompile frames and error conversion are compared.
fn flat_call_tracer_with_precompiles_opts() -> GethDebugTracingOptions {
    GethDebugTracingOptions::flat_call_tracer(FlatCallConfig {
        include_precompiles: Some(true),
        convert_parity_errors: Some(true),
    })
}

/// Returns tracing options for the geth `4byteTracer`.
fn four_byte_tracer_opts() -> GethDebugTracingOptions {
    GethDebugTracingOptions::default()
        .with_tracer(GethDebugTracerType::BuiltInTracer(GethDebugBuiltInTracerType::FourByteTracer))
}

/// Returns tracing options for the default struct logger without stack and storage, which
/// compares every executed opcode's gas, cost, depth and refund while keeping responses small.
fn struct_log_opts() -> GethDebugTracingOptions {
    GethDebugTracingOptions {
        config: GethDefaultTracingOptions::default().disable_stack().disable_storage(),
        ..Default::default()
    }
}

/// Builder for [`RpcTester`].
#[derive(Debug)]
pub struct RpcTesterBuilder<P: Provider<AnyNetwork>> {
    /// First RPC node.
    rpc1: P,
    /// Second RPC node.
    rpc2: P,
    /// Whether to query tracing methods.
    use_tracing: bool,
    /// Whether to query reth namespace.
    use_reth: bool,
    /// Whether to call rpc transaction methods for every transaction. Otherwise, the first
    /// transaction of each distinct type in the block.
    use_all_txes: bool,
    /// Whether to skip extended eth methods not supported by all clients.
    skip_extended_eth: bool,
    /// Whether to compare the moving `safe` and `finalized` tags.
    use_finality_tags: bool,
    /// Whether to compare `debug_executionWitness` for the newest tested block.
    use_execution_witness: bool,
    /// Whether to query Otterscan's `ots` namespace.
    use_ots: bool,
    /// Whether to query Tempo's `tempo` namespace.
    use_tempo: bool,
    /// Oldest block both nodes still serve.
    history_floor: BlockNumber,
    /// Maximum requests per second for rate limiting.
    rate_limit_rps: Option<u32>,
}

impl<P: Provider<AnyNetwork>> RpcTesterBuilder<P> {
    /// Creates a new builder with default settings.
    pub const fn new(rpc1: P, rpc2: P) -> Self {
        Self {
            rpc1,
            rpc2,
            use_tracing: false,
            use_reth: false,
            use_all_txes: false,
            skip_extended_eth: false,
            use_finality_tags: false,
            use_execution_witness: false,
            use_ots: false,
            use_tempo: false,
            history_floor: 0,
            rate_limit_rps: None,
        }
    }

    /// Enables or disables tracing calls.
    pub const fn with_tracing(mut self, is_enabled: bool) -> Self {
        self.use_tracing = is_enabled;
        self
    }

    /// Enables or disables reth namespace.
    pub const fn with_reth(mut self, is_enabled: bool) -> Self {
        self.use_reth = is_enabled;
        self
    }

    /// Enables or disables querying all transactions. Will only query the first transaction of
    /// each distinct type in the block if disabled.
    pub const fn with_all_txes(mut self, is_enabled: bool) -> Self {
        self.use_all_txes = is_enabled;
        self
    }

    /// Skips extended eth methods not supported by all clients (e.g.,
    /// `eth_getRawTransactionByBlockNumberAndIndex`).
    pub const fn skip_extended_eth(mut self, skip: bool) -> Self {
        self.skip_extended_eth = skip;
        self
    }

    /// Enables or disables comparing the moving `safe` and `finalized` tags. Requires both nodes
    /// to share a consensus view.
    pub const fn with_finality_tags(mut self, is_enabled: bool) -> Self {
        self.use_finality_tags = is_enabled;
        self
    }

    /// Enables or disables comparing `debug_executionWitness` for the newest tested block.
    /// Witness generation re-executes the block, so this is opt-in.
    pub const fn with_execution_witness(mut self, is_enabled: bool) -> Self {
        self.use_execution_witness = is_enabled;
        self
    }

    /// Enables or disables Otterscan's `ots` namespace, which only some clients serve.
    pub const fn with_ots(mut self, is_enabled: bool) -> Self {
        self.use_ots = is_enabled;
        self
    }

    /// Enables or disables querying Tempo's `tempo` namespace.
    pub const fn with_tempo(mut self, is_enabled: bool) -> Self {
        self.use_tempo = is_enabled;
        self
    }

    /// Sets the oldest block both nodes still serve, such as the result of
    /// [`earliest_available_block`] for a pruned node. A non-zero floor skips the genesis-pinned
    /// `earliest` tag comparisons.
    pub const fn with_history_floor(mut self, block: BlockNumber) -> Self {
        self.history_floor = block;
        self
    }

    /// Sets the rate limit in requests per second.
    /// If None, no rate limiting is applied.
    pub const fn with_rate_limit(mut self, rps: Option<u32>) -> Self {
        self.rate_limit_rps = rps;
        self
    }

    /// Builds and returns the [`RpcTester`].
    pub fn build(self) -> RpcTester<P> {
        RpcTester {
            rpc1: self.rpc1,
            rpc2: self.rpc2,
            use_tracing: self.use_tracing,
            use_reth: self.use_reth,
            use_all_txes: self.use_all_txes,
            skip_extended_eth: self.skip_extended_eth,
            use_finality_tags: self.use_finality_tags,
            use_execution_witness: self.use_execution_witness,
            use_ots: self.use_ots,
            use_tempo: self.use_tempo,
            history_floor: self.history_floor,
            rate_limit_rps: self.rate_limit_rps,
            last_request_time: tokio::sync::Mutex::new(std::time::Instant::now()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_json_rpc::ErrorPayload;
    use alloy_provider::{ProviderBuilder, RootProvider};
    use alloy_transport::{mock::Asserter, TransportErrorKind};
    use serde_json::json;

    fn error_resp(code: i64, message: &str) -> TransportError {
        TransportError::ErrorResp(ErrorPayload {
            code,
            message: message.to_string().into(),
            data: None,
        })
    }

    fn error_resp_with_data(code: i64, message: &str, data: &str) -> TransportError {
        TransportError::ErrorResp(ErrorPayload {
            code,
            message: message.to_string().into(),
            data: Some(serde_json::value::RawValue::from_string(data.to_string()).unwrap()),
        })
    }

    #[test]
    fn parses_earliest_available_from_pruned_history_error() {
        assert_eq!(
            parse_earliest_available(
                "pruned history unavailable: requested 25892652, earliest available 26350000"
            ),
            Some(26_350_000)
        );
        assert_eq!(parse_earliest_available("header not found"), None);
        assert_eq!(parse_earliest_available("earliest available"), None);
    }

    #[test]
    fn classifies_block_probes() {
        assert_eq!(classify_probe(Ok(true)).unwrap(), Probe::Available);
        assert_eq!(classify_probe(Ok(false)).unwrap(), Probe::Pruned(None));
        assert_eq!(
            classify_probe(Err(error_resp(
                4444,
                "pruned history unavailable: requested 1, earliest available 26350000"
            )))
            .unwrap(),
            Probe::Pruned(Some(26_350_000))
        );
        assert_eq!(
            classify_probe(Err(error_resp(4444, "pruned history unavailable"))).unwrap(),
            Probe::Pruned(None)
        );
        // Rate limits, transport failures and other errors must not count as pruned.
        assert!(classify_probe(Err(error_resp(429, "too many requests"))).is_err());
        assert!(classify_probe(Err(error_resp(-32000, "internal error"))).is_err());
        assert!(classify_probe(Err(TransportErrorKind::backend_gone())).is_err());
    }

    #[test]
    fn matching_error_responses_agree() {
        assert!(errors_match(
            &error_resp(-32000, "transaction not found"),
            &error_resp(-32000, "transaction not found")
        ));
    }

    #[test]
    fn different_error_responses_diverge() {
        assert!(!errors_match(
            &error_resp(-32000, "transaction not found"),
            &error_resp(-32601, "transaction not found")
        ));
        assert!(!errors_match(&error_resp(-32000, "a"), &error_resp(-32000, "b")));
    }

    #[test]
    fn different_revert_data_diverges() {
        // Reverts share code 3 and a generic message; the revert bytes live in data.
        assert!(!errors_match(
            &error_resp_with_data(3, "execution reverted", "\"0xdead\""),
            &error_resp_with_data(3, "execution reverted", "\"0xbeef\"")
        ));
        assert!(!errors_match(
            &error_resp_with_data(3, "execution reverted", "\"0xdead\""),
            &error_resp(3, "execution reverted")
        ));
        assert!(errors_match(
            &error_resp_with_data(3, "execution reverted", "\"0xdead\""),
            &error_resp_with_data(3, "execution reverted", "\"0xdead\"")
        ));
    }

    #[test]
    fn empty_revert_data_matches_missing_data() {
        assert!(errors_match(
            &error_resp_with_data(3, "execution reverted", "\"0x\""),
            &error_resp(3, "execution reverted")
        ));
        assert!(errors_match(
            &error_resp(3, "execution reverted"),
            &error_resp_with_data(3, "execution reverted", "\"0x\"")
        ));
    }

    #[test]
    fn transport_errors_never_agree() {
        assert!(!errors_match(
            &TransportErrorKind::backend_gone(),
            &TransportErrorKind::backend_gone()
        ));
        assert!(!errors_match(&TransportErrorKind::backend_gone(), &error_resp(-32000, "a")));
    }

    #[test]
    fn fee_replay_pays_the_transaction_fees() {
        let tx = json!({
            "type": "0x76",
            "from": "0xaa",
            "gas": "0x5208",
            "gasPrice": "0x5",
            "maxFeePerGas": "0x9",
            "maxPriorityFeePerGas": "0x1",
            "calls": [{ "to": "0xbb", "value": "0x0", "input": "0x01" }],
            "feeToken": "0xcc",
            "nonce": "0x2",
            "nonceKey": "0x1",
            "validBefore": null,
            "feePayerSignature": { "r": "0x1" },
            "aaAuthorizationList": [],
        });
        assert_eq!(
            fee_call_request_json(&tx),
            json!({
                "from": "0xaa",
                "gas": "0x5208",
                "calls": [{ "to": "0xbb", "value": "0x0", "input": "0x01" }],
                "maxFeePerGas": "0x9",
                "maxPriorityFeePerGas": "0x1",
                "feeToken": "0xcc",
                "nonce": "0x2",
                "nonceKey": "0x1",
            })
        );

        let legacy = json!({ "type": "0x0", "from": "0xaa", "to": "0xbb", "gasPrice": "0x5" });
        assert_eq!(
            fee_call_request_json(&legacy),
            json!({ "from": "0xaa", "to": "0xbb", "gasPrice": "0x5" })
        );
    }

    #[test]
    fn samples_tempo_transactions_by_feature() {
        let tx = |calls: usize, nonce_key: &str| {
            json!({
                "type": "0x76",
                "calls": vec![json!({ "to": "0xbb" }); calls],
                "nonceKey": nonce_key,
                "signature": { "type": "secp256k1" },
            })
        };
        let single = sample_key(&tx(1, "0x0"));
        assert_ne!(single, sample_key(&tx(2, "0x0")));
        assert_ne!(single, sample_key(&tx(1, "0x5")));
        assert_ne!(sample_key(&tx(1, "0x5")), sample_key(&tx(1, &format!("0x{}", "f".repeat(64)))));
        assert_eq!(single, sample_key(&tx(1, "0x0")));

        let mut create = tx(2, "0x0");
        create["calls"][0]["to"] = JsonValue::Null;
        assert_ne!(sample_key(&tx(2, "0x0")), sample_key(&create));
        assert_eq!(sample_key(&json!({ "type": "0x2", "to": "0xbb" })), json!("0x2"));
    }

    #[test]
    fn replays_tempo_calls() {
        let tx = json!({
            "type": "0x76",
            "from": "0xaa",
            "gas": "0x5208",
            "calls": [{ "to": "0xbb", "value": "0x0", "input": "0x01", "data": null }],
            "nonceKey": "0x1",
            "validBefore": "0x64",
            "validAfter": null,
        });
        assert_eq!(
            call_request_json(&tx),
            json!({
                "from": "0xaa",
                "gas": "0x5208",
                "calls": [{ "to": "0xbb", "value": "0x0", "input": "0x01" }],
                "nonceKey": "0x1",
                "validBefore": "0x64",
                "maxFeePerGas": "0x0",
                "maxPriorityFeePerGas": "0x0",
            })
        );
    }

    #[test]
    fn normalizes_access_list_item_and_storage_key_order() {
        let mut rpc1 = json!({
            "accessList": [
                {"address": "0xbb", "storageKeys": ["0x02", "0x01"]},
                {"address": "0xaa", "storageKeys": []}
            ],
            "gasUsed": "0x1234"
        });
        let mut rpc2 = json!({
            "accessList": [
                {"address": "0xaa", "storageKeys": []},
                {"address": "0xbb", "storageKeys": ["0x01", "0x02"]}
            ],
            "gasUsed": "0x1234"
        });

        normalize_access_list_result(&mut rpc1);
        normalize_access_list_result(&mut rpc2);

        assert_eq!(rpc1, rpc2);

        rpc2["gasUsed"] = json!("0x1235");
        assert_ne!(rpc1, rpc2);
    }

    #[tokio::test]
    async fn matching_missing_transaction_traces_agree() {
        assert!(compare_trace_responses(json!(null), json!(null)).await.is_ok());
        assert!(compare_trace_responses(json!([]), json!([])).await.is_ok());
    }

    #[tokio::test]
    async fn null_and_empty_transaction_traces_diverge() {
        let result = compare_trace_responses(json!(null), json!([])).await;
        let Err(TestError::Diff { rpc1, rpc2, .. }) = result else {
            panic!("null and an empty trace array must differ");
        };
        assert_eq!(rpc1, json!(null));
        assert_eq!(rpc2, json!([]));
    }

    #[tokio::test]
    async fn malformed_transaction_traces_do_not_agree() {
        for response in [json!({}), json!([{}])] {
            let result = compare_trace_responses(response.clone(), response).await;
            assert!(matches!(result, Err(TestError::ErrDiff { .. })));
        }
    }

    async fn compare_trace_responses(rpc1: JsonValue, rpc2: JsonValue) -> Result<(), TestError> {
        let responses1 = Asserter::new();
        responses1.push_success(&rpc1);
        let responses2 = Asserter::new();
        responses2.push_success(&rpc2);
        let tester = RpcTester::builder(
            ProviderBuilder::new()
                .network::<AnyNetwork>()
                .connect_mocked_client(responses1)
                .root()
                .clone(),
            ProviderBuilder::new()
                .network::<AnyNetwork>()
                .connect_mocked_client(responses2)
                .root()
                .clone(),
        )
        .with_tracing(true)
        .build();

        // The raw-request macro needs the concrete provider type in scope.
        type P = RootProvider<AnyNetwork>;
        rpc_raw!(
            tester,
            trace_transaction,
            Option<Vec<LocalizedTransactionTrace>>,
            (B256::repeat_byte(0xff),)
        )
        .await
        .1
    }
}
