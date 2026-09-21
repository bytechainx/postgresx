#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable
)]
//! live 真连服测试（特性 002，postgresx）。
//!
//! 全部用例 `#[ignore]`，默认不跑（CI 行为不变）。本地显式运行：
//!
//! ```bash
//! set -a; source /home/workspace/sre/secrets/env/postgresx.env; set +a
//! cd /home/workspace/bytechainx/postgresx
//! CARGO_TARGET_DIR=/home/workspace/bytechainx/.cargo-target \
//!   cargo test --test live_postgres -- --ignored --test-threads=1
//! ```
//!
//! **凭据只从环境变量读取，绝不硬编码**；失败信息只报所需变量前缀，不回显取值。
//! postgresx 没有 `connect_from_env`，故用 [`PostgresConfig::from_env`] + [`PostgresPool::connect`]。

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use postgresx::{PostgresConfig, PostgresPool};

/// live 用例整体超时上限（含建连、往返与清理）。
const LIVE_TIMEOUT: Duration = Duration::from_secs(60);

/// 进程内唯一后缀：`进程号 + 纳秒时间戳`，避免并发/重复运行的命名冲突。
fn unique_suffix() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|delta| delta.as_nanos())
        .unwrap_or(0);
    format!("{}_{}", std::process::id(), nanos)
}

/// 从环境变量加载配置并建池（会做 `SELECT 1` 冒烟）。
async fn connect_pool() -> PostgresPool {
    let config = PostgresConfig::from_env().unwrap_or_else(|error| {
        panic!(
            "live 测试需要 FOUNDATIONX_POSTGRESX_HOST/DATABASE/USER（及可选 PORT/PASSWORD/SSLMODE）: {error}"
        )
    });
    PostgresPool::connect(config)
        .await
        .unwrap_or_else(|error| panic!("建池失败，请确认 live 服务可达: {error}"))
}

/// 建连 + 结构化探活 + close 收尾。
#[tokio::test]
#[ignore = "需要真实 PostgreSQL 实例与 FOUNDATIONX_POSTGRESX_* 环境变量"]
async fn live_connect_ping_health_and_close() {
    tokio::time::timeout(LIVE_TIMEOUT, async {
        let pool = connect_pool().await;

        // 建连成功断言（E2）：池可用且摘要含端点信息（不含密码）。
        assert!(!pool.summary().is_empty(), "池摘要不得为空");

        // 探活（E3）：ping 成功。
        pool.ping().await.expect("ping 应成功");

        // 结构化健康检查（E3）：服务端版本、延迟与池快照。
        let health = pool.health_check().await.expect("health_check 应成功");
        assert!(!health.server_version.is_empty(), "server_version 不得为空");
        assert!(
            health.server_version.chars().any(|c| c.is_ascii_digit()),
            "server_version 应含数字: {}",
            health.server_version
        );
        assert!(health.pool.max_size >= 1, "池上限应 >= 1");
        assert!(
            health.latency <= LIVE_TIMEOUT,
            "健康检查延迟异常: {:?}",
            health.latency
        );

        // close 收尾（E5）：关闭后 ping 必须失败。
        pool.close();
        assert!(pool.stats().closed, "关闭后统计应标记 closed");
        assert!(pool.ping().await.is_err(), "关闭后 ping 必须失败");
    })
    .await
    .expect("live 用例不得超时");
}

/// 唯一名临时表的数据面往返 + 强制清理：insert → select → drop → 断言表已不存在。
#[tokio::test]
#[ignore = "需要真实 PostgreSQL 实例与 FOUNDATIONX_POSTGRESX_* 环境变量"]
async fn live_unique_table_roundtrip_and_cleanup() {
    tokio::time::timeout(LIVE_TIMEOUT, async {
        let pool = connect_pool().await;
        // 唯一化表名（E4）：加进程号 + 纳秒时间戳，绝不触碰既有业务表。
        let table = format!("pgx_live_{}", unique_suffix());

        let outcome = async {
            let create =
                format!("CREATE TABLE {table} (id BIGINT PRIMARY KEY, name TEXT NOT NULL)");
            let insert = format!("INSERT INTO {table} (id, name) VALUES ($1, $2)");
            let select = format!("SELECT name FROM {table} WHERE id = $1");

            pool.execute(&create, &[]).await?;
            let affected = pool.execute(&insert, &[&1_i64, &"live-value"]).await?;
            assert_eq!(affected, 1, "insert 应影响 1 行");

            let row = pool.query_one(&select, &[&1_i64]).await?;
            let name: String = row.get(0);
            assert_eq!(name, "live-value", "数据面往返值应一致");

            // 事务路径同样落在真实服务上。
            pool.with_transaction(|tx| {
                Box::pin(async move {
                    tx.execute(&insert, &[&2_i64, &"tx-value"]).await?;
                    Ok::<_, postgresx::PostgresError>(())
                })
            })
            .await?;
            let count = pool
                .query_one(&format!("SELECT count(*) FROM {table}"), &[])
                .await?;
            let total: i64 = count.get(0);
            assert_eq!(total, 2, "事务提交后应有 2 行");
            Ok::<(), postgresx::PostgresError>(())
        }
        .await;

        // 无论往返成败都强制清理（E4）。
        let drop_sql = format!("DROP TABLE IF EXISTS {table}");
        let cleaned = pool.execute(&drop_sql, &[]).await;

        outcome.expect("数据面往返应成功");
        cleaned.expect("临时表清理应成功");

        // 断言清理生效：表不存在后查询应报 Missing。
        let after = pool.query(&format!("SELECT * FROM {table}"), &[]).await;
        assert!(after.is_err(), "清理后表不应仍可查询");

        pool.close();
        assert!(pool.ping().await.is_err(), "关闭后 ping 必须失败");
    })
    .await
    .expect("live 用例不得超时");
}
