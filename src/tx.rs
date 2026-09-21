//! 事务句柄与状态机。
//!
//! 状态迁移：[`TxStatus::Active`] → [`TxStatus::Committed`] / [`TxStatus::RolledBack`] /
//! [`TxStatus::Failed`]。
//!
//! 语义要点（与「借用式事务」不同，本类型跨 `await` 持有连接）：
//!
//! - 使用显式 `BEGIN` / `COMMIT` / `ROLLBACK` SQL 驱动状态机；
//! - 每条语句执行**前**先进入 [`TxStatus::Failed`]，只有操作明确成功才回到
//!   [`TxStatus::Active`]：future 被 drop 或任务 abort 时停留在 `Failed`，
//!   调用方据此知道「不能再执行 SQL，但可以尝试回滚」；
//! - `Drop` 时若仍持有连接，直接把连接移出池并关闭 session（由服务端回滚事务），
//!   绝不做 fire-and-forget 的异步回滚。

use std::time::Duration;

use deadpool_postgres::Object;
use tokio_postgres::types::ToSql;
use tokio_postgres::Row;

use crate::error::{map_tokio_error, PostgresError, PostgresResult};
use crate::guard::PooledObjectGuard;

/// 事务状态。
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TxStatus {
    /// 已 `BEGIN` 且仍可执行 SQL。
    Active,
    /// 已成功 `COMMIT`。
    Committed,
    /// 已成功 `ROLLBACK`。
    RolledBack,
    /// 上一次操作失败；仅当连接仍可用时允许 `ROLLBACK`。
    Failed,
}

impl TxStatus {
    /// 稳定短名（用于日志）。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Committed => "committed",
            Self::RolledBack => "rolled_back",
            Self::Failed => "failed",
        }
    }

    /// 是否已终结（提交或回滚）。
    #[must_use]
    pub const fn is_finished(self) -> bool {
        matches!(self, Self::Committed | Self::RolledBack)
    }
}

/// Postgres 事务句柄。
pub struct PgTransaction {
    client: Option<Object>,
    state: TxStatus,
    operation_timeout: Duration,
}

impl PgTransaction {
    /// 在已借出连接上执行 `BEGIN`。
    pub(crate) async fn begin(client: Object, operation_timeout: Duration) -> PostgresResult<Self> {
        let guard = PooledObjectGuard::new(client);
        match tokio::time::timeout(operation_timeout, guard.object()?.batch_execute("BEGIN")).await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return Err(map_tokio_error(error)),
            Err(error) => {
                return Err(PostgresError::Timeout(format!(
                    "postgres BEGIN 超时；连接已丢弃: {error}"
                )));
            }
        }
        let client = guard.release()?;
        Ok(Self {
            client: Some(client),
            state: TxStatus::Active,
            operation_timeout,
        })
    }

    /// 当前准确状态。
    #[must_use]
    pub fn status(&self) -> TxStatus {
        self.state
    }

    /// 是否仍可执行 SQL。
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.state == TxStatus::Active
    }

    fn take_guard(&mut self) -> PostgresResult<PooledObjectGuard> {
        self.ensure_active()?;
        self.take_guard_after_validation()
    }

    fn take_guard_after_validation(&mut self) -> PostgresResult<PooledObjectGuard> {
        let guard = self
            .client
            .take()
            .map(PooledObjectGuard::new)
            .ok_or_else(|| PostgresError::Backend("事务连接已释放".to_string()))?;
        // 先进入失败态，再把连接交给可被取消的 await；只有操作明确完成且连接
        // 恢复到事务句柄后才回到 Active。
        self.state = TxStatus::Failed;
        Ok(guard)
    }

    fn ensure_active(&self) -> PostgresResult<()> {
        match self.state {
            TxStatus::Active => Ok(()),
            TxStatus::Committed => Err(PostgresError::Backend(
                "事务已 COMMIT，禁止再操作".to_string(),
            )),
            TxStatus::RolledBack => Err(PostgresError::Backend(
                "事务已 ROLLBACK，禁止再操作".to_string(),
            )),
            TxStatus::Failed => Err(PostgresError::Backend(
                "事务已失败，仅允许在连接可用时 ROLLBACK".to_string(),
            )),
        }
    }

    fn ensure_rollbackable(&self) -> PostgresResult<()> {
        match self.state {
            TxStatus::Active | TxStatus::Failed => Ok(()),
            TxStatus::Committed => Err(PostgresError::Backend(
                "事务已 COMMIT，禁止再 ROLLBACK".to_string(),
            )),
            TxStatus::RolledBack => Err(PostgresError::Backend(
                "事务已 ROLLBACK，禁止重复操作".to_string(),
            )),
        }
    }

    /// 执行多语句 SQL（simple query / `batch_execute`）。
    ///
    /// 仅用于 crate 内**受信任**脚本（迁移 DDL）；禁止拼接用户输入。
    pub(crate) async fn batch_execute(&mut self, sql: &str) -> PostgresResult<()> {
        if sql.trim().is_empty() {
            return Err(PostgresError::Config(
                "batch_execute sql 不能为空".to_string(),
            ));
        }
        let guard = self.take_guard()?;
        match tokio::time::timeout(self.operation_timeout, guard.object()?.batch_execute(sql)).await
        {
            Ok(Ok(())) => {
                self.client = Some(guard.release()?);
                self.state = TxStatus::Active;
                Ok(())
            }
            Ok(Err(error)) => {
                self.client = Some(guard.release()?);
                Err(map_tokio_error(error))
            }
            Err(error) => Err(PostgresError::Timeout(format!(
                "postgres 事务 batch_execute 超时；连接已丢弃: {error}"
            ))),
        }
    }

    /// 参数化 `EXECUTE`。
    pub async fn execute(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> PostgresResult<u64> {
        let guard = self.take_guard()?;
        match tokio::time::timeout(self.operation_timeout, guard.object()?.execute(sql, params))
            .await
        {
            Ok(Ok(affected)) => {
                self.client = Some(guard.release()?);
                self.state = TxStatus::Active;
                Ok(affected)
            }
            Ok(Err(error)) => {
                self.client = Some(guard.release()?);
                Err(map_tokio_error(error))
            }
            Err(error) => Err(PostgresError::Timeout(format!(
                "postgres 事务 execute 超时；连接已丢弃: {error}"
            ))),
        }
    }

    /// 参数化查询（恰好一行）。
    pub async fn query_one(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> PostgresResult<Row> {
        let guard = self.take_guard()?;
        match tokio::time::timeout(
            self.operation_timeout,
            guard.object()?.query_one(sql, params),
        )
        .await
        {
            Ok(Ok(row)) => {
                self.client = Some(guard.release()?);
                self.state = TxStatus::Active;
                Ok(row)
            }
            Ok(Err(error)) => {
                self.client = Some(guard.release()?);
                Err(map_tokio_error(error))
            }
            Err(error) => Err(PostgresError::Timeout(format!(
                "postgres 事务 query_one 超时；连接已丢弃: {error}"
            ))),
        }
    }

    /// 参数化查询（0..N 行）。
    pub async fn query(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> PostgresResult<Vec<Row>> {
        let guard = self.take_guard()?;
        match tokio::time::timeout(self.operation_timeout, guard.object()?.query(sql, params)).await
        {
            Ok(Ok(rows)) => {
                self.client = Some(guard.release()?);
                self.state = TxStatus::Active;
                Ok(rows)
            }
            Ok(Err(error)) => {
                self.client = Some(guard.release()?);
                Err(map_tokio_error(error))
            }
            Err(error) => Err(PostgresError::Timeout(format!(
                "postgres 事务 query 超时；连接已丢弃: {error}"
            ))),
        }
    }

    /// 参数化查询（可选单行）。
    pub async fn query_opt(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> PostgresResult<Option<Row>> {
        let guard = self.take_guard()?;
        match tokio::time::timeout(
            self.operation_timeout,
            guard.object()?.query_opt(sql, params),
        )
        .await
        {
            Ok(Ok(row)) => {
                self.client = Some(guard.release()?);
                self.state = TxStatus::Active;
                Ok(row)
            }
            Ok(Err(error)) => {
                self.client = Some(guard.release()?);
                Err(map_tokio_error(error))
            }
            Err(error) => Err(PostgresError::Timeout(format!(
                "postgres 事务 query_opt 超时；连接已丢弃: {error}"
            ))),
        }
    }

    /// 提交事务。
    ///
    /// `COMMIT` 失败或超时时结果未知，连接一律脱池并返回 [`PostgresError::Connection`] /
    /// [`PostgresError::Timeout`]。
    pub async fn commit(mut self) -> PostgresResult<()> {
        self.ensure_active()?;
        let guard = self.take_guard()?;
        match tokio::time::timeout(
            self.operation_timeout,
            guard.object()?.batch_execute("COMMIT"),
        )
        .await
        {
            Ok(Ok(())) => {
                self.state = TxStatus::Committed;
                drop(guard.release()?);
                Ok(())
            }
            Ok(Err(error)) => Err(PostgresError::Connection(format!(
                "postgres COMMIT 失败且结果未知；连接已丢弃: {error}"
            ))),
            Err(error) => Err(PostgresError::Timeout(format!(
                "postgres COMMIT 超时且结果未知；连接已丢弃: {error}"
            ))),
        }
    }

    /// 回滚事务。
    pub async fn rollback(mut self) -> PostgresResult<()> {
        self.ensure_rollbackable()?;
        let guard = self.take_guard_after_validation()?;
        match tokio::time::timeout(
            self.operation_timeout,
            guard.object()?.batch_execute("ROLLBACK"),
        )
        .await
        {
            Ok(Ok(())) => {
                self.state = TxStatus::RolledBack;
                drop(guard.release()?);
                Ok(())
            }
            Ok(Err(error)) => Err(PostgresError::Connection(format!(
                "postgres ROLLBACK 失败；连接已丢弃: {error}"
            ))),
            Err(error) => Err(PostgresError::Timeout(format!(
                "postgres ROLLBACK 超时；连接已丢弃: {error}"
            ))),
        }
    }
}

impl Drop for PgTransaction {
    fn drop(&mut self) {
        if let Some(client) = self.client.take() {
            // Active / Failed 都可能仍持有 open/aborted 事务。Drop 不能监督异步回滚，
            // 因此永久脱离池并关闭 session，由服务端回滚。
            drop(Object::take(client));
        }
    }
}

impl std::fmt::Debug for PgTransaction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgTransaction")
            .field("status", &self.state)
            .field("operation_timeout", &self.operation_timeout)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_helpers() {
        assert!(TxStatus::Committed.is_finished());
        assert!(TxStatus::RolledBack.is_finished());
        assert!(!TxStatus::Active.is_finished());
        assert!(!TxStatus::Failed.is_finished());
        assert_eq!(TxStatus::Failed.as_str(), "failed");
    }
}
