//! 重试策略：指数退避 + 抖动 + 总预算（deadline）。
//!
//! 本模块是 crate 内独立实现，不依赖任何外部可靠性框架：
//!
//! - 只重试 [`PostgresError::is_retryable`] 为真的错误；其余错误立即返回；
//! - 退避 `delay = min(initial_delay * multiplier^(attempt-1), max_delay)`；
//! - 抖动对退避做 ±25% 抖动，避免集群内同步重试风暴（可用
//!   [`PgRetryConfig::without_jitter`] 关闭以获得确定性）；
//! - `deadline` 为整段重试的总预算，超出即返回 [`PostgresError::Timeout`]；
//! - 每次重试都打 `tracing::warn`，包含操作名、尝试次数与退避时长。

use std::future::Future;
use std::time::{Duration, Instant};

use crate::error::{PostgresError, PostgresResult};

/// 重试配置。
#[derive(Debug, Clone, PartialEq)]
pub struct PgRetryConfig {
    /// 最大尝试次数（含首次调用）。`0` 与 `1` 等价：只调用一次。
    pub max_attempts: u32,
    /// 首次退避时长。
    pub initial_delay: Duration,
    /// 退避上限。
    pub max_delay: Duration,
    /// 退避倍数（`1.0` 表示固定间隔）。
    pub multiplier: f64,
    /// 是否对退避施加 ±25% 抖动。
    pub jitter: bool,
    /// 整段重试的总预算（`None` 表示不限制）。
    pub deadline: Option<Duration>,
}

impl PgRetryConfig {
    /// 默认配置：3 次尝试、100ms 起、2.0 倍退避、上限 2s、抖动开、无总预算。
    #[must_use]
    pub fn new(max_attempts: u32) -> Self {
        Self {
            max_attempts,
            initial_delay: Duration::from_millis(100),
            max_delay: Duration::from_secs(2),
            multiplier: 2.0,
            jitter: true,
            deadline: None,
        }
    }

    /// 固定间隔重试（关闭抖动，便于确定性测试）。
    #[must_use]
    pub fn fixed(max_attempts: u32, delay: Duration) -> Self {
        Self {
            max_attempts,
            initial_delay: delay,
            max_delay: delay,
            multiplier: 1.0,
            jitter: false,
            deadline: None,
        }
    }

    /// 指数退避重试（关闭抖动）。
    #[must_use]
    pub fn exponential(max_attempts: u32, initial_delay: Duration, max_delay: Duration) -> Self {
        Self {
            initial_delay,
            max_delay,
            jitter: false,
            ..Self::new(max_attempts)
        }
    }

    /// 关闭抖动（确定性退避）。
    #[must_use]
    pub fn without_jitter(mut self) -> Self {
        self.jitter = false;
        self
    }

    /// 设置重试总预算。
    #[must_use]
    pub fn with_deadline(mut self, deadline: Duration) -> Self {
        self.deadline = Some(deadline);
        self
    }

    /// 第 `attempt` 次失败后的退避时长（`attempt` 从 1 开始）。
    ///
    /// 纯函数：同参数下抖动关闭时结果确定。
    #[must_use]
    pub fn delay_for_attempt(&self, attempt: u32) -> Duration {
        let exponent = attempt.saturating_sub(1);
        let mut delay = if self.multiplier <= 1.0 {
            self.initial_delay
        } else {
            let factor = self.multiplier.powi(exponent.min(32) as i32);
            self.initial_delay.mul_f64(factor)
        };
        if delay > self.max_delay {
            delay = self.max_delay;
        }
        if self.jitter && !delay.is_zero() {
            // 0.75 ~ 1.25 倍：抖动幅度 ±25%
            let factor = 750 + (jitter_seed() % 501) as u32;
            delay = delay.mul_f64(f64::from(factor) / 1000.0);
        }
        delay
    }
}

impl Default for PgRetryConfig {
    fn default() -> Self {
        Self::new(3)
    }
}

/// 每次调用返回一个新的伪随机种子（时间 + 单调计数器，无需额外依赖）。
fn jitter_seed() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos() as u64);
    let mut state = nanos ^ counter.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    state ^= state << 13;
    state ^= state >> 7;
    state ^= state << 17;
    state
}

/// 本轮是否继续重试的决策。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetryPlan {
    /// 再试一次，先等待给定时长。
    Retry(Duration),
    /// 放弃（不可重试或尝试次数已耗尽），返回最后一个错误。
    Stop,
    /// 总预算耗尽。
    DeadlineExceeded,
}

fn plan_next(
    config: &PgRetryConfig,
    error: &PostgresError,
    attempt: u32,
    elapsed: Duration,
    honor_delay: bool,
) -> RetryPlan {
    if !error.is_retryable() || attempt >= config.max_attempts {
        return RetryPlan::Stop;
    }
    let delay = if honor_delay {
        config.delay_for_attempt(attempt)
    } else {
        Duration::ZERO
    };
    if let Some(deadline) = config.deadline {
        if elapsed.saturating_add(delay) >= deadline {
            return RetryPlan::DeadlineExceeded;
        }
    }
    RetryPlan::Retry(delay)
}

fn deadline_error(op: &str, attempts: u32, config: &PgRetryConfig) -> PostgresError {
    PostgresError::Timeout(format!(
        "{op} 重试超出总预算（尝试 {attempts} 次，预算 {:?}）",
        config.deadline
    ))
}

/// 同步重试，按 [`PgRetryConfig`] 指数退避。
///
/// `f` 至少被调用一次；不可重试的错误立即返回。
///
/// # 阻塞语义
///
/// 本函数在重试等待期间调用 [`std::thread::sleep`]，**会阻塞当前线程**。
/// 仅适用于启动阶段、同步工具上下文或已确认不在异步运行时工作线程上执行的场景。
///
/// # 误用风险
///
/// 在 tokio 多线程 runtime 的工作线程上调用本函数会导致该线程被长时间占用，
/// 极端情况下可能引发工作线程饥饿。异步上下文中应使用 [`with_retry_async`]
/// （其退避走 [`tokio::time::sleep`]，不阻塞工作线程）。
///
/// # 典型调用场景
///
/// - 应用启动阶段的同步配置校验/连接预检；
/// - 同步测试辅助函数（`tests/pure_functions.rs` 中的离线重试逻辑判定）；
/// - 非异步工具脚本。
///
/// 若不确定当前上下文是否有异步运行时，优先使用 [`with_retry_async`]。
pub fn with_retry_sync<T, F>(config: &PgRetryConfig, op: &str, mut f: F) -> PostgresResult<T>
where
    F: FnMut() -> PostgresResult<T>,
{
    let started = Instant::now();
    let mut attempt = 0_u32;
    loop {
        attempt = attempt.saturating_add(1);
        let error = match f() {
            Ok(value) => return Ok(value),
            Err(error) => error,
        };
        match plan_next(config, &error, attempt, started.elapsed(), true) {
            RetryPlan::Retry(delay) => {
                tracing::warn!(
                    op,
                    attempt,
                    delay_ms = delay.as_millis(),
                    error = %error,
                    "postgres 操作失败，准备重试"
                );
                std::thread::sleep(delay);
            }
            RetryPlan::Stop => return Err(error),
            RetryPlan::DeadlineExceeded => {
                return Err(deadline_error(op, attempt, config));
            }
        }
    }
}

/// 异步重试，退避使用 `tokio::time::sleep`。
pub async fn with_retry_async<T, F, Fut>(
    config: &PgRetryConfig,
    op: &str,
    mut f: F,
) -> PostgresResult<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = PostgresResult<T>> + Send,
{
    let started = Instant::now();
    let mut attempt = 0_u32;
    loop {
        attempt = attempt.saturating_add(1);
        let error = match f().await {
            Ok(value) => return Ok(value),
            Err(error) => error,
        };
        match plan_next(config, &error, attempt, started.elapsed(), true) {
            RetryPlan::Retry(delay) => {
                tracing::warn!(
                    op,
                    attempt,
                    delay_ms = delay.as_millis(),
                    error = %error,
                    "postgres 操作失败，准备重试"
                );
                tokio::time::sleep(delay).await;
            }
            RetryPlan::Stop => return Err(error),
            RetryPlan::DeadlineExceeded => {
                return Err(deadline_error(op, attempt, config));
            }
        }
    }
}

/// 异步重试，**不等待**退避（立即重试）。
///
/// 适用于幂等且期望快速收敛的场景（如本地连接抖动）；总预算仍然生效。
pub async fn with_retry_async_no_wait<T, F, Fut>(
    config: &PgRetryConfig,
    op: &str,
    mut f: F,
) -> PostgresResult<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = PostgresResult<T>> + Send,
{
    let started = Instant::now();
    let mut attempt = 0_u32;
    loop {
        attempt = attempt.saturating_add(1);
        let error = match f().await {
            Ok(value) => return Ok(value),
            Err(error) => error,
        };
        match plan_next(config, &error, attempt, started.elapsed(), false) {
            RetryPlan::Retry(_) => continue,
            RetryPlan::Stop => return Err(error),
            RetryPlan::DeadlineExceeded => {
                return Err(deadline_error(op, attempt, config));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn transient() -> PostgresError {
        PostgresError::Connection("连接重置".to_string())
    }

    #[test]
    fn fixed_delay_is_deterministic() {
        let config = PgRetryConfig::fixed(3, Duration::from_millis(50));
        assert_eq!(config.delay_for_attempt(1), Duration::from_millis(50));
        assert_eq!(config.delay_for_attempt(9), Duration::from_millis(50));
    }

    #[test]
    fn exponential_delay_is_capped() {
        let config =
            PgRetryConfig::exponential(5, Duration::from_millis(100), Duration::from_millis(250));
        assert_eq!(config.delay_for_attempt(1), Duration::from_millis(100));
        assert_eq!(config.delay_for_attempt(2), Duration::from_millis(200));
        assert_eq!(config.delay_for_attempt(3), Duration::from_millis(250));
        assert_eq!(config.delay_for_attempt(10), Duration::from_millis(250));
    }

    #[test]
    fn jitter_stays_within_bounds() {
        let config = PgRetryConfig::fixed(3, Duration::from_millis(1000));
        for attempt in 1..=16 {
            let delay = config.delay_for_attempt(attempt);
            assert!(
                delay >= Duration::from_millis(750) && delay <= Duration::from_millis(1250),
                "抖动越界: {delay:?}"
            );
        }
    }

    #[test]
    fn sync_retry_stops_on_non_retryable() {
        let config = PgRetryConfig::fixed(5, Duration::ZERO);
        let calls = AtomicU32::new(0);
        let error = with_retry_sync(&config, "pg.query", || {
            calls.fetch_add(1, Ordering::SeqCst);
            Err::<(), _>(PostgresError::Conflict("重复键".to_string()))
        })
        .expect_err("不可重试错误必须直接返回");
        assert!(matches!(error, PostgresError::Conflict(_)));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn sync_retry_recovers() {
        let config = PgRetryConfig::fixed(3, Duration::ZERO);
        let calls = AtomicU32::new(0);
        let value = with_retry_sync(&config, "pg.query", || {
            let attempt = calls.fetch_add(1, Ordering::SeqCst) + 1;
            if attempt < 3 {
                Err(transient())
            } else {
                Ok(attempt)
            }
        })
        .expect("第三次应成功");
        assert_eq!(value, 3);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn sync_retry_exhausts_attempts() {
        let config = PgRetryConfig::fixed(2, Duration::ZERO);
        let calls = AtomicU32::new(0);
        with_retry_sync(&config, "pg.query", || {
            calls.fetch_add(1, Ordering::SeqCst);
            Err::<(), _>(transient())
        })
        .expect_err("尝试次数耗尽");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn sync_retry_respects_deadline() {
        let config = PgRetryConfig::fixed(10, Duration::from_millis(20))
            .with_deadline(Duration::from_millis(5));
        let error = with_retry_sync(&config, "pg.query", || Err::<(), _>(transient()))
            .expect_err("总预算耗尽");
        assert!(matches!(error, PostgresError::Timeout(_)));
    }

    #[tokio::test]
    async fn async_retry_no_wait_converges() {
        let config = PgRetryConfig::fixed(5, Duration::from_secs(30));
        let calls = AtomicU32::new(0);
        let value = with_retry_async_no_wait(&config, "pg.execute", || {
            let attempt = calls.fetch_add(1, Ordering::SeqCst) + 1;
            async move {
                if attempt == 1 {
                    Err(transient())
                } else {
                    Ok(attempt)
                }
            }
        })
        .await
        .expect("立即重试应成功");
        assert_eq!(value, 2);
    }

    #[tokio::test]
    async fn async_retry_waits_and_recovers() {
        let config = PgRetryConfig::fixed(3, Duration::from_millis(1));
        let calls = AtomicU32::new(0);
        let value = with_retry_async(&config, "pg.execute", || {
            let attempt = calls.fetch_add(1, Ordering::SeqCst) + 1;
            async move {
                if attempt < 2 {
                    Err(transient())
                } else {
                    Ok(attempt)
                }
            }
        })
        .await
        .expect("退避重试应成功");
        assert_eq!(value, 2);
    }
}
