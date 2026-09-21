#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable
)]
//! 公共 API 表面：类型存在、`Send + Sync`、关键签名与 re-export 可用。

use std::time::Duration;

use postgresx::{
    build_client_config, build_client_config_with_ca, build_client_config_with_options,
    ensure_boot_ok, error_from_sqlstate, error_kind_from_sqlstate, host_is_local, map_pool_error,
    map_tokio_error, with_retry_async, with_retry_async_no_wait, with_retry_sync, AppliedMigration,
    ChecksumMismatch, ErrorKind, MakeRustlsConnect, Migration, MigrationReport, MigrationStatus,
    Migrator, PgConnection, PgRetryConfig, PgTransaction, PoolStats, PostgresConfig,
    PostgresConfigBuilder, PostgresError, PostgresHealth, PostgresPool, PostgresResult, Row,
    SslMode, ToSql, TxStatus, DEFAULT_COPY_IN_MAX_BYTES, DEFAULT_COPY_OUT_MAX_BYTES,
    DEFAULT_MAX_POOL_SIZE, DEFAULT_PORT, ENV_HOST, ENV_PASSWORD, ENV_URL, MIGRATION_LOCK_KEY1,
    MIGRATION_LOCK_KEY2, SCHEMA_MIGRATIONS_TABLE,
};

fn assert_send_sync<T: Send + Sync>() {}

fn assert_clone<T: Clone>() {}

fn assert_debug<T: std::fmt::Debug>() {}

fn assert_error<T: std::error::Error + Send + Sync + 'static>() {}

fn assert_deserialize<'de, T: serde::de::Deserialize<'de>>() {}

fn assert_default<T: Default>() {}

/// `with_retry_sync` 的闭包形参签名。
type SyncRetryFn = fn(&PgRetryConfig, &str, fn() -> PostgresResult<u8>) -> PostgresResult<u8>;
/// `build_client_config` 的签名。
type ClientConfigFn = fn() -> PostgresResult<rustls::ClientConfig>;
/// `build_client_config_with_ca` 的签名。
type ClientConfigCaFn = fn(Option<&std::path::Path>) -> PostgresResult<rustls::ClientConfig>;
/// `build_client_config_with_options` 的签名。
type ClientConfigAllFn = fn(
    Option<&std::path::Path>,
    Option<&std::path::Path>,
    Option<&std::path::Path>,
) -> PostgresResult<rustls::ClientConfig>;

#[test]
fn public_types_are_thread_safe() {
    assert_send_sync::<PostgresPool>();
    assert_send_sync::<PgConnection>();
    assert_send_sync::<PgTransaction>();
    assert_send_sync::<PostgresConfig>();
    assert_send_sync::<PostgresConfigBuilder>();
    assert_send_sync::<PostgresError>();
    assert_send_sync::<PoolStats>();
    assert_send_sync::<PostgresHealth>();
    assert_send_sync::<SslMode>();
    assert_send_sync::<TxStatus>();
    assert_send_sync::<ErrorKind>();
    assert_send_sync::<Migration>();
    assert_send_sync::<MigrationStatus>();
    assert_send_sync::<MigrationReport>();
    assert_send_sync::<AppliedMigration>();
    assert_send_sync::<ChecksumMismatch>();
    assert_send_sync::<Migrator>();
    assert_send_sync::<PgRetryConfig>();
    assert_send_sync::<MakeRustlsConnect>();
    assert_send_sync::<Row>();
}

#[test]
fn public_types_are_cloneable_or_debuggable_as_documented() {
    assert_clone::<PostgresPool>();
    assert_clone::<PostgresConfig>();
    assert_clone::<PostgresConfigBuilder>();
    assert_clone::<PgRetryConfig>();
    assert_clone::<MakeRustlsConnect>();
    assert_clone::<PoolStats>();
    assert_debug::<PostgresPool>();
    assert_debug::<PostgresConfig>();
    assert_debug::<PgConnection>();
    assert_debug::<PgTransaction>();
    assert_debug::<PoolStats>();
    assert_debug::<PostgresHealth>();
    assert_debug::<MigrationStatus>();
    assert_default::<PostgresConfig>();
    assert_deserialize::<PostgresConfig>();
}

#[test]
fn results_and_errors_are_std_errors() {
    assert_error::<PostgresError>();
    assert_send_sync::<PostgresResult<()>>();
    let error = error_from_sqlstate("08006", "connection failure");
    assert!(error.is_retryable());
    assert!(error.to_string().contains("08006"));
}

#[test]
fn documented_constants_are_stable() {
    assert_eq!(DEFAULT_PORT, 5432);
    assert_eq!(DEFAULT_MAX_POOL_SIZE, 16);
    assert_eq!(DEFAULT_COPY_IN_MAX_BYTES, 16 * 1024 * 1024);
    assert_eq!(DEFAULT_COPY_OUT_MAX_BYTES, 16 * 1024 * 1024);
    assert_eq!(SCHEMA_MIGRATIONS_TABLE, "infra_schema_migrations");
    assert_ne!(MIGRATION_LOCK_KEY1, MIGRATION_LOCK_KEY2);
    assert_eq!(ENV_HOST, "FOUNDATIONX_POSTGRESX_HOST");
    assert_eq!(ENV_PASSWORD, "FOUNDATIONX_POSTGRESX_PASSWORD");
    assert_eq!(ENV_URL, "FOUNDATIONX_POSTGRESX_URL");
}

#[test]
fn callback_and_helper_signatures_are_callable() {
    // 纯函数与映射函数作为函数指针可用（签名未被 feature gate 改变）
    let _kind: fn(&str) -> ErrorKind = error_kind_from_sqlstate;
    let _kind_err: fn(&str, String) -> PostgresError =
        |code, message| error_from_sqlstate(code, message);
    let _host: fn(&str) -> bool = host_is_local;
    let _boot: fn(&MigrationStatus) -> PostgresResult<()> = ensure_boot_ok;
    let _pool_map: fn(deadpool_postgres::PoolError) -> PostgresError = map_pool_error;
    let _tokio_map: fn(tokio_postgres::Error) -> PostgresError = map_tokio_error;
    let _retry_sync: SyncRetryFn = with_retry_sync;
    let _config_build: ClientConfigFn = build_client_config;
    let _config_ca: ClientConfigCaFn = build_client_config_with_ca;
    let _config_all: ClientConfigAllFn = build_client_config_with_options;
}

#[test]
fn sync_retry_helper_is_callable() {
    let config = PgRetryConfig::fixed(3, Duration::ZERO).without_jitter();
    let value = with_retry_sync(&config, "surface", || Ok(7_u8)).expect("重试包装");
    assert_eq!(value, 7);
}

#[tokio::test]
async fn async_retry_helpers_are_callable() {
    let config = PgRetryConfig::fixed(3, Duration::ZERO).without_jitter();
    let value = with_retry_async(&config, "surface", || async { Ok(11_u8) })
        .await
        .expect("异步重试包装");
    assert_eq!(value, 11);
    let value = with_retry_async_no_wait(&config, "surface", || async { Ok(13_u8) })
        .await
        .expect("无等待异步重试包装");
    assert_eq!(value, 13);
}

#[test]
fn pool_and_row_reexports_are_usable() {
    fn assert_type<T: ?Sized>() {}
    assert_type::<Row>();
    assert_type::<dyn ToSql>();

    let pool = PostgresPool::new(PostgresConfig::default()).expect("建池（不联网）");
    assert_eq!(pool.stats().max_size, DEFAULT_MAX_POOL_SIZE);
    assert!(pool.summary().contains("127.0.0.1"));
    pool.close();

    let _ = Duration::from_secs(1);
    let _ = SslMode::Require.as_str();
}
