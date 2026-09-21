//! `postgresx` —— PostgreSQL 适配器：连接池、参数化 SQL、事务、迁移与 rustls TLS。
//!
//! 本 crate 是零内部依赖的标准组件库，只依赖 crates.io 公开包，
//! 可直接 `cargo publish` 并被任意 Rust 工程复用。
//!
//! # 快速开始
//!
//! ```no_run
//! use postgresx::{PostgresConfig, PostgresPool, PostgresResult, SslMode};
//!
//! # async fn run() -> PostgresResult<()> {
//! // 1) 配置：env / TOML / URL / builder 四种入口等价
//! let config = PostgresConfig::builder()
//!     .host("127.0.0.1")
//!     .database("app")
//!     .user("app")
//!     .sslmode(SslMode::Disable)
//!     .build()?;
//!
//! // 2) 建池（会做一次 `SELECT 1` 冒烟）
//! let pool = PostgresPool::connect(config).await?;
//!
//! // 3) 参数化 SQL：只接受 `$N` + ToSql，禁止拼接用户输入
//! pool.execute("CREATE TABLE IF NOT EXISTS demo (id BIGINT PRIMARY KEY, name TEXT NOT NULL)", &[])
//!     .await?;
//! let affected = pool.execute("INSERT INTO demo (id, name) VALUES ($1, $2)", &[&1_i64, &"alice"])
//!     .await?;
//! assert_eq!(affected, 1);
//!
//! let row = pool.query_one("SELECT name FROM demo WHERE id = $1", &[&1_i64]).await?;
//! let name: String = row.get(0);
//! assert_eq!(name, "alice");
//!
//! // 4) 事务：Ok → COMMIT，Err → ROLLBACK
//! pool.with_transaction(|tx| Box::pin(async move {
//!     tx.execute("INSERT INTO demo (id, name) VALUES ($1, $2)", &[&2_i64, &"bob"]).await?;
//!     Ok(())
//! })).await?;
//!
//! // 5) 健康检查
//! pool.ping().await?;
//! let health = pool.health_check().await?;
//! println!("server={} latency={:?}", health.server_version, health.latency);
//!
//! pool.close();
//! # Ok(())
//! # }
//! ```
//!
//! # 安全
//!
//! - **参数化 SQL**：所有查询入口只接受 `$N` 占位符 + [`ToSql`] 参数，
//!   不提供任何把字符串拼进 SQL 的 API，从根本上避免 SQL 注入；
//! - **密码脱敏**：[`PostgresConfig`] 的 `Debug` 输出为 `***`，密码字段
//!   (`password`) 不在公开 API 中，只能经环境变量 / URL / builder 注入；
//! - **TLS 默认严格**：非 loopback 主机必须 `sslmode=require`（`validate()` 强制），
//!   rustls 配置始终校验服务端证书，无 insecure 旁路；
//! - **超时兜底**：`acquire_timeout` / `operation_timeout` 约束所有阻塞点，
//!   超时或取消时连接脱池而非复用未知状态连接。
//!
//! # 公共 API 一览
//!
//! | 入口 | 说明 |
//! | --- | --- |
//! | [`PostgresConfig`] / [`PostgresConfigBuilder`] / [`SslMode`] | 配置：`from_env` / `from_toml` / `from_url` / `validate` / `builder` |
//! | [`PostgresPool`] | `connect` / `new` / `acquire` / `execute` / `query` / `query_one` / `query_opt` / `with_transaction` / `begin` / `copy_in_bytes` / `copy_out_bytes` / `ping` / `health_check` / `stats` / `close` |
//! | [`PgConnection`] | 连接句柄：参数化 SQL + `COPY` 原语 + 上限常量 |
//! | [`PgTransaction`] / [`TxStatus`] | 事务句柄与准确状态机 |
//! | [`Migrator`] / [`Migration`] / [`MigrationStatus`] | 迁移：advisory lock + checksum 校验，`verify()` 不自动 DDL |
//! | [`MakeRustlsConnect`] / [`build_client_config`] | rustls TLS 连接器与自定义根证书 |
//! | [`with_retry_sync`] / [`with_retry_async`] / [`PgRetryConfig`] | 指数退避 + 抖动 + 总预算的重试 |
//! | [`PostgresError`] / [`PostgresResult`] / [`ErrorKind`] | 统一错误、结果别名与 SQLSTATE 分类 |
//! | [`Row`] / [`ToSql`] | 重新导出 `tokio_postgres` 的行类型与参数 trait |
//!
//! # 迁移相关
//!
//! [`Migrator::verify`] 是启动默认入口，**只**校验 checksum 与未知版本，绝不自动执行 DDL；
//! 需要执行 pending 迁移时必须显式调用 [`Migrator::apply`]。

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(unreachable_pub)]

mod config;
mod conn;
mod error;
mod migration;
mod pool;
mod resilience;
mod tls;
mod tx;

pub use config::{
    host_is_local, PostgresConfig, PostgresConfigBuilder, SslMode, DEFAULT_MAX_POOL_SIZE,
    DEFAULT_PORT, ENV_ACQUIRE_TIMEOUT_MS, ENV_APPLICATION_NAME, ENV_CONNECT_TIMEOUT_MS,
    ENV_DATABASE, ENV_HOST, ENV_MAX_POOL_SIZE, ENV_OPERATION_TIMEOUT_MS, ENV_PASSWORD, ENV_PORT,
    ENV_SSLMODE, ENV_TLS_CA_FILE, ENV_TLS_CLIENT_CERT, ENV_TLS_CLIENT_KEY, ENV_TLS_SERVER_NAME,
    ENV_URL, ENV_USER,
};
pub use conn::{PgConnection, DEFAULT_COPY_IN_MAX_BYTES, DEFAULT_COPY_OUT_MAX_BYTES};
pub use error::{
    error_from_sqlstate, error_kind_from_sqlstate, map_pool_error, map_tokio_error, ErrorKind,
    PostgresError, PostgresResult,
};
pub use migration::{
    ensure_boot_ok, AppliedMigration, ChecksumMismatch, Migration, MigrationReport,
    MigrationStatus, Migrator, MIGRATION_LOCK_KEY1, MIGRATION_LOCK_KEY2, SCHEMA_MIGRATIONS_TABLE,
};
pub use pool::{BoxFuture, PoolStats, PostgresHealth, PostgresPool};
pub use resilience::{with_retry_async, with_retry_async_no_wait, with_retry_sync, PgRetryConfig};
pub use tls::{
    build_client_config, build_client_config_with_ca, build_client_config_with_options,
    MakeRustlsConnect, RustlsConnect, RustlsStream,
};
pub use tx::{PgTransaction, TxStatus};

/// 常用 re-export：行类型与参数 trait。
pub use tokio_postgres::{types::ToSql, Row};

#[cfg(test)]
mod unit_smoke {
    use super::*;

    #[test]
    fn public_types_are_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<PostgresPool>();
        assert_send_sync::<PostgresConfig>();
        assert_send_sync::<PostgresConfigBuilder>();
        assert_send_sync::<PoolStats>();
        assert_send_sync::<PostgresHealth>();
        assert_send_sync::<SslMode>();
        assert_send_sync::<MakeRustlsConnect>();
        assert_send_sync::<PostgresError>();
        assert_send_sync::<ErrorKind>();
        assert_send_sync::<TxStatus>();
        assert_send_sync::<MigrationStatus>();
        assert_send_sync::<PgRetryConfig>();
        assert_send_sync::<Migrator>();
    }

    #[test]
    fn default_constants_are_stable() {
        assert_eq!(DEFAULT_PORT, 5432);
        assert_eq!(DEFAULT_MAX_POOL_SIZE, 16);
        assert_eq!(DEFAULT_COPY_IN_MAX_BYTES, 16 * 1024 * 1024);
        assert_eq!(DEFAULT_COPY_OUT_MAX_BYTES, 16 * 1024 * 1024);
        assert_eq!(SCHEMA_MIGRATIONS_TABLE, "infra_schema_migrations");
        assert!(ENV_HOST.starts_with("FOUNDATIONX_POSTGRESX_"));
        assert!(ENV_PASSWORD.starts_with("FOUNDATIONX_POSTGRESX_"));
    }

    #[test]
    fn reexports_are_usable() {
        fn assert_type<T: ?Sized>() {}
        assert_type::<PgConnection>();
        assert_type::<PgTransaction>();
        assert_type::<Row>();
        assert_type::<dyn ToSql>();
        let _ = map_pool_error;
        let _ = map_tokio_error;
        let _ = error_kind_from_sqlstate;
        let error = error_from_sqlstate("42P01", "missing");
        assert_eq!(error_kind_from_sqlstate("42P01"), ErrorKind::Missing);
        assert!(matches!(error, PostgresError::Missing(_)));

        let tls = MakeRustlsConnect::with_webpki_roots().expect("tls 连接器");
        assert!(!format!("{tls:?}").is_empty());
        let _ = build_client_config().expect("rustls client config");

        let retry = PgRetryConfig::fixed(1, std::time::Duration::ZERO);
        assert_eq!(
            with_retry_sync(&retry, "smoke", || Ok(1_i32)).expect("重试"),
            1
        );
    }
}
