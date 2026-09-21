//! 池对象取消守卫 [`PooledObjectGuard`]。
//!
//! 连接句柄（[`crate::conn::PgConnection`]）与事务句柄（[`crate::tx::PgTransaction`]）
//! 共用这一原语，因此它**不依赖它们中任何一个**——依赖方向为
//! `conn -> guard`、`tx -> guard`，从而不构成模块环。

use deadpool_postgres::Object;

use crate::error::{PostgresError, PostgresResult};

/// 池对象取消守卫。
///
/// 只有异步操作**明确**完成并调用 [`PooledObjectGuard::release`] 才会归还连接。
/// 外层 timeout、任务 abort 或 future 被 drop 时，`Drop` 会把连接从池中分离，
/// 因此未知状态的连接（可能仍处于事务中）不会污染下一个借用者。
pub(crate) struct PooledObjectGuard {
    object: Option<Object>,
}

impl PooledObjectGuard {
    pub(crate) fn new(object: Object) -> Self {
        Self {
            object: Some(object),
        }
    }

    pub(crate) fn object(&self) -> PostgresResult<&Object> {
        self.object
            .as_ref()
            .ok_or_else(|| PostgresError::Backend("postgres 连接守卫为空".to_string()))
    }

    pub(crate) fn release(mut self) -> PostgresResult<Object> {
        self.object
            .take()
            .ok_or_else(|| PostgresError::Backend("postgres 连接守卫重复释放".to_string()))
    }
}

impl Drop for PooledObjectGuard {
    fn drop(&mut self) {
        if let Some(object) = self.object.take() {
            drop(Object::take(object));
        }
    }
}
