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

/// How long a log filter scenario gives the chain to advance between its two polls.
///
/// Slightly more than a mainnet slot, so that on a live chain the second poll usually covers a
/// new block. If the chain does not advance in time the second poll is made anyway and has to be
/// empty.
pub(crate) const NEW_BLOCK_WAIT: Duration = Duration::from_secs(15);

/// Number of polls of a pending transaction filter after the first one.
const PENDING_POLLS: usize = 5;

/// Spacing between the polls of a pending transaction filter.
const PENDING_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// What polling a log filter twice observed, see [`poll_log_filter`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct LogFilterPolls {
    /// The error a poll failed with. A `toBlock` ahead of the chain bounds the filter's range, not
    /// the poll, so it must not fail the poll.
    poll_error: Option<String>,
    /// Logs of blocks below the head at install time. A poll only reports what is new since the
    /// filter was installed; the history a filter covers is served by `eth_getFilterLogs`.
    logs_below_install_head: usize,
    /// Logs of the second poll that the first one had already delivered.
    repeated_logs: usize,
}

/// Installs `filter`, polls it, gives the chain [`NEW_BLOCK_WAIT`] to advance, polls it again and
/// uninstalls it.
pub(crate) async fn poll_log_filter<P: Provider<AnyNetwork>>(
    provider: &P,
    filter: &Filter,
) -> TransportResult<LogFilterPolls> {
    // Read the head before installing: a block landing in between can then only raise the head
    // the node installs the filter at, never lower it below this one.
    let install_head = provider.get_block_number().await?;
    let id: FilterId = provider.raw_request("eth_newFilter".into(), (filter,)).await?;
    let outcome = poll_log_filter_twice(provider, &id, install_head).await;
    uninstall(provider, &id).await;
    outcome
}

async fn poll_log_filter_twice<P: Provider<AnyNetwork>>(
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
        repeated_logs: second.iter().filter(|log| delivered.contains(&log_key(log))).count(),
    })
}

/// Installs `filter` and returns what `eth_getFilterLogs` serves for it.
pub(crate) async fn filter_logs<P: Provider<AnyNetwork>>(
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct PendingTransactionFilterPolls {
    /// The error a poll failed with.
    poll_error: Option<String>,
    /// Hashes a poll repeated from an earlier one.
    repeated_hashes: usize,
    /// Transactions only ever arrived in polls that followed a new block although the node did
    /// announce some, so the filter drains the pool on head progress instead of on every poll.
    gated_on_new_blocks: bool,
}

/// Installs a pending transaction filter, polls it every [`PENDING_POLL_INTERVAL`] and uninstalls
/// it.
pub(crate) async fn poll_pending_transaction_filter<P: Provider<AnyNetwork>>(
    provider: &P,
) -> TransportResult<PendingTransactionFilterPolls> {
    let id: FilterId = provider.raw_request("eth_newPendingTransactionFilter".into(), ()).await?;
    let outcome = poll_pending_transactions(provider, &id).await;
    uninstall(provider, &id).await;
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
                    gated_on_new_blocks: false,
                })
            }
        };
        repeated_hashes += hashes.iter().filter(|hash| !delivered.insert(**hash)).count();
        polls.push(PendingPoll { after_new_block: current != head, hashes: hashes.len() });
        head = current;
    }

    Ok(PendingTransactionFilterPolls {
        poll_error: None,
        repeated_hashes,
        gated_on_new_blocks: gated_on_new_blocks(&polls),
    })
}

/// One poll of a pending transaction filter.
#[derive(Debug, Clone, Copy)]
struct PendingPoll {
    /// Whether the head moved since the previous poll.
    after_new_block: bool,
    /// Number of hashes the poll delivered.
    hashes: usize,
}

/// Whether the polls after the first one only ever delivered transactions right after a new block.
///
/// The first poll drains what accumulated since the install and says nothing about gating. A
/// chain without transactions delivers nothing at all, which is not gating either.
fn gated_on_new_blocks(polls: &[PendingPoll]) -> bool {
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

    const fn poll(after_new_block: bool, hashes: usize) -> PendingPoll {
        PendingPoll { after_new_block, hashes }
    }

    #[test]
    fn gating_needs_deliveries_that_only_follow_new_blocks() {
        // a pool that is drained on every poll
        assert!(!gated_on_new_blocks(&[
            poll(false, 3),
            poll(false, 2),
            poll(true, 5),
            poll(false, 1)
        ]));
        // a filter that only reports once the head moved
        assert!(gated_on_new_blocks(&[
            poll(false, 3),
            poll(false, 0),
            poll(true, 5),
            poll(false, 0)
        ]));
        // nothing announced at all, and the first poll does not count
        assert!(!gated_on_new_blocks(&[poll(true, 3), poll(false, 0), poll(false, 0)]));
        assert!(!gated_on_new_blocks(&[poll(false, 3)]));
    }
}
