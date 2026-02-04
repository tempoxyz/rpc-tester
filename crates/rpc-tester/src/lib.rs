//! rpc-tester library
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

#[doc(hidden)]
pub mod get_logs;
mod tester;
pub use tester::RpcTester;
mod report;

/// Equality rpc test error
enum TestError {
    Diff { rpc1: serde_json::Value, rpc2: serde_json::Value, args: Option<String> },
    Rpc1Err(String),
    Rpc2Err(String),
}

/// Alias type
type ReportResults = Vec<(String, Vec<(MethodName, Result<(), TestError>)>)>;

/// Alias type
type MethodName = String;

/// Provider macro that boxes all method future results.
#[macro_export]
macro_rules! rpc {
    ($self:expr, $method:ident $(, $args:expr )* ) => {{
        let args_str = Some(format!("{}", [$(format!("{:?}", $args)),*].join(", ")));
        Box::pin($self.test_rpc_call(
            stringify!($method),
            args_str,
            move |provider: &P| {
                provider.$method( $( $args.clone(), )*)
            }
        )) as Pin<Box<dyn Future<Output = (MethodName, Result<(), TestError>)> + Send>>
    }};
}

/// Provider macro to call methods that return `RpcWithBlock` and box the future results.
#[macro_export]
macro_rules! rpc_with_block {
    ($self:expr, $method:ident $(, $args:expr )*; $blockid:expr) => {{
        let args_str = Some(format!("{}, block_id: {:?}", [$(format!("{:?}", $args)),*].join(", "), $blockid));
        Box::pin($self.test_rpc_call(
            stringify!($method),
            args_str,
            move |provider: &P| {
                provider.$method( $( $args.clone(), )*).block_id($blockid).into_future()
            }
        )) as Pin<Box<dyn Future<Output = (MethodName, Result<(), TestError>)> + Send>>
    }};
}

/// Macro to call the `get_logs` rpc method and box the future result.
///
/// Uses [`get_logs::get_logs_with_retry`] to automatically handle "max results exceeded" errors
/// by retrying with the narrower block range suggested in the error message.
#[macro_export]
macro_rules! get_logs {
    ($self:expr, $arg:expr) => {{
        let args_str = Some(format!("{:?}", $arg));
        Box::pin(async move {
            let filter = $arg.clone();
            $self
                .test_rpc_call(stringify!(get_logs), args_str, move |provider: &P| {
                    let filter = filter.clone();
                    async move { $crate::get_logs::get_logs_with_retry(provider, &filter).await }
                })
                .await
        }) as Pin<Box<dyn Future<Output = (MethodName, Result<(), TestError>)> + Send>>
    }};
}

/// Macro to create raw request and box the future result.
#[macro_export]
macro_rules! rpc_raw {
    ($self:expr, $method:ident, $ret:ident $(, $args:expr )* ) => {{
        let args_str = Some(format!("{}", [$(format!("{:?}", $args)),*].join(", ")));
        Box::pin($self.test_rpc_call(
            stringify!($method),
            args_str,
            move |provider: &P| {
                provider.raw_request::<_, $ret>(stringify!($method).into(), $( $args.clone(), )*)
            }
        )) as Pin<Box<dyn Future<Output = (MethodName, Result<(), TestError>)> + Send>>
    }};
}
