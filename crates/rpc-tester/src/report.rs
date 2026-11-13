//! Writes a report from [`RpcTester`] run results.

use super::{ReportResults, TestError};
use assert_json_diff::assert_json_include;
use serde_json::Value;

/// Fields that are client-specific extensions of receipts and should be ignored during comparison.
/// For example, Nethermind includes an "error" field on reverted transaction receipts
/// which is not part of the Ethereum JSON-RPC specification.
const IGNORED_FIELDS: &[&str] = &["error"];

/// Methods returning receipts, the only results [`IGNORED_FIELDS`] are removed from. Elsewhere,
/// such as in call traces, `error` is a call's failure and is compared.
const RECEIPT_METHODS: &[&str] = &["get_transaction_receipt", "get_block_receipts"];

/// Simulated block header fields that depend on whether synthetic transfer logs (from
/// `traceTransfers`) count towards the block's receipts. Clients disagree on this: reth up to
/// v2.10 includes them, while newer reth leaves them out like geth. The transfer logs themselves
/// are still compared.
const SIMULATED_BLOCK_IGNORED_FIELDS: &[&str] = &["logsBloom", "receiptsRoot", "hash"];

/// Prints test results to console.
///
/// Returns error if RPC1 is missing/mismatching any element against RPC2 on any rpc method.
pub(crate) fn report(results_by_block: ReportResults) -> eyre::Result<()> {
    let mut passed = true;
    println!("\n--- RPC Method Test Results ---");
    println!("  (expected = rpc2, actual = rpc1)\n");

    for (title, results) in results_by_block {
        let mut passed_title = true;

        for (name, result) in results {
            match result {
                Ok(_) => {}
                Err(TestError::Diff { rpc1, rpc2, args }) => {
                    // Filter out client-specific extension fields before comparison
                    let (rpc1, rpc2) = if RECEIPT_METHODS.contains(&name.as_str()) {
                        (filter_ignored_fields(rpc1), filter_ignored_fields(rpc2))
                    } else if name == "eth_simulateV1" {
                        (filter_simulated_blocks(rpc1), filter_simulated_blocks(rpc2))
                    } else {
                        (rpc1, rpc2)
                    };

                    // While results are different, we only report it as error if __RPC1__ is
                    // missing/mismatching any element against RPC2.
                    if let Some(diffs) = verify_missing_or_mismatch(rpc1, rpc2, ["rpc1", "rpc2"]) {
                        if passed_title {
                            passed_title = false;
                            println!("\n{title} ❌");
                        }
                        println!("    {name}: ❌ Failure");
                        if let Some(args) = args {
                            println!("      args: {args}");
                        }
                        println!("{diffs}");
                    }
                }
                Err(TestError::StreamDiff { stream, http }) => {
                    if let Some(diffs) =
                        verify_missing_or_mismatch(stream, http, ["stream", "http"])
                    {
                        if passed_title {
                            passed_title = false;
                            println!("\n{title} ❌");
                        }
                        println!("    {name}: ❌ Failure");
                        println!("{diffs}");
                    }
                }
                Err(TestError::ErrDiff { rpc1, rpc2, args }) => {
                    if passed_title {
                        passed_title = false;
                        println!("\n{title} ❌");
                    }
                    println!("    {name}: ❌ Error mismatch");
                    if let Some(args) = args {
                        println!("      args: {args}");
                    }
                    println!("      rpc1: {rpc1}");
                    println!("      rpc2: {rpc2}");
                }
                Err(TestError::Rpc1Err(err) | TestError::Rpc2Err(err)) => {
                    if passed_title {
                        passed_title = false;
                        println!("\n{title} ❌");
                    }
                    println!("    {name}: ❌ {err}");
                }
            }
        }

        if passed_title {
            println!("{title} ✅");
        }
        passed &= passed_title;
    }

    println!("--------------------------------\n");
    if passed {
        Ok(())
    } else {
        Err(eyre::eyre!("Failed."))
    }
}

/// Recursively removes fields from JSON values that are in the [`IGNORED_FIELDS`] list.
/// This is used to filter out client-specific extensions before comparison.
fn filter_ignored_fields(value: Value) -> Value {
    match value {
        Value::Object(mut map) => {
            for field in IGNORED_FIELDS {
                map.remove(*field);
            }
            Value::Object(map.into_iter().map(|(k, v)| (k, filter_ignored_fields(v))).collect())
        }
        Value::Array(arr) => Value::Array(arr.into_iter().map(filter_ignored_fields).collect()),
        other => other,
    }
}

/// Removes [`SIMULATED_BLOCK_IGNORED_FIELDS`] from each block of an `eth_simulateV1` result, and
/// the `blockHash` of its calls' logs, which follows the block hash.
fn filter_simulated_blocks(mut value: Value) -> Value {
    for block in value.as_array_mut().into_iter().flatten() {
        let Some(block) = block.as_object_mut() else { continue };
        for field in SIMULATED_BLOCK_IGNORED_FIELDS {
            block.remove(*field);
        }
        let calls = block.get_mut("calls").and_then(Value::as_array_mut);
        for call in calls.into_iter().flatten() {
            let logs = call.get_mut("logs").and_then(Value::as_array_mut);
            for log in logs.into_iter().flatten().filter_map(Value::as_object_mut) {
                log.remove("blockHash");
            }
        }
    }
    value
}

/// Verifies if there is any missing field/element from rpc1 comparing it to rpc2. The sides are
/// named after `labels` in the returned diff.
///
/// Note that the superset semantics of `assert_json_include` only apply to object keys. Arrays
/// are compared positionally: extra trailing elements in rpc1 are tolerated, but an extra element
/// anywhere else shifts subsequent indices and fails.
fn verify_missing_or_mismatch(rpc1: Value, rpc2: Value, labels: [&str; 2]) -> Option<String> {
    let default_panic_hook = std::panic::take_hook();

    // Suppress the panic stderr output
    std::panic::set_hook(Box::new(|_| {}));

    let diff = std::panic::catch_unwind(|| {
        assert_json_include!(actual: rpc1, expected: rpc2);
    });

    // Restore the default hook
    std::panic::set_hook(default_panic_hook);

    if let Err(err) = diff {
        let err_msg = err
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .unwrap_or_else(|| err.downcast_ref::<String>().cloned().expect("should"))
            .replace("actual", &format!("actual ({})", labels[0]))
            .replace("expected", &format!("expected ({})", labels[1]));
        return Some(err_msg);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // A single test so the panic-hook manipulation in `verify_missing_or_mismatch` is never
    // raced by a parallel test.
    #[test]
    fn report_detects_diffs() {
        // rpc1 being a strict superset of rpc2 passes.
        let superset = TestError::Diff {
            rpc1: json!({ "a": 1, "extra": 2 }),
            rpc2: json!({ "a": 1 }),
            args: None,
        };
        assert!(report(vec![("t".to_string(), vec![("m".to_string(), Err(superset))])]).is_ok());

        // rpc1 missing a field that rpc2 has fails.
        let missing = TestError::Diff {
            rpc1: json!({ "a": 1 }),
            rpc2: json!({ "a": 1, "b": 2 }),
            args: None,
        };
        assert!(report(vec![("t".to_string(), vec![("m".to_string(), Err(missing))])]).is_err());

        // A mismatching field value fails.
        let mismatch =
            TestError::Diff { rpc1: json!({ "a": 1 }), rpc2: json!({ "a": 2 }), args: None };
        assert!(report(vec![("t".to_string(), vec![("m".to_string(), Err(mismatch))])]).is_err());

        // Client-specific receipt extension fields do not fail.
        let receipt = || TestError::Diff {
            rpc1: json!({ "a": 1 }),
            rpc2: json!({ "a": 1, "error": "reverted" }),
            args: None,
        };
        let method = "get_transaction_receipt".to_string();
        assert!(report(vec![("t".to_string(), vec![(method, Err(receipt()))])]).is_ok());

        // Elsewhere, such as in a call trace, a missing or different `error` fails.
        assert!(report(vec![("t".to_string(), vec![("m".to_string(), Err(receipt()))])]).is_err());
        let trace_error = TestError::Diff {
            rpc1: json!({ "calls": [{ "error": "out of gas" }] }),
            rpc2: json!({ "calls": [{ "error": "invalid opcode" }] }),
            args: None,
        };
        let method = "debug_trace_transaction".to_string();
        assert!(report(vec![("t".to_string(), vec![(method, Err(trace_error))])]).is_err());

        // Simulated blocks pass when only the header fields derived from transfer logs differ...
        let simulated = |bloom: &str, log_address: &str| {
            json!([{
                "hash": format!("0x{bloom}"),
                "logsBloom": bloom,
                "receiptsRoot": bloom,
                "number": "0x1",
                "calls": [{ "logs": [{ "address": log_address, "blockHash": format!("0x{bloom}") }] }],
            }])
        };
        let header_only = TestError::Diff {
            rpc1: simulated("00", "0xee"),
            rpc2: simulated("ff", "0xee"),
            args: None,
        };
        let method = "eth_simulateV1".to_string();
        assert!(report(vec![("t".to_string(), vec![(method.clone(), Err(header_only))])]).is_ok());

        // ...but still fail when the logs differ.
        let logs_differ = TestError::Diff {
            rpc1: simulated("00", "0xee"),
            rpc2: simulated("00", "0xff"),
            args: None,
        };
        assert!(report(vec![("t".to_string(), vec![(method, Err(logs_differ))])]).is_err());

        // A stream is compared with its node's HTTP response like rpc1 with rpc2.
        let stream_diff = |stream| TestError::StreamDiff { stream, http: json!({ "a": 1 }) };
        let superset = stream_diff(json!({ "a": 1, "extra": 2 }));
        assert!(report(vec![("t".to_string(), vec![("m".to_string(), Err(superset))])]).is_ok());
        let missing = stream_diff(json!({ "extra": 2 }));
        assert!(report(vec![("t".to_string(), vec![("m".to_string(), Err(missing))])]).is_err());

        // Error mismatches and node errors always fail.
        let err_diff =
            TestError::ErrDiff { rpc1: "a".to_string(), rpc2: "b".to_string(), args: None };
        assert!(report(vec![("t".to_string(), vec![("m".to_string(), Err(err_diff))])]).is_err());
        let rpc1_err = TestError::Rpc1Err("boom".to_string());
        assert!(report(vec![("t".to_string(), vec![("m".to_string(), Err(rpc1_err))])]).is_err());

        // Passing results pass.
        assert!(report(vec![("t".to_string(), vec![("m".to_string(), Ok(()))])]).is_ok());
    }
}
