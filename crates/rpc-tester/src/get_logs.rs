//! Helper for `eth_getLogs` with automatic retry on "max results exceeded" errors.

use alloy_primitives::BlockNumber;
use alloy_provider::{network::AnyNetwork, Provider};
use alloy_rpc_types::{Filter, Log};

/// The result type returned by `get_logs`.
pub type GetLogsResult<T> =
    Result<T, alloy_json_rpc::RpcError<alloy_transport::TransportErrorKind>>;

/// Fetches logs with automatic retry when the RPC returns a "max results exceeded" error.
///
/// Some RPC providers limit the number of logs returned in a single request. When exceeded,
/// they return an error like:
/// `"query exceeds max results 20000, retry with the range 24383075-24383096"`
///
/// This function parses such errors and retries with the suggested narrower block range.
pub async fn get_logs_with_retry<P: Provider<AnyNetwork>>(
    provider: &P,
    filter: &Filter,
) -> GetLogsResult<Vec<Log>> {
    match provider.get_logs(filter).await {
        Ok(logs) => Ok(logs),
        Err(e) => {
            if let Some((from, to)) = parse_max_results_error(&e) {
                let narrowed_filter = filter.clone().from_block(from).to_block(to);
                provider.get_logs(&narrowed_filter).await
            } else {
                Err(e)
            }
        }
    }
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
}
