//! 错误类型、SQLSTATE 分类映射与结果别名。
//!
//! 本模块提供两层错误表示：
//!
//! 1. [`PostgresError`]：对外统一错误类型，9 个稳定分类覆盖配置、连接、后端、
//!    序列化、I/O、超时、能力不足、冲突与目标缺失；
//! 2. [`ErrorKind`]：由 SQLSTATE 推出的**语义分类**，用于判断「值不值得重试」，
//!    并映射回 [`PostgresError`] 的分类。
//!
//! # SQLSTATE 映射锚点
//!
//! | SQLSTATE | 含义 | [`ErrorKind`] | [`PostgresError`] | 可重试 |
//! |----------|------|---------------|-------------------|--------|
//! | `23505` | unique_violation | [`ErrorKind::Conflict`] | [`PostgresError::Conflict`] | 否 |
//! | `23503` / `23502` / `23514` | FK / not-null / check | [`ErrorKind::Invalid`] | [`PostgresError::Backend`] | 否 |
//! | `40001` / `40P01` | serialization_failure / deadlock | [`ErrorKind::Serialization`] | [`PostgresError::Serialization`] | 是 |
//! | `42P01` | undefined_table | [`ErrorKind::Missing`] | [`PostgresError::Missing`] | 否 |
//! | `42704` | undefined_object | [`ErrorKind::Missing`] | [`PostgresError::Missing`] | 否 |
//! | `08*` | connection_exception | [`ErrorKind::Unavailable`] | [`PostgresError::Connection`] | 是 |
//! | `57014` | query_canceled | [`ErrorKind::Cancelled`] | [`PostgresError::Backend`] | 否 |
//! | `57P01` / `57P02` / `57P03` / `58*` | 运维干预 / 系统错误 | [`ErrorKind::Unavailable`] | [`PostgresError::Connection`] | 是 |
//! | `53300` / `55P03` | 资源/锁暂时不可用 | [`ErrorKind::Transient`] | [`PostgresError::Connection`] | 是 |
//! | `25*` | invalid_transaction_state | [`ErrorKind::Invariant`] | [`PostgresError::Backend`] | 否 |
//! | 其余 | 未知 | [`ErrorKind::Internal`] | [`PostgresError::Backend`] | 否 |
//!
//! 未识别的 SQLSTATE 一律回落为 [`ErrorKind::Internal`]，绝不猜测为可重试。

use std::fmt;

/// crate 专用 `Result` 别名。
pub type PostgresResult<T> = Result<T, PostgresError>;

/// postgresx 错误类型。
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PostgresError {
    /// 配置非法或缺少必需配置项。
    #[error("配置无效: {0}")]
    Config(String),
    /// 连接建立、借用或维护失败（含池关闭、连接类 SQLSTATE）。
    #[error("连接失败: {0}")]
    Connection(String),
    /// 远端返回业务/协议错误（含约束违反、语法错误、事务状态非法）。
    #[error("远端返回错误: {0}")]
    Backend(String),
    /// 序列化冲突（SQLSTATE `40001` / `40P01`），可安全重试。
    #[error("序列化失败: {0}")]
    Serialization(String),
    /// 本地 I/O 失败。
    #[error("I/O 失败: {0}")]
    Io(#[from] std::io::Error),
    /// 操作超时（调用侧 deadline、acquire 等待或重试总预算耗尽）。
    #[error("操作超时: {0}")]
    Timeout(String),
    /// 当前能力不支持。
    #[error("不支持的操作: {0}")]
    Unsupported(String),
    /// 唯一约束等非瞬时冲突（重试无意义，需要修正数据或调用方逻辑）。
    #[error("冲突: {0}")]
    Conflict(String),
    /// 目标对象（表、视图、对象）不存在。
    #[error("目标不存在: {0}")]
    Missing(String),
}

impl PostgresError {
    /// 是否属于可安全重试的瞬时错误。
    ///
    /// 仅 [`PostgresError::Connection`]、[`PostgresError::Serialization`]、
    /// [`PostgresError::Timeout`]、[`PostgresError::Io`] 返回 `true`：
    /// 它们分别对应 SQLSTATE `08*` / `57P0*` / `58*` / `53*` / `55P03`、
    /// `40001` / `40P01`，以及本地超时与 I/O 故障。
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::Connection(_) | Self::Serialization(_) | Self::Timeout(_) | Self::Io(_)
        )
    }

    /// 保持错误分类不变，替换错误消息（用于附加 rollback 失败等上下文）。
    pub(crate) fn with_message(self, message: String) -> Self {
        match self {
            Self::Config(_) => Self::Config(message),
            Self::Connection(_) => Self::Connection(message),
            Self::Backend(_) => Self::Backend(message),
            Self::Serialization(_) => Self::Serialization(message),
            Self::Io(err) => Self::Connection(format!("{message}: {err}")),
            Self::Timeout(_) => Self::Timeout(message),
            Self::Unsupported(_) => Self::Unsupported(message),
            Self::Conflict(_) => Self::Conflict(message),
            Self::Missing(_) => Self::Missing(message),
        }
    }

    /// 在保留原始分类的前提下追加人类可读上下文。
    pub(crate) fn context_message(self, context: &str) -> Self {
        let message = format!("{context}: {}", self.message());
        self.with_message(message)
    }

    /// 当前错误的消息正文（不含分类前缀）。
    fn message(&self) -> String {
        match self {
            Self::Config(m)
            | Self::Connection(m)
            | Self::Backend(m)
            | Self::Serialization(m)
            | Self::Timeout(m)
            | Self::Unsupported(m)
            | Self::Conflict(m)
            | Self::Missing(m) => m.clone(),
            Self::Io(err) => err.to_string(),
        }
    }
}

/// SQLSTATE 的语义分类。
///
/// 与 [`PostgresError`] 分离，因为「重试判断」需要比对外错误分类更细的粒度：
/// 例如唯一键冲突（[`Self::Conflict`]）与序列化失败（[`Self::Serialization`]）
/// 在 SQLSTATE 层是同类事故，但只有后者值得重试。
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorKind {
    /// 输入/约束非法（`22*`、`23*` 除唯一键、`28*`、`3D000`、`42*` 除缺失对象、`P0001`）。
    Invalid,
    /// 目标对象不存在（`42P01`、`42704`）。
    Missing,
    /// 唯一约束冲突等非瞬时冲突（`23505`、`23*` 兜底）。
    Conflict,
    /// 序列化失败 / 死锁（`40001`、`40P01`），可重试。
    Serialization,
    /// 资源或锁暂时不可用（`53*`、`55P03`），可重试。
    Transient,
    /// 连接类故障（`08*`、`57P0*`、`58*`），可重试。
    Unavailable,
    /// 查询被取消（`57014`）。
    Cancelled,
    /// 调用侧或重试预算超时。
    DeadlineExceeded,
    /// 事务状态非法（`25*`），属于调用时序错误。
    Invariant,
    /// 未知或服务端内部错误（`XX*` 及其它未识别码）。
    Internal,
}

impl ErrorKind {
    /// 稳定的短名称（用于日志与错误消息）。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Invalid => "invalid",
            Self::Missing => "missing",
            Self::Conflict => "conflict",
            Self::Serialization => "serialization",
            Self::Transient => "transient",
            Self::Unavailable => "unavailable",
            Self::Cancelled => "cancelled",
            Self::DeadlineExceeded => "deadline_exceeded",
            Self::Invariant => "invariant",
            Self::Internal => "internal",
        }
    }

    /// 该分类是否值得重试。
    #[must_use]
    pub const fn is_retryable(self) -> bool {
        matches!(
            self,
            Self::Serialization | Self::Transient | Self::Unavailable | Self::DeadlineExceeded
        )
    }

    /// 映射为对外错误类型。
    #[must_use]
    pub fn into_postgres_error(self, message: String) -> PostgresError {
        match self {
            Self::Invalid | Self::Cancelled | Self::Invariant | Self::Internal => {
                PostgresError::Backend(message)
            }
            Self::Missing => PostgresError::Missing(message),
            Self::Conflict => PostgresError::Conflict(message),
            Self::Serialization => PostgresError::Serialization(message),
            Self::Transient | Self::Unavailable => PostgresError::Connection(message),
            Self::DeadlineExceeded => PostgresError::Timeout(message),
        }
    }
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 将 SQLSTATE 五字符码映射为 [`ErrorKind`]。
///
/// 仅依赖码本身（纯函数，便于离线测试）；消息上下文由调用方附加。
#[must_use]
pub fn error_kind_from_sqlstate(code: &str) -> ErrorKind {
    match code {
        // Class 08 — Connection Exception
        c if c.starts_with("08") => ErrorKind::Unavailable,

        // Class 28 — Invalid Authorization Specification
        "28P01" | "28000" => ErrorKind::Invalid,

        // Class 3D — Invalid Catalog Name
        "3D000" => ErrorKind::Invalid,

        // Class 22 — Data Exception
        c if c.starts_with("22") => ErrorKind::Invalid,

        // Class 23 — Integrity Constraint Violation
        "23505" => ErrorKind::Conflict, // unique_violation
        "23503" => ErrorKind::Invalid,  // foreign_key_violation
        "23502" => ErrorKind::Invalid,  // not_null_violation
        "23514" => ErrorKind::Invalid,  // check_violation
        c if c.starts_with("23") => ErrorKind::Conflict,

        // Class 25 — Invalid Transaction State
        c if c.starts_with("25") => ErrorKind::Invariant,

        "40P01" => ErrorKind::Serialization, // deadlock_detected
        "40001" => ErrorKind::Serialization, // serialization_failure
        c if c.starts_with("40") => ErrorKind::Serialization,

        // Class 42 — Syntax Error or Access Rule Violation
        "42P01" => ErrorKind::Missing, // undefined_table
        "42703" => ErrorKind::Invalid, // undefined_column
        "42704" => ErrorKind::Missing, // undefined_object
        "42601" => ErrorKind::Invalid, // syntax_error
        "42501" => ErrorKind::Invalid, // insufficient_privilege
        c if c.starts_with("42") => ErrorKind::Invalid,

        // Class 53 — Insufficient Resources
        "53300" => ErrorKind::Transient, // too_many_connections
        "53200" => ErrorKind::Transient, // out_of_memory
        c if c.starts_with("53") => ErrorKind::Transient,

        // Class 55 — Object Not In Prerequisite State
        "55P03" => ErrorKind::Transient, // lock_not_available
        c if c.starts_with("55") => ErrorKind::Transient,

        // Class 57 — Operator Intervention
        "57014" => ErrorKind::Cancelled, // query_canceled
        "57P01" | "57P02" | "57P03" => ErrorKind::Unavailable,
        c if c.starts_with("57") => ErrorKind::Unavailable,

        // Class 58 — System Error
        c if c.starts_with("58") => ErrorKind::Unavailable,

        // Class XX — Internal Error
        c if c.starts_with("XX") => ErrorKind::Internal,

        // P0001 raise_exception
        "P0001" => ErrorKind::Invalid,

        _ => ErrorKind::Internal,
    }
}

/// 由 SQLSTATE 构造带上下文的 [`PostgresError`]。
///
/// 消息统一带 `postgres sqlstate=<code>` 前缀，便于日志检索。
///
/// # Examples
///
/// ```
/// use postgresx::error_from_sqlstate;
///
/// // 23505 = unique_violation（Class 23 → 参数/约束类错误）
/// let error = error_from_sqlstate("23505", "duplicate key value violates unique constraint");
/// assert!(error.to_string().contains("postgres sqlstate=23505"));
/// ```
#[must_use]
pub fn error_from_sqlstate(code: &str, message: impl Into<String>) -> PostgresError {
    let kind = error_kind_from_sqlstate(code);
    let context = format!("postgres sqlstate={code}: {}", message.into());
    kind.into_postgres_error(context)
}

/// 映射 `tokio_postgres::Error`。
#[must_use]
pub fn map_tokio_error(err: tokio_postgres::Error) -> PostgresError {
    if err.is_closed() {
        return PostgresError::Connection(format!("postgres 连接已关闭: {err}"));
    }
    if let Some(db) = err.as_db_error() {
        return error_from_sqlstate(db.code().code(), db.message());
    }
    let text = err.to_string();
    if text.contains("timed out") || text.contains("timeout") {
        return PostgresError::Timeout(format!("postgres: {text}"));
    }
    if text.contains("connect") || text.contains("Connection") || text.contains("connection") {
        return PostgresError::Connection(format!("postgres: {text}"));
    }
    PostgresError::Backend(format!("postgres: {text}"))
}

/// 映射 deadpool 取连接错误。
#[must_use]
pub fn map_pool_error(err: deadpool_postgres::PoolError) -> PostgresError {
    match err {
        deadpool_postgres::PoolError::Timeout(_) => {
            PostgresError::Timeout("postgres 连接池等待超时".to_string())
        }
        deadpool_postgres::PoolError::Backend(inner) => map_tokio_error(inner),
        deadpool_postgres::PoolError::Closed => {
            PostgresError::Connection("postgres 连接池已关闭".to_string())
        }
        deadpool_postgres::PoolError::NoRuntimeSpecified => PostgresError::Backend(
            "postgres 连接池未配置异步 runtime（需要 rt_tokio_1）".to_string(),
        ),
        deadpool_postgres::PoolError::PostCreateHook(inner) => {
            PostgresError::Connection(format!("postgres 连接创建后钩子失败: {inner}"))
        }
    }
}

/// 映射建池错误。
pub(crate) fn map_create_pool_error(err: deadpool_postgres::CreatePoolError) -> PostgresError {
    PostgresError::Connection(format!("postgres 创建连接池失败: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqlstate_unique_violation_is_conflict() {
        assert_eq!(error_kind_from_sqlstate("23505"), ErrorKind::Conflict);
        assert!(!error_kind_from_sqlstate("23505").is_retryable());
        assert!(!error_from_sqlstate("23505", "duplicate key").is_retryable());
    }

    #[test]
    fn sqlstate_fk_is_invalid_backend() {
        assert_eq!(error_kind_from_sqlstate("23503"), ErrorKind::Invalid);
        assert!(matches!(
            error_from_sqlstate("23503", "fk"),
            PostgresError::Backend(_)
        ));
    }

    #[test]
    fn sqlstate_undefined_table_is_missing() {
        assert_eq!(error_kind_from_sqlstate("42P01"), ErrorKind::Missing);
        assert!(matches!(
            error_from_sqlstate("42P01", "missing"),
            PostgresError::Missing(_)
        ));
    }

    #[test]
    fn sqlstate_serialization_is_retryable() {
        for code in ["40001", "40P01"] {
            assert_eq!(error_kind_from_sqlstate(code), ErrorKind::Serialization);
            assert!(error_from_sqlstate(code, "retry me").is_retryable());
        }
    }

    #[test]
    fn sqlstate_connection_class_is_retryable() {
        for code in ["08000", "08006", "57P01", "53300", "55P03"] {
            assert!(
                error_from_sqlstate(code, "unavailable").is_retryable(),
                "{code}"
            );
        }
    }

    #[test]
    fn sqlstate_query_canceled_is_not_retryable() {
        assert_eq!(error_kind_from_sqlstate("57014"), ErrorKind::Cancelled);
        assert!(!error_from_sqlstate("57014", "canceled").is_retryable());
    }

    #[test]
    fn sqlstate_unknown_is_internal() {
        assert_eq!(error_kind_from_sqlstate("99999"), ErrorKind::Internal);
        assert_eq!(error_kind_from_sqlstate(""), ErrorKind::Internal);
    }

    #[test]
    fn error_message_carries_sqlstate() {
        let err = error_from_sqlstate("23505", "duplicate key");
        assert!(err.to_string().contains("23505"));
    }

    #[test]
    fn context_message_preserves_variant() {
        let err = error_from_sqlstate("40001", "serialization").context_message("事务回滚也失败");
        assert!(matches!(err, PostgresError::Serialization(_)));
        assert!(err.to_string().contains("事务回滚也失败"));
        assert!(err.is_retryable());
    }

    #[test]
    fn error_kind_display_is_stable() {
        assert_eq!(ErrorKind::Serialization.to_string(), "serialization");
        assert_eq!(ErrorKind::DeadlineExceeded.as_str(), "deadline_exceeded");
    }
}
