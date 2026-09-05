//! Scenarios for the poll-based filter API.
//!
//! Filter ids are node-local state, so the nodes cannot be sent the same request the way the
//! stateless methods are. Each scenario installs its own filter on a node, polls it and reduces
//! what it observed to an outcome that is the same on every correctly behaving node no matter when
//! blocks and transactions arrive, so that the outcomes of both nodes can be compared.

use alloy_json_rpc::RpcRecv;
use alloy_primitives::{BlockNumber, B256};
use alloy_provider::{network::AnyNetwork, Provider};
use alloy_rpc_types::{Filter, FilterId, Log};
use alloy_transport::TransportResult;
use serde::Serialize;
use std::{
    collections::HashSet,
    time::{Duration, Instant},
};
use tracing::warn;

/// How long a log filter scenario gives the chain to advance between its two polls.
///
/// Slightly more than a mainnet slot, so that on a live chain the second poll usually covers a
/// new block. If the chain does not advance in time the second poll is made anyway and has to be
/// empty.
pub const NEW_BLOCK_WAIT: Duration = Duration::from_secs(15);

/// How far ahead of the head [`poll_future_to_block_filter`] places the filter's `toBlock`.
///
/// Far enough that no chain reaches it while the scenario runs.
pub const FUTURE_TO_BLOCK_OFFSET: u64 = 1_000_000;

/// Number of polls of a pending transaction filter after the first one.
const PENDING_POLLS: usize = 5;

/// Spacing between the polls of a pending transaction filter.
const PENDING_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// What polling a log filter twice observed, see [`poll_log_filter`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LogFilterPolls {
    /// The error a poll failed with. A `toBlock` ahead of the chain bounds the filter's range, not
    /// the poll, so it must not fail the poll.
    pub poll_error: Option<String>,
    /// Logs of blocks below the head at install time. A poll only reports what is new since the
    /// filter was installed; the history a filter covers is served by `eth_getFilterLogs`.
    pub logs_below_install_head: usize,
    /// Logs of the second poll that the first one had already delivered. A log a reorg removed
    /// is announced again with `removed` set, which is not a repeat.
    pub repeated_logs: usize,
}

/// Installs `filter`, polls it, gives the chain [`NEW_BLOCK_WAIT`] to advance, polls it again and
/// uninstalls it.
pub async fn poll_log_filter<P: Provider<AnyNetwork>>(
    provider: &P,
    filter: &Filter,
) -> TransportResult<LogFilterPolls> {
    // Read the head before installing: a block landing in between can then only raise the head
    // the node installs the filter at, never lower it below this one.
    let install_head = provider.get_block_number().await?;
    install_and_poll_twice(provider, filter, install_head).await
}

/// Like [`poll_log_filter`], with the filter's `toBlock` moved [`FUTURE_TO_BLOCK_OFFSET`] blocks
/// past the head the node reports.
///
/// The bound is derived from the node's head rather than from the range under test so that it is
/// ahead of the chain no matter how far back that range lies.
pub async fn poll_future_to_block_filter<P: Provider<AnyNetwork>>(
    provider: &P,
    filter: &Filter,
) -> TransportResult<LogFilterPolls> {
    let install_head = provider.get_block_number().await?;
    install_and_poll_twice(provider, &with_future_to_block(filter, install_head), install_head)
        .await
}

/// Returns `filter` with its `toBlock` set [`FUTURE_TO_BLOCK_OFFSET`] blocks past `head`.
pub fn with_future_to_block(filter: &Filter, head: BlockNumber) -> Filter {
    filter.clone().to_block(head.saturating_add(FUTURE_TO_BLOCK_OFFSET))
}

async fn install_and_poll_twice<P: Provider<AnyNetwork>>(
    provider: &P,
    filter: &Filter,
    install_head: BlockNumber,
) -> TransportResult<LogFilterPolls> {
    let id: FilterId = provider.raw_request("eth_newFilter".into(), (filter,)).await?;
    let outcome = poll_twice(provider, &id, install_head).await;
    uninstall(provider, &id).await;
    outcome
}

async fn poll_twice<P: Provider<AnyNetwork>>(
    provider: &P,
    id: &FilterId,
    install_head: BlockNumber,
) -> TransportResult<LogFilterPolls> {
    let failed = |error| LogFilterPolls {
        poll_error: Some(error),
        logs_below_install_head: 0,
        repeated_logs: 0,
    };

    let first: Vec<Log> = match changes(provider, id).await? {
        Ok(logs) => logs,
        Err(error) => return Ok(failed(error)),
    };
    let head = provider.get_block_number().await?;
    let delivered: HashSet<_> = first.iter().map(log_key).collect();

    wait_for_new_block(provider, head).await?;
    let second: Vec<Log> = match changes(provider, id).await? {
        Ok(logs) => logs,
        Err(error) => return Ok(failed(error)),
    };

    Ok(LogFilterPolls {
        poll_error: None,
        logs_below_install_head: first
            .iter()
            .chain(&second)
            .filter(|log| log.block_number.is_some_and(|number| number < install_head))
            .count(),
        repeated_logs: second
            .iter()
            .filter(|log| !log.removed && delivered.contains(&log_key(log)))
            .count(),
    })
}

/// Installs `filter` and returns what `eth_getFilterLogs` serves for it.
pub async fn filter_logs<P: Provider<AnyNetwork>>(
    provider: &P,
    filter: &Filter,
) -> TransportResult<Vec<Log>> {
    let id: FilterId = provider.raw_request("eth_newFilter".into(), (filter,)).await?;
    let logs = provider.raw_request("eth_getFilterLogs".into(), (&id,)).await;
    uninstall(provider, &id).await;
    logs
}

/// What polling a pending transaction filter observed, see
/// [`poll_pending_transaction_filter`].
///
/// Only the error and the repeats take part in the comparison. When the hashes arrived relative
/// to new blocks depends on when the pool saw the transactions, which two correct nodes can
/// legitimately see on different sides of a block boundary, so the polls themselves are kept for
/// diagnostics only.
#[derive(Debug, Clone, Serialize)]
pub struct PendingTransactionFilterPolls {
    /// The error a poll failed with.
    pub poll_error: Option<String>,
    /// Hashes a poll repeated from an earlier one.
    pub repeated_hashes: usize,
    /// The polls as observed, see [`suggests_head_gating`].
    #[serde(skip)]
    pub polls: Vec<PendingPoll>,
}

impl PartialEq for PendingTransactionFilterPolls {
    fn eq(&self, other: &Self) -> bool {
        self.poll_error == other.poll_error && self.repeated_hashes == other.repeated_hashes
    }
}

impl Eq for PendingTransactionFilterPolls {}

/// One poll of a pending transaction filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingPoll {
    /// Whether the head moved since the previous poll.
    pub after_new_block: bool,
    /// Number of hashes the poll delivered.
    pub hashes: usize,
}

/// Installs a pending transaction filter, polls it every [`PENDING_POLL_INTERVAL`] and uninstalls
/// it. `node` names the node in the diagnostic for polls that look gated on head progress.
pub async fn poll_pending_transaction_filter<P: Provider<AnyNetwork>>(
    provider: &P,
    node: &str,
) -> TransportResult<PendingTransactionFilterPolls> {
    let id: FilterId = provider.raw_request("eth_newPendingTransactionFilter".into(), ()).await?;
    let outcome = poll_pending_transactions(provider, &id).await;
    uninstall(provider, &id).await;

    if let Ok(outcome) = &outcome {
        if suggests_head_gating(&outcome.polls) {
            warn!(
                node,
                polls = ?outcome.polls,
                "pending transactions only arrived in polls that followed a new block, which is \
                 what a filter gated on head progress looks like; a live node cannot prove this \
                 either way, so it is not compared"
            );
        }
    }
    outcome
}

async fn poll_pending_transactions<P: Provider<AnyNetwork>>(
    provider: &P,
    id: &FilterId,
) -> TransportResult<PendingTransactionFilterPolls> {
    let mut delivered = HashSet::new();
    let mut repeated_hashes = 0;
    let mut polls = Vec::with_capacity(PENDING_POLLS + 1);
    let mut head = provider.get_block_number().await?;

    for poll in 0..=PENDING_POLLS {
        if poll > 0 {
            tokio::time::sleep(PENDING_POLL_INTERVAL).await;
        }
        let current = provider.get_block_number().await?;
        let hashes: Vec<B256> = match changes(provider, id).await? {
            Ok(hashes) => hashes,
            Err(error) => {
                return Ok(PendingTransactionFilterPolls {
                    poll_error: Some(error),
                    repeated_hashes,
                    polls,
                })
            }
        };
        repeated_hashes += hashes.iter().filter(|hash| !delivered.insert(**hash)).count();
        polls.push(PendingPoll { after_new_block: current != head, hashes: hashes.len() });
        head = current;
    }

    Ok(PendingTransactionFilterPolls { poll_error: None, repeated_hashes, polls })
}

/// Whether the polls after the first one only ever delivered transactions right after a new
/// block, which is how a filter gated on head progress behaves.
///
/// This is a hint, not proof: a correct node whose only transactions happened to arrive in the
/// poll intervals that also contained a new block looks the same. The first poll drains what
/// accumulated since the install and says nothing about gating, and a chain without transactions
/// delivers nothing at all, which is not gating either.
pub fn suggests_head_gating(polls: &[PendingPoll]) -> bool {
    let later = polls.get(1..).unwrap_or_default();
    later.iter().any(|poll| poll.hashes > 0) &&
        later.iter().all(|poll| poll.hashes == 0 || poll.after_new_block)
}

/// Polls the filter, keeping an error response from the node as an observation while a transport
/// failure is propagated.
async fn changes<P: Provider<AnyNetwork>, R: RpcRecv>(
    provider: &P,
    id: &FilterId,
) -> TransportResult<Result<Vec<R>, String>> {
    match provider.raw_request("eth_getFilterChanges".into(), (id,)).await {
        Ok(changes) => Ok(Ok(changes)),
        Err(error) => match error.as_error_resp() {
            Some(response) => Ok(Err(response.message.to_string())),
            None => Err(error),
        },
    }
}

/// Uninstalls the filter. Best effort: a filter that stays installed expires on its own.
async fn uninstall<P: Provider<AnyNetwork>>(provider: &P, id: &FilterId) {
    let _: TransportResult<bool> = provider.raw_request("eth_uninstallFilter".into(), (id,)).await;
}

/// Waits up to [`NEW_BLOCK_WAIT`] for the chain to advance past `head`.
async fn wait_for_new_block<P: Provider<AnyNetwork>>(
    provider: &P,
    head: BlockNumber,
) -> TransportResult<()> {
    let deadline = Instant::now() + NEW_BLOCK_WAIT;
    while Instant::now() < deadline {
        tokio::time::sleep(Duration::from_secs(1)).await;
        if provider.get_block_number().await? > head {
            break;
        }
    }
    Ok(())
}

/// Identifies a log across polls.
const fn log_key(log: &Log) -> (Option<B256>, Option<B256>, Option<u64>) {
    (log.block_hash, log.transaction_hash, log.log_index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_rpc_types::BlockNumberOrTag;

    const fn poll(after_new_block: bool, hashes: usize) -> PendingPoll {
        PendingPoll { after_new_block, hashes }
    }

    fn outcome(polls: Vec<PendingPoll>) -> PendingTransactionFilterPolls {
        PendingTransactionFilterPolls { poll_error: None, repeated_hashes: 0, polls }
    }

    #[test]
    fn future_to_block_is_ahead_of_the_head_for_any_range() {
        let head = 20_000_000;
        // a range far more than the offset behind the head
        let filter = Filter::new().from_block(1_000u64).to_block(2_000u64);
        let future = with_future_to_block(&filter, head);
        assert_eq!(future.get_from_block(), Some(1_000));
        assert!(matches!(
            future.block_option.get_to_block(),
            Some(BlockNumberOrTag::Number(to)) if *to > head
        ));
    }

    #[test]
    fn head_gating_hint_needs_deliveries_that_only_follow_new_blocks() {
        // a pool that is drained on every poll
        assert!(!suggests_head_gating(&[
            poll(false, 3),
            poll(false, 2),
            poll(true, 5),
            poll(false, 1)
        ]));
        // a filter that only reports once the head moved
        assert!(suggests_head_gating(&[
            poll(false, 3),
            poll(false, 0),
            poll(true, 5),
            poll(false, 0)
        ]));
        // nothing announced at all, and the first poll does not count
        assert!(!suggests_head_gating(&[poll(true, 3), poll(false, 0), poll(false, 0)]));
        assert!(!suggests_head_gating(&[poll(false, 3)]));
    }

    #[test]
    fn transaction_arrival_timing_does_not_split_correct_nodes() {
        // the same single transaction seen on either side of a block boundary, both valid for a
        // filter that drains on every poll, so the hint differs but the outcomes compare equal
        let before_block = vec![poll(false, 0), poll(true, 1), poll(false, 0)];
        let after_block = vec![poll(false, 0), poll(false, 1), poll(true, 0)];
        assert!(suggests_head_gating(&before_block));
        assert!(!suggests_head_gating(&after_block));
        assert_eq!(outcome(before_block), outcome(after_block));
    }
}
