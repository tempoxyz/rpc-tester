//! Helper for `eth_getLogs` with automatic retry on "max results exceeded" errors.

use alloy_primitives::BlockNumber;
use alloy_provider::{network::AnyNetwork, Provider};
use alloy_rpc_types::{BlockNumberOrTag, Filter, Log};
use futures::FutureExt;

/// The result type returned by `get_logs`.
pub type GetLogsResult<T> =
    Result<T, alloy_json_rpc::RpcError<alloy_transport::TransportErrorKind>>;

/// Maximum recursion depth to prevent infinite loops.
const MAX_RECURSION_DEPTH: u32 = 10;

/// Fetches logs with automatic pagination when the RPC returns a "max results exceeded" error.
///
/// Some RPC providers limit the number of logs returned in a single request. When exceeded,
/// they return an error like:
/// `"query exceeds max results 20000, retry with the range 24383075-24383096"`
///
/// This function parses such errors and paginates through the full block range using the
/// suggested chunk size, collecting all results.
pub async fn get_logs_with_retry<P: Provider<AnyNetwork>>(
    provider: &P,
    filter: &Filter,
) -> GetLogsResult<Vec<Log>> {
    get_logs_paginated(provider, filter.clone(), 0).await
}

/// Recursively fetches logs, splitting the range when "max results exceeded" is returned.
fn get_logs_paginated<'a, P: Provider<AnyNetwork>>(
    provider: &'a P,
    filter: Filter,
    depth: u32,
) -> futures::future::BoxFuture<'a, GetLogsResult<Vec<Log>>> {
    async move {
        if depth > MAX_RECURSION_DEPTH {
            return provider.get_logs(&filter).await;
        }

        match provider.get_logs(&filter).await {
            Ok(logs) => Ok(logs),
            Err(e) => {
                let Some((suggested_from, suggested_to)) = parse_max_results_error(&e) else {
                    return Err(e);
                };

                let Some(chunk_size) =
                    suggested_to.checked_sub(suggested_from).and_then(|d| d.checked_add(1))
                else {
                    return Err(e);
                };

                let (original_from, original_to) =
                    extract_block_range(&filter).unwrap_or((suggested_from, suggested_to));

                if original_from > original_to {
                    return Err(e);
                }

                let original_len = original_to - original_from + 1;
                if chunk_size >= original_len && depth > 0 {
                    return Err(e);
                }

                let mut all_logs = Vec::new();
                let mut current_from = original_from;

                while current_from <= original_to {
                    let current_to = current_from.saturating_add(chunk_size - 1).min(original_to);
                    let chunk_filter = filter.clone().from_block(current_from).to_block(current_to);
                    let chunk_logs = get_logs_paginated(provider, chunk_filter, depth + 1).await?;
                    all_logs.extend(chunk_logs);
                    current_from = match current_to.checked_add(1) {
                        Some(v) => v,
                        None => break,
                    };
                }

                Ok(all_logs)
            }
        }
    }
    .boxed()
}

/// Extracts the from/to block numbers from a filter's block option.
fn extract_block_range(filter: &Filter) -> Option<(BlockNumber, BlockNumber)> {
    let from = filter.block_option.get_from_block().and_then(|b| match b {
        BlockNumberOrTag::Number(n) => Some(*n),
        _ => None,
    })?;
    let to = filter.block_option.get_to_block().and_then(|b| match b {
        BlockNumberOrTag::Number(n) => Some(*n),
        _ => None,
    })?;
    Some((from, to))
}

/// Parses an error to extract the suggested block range from "max results exceeded" errors.
///
/// Expected format: "query exceeds max results N, retry with the range FROM-TO"
fn parse_max_results_error<E: std::fmt::Display>(error: &E) -> Option<(BlockNumber, BlockNumber)> {
    let msg = error.to_string();

    if !msg.contains("max results") {
        return None;
    }

    // Look for pattern like "range 24383075-24383096"
    let range_prefix = "range ";
    let range_start = msg.find(range_prefix)?;
    let range_part = &msg[range_start + range_prefix.len()..];

    // Parse "FROM-TO" (stop at first non-numeric, non-dash char)
    let range_end =
        range_part.find(|c: char| !c.is_ascii_digit() && c != '-').unwrap_or(range_part.len());
    let range_str = &range_part[..range_end];

    let mut parts = range_str.split('-');
    let from: BlockNumber = parts.next()?.parse().ok()?;
    let to: BlockNumber = parts.next()?.parse().ok()?;

    Some((from, to))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_max_results_error_message() {
        let error_msg = "query exceeds max results 20000, retry with the range 24383075-24383096";
        let result = parse_max_results_error(&error_msg);
        assert_eq!(result, Some((24383075, 24383096)));
    }

    #[test]
    fn test_parse_non_matching_error() {
        let error_msg = "some other error";
        let result = parse_max_results_error(&error_msg);
        assert_eq!(result, None);
    }

    #[test]
    fn test_parse_with_trailing_text() {
        let error_msg = "query exceeds max results 20000, retry with the range 100-200, extra info";
        let result = parse_max_results_error(&error_msg);
        assert_eq!(result, Some((100, 200)));
    }

    #[test]
    fn test_extract_block_range() {
        let filter = Filter::new().from_block(100u64).to_block(200u64);
        assert_eq!(extract_block_range(&filter), Some((100, 200)));
    }

    #[test]
    fn test_extract_block_range_no_range() {
        let filter = Filter::new();
        assert_eq!(extract_block_range(&filter), None);
    }
}
