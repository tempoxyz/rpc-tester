//! [`RpcTester`] implementation.

use super::{MethodName, TestError};
use crate::{filters, get_logs, report::report, rpc, rpc_raw, rpc_with_block};
use alloy_primitives::{Address, BlockHash, BlockNumber, Bytes, B256, U256};
use alloy_provider::{
    ext::{DebugApi, TraceApi},
    network::{AnyNetwork, AnyRpcBlock, TransactionResponse},
    Provider,
};
use alloy_rpc_types::{AccessListResult, BlockId, BlockNumberOrTag, Filter, FilterId};
use alloy_rpc_types_trace::{
    filter::TraceFilter,
    geth::{GethDebugBuiltInTracerType, GethDebugTracerType, GethDebugTracingOptions},
    parity::TraceType,
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
        let filters = self.test_filters(block_range).await;
        per_block.and(block_range_result).and(negative).and(tags).and(witness).and(filters)
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
                    block_hash,
                    alloy_rpc_types::BlockTransactionsKind::Full
                ),
                rpc!(
                    self,
                    get_block_by_number,
                    block_tag,
                    alloy_rpc_types::BlockTransactionsKind::Full
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
                rpc!(self, trace_replay_block_transactions, block_id, &[TraceType::StateDiff][..]),
                rpc!(self, debug_trace_block_by_hash, block_hash, call_tracer_opts()),
                rpc!(self, debug_trace_block_by_number, block_tag, call_tracer_opts()),
                rpc!(self, debug_trace_block_by_number, block_tag, prestate_tracer_opts()),
                get_logs!(self, &Filter::new().select(block_number)),
                get_logs!(self, &Filter::new().at_block_hash(block_hash)),
                get_logs!(self, &Filter::new().select(block_number).address(vec![
                    "0x6b175474e89094c44da98b954eedeac495271d0f".parse::<Address>().unwrap(), // dai
                    "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48".parse::<Address>().unwrap(), // usdc
                ]))
            ];

            tests.extend(block_calls);

            // // Transaction/Receipt based RPCs
            let mut seen_tx_types = Vec::new();
            for (index, tx) in block.transactions.txns().enumerate() {
                let tx_json = serde_json::to_value(tx).expect("should json");

                // Without --use-all-txes, sample the first transaction of each distinct type
                // instead of just the first of the block: the first transaction is often the same
                // kind of searcher activity, while rarer types (blob, 7702, legacy) exercise
                // different serialization and execution paths.
                if !self.use_all_txes {
                    let tx_type = tx_json.get("type").cloned();
                    if seen_tx_types.contains(&tx_type) {
                        continue;
                    }
                    seen_tx_types.push(tx_type);
                }

                let (tx_hash, tx_from) = (tx.tx_hash(), tx.from);

                // Replaying the transaction as a call at the parent block exercises EVM execution
                // on historical state, which no data-retrieval method touches. Both nodes get the
                // identical request, so results (or revert errors) must agree. Note that gas
                // estimates and access lists are implementation-defined, so those two comparisons
                // are meaningful primarily for same-client pairs.
                let call_args =
                    (call_request_json(&tx_json), BlockId::number(block_number.saturating_sub(1)));
                let estimate_args = call_args.clone();
                let access_list_args = call_args.clone();
                #[rustfmt::skip]
                let exec_calls = vec![
                    rpc_raw!(self, eth_call, Bytes, call_args),
                    rpc_raw!(self, eth_estimateGas, U256, estimate_args),
                    rpc_raw!(self, eth_createAccessList, AccessListResult, access_list_args),
                ];
                tests.extend(exec_calls);

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
                    rpc!(self, debug_trace_transaction, tx_hash, prestate_tracer_opts()),
                ];
                tests.extend(tx_calls);

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
            rpc!(self, get_block_by_hash, missing_hash, alloy_rpc_types::BlockTransactionsKind::Full),
            rpc!(self, get_block_by_number, future_tag, alloy_rpc_types::BlockTransactionsKind::Full),
            rpc!(self, get_block_transaction_count_by_number, future_tag),
            rpc!(self, get_block_receipts, BlockId::Number(future_tag)),
            rpc!(self, get_transaction_by_hash, missing_hash),
            rpc!(self, get_raw_transaction_by_hash, missing_hash),
            rpc!(self, get_transaction_receipt, missing_hash),
            rpc!(self, get_transaction_by_block_number_and_index, head_tag, 100_000usize),
            rpc!(self, trace_transaction, missing_hash),
            rpc!(self, debug_trace_transaction, missing_hash, call_tracer_opts()),
        ];

        report(vec![("Negative probes".to_string(), futures::future::join_all(tests).await)])
    }

    /// Verifies RPC calls addressed by block tag rather than number or hash.
    ///
    /// `earliest` is compared unconditionally since it is pinned to genesis. `safe` and
    /// `finalized` move with the chain and require both nodes to share a consensus view, so they
    /// are gated behind the finality tags option.
    async fn test_tags(&self) -> Result<(), eyre::Error> {
        #[rustfmt::skip]
        let mut tests = vec![
            rpc!(self, get_block_by_number, BlockNumberOrTag::Earliest, alloy_rpc_types::BlockTransactionsKind::Full),
            rpc!(self, get_block_transaction_count_by_number, BlockNumberOrTag::Earliest),
            rpc!(self, get_block_receipts, BlockId::Number(BlockNumberOrTag::Earliest)),
            rpc!(self, get_uncle_count, BlockId::Number(BlockNumberOrTag::Earliest)),
        ];

        if self.use_finality_tags {
            for tag in [BlockNumberOrTag::Safe, BlockNumberOrTag::Finalized] {
                #[rustfmt::skip]
                tests.push(rpc!(self, get_block_by_number, tag, alloy_rpc_types::BlockTransactionsKind::Hashes));
            }
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

    /// Verifies the poll-based filter API.
    ///
    /// Filters are node-local state, so the nodes cannot be sent identical requests. Every test
    /// runs the same scenario against each node and compares what the scenarios observed, reduced
    /// to what holds on any correct node no matter when blocks and transactions arrive (see
    /// [`filters`]). The scenarios wait for the chain to move, which makes this suite slower than
    /// the others, and they need endpoints that route every request to the same backend.
    async fn test_filters(&self, block_range: RangeInclusive<u64>) -> Result<(), eyre::Error> {
        let start = *block_range.start();
        let end = *block_range.end();
        let addresses = vec![
            "0x6b175474e89094c44da98b954eedeac495271d0f".parse::<Address>().unwrap(), // dai
            "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48".parse::<Address>().unwrap(), // usdc
        ];

        // The tested range, served as history through `eth_getFilterLogs`.
        let range_filter = Filter::new().from_block(start).to_block(end).address(addresses.clone());
        // A filter that already covers history when it is installed, which polls must not rescan.
        // The future variant additionally bounds it with a `toBlock` far ahead of the node's head,
        // which bounds the filter, not the poll.
        let watch_filter = Filter::new().from_block(start).address(addresses);
        let missing_args = (FilterId::Str("0xdeadbeef".to_string()),);

        let filter_logs = Box::pin(self.test_rpc_call(
            "eth_getFilterLogs",
            Some(format!("{range_filter:?}")),
            |provider: &P| filters::filter_logs(provider, &range_filter),
        ))
            as Pin<Box<dyn Future<Output = (MethodName, Result<(), TestError>)> + Send>>;
        let watch_polls = Box::pin(self.test_rpc_call(
            "eth_getFilterChanges",
            Some(format!("{watch_filter:?}")),
            |provider: &P| filters::poll_log_filter(provider, &watch_filter),
        ))
            as Pin<Box<dyn Future<Output = (MethodName, Result<(), TestError>)> + Send>>;
        let future_polls = Box::pin(self.test_rpc_call(
            "eth_getFilterChanges",
            Some(format!(
                "{watch_filter:?} with toBlock {} blocks ahead of the head",
                filters::FUTURE_TO_BLOCK_OFFSET
            )),
            |provider: &P| filters::poll_future_to_block_filter(provider, &watch_filter),
        ))
            as Pin<Box<dyn Future<Output = (MethodName, Result<(), TestError>)> + Send>>;
        let pending_polls = Box::pin(self.test_rpc_call(
            "eth_getFilterChanges",
            Some("pending transactions".to_string()),
            |provider: &P| {
                let node =
                    if std::ptr::eq(provider, &raw const self.rpc1) { "rpc1" } else { "rpc2" };
                filters::poll_pending_transaction_filter(provider, node)
            },
        ))
            as Pin<Box<dyn Future<Output = (MethodName, Result<(), TestError>)> + Send>>;

        #[rustfmt::skip]
        let tests = vec![
            filter_logs,
            watch_polls,
            future_polls,
            pending_polls,
            // Uninstalling an unknown filter is a plain `false`, not an error.
            rpc_raw!(self, eth_uninstallFilter, bool, missing_args),
        ];

        report(vec![("Filters".to_string(), futures::future::join_all(tests).await)])
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
                .get_block_by_number(block_number.into(), true.into())
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
                .get_block_by_number(block_number.into(), false.into())
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
    /// retries, block/receipt prefetches and the requests within a filter scenario are not
    /// throttled.
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
        // independent of the tracing gate.
        let skip = if name == "debug_executionWitness" {
            !self.use_execution_witness
        } else {
            name.starts_with("reth") && !self.use_reth ||
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
/// subset of fields (`from`, `to`, `gas`, `value`, `input`).
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
    request.insert("gasPrice".to_string(), serde_json::Value::String("0x0".to_string()));
    serde_json::Value::Object(request)
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
                r1.data.as_ref().map(|data| data.get()) ==
                    r2.data.as_ref().map(|data| data.get())
        }
        _ => false,
    }
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
            rate_limit_rps: self.rate_limit_rps,
            last_request_time: tokio::sync::Mutex::new(std::time::Instant::now()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_json_rpc::ErrorPayload;
    use alloy_transport::TransportErrorKind;
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
    fn transport_errors_never_agree() {
        assert!(!errors_match(
            &TransportErrorKind::backend_gone(),
            &TransportErrorKind::backend_gone()
        ));
        assert!(!errors_match(&TransportErrorKind::backend_gone(), &error_resp(-32000, "a")));
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
}
