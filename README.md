# rpc-tester

```yaml
Verifies that results from `rpc1` are at the very least a superset of `rpc2`

Usage: rpc-tester-cli [OPTIONS] --rpc1 <RPC_URL1> --rpc2 <RPC_URL2>

Options:
      --rpc1 <RPC_URL1>          RPC URL 1
      --rpc2 <RPC_URL2>          RPC URL 2
      --num-blocks <NUM_BLOCKS>  Number of blocks to test from the tip [default: 8]
      --use-reth                 Whether to query reth namespace
      --use-tracing              Whether to query tracing methods
      --use-all-txes             Whether to query every transaction from a block. Otherwise, the first transaction of each distinct type is sampled
      --skip-extended-eth        Skip extended eth methods not supported by all clients (e.g., `eth_getRawTransactionByBlockNumberAndIndex`)
      --use-finality-tags        Whether to compare the moving `safe` and `finalized` tags
      --use-execution-witness    Whether to compare `debug_executionWitness` for the newest tested block
      --use-ots                  Whether to query Otterscan's `ots` namespace
      --use-tempo                Whether to query Tempo's `tempo` namespace
      --minimal                  Whether either node runs with `--minimal` pruning
      --ws1 <WS_URL1>            WS URL of rpc1's node. Enables the `eth_subscribe` checks, which need `--ws2` too
      --ws2 <WS_URL2>            WS URL of rpc2's node
      --ws-timeout <WS_TIMEOUT>  Maximum time to wait for the next subscription message in seconds [default: 60]
      --timeout <TIMEOUT>        Maximum time to wait for syncing in seconds [default: 300]
      --rate-limit <RATE_LIMIT>  Maximum requests per second (rate limit). If not provided, no rate limiting is applied
  -h, --help                     Print help
```

In addition to the `[head - num_blocks + 1, head]` tip range, every run randomly samples
historical blocks: `num_blocks` picks from the near-history window `[head-128, head-8]`, which
crosses the boundary where lazily persisting clients move blocks from memory to storage, plus one
pick per log-spaced deep-history stratum (offsets up to 1024, 10000, 100000 and 1000000, capped
at genesis). Deep samples exercise cold history such as static files and pruned tables that
near-tip blocks do not, and randomness means repeated runs accumulate coverage instead of
re-testing the same blocks. The sampled set is logged at startup.

Note that the deep samples query state and replay transactions at historical blocks, which
assumes both nodes serve full historical state (archive). Also, `--num-blocks` values of 128 or
more make the tip range swallow the near-history window, dropping those samples.

With `--ws1` and `--ws2`, `eth_subscribe` streams are checked over WebSocket after the HTTP
suites. `newHeads`, `logs` and `transactionReceipts` are collected for the next `--num-blocks`
blocks on both nodes: heads must extend each other in block order, logs and receipts must arrive
grouped by block in index order, and each block's items must match the node's own
`eth_getBlockByNumber`, `eth_getLogs` and `eth_getBlockReceipts` responses as well as the other
node's stream. Blocks replaced by a reorg are skipped. Pending transaction and `syncing` streams
are node-local, so they are only checked for a decodable first message within `--ws-timeout`
(`syncing` falls back to `eth_syncing`, since clients may only emit on status changes). A stream
both nodes reject with the same error is skipped.
