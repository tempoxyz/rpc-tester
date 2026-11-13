//! CLI definition and entrypoint to executable

use alloy_provider::{network::AnyNetwork, Provider, ProviderBuilder};
use alloy_rpc_types::SyncStatus;
use clap::Parser;
use rpc_tester::{
    earliest_available_block, earliest_available_state, historical_blocks, RpcTester,
    SubscriptionTester,
};
use std::{
    ops::RangeInclusive,
    time::{Duration, Instant},
};
use tracing::info;
use url::Url;

/// The rpc-tester-cli interface.
#[derive(Debug, Parser)]
#[command(about = "Verifies that results from `rpc1` are at the very least a superset of `rpc2`")]
pub struct CliArgs {
    /// RPC URL 1
    #[arg(long, value_name = "RPC_URL1", value_parser = Url::parse)]
    pub rpc1: Url,

    /// RPC URL 2
    #[arg(long, value_name = "RPC_URL2")]
    pub rpc2: Url,

    /// Number of blocks to test from the tip.
    #[arg(long, value_name = "NUM_BLOCKS", default_value = "8")]
    pub num_blocks: u64,

    /// Whether to query reth namespace
    #[arg(long, value_name = "RETH", default_value = "false")]
    pub use_reth: bool,

    /// Whether to query tracing methods
    #[arg(long, value_name = "TRACING", default_value = "false")]
    pub use_tracing: bool,

    /// Whether to query every transaction from a block. Otherwise, the first transaction of each
    /// distinct type is sampled.
    #[arg(long, value_name = "ALL_TXES", default_value = "false")]
    pub use_all_txes: bool,

    /// Skip extended eth methods not supported by all clients (e.g.,
    /// `eth_getRawTransactionByBlockNumberAndIndex`).
    #[arg(long)]
    pub skip_extended_eth: bool,

    /// Whether to compare the moving `safe` and `finalized` tags.
    ///
    /// Requires both nodes to share a consensus view, otherwise the tags can legitimately differ.
    #[arg(long)]
    pub use_finality_tags: bool,

    /// Whether to compare `debug_executionWitness` for the newest tested block.
    ///
    /// Witness generation re-executes the block and is expensive, so this is opt-in.
    #[arg(long)]
    pub use_execution_witness: bool,

    /// Whether to query Otterscan's `ots` namespace.
    #[arg(long)]
    pub use_ots: bool,
    /// Whether to query Tempo's `tempo` namespace.
    #[arg(long)]
    pub use_tempo: bool,

    /// Whether either node runs with `--minimal` pruning.
    ///
    /// Skips lookups by transaction hash and anything read from receipts, which such a node
    /// prunes, and keeps historical samples within the state both nodes still serve.
    #[arg(long)]
    pub minimal: bool,

    /// WS URL of rpc1's node. Enables the `eth_subscribe` checks, which need `--ws2` too.
    ///
    /// Heads, logs and receipts are collected for `--num-blocks` new blocks on both nodes and
    /// compared with each other and with each node's HTTP responses; pending transaction and
    /// syncing streams are only checked for liveness.
    #[arg(long, value_name = "WS_URL1", requires = "ws2")]
    pub ws1: Option<Url>,

    /// WS URL of rpc2's node.
    #[arg(long, value_name = "WS_URL2", requires = "ws1")]
    pub ws2: Option<Url>,

    /// Maximum time to wait for the next subscription message in seconds.
    #[arg(long, value_name = "WS_TIMEOUT", default_value = "60")]
    pub ws_timeout: u64,

    /// Maximum time to wait for syncing in seconds
    #[arg(long, value_name = "TIMEOUT", default_value = "300")]
    pub timeout: u64,

    /// Maximum requests per second (rate limit).
    /// If not provided, no rate limiting is applied.
    #[arg(long, value_name = "RATE_LIMIT")]
    pub rate_limit: Option<u32>,
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    tracing_subscriber::fmt::init();

    let args = CliArgs::parse();

    let rpc1 = ProviderBuilder::new()
        .disable_recommended_fillers()
        .network::<AnyNetwork>()
        .connect_http(args.rpc1.clone());
    let rpc2 = ProviderBuilder::new()
        .disable_recommended_fillers()
        .network::<AnyNetwork>()
        .connect_http(args.rpc2.clone());

    let block_range =
        wait_for_readiness(&rpc1, &rpc2, args.num_blocks, Duration::from_secs(args.timeout))
            .await?;

    // A pruned node cannot serve history below its earliest block, so blocks either node pruned
    // are left out instead of failing as diffs.
    let head = *block_range.end();
    let mut history_floor = earliest_available_block(&rpc1, head, args.rate_limit)
        .await?
        .max(earliest_available_block(&rpc2, head, args.rate_limit).await?);
    if args.minimal {
        let state_floor = earliest_available_state(&rpc1, head, args.rate_limit)
            .await?
            .max(earliest_available_state(&rpc2, head, args.rate_limit).await?);
        let floor = history_floor.max(state_floor);
        // Pruning keeps raising the floor while the run is in flight, so a quarter of the
        // retained window is left as headroom.
        if floor > 0 {
            history_floor = floor + (head - floor) / 4;
        }
    }
    if history_floor > 0 {
        info!(history_floor, "history below this block is pruned on at least one node");
    }
    let block_range = (*block_range.start()).max(history_floor)..=head;

    let tester = RpcTester::builder(rpc1.clone(), rpc2.clone())
        .with_tracing(args.use_tracing)
        .with_reth(args.use_reth)
        .with_all_txes(args.use_all_txes)
        .skip_extended_eth(args.skip_extended_eth)
        .with_finality_tags(args.use_finality_tags)
        .with_execution_witness(args.use_execution_witness)
        .with_ots(args.use_ots)
        .with_tempo(args.use_tempo)
        .with_history_floor(history_floor)
        .with_minimal(args.minimal)
        .with_rate_limit(args.rate_limit)
        .build();

    // Random historical samples beyond the tip range; see `historical_blocks` for the scheme.
    let mut historical = historical_blocks(*block_range.end(), args.num_blocks as usize);
    historical.retain(|block| *block >= history_floor && !block_range.contains(block));
    info!(blocks = ?historical, "sampled historical blocks");

    // Run both suites to completion before failing so a diff in one does not hide the other.
    let range_result = tester.run(block_range).await;
    let historical_result =
        if historical.is_empty() { Ok(()) } else { tester.run_blocks(historical).await };
    let subscription_result = match (args.ws1, args.ws2) {
        (Some(ws1), Some(ws2)) => {
            SubscriptionTester::connect(
                rpc1,
                rpc2,
                ws1.as_str(),
                ws2.as_str(),
                args.num_blocks,
                Duration::from_secs(args.ws_timeout),
            )
            .await?
            .run()
            .await
        }
        _ => Ok(()),
    };
    range_result.and(historical_result).and(subscription_result)
}

/// Waits until rpc1 is synced to the tip and returns a valid block range to test against rpc2.
pub async fn wait_for_readiness<P: Provider<AnyNetwork>>(
    rpc1: &P,
    rpc2: &P,
    block_size_range: u64,
    timeout: Duration,
) -> eyre::Result<RangeInclusive<u64>> {
    let start_time = Instant::now();

    // Waits until it's done syncing
    while let SyncStatus::Info(sync_info) = rpc1.syncing().await? {
        if start_time.elapsed() > timeout {
            return Err(eyre::eyre!("Timeout waiting for rpc1 to sync after {timeout:?}"));
        }
        info!(?sync_info, "rpc1 still syncing");
        tokio::time::sleep(Duration::from_secs(5)).await;
    }

    // Waits until rpc1 has _mostly_ catch up to rpc2 or beyond
    loop {
        let tip1 = rpc1.get_block_number().await?;
        let tip2 = rpc2.get_block_number().await?;

        if tip1 >= tip2 || tip2 - tip1 <= 5 {
            let common = tip1.min(tip2);
            // Saturate so young chains with fewer blocks than the requested range still work.
            let range = common.saturating_sub(block_size_range.saturating_sub(1))..=common;
            info!(?range, "testing block range");
            return Ok(range);
        }
        info!(?tip1, ?tip2, "rpc1 is behind rpc2, waiting for it to catch up");
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}
