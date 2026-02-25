# rpc-tester

```yaml
Verifies that results from `rpc1` are at the very least a superset of `rpc2`

Usage: rpc-tester-cli [OPTIONS] --rpc1 <RPC_URL1> --rpc2 <RPC_URL2>

Options:
      --rpc1 <RPC_URL1>          RPC URL 1
      --rpc2 <RPC_URL2>          RPC URL 2
      --num-blocks <NUM_BLOCKS>  Number of blocks to test from the tip [default: 32]
      --use-reth                 Whether to query reth namespace
      --use-tracing              Whether to query tracing methods
      --use-all-txes             Whether to query every transaction from a block or just the first
      --rate-limit <RATE_LIMIT>  Maximum requests per second (rate limit)
      --timeout <TIMEOUT>        Maximum time to wait for syncing in seconds [default: 300]
  -h, --help                     Print help
```
