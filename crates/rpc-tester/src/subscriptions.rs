//! [`SubscriptionTester`] implementation, covering `eth_subscribe`.
//!
//! Deterministic streams (`newHeads`, `logs`, `transactionReceipts`) are collected on both nodes
//! over the same window of new blocks, checked for ordering, and compared with each other and with
//! the matching HTTP queries. Node-local streams (pending transactions, `syncing`) can't be
//! compared between nodes, so only their liveness and encoding are checked.

use super::{MethodName, ReportResults, TestError};
use crate::{report::report, tester::errors_match};
use alloy_primitives::{BlockHash, BlockNumber, B256};
use alloy_provider::{
    network::{AnyNetwork, AnyRpcHeader, AnyRpcTransaction, AnyTransactionReceipt},
    Provider, RootProvider,
};
use alloy_pubsub::Subscription;
use alloy_rpc_types::{Log, SyncStatus};
use alloy_transport::TransportError;
use eyre::Result;
use futures::future::{join_all, OptionFuture};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    future::pending,
    ops::RangeInclusive,
    time::Duration,
};
use tokio::{
    sync::broadcast::error::RecvError,
    time::{sleep_until, timeout, Instant},
};
use tracing::{info, warn};

/// How long the log and receipt streams are drained after the last head, so items for the last
/// compared block that arrive just behind its successor's head are still collected.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// Block fields that are not part of the header, and so not expected in a `newHeads` message.
const NON_HEADER_FIELDS: &[&str] =
    &["transactions", "uncles", "withdrawals", "size", "totalDifficulty"];

/// Subscribes to `eth_subscribe` streams on two nodes over `WebSocket` and verifies them, see the
/// [module docs](self).
#[derive(Debug)]
pub struct SubscriptionTester<P> {
    /// First node over HTTP, the source of truth for its own streams.
    rpc1: P,
    /// Second node over HTTP.
    rpc2: P,
    /// First node over `WebSocket`.
    ws1: RootProvider<AnyNetwork>,
    /// Second node over `WebSocket`.
    ws2: RootProvider<AnyNetwork>,
    /// Number of new blocks whose stream items are compared.
    num_blocks: u64,
    /// Maximum wait for the next head, and for the first message of a node-local stream.
    timeout: Duration,
}

impl<P> SubscriptionTester<P>
where
    P: Provider<AnyNetwork>,
{
    /// Connects to both `WebSocket` endpoints. `rpc1` and `rpc2` serve the HTTP side of the same
    /// nodes and are used to check each node's streams against its own HTTP responses.
    pub async fn connect(
        rpc1: P,
        rpc2: P,
        ws1: &str,
        ws2: &str,
        num_blocks: u64,
        timeout: Duration,
    ) -> Result<Self> {
        let ws1 = RootProvider::<AnyNetwork>::connect(ws1)
            .await
            .map_err(|err| eyre::eyre!("failed to connect to ws1: {err}"))?;
        let ws2 = RootProvider::<AnyNetwork>::connect(ws2)
            .await
            .map_err(|err| eyre::eyre!("failed to connect to ws2: {err}"))?;
        Ok(Self { rpc1, rpc2, ws1, ws2, num_blocks, timeout })
    }

    /// Collects every stream over the next `num_blocks` blocks (plus one, so items for the last
    /// compared block have settled) and reports all checks.
    pub async fn run(&self) -> Result<()> {
        let mut general = Vec::new();

        let (subs1, subs2) = tokio::join!(subscribe_all(&self.ws1), subscribe_all(&self.ws2));
        let (mut subs1, mut subs2) = accept_subscriptions(subs1, subs2, &mut general);

        // Subscribing first means every block above the tip arrives on the streams.
        let (tip1, tip2) = tokio::join!(self.rpc1.get_block_number(), self.rpc2.get_block_number());
        let start = tip1?.max(tip2?);
        let window = start + 1..=start + self.num_blocks;
        let target = start + self.num_blocks + 1;
        info!(?window, "collecting subscriptions");

        let local1 = subs1.extract_if(|stream, _| !stream.is_deterministic()).collect();
        let local2 = subs2.extract_if(|stream, _| !stream.is_deterministic()).collect();
        let collect1 = collect(&mut subs1, target, self.timeout);
        let collect2 = collect(&mut subs2, target, self.timeout);
        let liveness1 = liveness(&self.rpc1, local1, self.timeout);
        let liveness2 = liveness(&self.rpc2, local2, self.timeout);
        let (collected1, collected2, liveness1, liveness2) =
            tokio::join!(collect1, collect2, liveness1, liveness2);

        for (node, results) in [("rpc1", liveness1), ("rpc2", liveness2)] {
            for (stream, result) in results {
                general.push((format!("{node} {}", stream.name()), result));
            }
        }

        let streams1 = NodeStreams::analyze("rpc1", collected1, &mut general);
        let streams2 = NodeStreams::analyze("rpc2", collected2, &mut general);

        let mut results: ReportResults = vec![("subscriptions".to_string(), general)];
        results.extend(self.compare_window(window, streams1.as_ref(), streams2.as_ref()).await);
        report(results)
    }

    /// Compares the stream items of every block in `window` with each node's HTTP responses and
    /// between the nodes.
    async fn compare_window(
        &self,
        window: RangeInclusive<BlockNumber>,
        streams1: Option<&NodeStreams>,
        streams2: Option<&NodeStreams>,
    ) -> ReportResults {
        let mut results = Vec::new();
        for number in window {
            let mut block_results = Vec::new();

            let (http1, http2) = tokio::join!(
                OptionFuture::from(streams1.map(|s| s.canonical(&self.rpc1, number))),
                OptionFuture::from(streams2.map(|s| s.canonical(&self.rpc2, number))),
            );
            let mut compared = [None, None];
            for (index, (node, streams, http)) in
                [("rpc1", streams1, http1), ("rpc2", streams2, http2)].into_iter().enumerate()
            {
                let (Some(streams), Some(http)) = (streams, http) else { continue };
                match http {
                    Ok(http) => {
                        compared[index] =
                            streams.compare_http(node, number, &http, &mut block_results);
                    }
                    Err(err) => block_results.push((
                        format!("{node} eth_getBlockByNumber"),
                        Err(node_err(node, format!("{err:?}"))),
                    )),
                }
            }

            if let [Some(items1), Some(items2)] = &compared {
                if items1.hash == items2.hash {
                    items1.compare(items2, &mut block_results);
                } else {
                    block_results.push((
                        "canonical hash".to_string(),
                        Err(TestError::Rpc1Err(format!(
                            "nodes disagree on the canonical hash: rpc1 {} vs rpc2 {}",
                            items1.hash, items2.hash
                        ))),
                    ));
                }
            }

            results.push((format!("subscriptions block {number}"), block_results));
        }
        results
    }
}

/// A subscription stream exercised by [`SubscriptionTester`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Stream {
    NewHeads,
    Logs,
    TransactionReceipts,
    PendingTransactions,
    FullPendingTransactions,
    Syncing,
}

impl Stream {
    const ALL: [Self; 6] = [
        Self::NewHeads,
        Self::Logs,
        Self::TransactionReceipts,
        Self::PendingTransactions,
        Self::FullPendingTransactions,
        Self::Syncing,
    ];

    const fn name(self) -> &'static str {
        match self {
            Self::NewHeads => "newHeads",
            Self::Logs => "logs",
            Self::TransactionReceipts => "transactionReceipts",
            Self::PendingTransactions => "newPendingTransactions",
            Self::FullPendingTransactions => "newPendingTransactions(full)",
            Self::Syncing => "syncing",
        }
    }

    /// `eth_subscribe` parameters. `logs` gets an empty filter so every log of a block is
    /// emitted and can be compared with `eth_getLogs` for the block hash.
    fn params(self) -> Vec<Value> {
        match self {
            Self::Logs => vec![json!("logs"), json!({})],
            Self::FullPendingTransactions => vec![json!("newPendingTransactions"), json!(true)],
            Self::NewHeads => vec![json!("newHeads")],
            Self::TransactionReceipts => vec![json!("transactionReceipts")],
            Self::PendingTransactions => vec![json!("newPendingTransactions")],
            Self::Syncing => vec![json!("syncing")],
        }
    }

    /// Local buffer size. Every log is its own message, so a busy block emits thousands at once,
    /// and a lagged buffer drops messages, which fails the ordering checks.
    const fn channel_size(self) -> usize {
        match self {
            Self::Logs | Self::TransactionReceipts | Self::FullPendingTransactions => 1 << 16,
            _ => 1 << 10,
        }
    }

    const fn is_deterministic(self) -> bool {
        matches!(self, Self::NewHeads | Self::Logs | Self::TransactionReceipts)
    }
}

/// Accepted subscriptions of one node.
type NodeSubscriptions = HashMap<Stream, Subscription<Value>>;

/// Result of each subscription request of one node, in [`Stream::ALL`] order.
type SubscribeResults = Vec<(Stream, Result<Subscription<Value>, TransportError>)>;

async fn subscribe_all(ws: &RootProvider<AnyNetwork>) -> SubscribeResults {
    let mut results = Vec::new();
    for stream in Stream::ALL {
        let sub =
            ws.subscribe::<_, Value>(stream.params()).channel_size(stream.channel_size()).await;
        results.push((stream, sub));
    }
    results
}

/// Applies the superset semantics to subscription requests: both nodes rejecting a stream with
/// the same error skips it, rpc1 rejecting one rpc2 serves fails, and rpc1 serving one rpc2
/// rejects is checked on rpc1 alone. `newHeads` is required, since every deterministic check is
/// keyed by its blocks.
fn accept_subscriptions(
    subs1: SubscribeResults,
    subs2: SubscribeResults,
    results: &mut Vec<(MethodName, Result<(), TestError>)>,
) -> (NodeSubscriptions, NodeSubscriptions) {
    let (mut accepted1, mut accepted2) = (HashMap::new(), HashMap::new());
    for ((stream, sub1), (_, sub2)) in subs1.into_iter().zip(subs2) {
        let result = match (sub1, sub2) {
            (Ok(sub1), Ok(sub2)) => {
                accepted1.insert(stream, sub1);
                accepted2.insert(stream, sub2);
                Ok(())
            }
            (Err(e1), Err(e2)) if stream != Stream::NewHeads && errors_match(&e1, &e2) => {
                warn!(stream = stream.name(), error = ?e1, "both nodes reject the subscription, skipped");
                Ok(())
            }
            (Err(e1), Err(e2)) => Err(TestError::ErrDiff {
                rpc1: format!("{e1:?}"),
                rpc2: format!("{e2:?}"),
                args: None,
            }),
            (Err(err), Ok(_)) => Err(TestError::Rpc1Err(format!("rpc1: {err:?}"))),
            (Ok(sub1), Err(err)) => {
                warn!(stream = stream.name(), error = ?err, "rpc2 rejects the subscription, checking rpc1 alone");
                accepted1.insert(stream, sub1);
                Ok(())
            }
        };
        results.push((format!("eth_subscribe({})", stream.name()), result));
    }
    (accepted1, accepted2)
}

/// Raw messages of the deterministic streams of one node.
#[derive(Debug, Default)]
struct Collected {
    /// Whether the node serves `newHeads`; without it nothing is collected.
    has_heads: bool,
    heads: Vec<Value>,
    /// `None` when the node does not serve the stream.
    logs: Option<Vec<Value>>,
    receipts: Option<Vec<Value>>,
    /// Stream failures, such as a timeout, a closed connection, or dropped messages.
    errors: Vec<(Stream, String)>,
}

/// Collects heads until one at or above `target` arrives, along with the logs and receipts
/// emitted meanwhile, failing a stream when no head arrives within `head_timeout`.
async fn collect(
    subs: &mut NodeSubscriptions,
    target: BlockNumber,
    head_timeout: Duration,
) -> Collected {
    let mut collected = Collected::default();
    let Some(mut heads) = subs.remove(&Stream::NewHeads) else { return collected };
    collected.has_heads = true;
    let mut logs = subs.remove(&Stream::Logs);
    let mut receipts = subs.remove(&Stream::TransactionReceipts);
    collected.logs = logs.is_some().then(Vec::new);
    collected.receipts = receipts.is_some().then(Vec::new);

    let mut head_deadline = Instant::now() + head_timeout;
    loop {
        tokio::select! {
            head = heads.recv() => match head {
                Ok(head) => {
                    let Some(number) = head.get("number").and_then(parse_quantity) else {
                        collected.errors.push((Stream::NewHeads, format!("head without a number: {head}")));
                        break;
                    };
                    collected.heads.push(head);
                    if number >= target {
                        break;
                    }
                    head_deadline = Instant::now() + head_timeout;
                }
                Err(err) => {
                    collected.errors.push((Stream::NewHeads, recv_error(err)));
                    break;
                }
            },
            item = recv_opt(&mut logs) => push_item(&mut logs, &mut collected, Stream::Logs, item),
            item = recv_opt(&mut receipts) => {
                push_item(&mut receipts, &mut collected, Stream::TransactionReceipts, item)
            }
            _ = sleep_until(head_deadline) => {
                collected.errors.push((
                    Stream::NewHeads,
                    format!("no message within {head_timeout:?}"),
                ));
                break;
            }
        }
    }

    let drain_until = Instant::now() + DRAIN_GRACE;
    loop {
        tokio::select! {
            item = recv_opt(&mut logs) => push_item(&mut logs, &mut collected, Stream::Logs, item),
            item = recv_opt(&mut receipts) => {
                push_item(&mut receipts, &mut collected, Stream::TransactionReceipts, item)
            }
            _ = sleep_until(drain_until) => break,
        }
    }
    collected
}

/// Receives from a subscription, or never resolves without one.
async fn recv_opt(sub: &mut Option<Subscription<Value>>) -> Result<Value, RecvError> {
    match sub {
        Some(sub) => sub.recv().await,
        None => pending().await,
    }
}

/// Stores a log or receipt message, or records the stream failure and stops receiving from it.
fn push_item(
    sub: &mut Option<Subscription<Value>>,
    collected: &mut Collected,
    stream: Stream,
    item: Result<Value, RecvError>,
) {
    let items = if stream == Stream::Logs { &mut collected.logs } else { &mut collected.receipts };
    match item {
        Ok(item) => items.get_or_insert_with(Vec::new).push(item),
        Err(err) => {
            collected.errors.push((stream, recv_error(err)));
            *sub = None;
        }
    }
}

fn recv_error(err: RecvError) -> String {
    match err {
        RecvError::Closed => "subscription closed".to_string(),
        RecvError::Lagged(skipped) => format!("{skipped} messages dropped by the local buffer"),
    }
}

/// Checks that each node-local stream is alive and its first message decodes.
///
/// `syncing` only emits on status changes in some clients, so without a message the node's
/// `eth_syncing` response is checked as the initial state instead.
async fn liveness<P: Provider<AnyNetwork>>(
    rpc: &P,
    subs: NodeSubscriptions,
    wait: Duration,
) -> Vec<(Stream, Result<(), TestError>)> {
    let first_messages = subs
        .into_iter()
        .map(|(stream, mut sub)| async move { (stream, timeout(wait, sub.recv()).await) });

    let mut results = Vec::new();
    for (stream, message) in join_all(first_messages.collect::<Vec<_>>()).await {
        let result = match message {
            Ok(Ok(message)) => check_message(stream, &message)
                .map_err(|err| TestError::Rpc1Err(format!("undecodable message {message}: {err}"))),
            Ok(Err(err)) => Err(TestError::Rpc1Err(recv_error(err))),
            Err(_) if stream == Stream::Syncing => {
                match rpc.raw_request::<_, SyncStatus>("eth_syncing".into(), ()).await {
                    Ok(status) => {
                        info!(?status, "no syncing message, eth_syncing serves the initial state");
                        Ok(())
                    }
                    Err(err) => Err(TestError::Rpc1Err(format!(
                        "no message within {wait:?} and eth_syncing failed: {err:?}"
                    ))),
                }
            }
            Err(_) => Err(TestError::Rpc1Err(format!("no message within {wait:?}"))),
        };
        results.push((stream, result));
    }
    results.sort_by_key(|(stream, _)| Stream::ALL.iter().position(|s| s == stream));
    results
}

/// Decodes a message of a node-local stream.
fn check_message(stream: Stream, message: &Value) -> Result<(), String> {
    match stream {
        Stream::PendingTransactions => {
            serde_json::from_value::<B256>(message.clone()).map(drop).map_err(|err| err.to_string())
        }
        Stream::FullPendingTransactions => {
            serde_json::from_value::<AnyRpcTransaction>(message.clone())
                .map(drop)
                .map_err(|err| err.to_string())
        }
        Stream::Syncing if !is_sync_status(message) => {
            Err("expected a boolean or an object with a `syncing` boolean".to_string())
        }
        _ => Ok(()),
    }
}

/// Whether a `syncing` message is a boolean or an object carrying a `syncing` boolean, the two
/// shapes clients emit.
fn is_sync_status(message: &Value) -> bool {
    message.is_boolean() || message.get("syncing").is_some_and(Value::is_boolean)
}

/// The deterministic streams of one node, decoded and checked for ordering.
#[derive(Debug)]
struct NodeStreams {
    heads: Vec<(HeadRef, Value)>,
    logs: Option<Vec<(Positioned, Value)>>,
    receipts: Option<Vec<(Positioned, Value)>>,
}

impl NodeStreams {
    /// Decodes and checks the ordering of `collected`, recording one result per stream. Returns
    /// `None` without heads, since every block comparison is keyed by them.
    fn analyze(
        node: &str,
        collected: Collected,
        results: &mut Vec<(MethodName, Result<(), TestError>)>,
    ) -> Option<Self> {
        if !collected.has_heads {
            return None;
        }
        let mut errors: HashMap<Stream, String> = HashMap::new();
        for (stream, error) in collected.errors {
            errors.entry(stream).or_insert(error);
        }

        let mut heads = Vec::new();
        for head in collected.heads {
            match head_ref(&head) {
                Ok(head_ref) => heads.push((head_ref, head)),
                Err(err) => {
                    errors.entry(Stream::NewHeads).or_insert(err);
                }
            }
        }
        let order = check_heads(&heads.iter().map(|(head, _)| *head).collect::<Vec<_>>());
        results.push(order_result(
            node,
            Stream::NewHeads,
            errors.remove(&Stream::NewHeads),
            order,
            heads.len(),
        ));

        let logs = collected.logs.map(|logs| {
            let logs = decode_positioned(logs, log_position, &mut errors, Stream::Logs);
            let order = check_block_order(&logs.iter().map(|(pos, _)| *pos).collect::<Vec<_>>());
            results.push(order_result(
                node,
                Stream::Logs,
                errors.remove(&Stream::Logs),
                order,
                logs.len(),
            ));
            logs
        });

        let receipts = collected.receipts.map(|messages| {
            // A message carries the receipts of a block as an array.
            let receipts = messages
                .into_iter()
                .flat_map(|message| match message {
                    Value::Array(receipts) => receipts,
                    receipt => vec![receipt],
                })
                .collect();
            let receipts = decode_positioned(
                receipts,
                receipt_position,
                &mut errors,
                Stream::TransactionReceipts,
            );
            let order =
                check_block_order(&receipts.iter().map(|(pos, _)| *pos).collect::<Vec<_>>());
            results.push(order_result(
                node,
                Stream::TransactionReceipts,
                errors.remove(&Stream::TransactionReceipts),
                order,
                receipts.len(),
            ));
            receipts
        });

        Some(Self { heads, logs, receipts })
    }

    /// Fetches the node's canonical block `number` and, for the streams it serves, the block's
    /// logs and receipts over HTTP.
    async fn canonical<P: Provider<AnyNetwork>>(
        &self,
        rpc: &P,
        number: BlockNumber,
    ) -> Result<CanonicalBlock, TransportError> {
        let block: Value =
            rpc.raw_request("eth_getBlockByNumber".into(), (format!("{number:#x}"), false)).await?;
        let hash = block
            .get("hash")
            .and_then(|hash| serde_json::from_value::<B256>(hash.clone()).ok())
            .ok_or_else(|| {
                TransportError::local_usage_str(&format!(
                    "block {number} not found or without a hash"
                ))
            })?;
        let logs = match self.logs {
            Some(_) => {
                Some(rpc.raw_request("eth_getLogs".into(), [json!({ "blockHash": hash })]).await?)
            }
            None => None,
        };
        let receipts = match self.receipts {
            Some(_) => Some(rpc.raw_request("eth_getBlockReceipts".into(), [hash]).await?),
            None => None,
        };
        Ok(CanonicalBlock { hash, block, logs, receipts })
    }

    /// Compares the stream items of the canonical block with the node's HTTP responses, and
    /// returns them for the cross-node comparison. Returns `None` when the block was replaced by
    /// a reorg, which is skipped, or missing from the stream, which fails.
    fn compare_http(
        &self,
        node: &str,
        number: BlockNumber,
        http: &CanonicalBlock,
        results: &mut Vec<(MethodName, Result<(), TestError>)>,
    ) -> Option<BlockItems> {
        let head = self.heads.iter().find(|(head, _)| head.hash == http.hash);
        let Some((_, head)) = head else {
            if self.heads.iter().any(|(head, _)| head.number == number) {
                warn!(node, number, hash = %http.hash, "canonical block replaced by a reorg, skipped");
            } else {
                results.push((
                    format!("{node} newHeads"),
                    Err(node_err(
                        node,
                        format!("no head for canonical block {number} ({})", http.hash),
                    )),
                ));
            }
            return None;
        };

        let mut header = http.block.clone();
        if let Some(header) = header.as_object_mut() {
            for field in NON_HEADER_FIELDS {
                header.remove(*field);
            }
        }
        results.push(diff(
            format!("{node} newHeads vs eth_getBlockByNumber"),
            head.clone(),
            header,
        ));

        let logs = self.logs.as_ref().zip(http.logs.as_ref()).and_then(|(logs, expected)| {
            let logs = block_items(logs, http.hash)?;
            results.push(diff_items(format!("{node} logs vs eth_getLogs"), &logs, expected));
            Some(logs)
        });
        let receipts =
            self.receipts.as_ref().zip(http.receipts.as_ref()).and_then(|(receipts, expected)| {
                let receipts = block_items(receipts, http.hash)?;
                results.push(diff_items(
                    format!("{node} transactionReceipts vs eth_getBlockReceipts"),
                    &receipts,
                    expected,
                ));
                Some(receipts)
            });

        Some(BlockItems { hash: http.hash, head: head.clone(), logs, receipts })
    }
}

/// A node's canonical block and, for the streams it serves, its logs and receipts over HTTP.
#[derive(Debug)]
struct CanonicalBlock {
    hash: BlockHash,
    block: Value,
    logs: Option<Value>,
    receipts: Option<Value>,
}

/// One node's stream items for a canonical block.
#[derive(Debug)]
struct BlockItems {
    hash: BlockHash,
    head: Value,
    logs: Option<Value>,
    receipts: Option<Value>,
}

impl BlockItems {
    /// Compares rpc1's items (`self`) with rpc2's.
    fn compare(&self, rpc2: &Self, results: &mut Vec<(MethodName, Result<(), TestError>)>) {
        results.push(diff("newHeads".to_string(), self.head.clone(), rpc2.head.clone()));
        if let (Some(logs1), Some(logs2)) = (&self.logs, &rpc2.logs) {
            results.push(diff_items("logs".to_string(), logs1, logs2));
        }
        if let (Some(receipts1), Some(receipts2)) = (&self.receipts, &rpc2.receipts) {
            results.push(diff_items("transactionReceipts".to_string(), receipts1, receipts2));
        }
    }
}

/// The non-removed items emitted for `hash`, in arrival order, or `None` when a reorg removed
/// some of them, in which case the block is skipped.
fn block_items(items: &[(Positioned, Value)], hash: BlockHash) -> Option<Value> {
    let mut block = Vec::new();
    for (position, item) in items.iter().filter(|(position, _)| position.block_hash == hash) {
        if position.removed {
            warn!(%hash, "items removed by a reorg, skipped");
            return None;
        }
        block.push(item.clone());
    }
    Some(Value::Array(block))
}

/// Block identity of a `newHeads` message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HeadRef {
    number: BlockNumber,
    hash: BlockHash,
    parent_hash: BlockHash,
}

fn head_ref(head: &Value) -> Result<HeadRef, String> {
    let header: AnyRpcHeader = serde_json::from_value(head.clone())
        .map_err(|err| format!("undecodable head {head}: {err}"))?;
    Ok(HeadRef {
        number: header.inner.number,
        hash: header.hash,
        parent_hash: header.inner.parent_hash,
    })
}

/// Checks that heads arrive in block order, each extending its predecessor. A head that does not
/// extend its predecessor but stays at or below its successor height is a reorg, which is counted
/// and tolerated. Skipped heights and repeated heads fail. Returns the number of reorgs.
fn check_heads(heads: &[HeadRef]) -> Result<usize, String> {
    let mut reorgs = 0;
    for pair in heads.windows(2) {
        let (prev, head) = (pair[0], pair[1]);
        if head.hash == prev.hash {
            return Err(format!("head {} ({}) emitted twice in a row", head.number, head.hash));
        }
        if head.number > prev.number + 1 {
            return Err(format!(
                "head {} follows head {}, skipping {} blocks",
                head.number,
                prev.number,
                head.number - prev.number - 1
            ));
        }
        if head.number != prev.number + 1 || head.parent_hash != prev.hash {
            reorgs += 1;
        }
    }
    Ok(reorgs)
}

/// Position of a log or receipt within the chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Positioned {
    block_number: BlockNumber,
    block_hash: BlockHash,
    /// Log index for logs, transaction index for receipts.
    index: u64,
    /// Whether a reorg removed the log; receipts are never removed.
    removed: bool,
}

fn log_position(log: &Value) -> Result<Positioned, String> {
    let log: Log = serde_json::from_value(log.clone()).map_err(|err| err.to_string())?;
    match (log.block_number, log.block_hash, log.log_index) {
        (Some(block_number), Some(block_hash), Some(index)) => {
            Ok(Positioned { block_number, block_hash, index, removed: log.removed })
        }
        _ => Err("log without a block number, block hash, or log index".to_string()),
    }
}

fn receipt_position(receipt: &Value) -> Result<Positioned, String> {
    let receipt: AnyTransactionReceipt =
        serde_json::from_value(receipt.clone()).map_err(|err| err.to_string())?;
    match (receipt.block_number, receipt.block_hash, receipt.transaction_index) {
        (Some(block_number), Some(block_hash), Some(index)) => {
            Ok(Positioned { block_number, block_hash, index, removed: false })
        }
        _ => Err("receipt without a block number, block hash, or transaction index".to_string()),
    }
}

/// Decodes stream items, recording the first undecodable one as the stream's error.
fn decode_positioned(
    items: Vec<Value>,
    position: fn(&Value) -> Result<Positioned, String>,
    errors: &mut HashMap<Stream, String>,
    stream: Stream,
) -> Vec<(Positioned, Value)> {
    let mut decoded = Vec::with_capacity(items.len());
    for item in items {
        match position(&item) {
            Ok(pos) => decoded.push((pos, item)),
            Err(err) => {
                errors.entry(stream).or_insert_with(|| format!("undecodable item {item}: {err}"));
            }
        }
    }
    decoded
}

/// Checks that logs or receipts arrive grouped by block, in index order within a block. Moving to
/// a block at or below the previous one is a reorg, as is a removed log, and both are counted and
/// tolerated. A block whose items resume after another block's, unless a reorg removed them in
/// between, fails. Returns the number of reorgs.
fn check_block_order(items: &[Positioned]) -> Result<usize, String> {
    let mut reorgs = 0;
    let mut prev: Option<Positioned> = None;
    let mut finished = HashSet::new();
    let mut removing = false;
    for item in items {
        if item.removed {
            if !removing {
                reorgs += 1;
                removing = true;
            }
            // The removal ends the group in progress, and lets a removed block come back when
            // the chain reorgs back to it.
            if let Some(prev) = prev.take() {
                finished.insert(prev.block_hash);
            }
            finished.remove(&item.block_hash);
            continue;
        }
        removing = false;
        match prev {
            Some(prev) if prev.block_hash == item.block_hash => {
                if item.block_number != prev.block_number {
                    return Err(format!(
                        "block {} reported as both {} and {}",
                        item.block_hash, prev.block_number, item.block_number
                    ));
                }
                if item.index <= prev.index {
                    return Err(format!(
                        "index {} follows index {} in block {}",
                        item.index, prev.index, item.block_number
                    ));
                }
            }
            Some(prev) => {
                finished.insert(prev.block_hash);
                if finished.contains(&item.block_hash) {
                    return Err(format!(
                        "items of block {} ({}) resume after block {}",
                        item.block_number, item.block_hash, prev.block_number
                    ));
                }
                if item.block_number <= prev.block_number {
                    reorgs += 1;
                }
            }
            None => {}
        }
        prev = Some(*item);
    }
    Ok(reorgs)
}

fn order_result(
    node: &str,
    stream: Stream,
    error: Option<String>,
    order: Result<usize, String>,
    count: usize,
) -> (MethodName, Result<(), TestError>) {
    let name = format!("{node} {} order", stream.name());
    let result = match (error, order) {
        (Some(err), _) | (None, Err(err)) => Err(node_err(node, err)),
        (None, Ok(reorgs)) => {
            if reorgs > 0 {
                warn!(
                    node,
                    stream = stream.name(),
                    reorgs,
                    "reorgs observed, replaced blocks skipped"
                );
            }
            info!(node, stream = stream.name(), count, "collected");
            Ok(())
        }
    };
    (name, result)
}

/// Compares JSON values with the report's superset semantics: `actual` may carry extra fields.
fn diff(name: MethodName, actual: Value, expected: Value) -> (MethodName, Result<(), TestError>) {
    let result = if actual == expected {
        Ok(())
    } else {
        Err(TestError::Diff { rpc1: actual, rpc2: expected, args: None })
    };
    (name, result)
}

/// Like [`diff`] for item lists, which must also have the same length: the report tolerates
/// extra trailing array elements, but a stream emitting more or fewer items than the block holds
/// is a bug either way.
fn diff_items(
    name: MethodName,
    actual: &Value,
    expected: &Value,
) -> (MethodName, Result<(), TestError>) {
    let (actual_len, expected_len) =
        (actual.as_array().map_or(0, Vec::len), expected.as_array().map_or(0, Vec::len));
    if actual_len != expected_len {
        let err = format!("{actual_len} items, expected {expected_len}");
        return (name, Err(TestError::Rpc1Err(err)));
    }
    diff(name, actual.clone(), expected.clone())
}

fn node_err(node: &str, err: String) -> TestError {
    if node == "rpc1" {
        TestError::Rpc1Err(format!("rpc1: {err}"))
    } else {
        TestError::Rpc2Err(format!("rpc2: {err}"))
    }
}

fn parse_quantity(value: &Value) -> Option<u64> {
    u64::from_str_radix(value.as_str()?.strip_prefix("0x")?, 16).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(n: u8) -> B256 {
        B256::repeat_byte(n)
    }

    /// Head `number` whose hash is `id` and whose parent hash is `parent`.
    const fn head(number: u64, id: u8, parent: u8) -> HeadRef {
        HeadRef { number, hash: B256::repeat_byte(id), parent_hash: B256::repeat_byte(parent) }
    }

    fn item(block_number: u64, block: u8, index: u64) -> Positioned {
        Positioned { block_number, block_hash: hash(block), index, removed: false }
    }

    fn removed(block_number: u64, block: u8, index: u64) -> Positioned {
        Positioned { removed: true, ..item(block_number, block, index) }
    }

    #[test]
    fn heads_in_order_pass() {
        assert_eq!(check_heads(&[]), Ok(0));
        assert_eq!(check_heads(&[head(10, 1, 0)]), Ok(0));
        assert_eq!(check_heads(&[head(10, 1, 0), head(11, 2, 1), head(12, 3, 2)]), Ok(0));
    }

    #[test]
    fn head_reorgs_are_tolerated() {
        // A sibling replacing the tip.
        assert_eq!(check_heads(&[head(10, 1, 0), head(11, 2, 1), head(11, 3, 1)]), Ok(1));
        // The new chain re-emitted from below the tip.
        let deep = [head(10, 1, 0), head(11, 2, 1), head(12, 3, 2), head(11, 4, 1), head(12, 5, 4)];
        assert_eq!(check_heads(&deep), Ok(1));
        // The successor height on another fork, with a parent the stream never emitted.
        assert_eq!(check_heads(&[head(10, 1, 0), head(11, 2, 9)]), Ok(1));
    }

    #[test]
    fn head_gaps_and_repeats_fail() {
        let gap = check_heads(&[head(10, 1, 0), head(12, 3, 2)]).unwrap_err();
        assert!(gap.contains("skipping 1 blocks"), "{gap}");
        let repeat = check_heads(&[head(10, 1, 0), head(10, 1, 0)]).unwrap_err();
        assert!(repeat.contains("twice"), "{repeat}");
    }

    #[test]
    fn items_in_block_and_index_order_pass() {
        assert_eq!(check_block_order(&[]), Ok(0));
        // Blocks without items are simply absent.
        let items = [item(10, 1, 0), item(10, 1, 1), item(12, 3, 0), item(12, 3, 4)];
        assert_eq!(check_block_order(&items), Ok(0));
    }

    #[test]
    fn item_index_must_increase_within_a_block() {
        let repeated = check_block_order(&[item(10, 1, 0), item(10, 1, 0)]).unwrap_err();
        assert!(repeated.contains("index 0 follows index 0"), "{repeated}");
        let decreasing = check_block_order(&[item(10, 1, 2), item(10, 1, 1)]).unwrap_err();
        assert!(decreasing.contains("index 1 follows index 2"), "{decreasing}");
    }

    #[test]
    fn interleaved_blocks_fail() {
        let resumed =
            check_block_order(&[item(10, 1, 0), item(11, 2, 0), item(10, 1, 1)]).unwrap_err();
        assert!(resumed.contains("resume after block 11"), "{resumed}");
        let renumbered = check_block_order(&[item(10, 1, 0), item(11, 1, 1)]).unwrap_err();
        assert!(renumbered.contains("reported as both"), "{renumbered}");
    }

    #[test]
    fn item_reorgs_are_tolerated() {
        // Logs of the replaced block are removed, then the new chain's logs follow.
        let removal =
            [item(10, 1, 0), item(11, 2, 0), removed(11, 2, 0), item(11, 3, 0), item(12, 4, 0)];
        assert_eq!(check_block_order(&removal), Ok(1));
        // Receipts have no removals, so a reorg shows as a block at or below the previous one.
        let receipts = [item(10, 1, 0), item(11, 2, 0), item(11, 3, 0), item(12, 4, 0)];
        assert_eq!(check_block_order(&receipts), Ok(1));
        // A removed block may come back when the chain reorgs back to it.
        let back =
            [item(11, 2, 0), removed(11, 2, 0), item(11, 3, 0), removed(11, 3, 0), item(11, 2, 0)];
        assert_eq!(check_block_order(&back), Ok(2));
    }

    #[test]
    fn decodes_stream_items() {
        let block_hash = hash(1);
        let log = json!({
            "address": "0x0000000000000000000000000000000000000001",
            "topics": [],
            "data": "0x",
            "blockNumber": "0xa",
            "blockHash": block_hash,
            "transactionHash": hash(2),
            "transactionIndex": "0x0",
            "logIndex": "0x3",
            "removed": true,
        });
        assert_eq!(log_position(&log), Ok(removed(10, 1, 3)));

        let mut pending = log;
        pending["blockHash"] = Value::Null;
        assert!(log_position(&pending).is_err());

        let head = json!({
            "hash": hash(1),
            "parentHash": hash(0),
            "sha3Uncles": B256::ZERO,
            "miner": "0x0000000000000000000000000000000000000000",
            "stateRoot": B256::ZERO,
            "transactionsRoot": B256::ZERO,
            "receiptsRoot": B256::ZERO,
            "logsBloom": format!("0x{}", "00".repeat(256)),
            "difficulty": "0x0",
            "number": "0xa",
            "gasLimit": "0x1",
            "gasUsed": "0x0",
            "timestamp": "0x1",
            "extraData": "0x",
            "mixHash": B256::ZERO,
            "nonce": "0x0000000000000000",
        });
        assert_eq!(head_ref(&head), Ok(super::tests::head(10, 1, 0)));
        assert!(head_ref(&json!({ "number": "0xa" })).is_err());
    }

    #[test]
    fn checks_node_local_messages() {
        assert!(check_message(Stream::PendingTransactions, &json!(hash(1))).is_ok());
        assert!(check_message(Stream::PendingTransactions, &json!("0x1234")).is_err());
        assert!(check_message(Stream::FullPendingTransactions, &json!(hash(1))).is_err());
        assert!(check_message(Stream::Syncing, &json!(false)).is_ok());
        let detailed = json!({ "syncing": true, "status": { "currentBlock": "0x1" } });
        assert!(check_message(Stream::Syncing, &detailed).is_ok());
        assert!(check_message(Stream::Syncing, &json!({ "status": {} })).is_err());
    }

    #[test]
    fn block_items_skip_reorged_blocks() {
        let items = vec![
            (item(10, 1, 0), json!("a")),
            (item(11, 2, 0), json!("b")),
            (item(11, 2, 1), json!("c")),
            (removed(11, 2, 0), json!("b")),
        ];
        assert_eq!(block_items(&items, hash(1)), Some(json!(["a"])));
        assert_eq!(block_items(&items, hash(2)), None);
        assert_eq!(block_items(&items, hash(3)), Some(json!([])));
    }

    #[test]
    fn item_lists_must_match_in_length() {
        let (_, result) = diff_items("logs".to_string(), &json!([1, 2]), &json!([1]));
        assert!(matches!(result, Err(TestError::Rpc1Err(err)) if err == "2 items, expected 1"));
        let (_, result) = diff_items("logs".to_string(), &json!([1]), &json!([1]));
        assert!(result.is_ok());
        let (_, result) = diff_items("logs".to_string(), &json!([1]), &json!([2]));
        assert!(matches!(result, Err(TestError::Diff { .. })));
    }
}
