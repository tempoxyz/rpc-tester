//! Writes a report from [`RpcTester`] run results.

use super::{ReportResults, TestError};
use assert_json_diff::assert_json_include;
use serde_json::Value;

/// Fields that are client-specific extensions and should be ignored during comparison.
/// For example, Nethermind includes an "error" field on reverted transaction receipts
/// which is not part of the Ethereum JSON-RPC specification.
const IGNORED_FIELDS: &[&str] = &["error"];

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
                    let rpc1 = filter_ignored_fields(rpc1);
                    let rpc2 = filter_ignored_fields(rpc2);

                    // While results are different, we only report it as error if __RPC1__ is
                    // missing/mismatching any element against RPC2.
                    if let Some(diffs) = verify_missing_or_mismatch(rpc1, rpc2) {
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
                Err(TestError::ErrDiff { rpc1, rpc2, args }) => {
                    passed_title = false;
                    println!("\n{title} ❌");
                    println!("    {name}: ❌ Error mismatch");
                    if let Some(args) = args {
                        println!("      args: {args}");
                    }
                    println!("      rpc1: {rpc1}");
                    println!("      rpc2: {rpc2}");
                }
                Err(TestError::Rpc1Err(err) | TestError::Rpc2Err(err)) => {
                    passed_title = false;
                    println!("\n{title} ❌");
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

/// Verifies if there is any missing field/element from rpc1 comparing it to rpc2.
fn verify_missing_or_mismatch(rpc1: Value, rpc2: Value) -> Option<String> {
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
            .replace("actual", "actual (rpc1)")
            .replace("expected", "expected (rpc2)");
        return Some(err_msg);
    }
    None
}
