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
      --use-all-txes             Whether to query every transacion from a block or just the first
      --skip-extended-eth        Skip extended eth methods not supported by all clients (e.g., `eth_getRawTransactionByBlockNumberAndIndex`)
      --timeout <TIMEOUT>        Maximum time to wait for syncing in seconds [default: 300]
      --rate-limit <RATE_LIMIT>  Maximum requests per second (rate limit)
  -h, --help                     Print help
```

In addition to the `[head - num_blocks + 1, head]` tip range, every run samples log-spaced
historical blocks backwards from the tip (`head-128`, `head-1024`, `head-10000`, `head-100000`,
`head-1000000`, capped at genesis). These exercise cold history such as static files and pruned
tables that near-tip blocks do not.
