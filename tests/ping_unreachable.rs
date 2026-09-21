//! 不可达地址：`ping` / `health_check` / `connect` 必须返回 `Err` 而非 panic 或挂起。

use std::time::Duration;

use postgresx::{PostgresConfig, PostgresError, PostgresPool, SslMode};

/// 必然被拒绝的地址（`127.0.0.1:1`），短超时避免测试拖长。
fn unreachable_config() -> PostgresConfig {
    PostgresConfig::builder()
        .host("127.0.0.1")
        .port(1)
        .database("postgres")
        .user("postgres")
        .sslmode(SslMode::Disable)
        .connect_timeout(Duration::from_millis(300))
        .acquire_timeout(Duration::from_millis(500))
        .operation_timeout(Duration::from_millis(500))
        .max_pool_size(2)
        .build()
        .expect("配置合法")
}

#[tokio::test]
async fn ping_unreachable_address_returns_error() {
    let pool = PostgresPool::new(unreachable_config()).expect("建池不联网");
    let outcome = tokio::time::timeout(Duration::from_secs(5), pool.ping()).await;
    match outcome {
        Ok(Err(error)) => {
            assert!(error.is_retryable(), "连接类失败应可重试: {error}");
            assert!(
                matches!(
                    error,
                    PostgresError::Connection(_) | PostgresError::Timeout(_)
                ),
                "分类应为连接失败或超时: {error}"
            );
        }
        Ok(Ok(())) => panic!("不可达地址不应 ping 成功"),
        Err(_) => panic!("ping 必须受内部截止时间约束"),
    }
}

#[tokio::test]
async fn health_check_unreachable_address_returns_error() {
    let pool = PostgresPool::new(unreachable_config()).expect("建池不联网");
    let outcome = tokio::time::timeout(Duration::from_secs(5), pool.health_check()).await;
    match outcome {
        Ok(Err(error)) => assert!(error.is_retryable(), "健康检查失败应可重试: {error}"),
        Ok(Ok(health)) => panic!("不可达地址不应返回健康结果: {health:?}"),
        Err(_) => panic!("health_check 必须受内部截止时间约束"),
    }
}

#[tokio::test]
async fn connect_unreachable_address_returns_error() {
    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        PostgresPool::connect(unreachable_config()),
    )
    .await;
    match outcome {
        Ok(Err(error)) => assert!(error.is_retryable(), "connect 失败应可重试: {error}"),
        Ok(Ok(_)) => panic!("不可达地址不应连接成功"),
        Err(_) => panic!("connect 必须受内部截止时间约束"),
    }
}

#[tokio::test]
async fn execute_on_unreachable_pool_returns_error() {
    let pool = PostgresPool::new(unreachable_config()).expect("建池不联网");
    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        pool.query_one("SELECT $1::BIGINT", &[&1_i64]),
    )
    .await;
    match outcome {
        Ok(Err(error)) => assert!(error.is_retryable(), "查询失败应可重试: {error}"),
        Ok(Ok(row)) => panic!("不可达地址不应返回行: {row:?}"),
        Err(_) => panic!("query_one 必须受内部截止时间约束"),
    }
}
