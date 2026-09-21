//! 池化连接句柄 [`PgConnection`] 与 `COPY` 原语。
//!
//! # 安全
//!
//! 所有 SQL 入口都只接受参数化 `$N` 占位符与 [`ToSql`] 参数，
//! **禁止**把用户输入拼接进 SQL 字符串。多语句 SQL（simple query）只在
//! crate 内部的迁移脚本路径使用（见 [`crate::tx::PgTransaction`]），不对调用方公开。

use std::time::Duration;

use bytes::Bytes;
use deadpool_postgres::Object;
use futures_util::{SinkExt, StreamExt};
use tokio_postgres::types::ToSql;
use tokio_postgres::Row;

use crate::error::{map_tokio_error, PostgresError, PostgresResult};
use crate::guard::PooledObjectGuard;
use crate::tx::PgTransaction;

/// 单次 `COPY IN` 默认最大载荷（16 MiB）。
pub const DEFAULT_COPY_IN_MAX_BYTES: usize = 16 * 1024 * 1024;

/// 单次 `COPY OUT` 默认最大载荷（16 MiB）。
pub const DEFAULT_COPY_OUT_MAX_BYTES: usize = 16 * 1024 * 1024;

/// 从连接池借出的连接（归还由 drop 完成）。
pub struct PgConnection {
    pub(crate) client: Option<Object>,
    pub(crate) operation_timeout: Duration,
}

impl PgConnection {
    /// 包装 deadpool 对象。
    pub(crate) fn new(client: Object, operation_timeout: Duration) -> Self {
        Self {
            client: Some(client),
            operation_timeout,
        }
    }

    fn take_guard(&mut self) -> PostgresResult<PooledObjectGuard> {
        self.client
            .take()
            .map(PooledObjectGuard::new)
            .ok_or_else(|| PostgresError::Connection("postgres 连接已丢弃".to_string()))
    }

    /// 参数化 `EXECUTE`，返回影响行数。
    ///
    /// # 安全
    /// 调用方必须使用 `$1..$N` 占位符；禁止字符串拼接用户输入。
    pub async fn execute(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> PostgresResult<u64> {
        let guard = self.take_guard()?;
        match tokio::time::timeout(self.operation_timeout, guard.object()?.execute(sql, params))
            .await
        {
            Ok(result) => {
                self.client = Some(guard.release()?);
                result.map_err(map_tokio_error)
            }
            Err(error) => Err(PostgresError::Timeout(format!(
                "postgres execute 超时；连接已丢弃: {error}"
            ))),
        }
    }

    /// 参数化查询，期望恰好一行。
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
            Ok(result) => {
                self.client = Some(guard.release()?);
                result.map_err(map_tokio_error)
            }
            Err(error) => Err(PostgresError::Timeout(format!(
                "postgres query_one 超时；连接已丢弃: {error}"
            ))),
        }
    }

    /// 参数化查询，返回 0..N 行。
    pub async fn query(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> PostgresResult<Vec<Row>> {
        let guard = self.take_guard()?;
        match tokio::time::timeout(self.operation_timeout, guard.object()?.query(sql, params)).await
        {
            Ok(result) => {
                self.client = Some(guard.release()?);
                result.map_err(map_tokio_error)
            }
            Err(error) => Err(PostgresError::Timeout(format!(
                "postgres query 超时；连接已丢弃: {error}"
            ))),
        }
    }

    /// 可选单行：0 行 → `Ok(None)`，超过 1 行 → 错误。
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
            Ok(result) => {
                self.client = Some(guard.release()?);
                result.map_err(map_tokio_error)
            }
            Err(error) => Err(PostgresError::Timeout(format!(
                "postgres query_opt 超时；连接已丢弃: {error}"
            ))),
        }
    }

    /// `COPY ... FROM STDIN`：把 `data` 作为单块写入，返回影响行数。
    ///
    /// - `statement` 必须是完整 `COPY ... FROM STDIN ...` SQL（禁止拼接不可信标识符）；
    /// - 载荷上限 [`DEFAULT_COPY_IN_MAX_BYTES`]；超时或出错时连接脱池。
    pub async fn copy_in_bytes(&mut self, statement: &str, data: &[u8]) -> PostgresResult<u64> {
        if statement.trim().is_empty() {
            return Err(PostgresError::Config(
                "COPY IN statement 不能为空".to_string(),
            ));
        }
        if data.len() > DEFAULT_COPY_IN_MAX_BYTES {
            return Err(PostgresError::Config(format!(
                "COPY IN 载荷 {} 字节超过上限 {}",
                data.len(),
                DEFAULT_COPY_IN_MAX_BYTES
            )));
        }
        let guard = self.take_guard()?;
        let statement = statement.to_owned();
        let payload = Bytes::copy_from_slice(data);
        let future = async {
            let sink = guard
                .object()?
                .copy_in(&statement)
                .await
                .map_err(map_tokio_error)?;
            let mut sink = std::pin::pin!(sink);
            sink.send(payload).await.map_err(map_tokio_error)?;
            sink.finish().await.map_err(map_tokio_error)
        };
        match tokio::time::timeout(self.operation_timeout, future).await {
            Ok(Ok(rows)) => {
                self.client = Some(guard.release()?);
                Ok(rows)
            }
            Ok(Err(error)) => Err(error),
            Err(error) => Err(PostgresError::Timeout(format!(
                "postgres COPY IN 超时；连接已丢弃: {error}"
            ))),
        }
    }

    /// `COPY ... TO STDOUT`：聚合数据块，受 `max_bytes` 上限约束。
    ///
    /// - `max_bytes == 0` 时使用 [`DEFAULT_COPY_OUT_MAX_BYTES`]；
    /// - 超过上限返回配置错误并脱池（流可能未读完，连接不可复用）。
    pub async fn copy_out_bytes(
        &mut self,
        statement: &str,
        max_bytes: usize,
    ) -> PostgresResult<Vec<u8>> {
        if statement.trim().is_empty() {
            return Err(PostgresError::Config(
                "COPY OUT statement 不能为空".to_string(),
            ));
        }
        let limit = if max_bytes == 0 {
            DEFAULT_COPY_OUT_MAX_BYTES
        } else {
            max_bytes
        };
        let guard = self.take_guard()?;
        let statement = statement.to_owned();
        let future = async {
            let stream = guard
                .object()?
                .copy_out(&statement)
                .await
                .map_err(map_tokio_error)?;
            let mut stream = std::pin::pin!(stream);
            let mut out = Vec::new();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(map_tokio_error)?;
                if out.len().saturating_add(chunk.len()) > limit {
                    return Err(PostgresError::Config(format!(
                        "COPY OUT 聚合大小将超过上限 {limit} 字节"
                    )));
                }
                out.extend_from_slice(&chunk);
            }
            Ok(out)
        };
        match tokio::time::timeout(self.operation_timeout, future).await {
            Ok(Ok(bytes)) => {
                self.client = Some(guard.release()?);
                Ok(bytes)
            }
            Ok(Err(error)) => Err(error),
            Err(error) => Err(PostgresError::Timeout(format!(
                "postgres COPY OUT 超时；连接已丢弃: {error}"
            ))),
        }
    }

    /// 开启事务，消费本连接。
    pub async fn begin(mut self) -> PostgresResult<PgTransaction> {
        let client = self
            .client
            .take()
            .ok_or_else(|| PostgresError::Connection("postgres 连接已丢弃".to_string()))?;
        PgTransaction::begin(client, self.operation_timeout).await
    }
}

impl std::fmt::Debug for PgConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgConnection")
            .field("borrowed", &self.client.is_some())
            .field("operation_timeout", &self.operation_timeout)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_limits_are_stable() {
        const _: () = assert!(DEFAULT_COPY_IN_MAX_BYTES >= 1024 * 1024);
        const _: () = assert!(DEFAULT_COPY_OUT_MAX_BYTES >= 1024 * 1024);
        assert_eq!(DEFAULT_COPY_IN_MAX_BYTES, DEFAULT_COPY_OUT_MAX_BYTES);
    }
}
