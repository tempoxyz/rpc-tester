//! [`RpcTester`] implementation.

use super::{MethodName, TestError};
use crate::{get_logs, report::report, rpc, rpc_raw, rpc_with_block};
use alloy_primitives::{Address, BlockHash, BlockNumber, B256, U256};
use alloy_provider::{
    ext::{DebugApi, TraceApi},
    network::{AnyNetwork, AnyRpcBlock, TransactionResponse},
    Provider,
};
use alloy_rpc_types::{BlockId, BlockNumberOrTag, Filter};
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
use tracing::{debug, info, trace};

// Alias type
type BlockTestResults = BTreeMap<BlockNumber, Vec<(MethodName, Result<(), TestError>)>>;

// Alias type for BalanceChanges
type BalanceChanges = HashMap<Address, U256>;
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
    /// Whether to call rpc transaction methods for every transaction. Otherwise, just the first of
    /// the block.
    use_all_txes: bool,
    /// Whether to skip extended eth methods not supported by all clients (e.g.,
    /// `eth_getRawTransactionByBlockNumberAndIndex`).
    skip_extended_eth: bool,
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
    pub async fn run(&self, block_range: RangeInclusive<BlockNumber>) -> Result<()> {
        self.test_per_block(block_range.clone()).await?;
        self.test_block_range(block_range.clone()).await?;
        self.test_negative(*block_range.end()).await?;
        Ok(())
    }

    /// Verifies RPC calls applicable to single blocks for each of the given blocks.
    ///
    /// Unlike [`Self::run`], this does not run block range tests, so the blocks do not need to be
    /// contiguous. This is intended for sampled historical blocks, see
    /// [`historical_blocks`](crate::historical_blocks).
    pub async fn run_blocks(&self, blocks: impl IntoIterator<Item = BlockNumber>) -> Result<()> {
        self.test_per_block(blocks).await
    }

    /// Verifies RPC calls applicable to single blocks.
    async fn test_per_block(
        &self,
        blocks: impl IntoIterator<Item = BlockNumber>,
    ) -> Result<(), eyre::Error> {
        let mut results = BlockTestResults::new();

        for block_number in blocks {
            info!(block_number, "testing rpc");

            let mut tests = vec![];

            let (block, block_hash, block_tag, block_id) = self.fetch_block(block_number).await?;

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
                rpc!(self, trace_block, block_id),
                rpc!(self, trace_replay_block_transactions, block_id, &[TraceType::StateDiff][..]),
                rpc!(self, debug_trace_block_by_hash, block_hash, call_tracer_opts()),
                rpc!(self, debug_trace_block_by_number, block_tag, call_tracer_opts()),
                rpc!(self, debug_trace_block_by_number, block_tag, prestate_tracer_opts()),
                get_logs!(self, &Filter::new().select(block_number)),
                get_logs!(self, &Filter::new().select(block_number).address(vec![
                    "0x6b175474e89094c44da98b954eedeac495271d0f".parse::<Address>().unwrap(), // dai
                    "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48".parse::<Address>().unwrap(), // usdc
                ]))
            ];

            tests.extend(block_calls);

            // // Transaction/Receipt based RPCs
            for (index, (tx_hash, tx_from)) in
                block.transactions.txns().map(|t| (t.tx_hash(), t.from)).enumerate()
            {
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

                if !self.use_all_txes {
                    break;
                }
            }
            let block_results = futures::future::join_all(tests).await;
            results.insert(block_number, block_results);
        }
        report(results.into_iter().map(|(k, v)| (format!("Block Number {k}"), v)).collect())
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

    /// Fetches block and block identifiers from `self.truth`.
    async fn fetch_block(
        &self,
        block_number: u64,
    ) -> Result<(AnyRpcBlock, BlockHash, BlockNumberOrTag, BlockId), eyre::Error> {
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
        let block_tag = BlockNumberOrTag::Number(block_number);
        let block_id = BlockId::Number(block_tag);
        Ok((block, block_hash, block_tag, block_id))
    }

    /// Apply rate limiting if configured.
    /// Sleeps if necessary to maintain the configured rate limit.
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
        if name.starts_with("reth") && !self.use_reth || name.contains("trace") && !self.use_tracing
        {
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
                    Err(TestError::Diff {
                        rpc1: serde_json::to_value(&rpc1).expect("should json"),
                        rpc2: serde_json::to_value(&rpc2).expect("should json"),
                        args,
                    })
                }
            }
            // Both nodes rejecting the call with the same error response is agreement, e.g. a
            // deliberate miss-path probe or a namespace disabled on both nodes.
            (Err(e1), Err(e2)) => {
                if errors_match(&e1, &e2) {
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

/// Returns whether two RPC errors are the same error response.
///
/// Only JSON-RPC error responses are compared (by code and message); transport-level failures
/// such as timeouts are never treated as agreement.
fn errors_match(e1: &TransportError, e2: &TransportError) -> bool {
    match (e1.as_error_resp(), e2.as_error_resp()) {
        (Some(r1), Some(r2)) => r1.code == r2.code && r1.message == r2.message,
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
    /// Whether to call rpc transaction methods for every transaction. Otherwise, just the first of
    /// the block.
    use_all_txes: bool,
    /// Whether to skip extended eth methods not supported by all clients.
    skip_extended_eth: bool,
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

    /// Enables or disables querying all transactions. Will only query the first of the block if
    /// disabled.
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

    fn error_resp(code: i64, message: &str) -> TransportError {
        TransportError::ErrorResp(ErrorPayload {
            code,
            message: message.to_string().into(),
            data: None,
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
    fn transport_errors_never_agree() {
        assert!(!errors_match(
            &TransportErrorKind::backend_gone(),
            &TransportErrorKind::backend_gone()
        ));
        assert!(!errors_match(&TransportErrorKind::backend_gone(), &error_resp(-32000, "a")));
    }
}
