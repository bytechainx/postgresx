//! [`PostgresPool`]：连接池、参数化 SQL、事务、健康检查与统计。
//!
//! # 安全
//!
//! 所有 SQL 入口（[`PostgresPool::execute`] / `query*` / 事务内同名方法）
//! **只**接受 `$1..$N` 占位符与 [`ToSql`] 参数；不提供任何字符串拼接 SQL 的入口。
//!
//! # TLS
//!
//! [`SslMode::Disable`] 使用 `NoTls`；[`SslMode::Prefer`] / [`SslMode::Require`]
//! 使用 [`MakeRustlsConnect`]（webpki 公共根 + 系统信任库 + 可选自定义 CA/mTLS）。

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use deadpool_postgres::Pool;
use tokio_postgres::types::ToSql;
use tokio_postgres::NoTls;
use tokio_postgres::Row;

use crate::config::{PostgresConfig, SslMode};
use crate::conn::PgConnection;
use crate::error::{
    map_create_pool_error, map_pool_error, map_tokio_error, PostgresError, PostgresResult,
};
use crate::tls::MakeRustlsConnect;
use crate::tx::PgTransaction;

/// 事务闭包返回的 boxed future。
///
/// [`PostgresPool::with_transaction`] 要求闭包在借用事务时返回该类型，
/// 以便在 `async` 闭包内跨 `await` 持有 `&mut PgTransaction`
/// （MSRV 1.75 下的稳定写法）：
///
/// ```ignore
/// pool.with_transaction(|tx| Box::pin(async move {
///     tx.execute("INSERT INTO t (id) VALUES ($1)", &[&1_i64]).await?;
///     Ok(())
/// })).await?;
/// ```
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// 连接池快照统计。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolStats {
    /// 配置的最大连接数。
    pub max_size: usize,
    /// 当前池内连接数。
    pub size: usize,
    /// 当前空闲可借连接数。
    pub available: usize,
    /// 等待获取连接的任务数。
    pub waiting: usize,
    /// 是否已 [`PostgresPool::close`]。
    pub closed: bool,
}

/// 结构化健康检查结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostgresHealth {
    /// 服务端版本（`SHOW server_version` 的返回值）。
    pub server_version: String,
    /// 本次健康检查耗时（含获取连接）。
    pub latency: Duration,
    /// 检查时刻的池快照。
    pub pool: PoolStats,
}

/// 生产 Postgres 连接池。
///
/// `Clone` 廉价（内部 `Arc`），可自由跨任务共享。
#[derive(Clone)]
pub struct PostgresPool {
    inner: Pool,
    closed: Arc<AtomicBool>,
    summary: Arc<String>,
    acquire_timeout: Duration,
    operation_timeout: Duration,
}

impl std::fmt::Debug for PostgresPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PostgresPool")
            .field("closed", &self.closed.load(Ordering::Relaxed))
            .field("summary", &self.summary)
            .field("stats", &self.stats())
            .finish()
    }
}

impl PostgresPool {
    /// 按配置建立连接池，并验证至少能借出连接（`SELECT 1` 冒烟）。
    pub async fn connect(config: PostgresConfig) -> PostgresResult<Self> {
        let pool = Self::new(config)?;
        pool.ping().await?;
        Ok(pool)
    }

    /// 按配置构造连接池：**只做本地校验与建池，不建立网络连接**。
    ///
    /// 用于启动阶段分离「配置错误」与「服务不可达」，或希望自行控制首次连接时机
    /// 的场景；首次 `acquire` / `ping` 才可能因网络失败。生产默认建议用
    /// [`Self::connect`]。
    pub fn new(config: PostgresConfig) -> PostgresResult<Self> {
        config.validate()?;
        let deadpool_config = config.to_deadpool_config();
        let pool = match config.sslmode {
            SslMode::Disable => deadpool_config
                .create_pool(Some(deadpool_postgres::Runtime::Tokio1), NoTls)
                .map_err(map_create_pool_error)?,
            SslMode::Prefer | SslMode::Require => {
                let tls = MakeRustlsConnect::with_options(
                    config.tls_ca_file.as_deref(),
                    config.tls_client_cert_path(),
                    config.tls_client_key_path(),
                )?;
                deadpool_config
                    .create_pool(Some(deadpool_postgres::Runtime::Tokio1), tls)
                    .map_err(map_create_pool_error)?
            }
        };

        Ok(Self {
            inner: pool,
            closed: Arc::new(AtomicBool::new(false)),
            summary: Arc::new(format!(
                "{}:{}/{} user={} sslmode={} pool={}",
                config.host,
                config.port,
                config.database,
                config.user,
                config.sslmode.as_str(),
                config.max_pool_size
            )),
            acquire_timeout: config.acquire_timeout,
            operation_timeout: config.operation_timeout,
        })
    }

    /// 配置摘要（不含密码），用于日志与诊断。
    #[must_use]
    pub fn summary(&self) -> &str {
        &self.summary
    }

    fn ensure_open(&self) -> PostgresResult<()> {
        if self.closed.load(Ordering::Acquire) {
            Err(PostgresError::Connection(
                "postgres 连接池已关闭".to_string(),
            ))
        } else {
            Ok(())
        }
    }

    /// 借出连接（使用配置中的 `acquire_timeout`）。
    pub async fn acquire(&self) -> PostgresResult<PgConnection> {
        self.acquire_with(self.acquire_timeout).await
    }

    /// 在给定截止时间内借出连接。
    ///
    /// `deadline` 为零返回配置错误；等待超时返回 [`PostgresError::Timeout`]。
    pub async fn acquire_with(&self, deadline: Duration) -> PostgresResult<PgConnection> {
        self.ensure_open()?;
        if deadline.is_zero() {
            return Err(PostgresError::Config(
                "postgres acquire_with deadline 必须大于零".to_string(),
            ));
        }
        let client = tokio::time::timeout(deadline, self.inner.get())
            .await
            .map_err(|_| PostgresError::Timeout("postgres acquire_with 超时".to_string()))?
            .map_err(map_pool_error)?;
        Ok(PgConnection::new(client, self.operation_timeout))
    }

    /// 参数化 `EXECUTE`（短借连接），返回影响行数。
    pub async fn execute(&self, sql: &str, params: &[&(dyn ToSql + Sync)]) -> PostgresResult<u64> {
        let mut conn = self.acquire().await?;
        conn.execute(sql, params).await
    }

    /// 参数化查询，恰好一行。
    pub async fn query_one(
        &self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> PostgresResult<Row> {
        let mut conn = self.acquire().await?;
        conn.query_one(sql, params).await
    }

    /// 参数化查询，0..N 行。
    pub async fn query(
        &self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> PostgresResult<Vec<Row>> {
        let mut conn = self.acquire().await?;
        conn.query(sql, params).await
    }

    /// 参数化查询，可选单行（0 行 → `Ok(None)`，多行 → 错误）。
    pub async fn query_opt(
        &self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> PostgresResult<Option<Row>> {
        let mut conn = self.acquire().await?;
        conn.query_opt(sql, params).await
    }

    /// `COPY ... FROM STDIN`：写入有界字节缓冲，返回影响行数。
    pub async fn copy_in_bytes(&self, statement: &str, data: &[u8]) -> PostgresResult<u64> {
        let mut conn = self.acquire().await?;
        conn.copy_in_bytes(statement, data).await
    }

    /// `COPY ... TO STDOUT`：读出有界字节缓冲。
    pub async fn copy_out_bytes(
        &self,
        statement: &str,
        max_bytes: usize,
    ) -> PostgresResult<Vec<u8>> {
        let mut conn = self.acquire().await?;
        conn.copy_out_bytes(statement, max_bytes).await
    }

    /// 在事务中执行闭包：`Ok` → commit，`Err` → rollback。
    ///
    /// 闭包获得 [`PgTransaction`]，可在同一事务内执行多条参数化 SQL：
    ///
    /// ```ignore
    /// pool.with_transaction(|tx| Box::pin(async move {
    ///     tx.execute("INSERT INTO t (id) VALUES ($1)", &[&1_i64]).await?;
    ///     Ok(())
    /// })).await?;
    /// ```
    ///
    /// 业务失败时尽力回滚；若回滚同样失败，返回**保留原始分类**并附带回滚上下文
    /// 的错误，不会把「业务失败」伪装成「回滚失败」。
    pub async fn with_transaction<F, T>(&self, f: F) -> PostgresResult<T>
    where
        F: for<'a> FnOnce(&'a mut PgTransaction) -> BoxFuture<'a, PostgresResult<T>>,
    {
        let conn = self.acquire().await?;
        let mut tx = conn.begin().await?;
        match f(&mut tx).await {
            Ok(value) => {
                tx.commit().await?;
                Ok(value)
            }
            Err(error) => match tx.rollback().await {
                Ok(()) => Err(error),
                Err(rollback_error) => Err(error.context_message(&format!(
                    "with_transaction 业务操作失败且 ROLLBACK 也失败: {rollback_error}"
                ))),
            },
        }
    }

    /// 显式开启事务（调用方负责 commit / rollback）。
    pub async fn begin(&self) -> PostgresResult<PgTransaction> {
        let conn = self.acquire().await?;
        conn.begin().await
    }

    /// 健康检查：`SELECT 1` 成功返回 `Ok(())`。
    pub async fn ping(&self) -> PostgresResult<()> {
        self.ensure_open()?;
        let mut conn = self.acquire().await?;
        let row = conn.query_one("SELECT 1", &[]).await?;
        let value: i32 = row.try_get(0).map_err(map_tokio_error)?;
        if value != 1 {
            return Err(PostgresError::Backend(format!("健康检查异常结果: {value}")));
        }
        Ok(())
    }

    /// 健康检查：返回服务端版本、延迟与池快照。
    pub async fn health_check(&self) -> PostgresResult<PostgresHealth> {
        self.ensure_open()?;
        let started = Instant::now();
        let mut conn = self.acquire().await?;
        let row = conn.query_one("SHOW server_version", &[]).await?;
        let server_version: String = row.try_get(0).map_err(map_tokio_error)?;
        Ok(PostgresHealth {
            server_version,
            latency: started.elapsed(),
            pool: self.stats(),
        })
    }

    /// 池统计快照。
    #[must_use]
    pub fn stats(&self) -> PoolStats {
        let status = self.inner.status();
        PoolStats {
            max_size: status.max_size,
            size: status.size,
            available: status.available,
            waiting: status.waiting,
            closed: self.closed.load(Ordering::Relaxed),
        }
    }

    /// 关闭池；此后 `acquire` 与所有 SQL 入口返回 [`PostgresError::Connection`]。
    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.inner.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PostgresConfig;

    #[test]
    fn stats_reflect_config() {
        let config = PostgresConfig::builder()
            .host("127.0.0.1")
            .database("db")
            .user("u")
            .max_pool_size(3)
            .build()
            .expect("配置");
        let pool = PostgresPool::new(config).expect("建池");
        let stats = pool.stats();
        assert_eq!(stats.max_size, 3);
        assert!(!stats.closed);
        assert!(pool.summary().contains("127.0.0.1"));
        assert!(!format!("{pool:?}").is_empty());

        pool.close();
        assert!(pool.stats().closed);
    }

    #[tokio::test]
    async fn acquire_rejects_zero_deadline() {
        let config = PostgresConfig::default();
        let pool = PostgresPool::new(config).expect("建池");
        let error = pool
            .acquire_with(Duration::ZERO)
            .await
            .expect_err("零 deadline");
        assert!(matches!(error, PostgresError::Config(_)));
    }

    #[tokio::test]
    async fn closed_pool_rejects_operations() {
        let pool = PostgresPool::new(PostgresConfig::default()).expect("建池");
        pool.close();
        let error = pool.ping().await.expect_err("已关闭池");
        assert!(matches!(error, PostgresError::Connection(_)));
    }

    #[tokio::test]
    async fn connect_refused_returns_error() {
        let config = PostgresConfig::builder()
            .host("127.0.0.1")
            .port(1)
            .database("x")
            .user("x")
            .sslmode(SslMode::Disable)
            .connect_timeout(Duration::from_millis(300))
            .acquire_timeout(Duration::from_millis(500))
            .build()
            .expect("配置");
        let result =
            tokio::time::timeout(Duration::from_secs(5), PostgresPool::connect(config)).await;
        match result {
            Ok(Err(error)) => assert!(error.is_retryable(), "连接失败应可重试: {error}"),
            Ok(Ok(_)) => panic!("不可达地址不应连接成功"),
            Err(_) => panic!("connect 必须受内部截止时间约束"),
        }
    }
}
