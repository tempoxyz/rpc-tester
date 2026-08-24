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
      --timeout <TIMEOUT>        Maximum time to wait for syncing in seconds [default: 300]
      --rate-limit <RATE_LIMIT>  Maximum requests per second (rate limit)
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
