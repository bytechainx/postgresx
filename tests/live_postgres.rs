#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::too_many_lines
)]
//! live 真连服测试（特性 002 起，e2e 全公开接口补齐）。
//!
//! 全部用例 `#[ignore]`，默认不跑（CI 行为不变）。本地显式运行：
//!
//! ```bash
//! set -a; source /home/zone/workspace/sre/secrets/env/postgresx.env; set +a
//! cd /home/workspace/bytechainx/postgresx
//! CARGO_TARGET_DIR=/home/workspace/bytechainx/.cargo/target \
//!   cargo test --test live_postgres -- --ignored --test-threads=1
//! ```
//!
//! **凭据只从环境变量读取，绝不硬编码**；失败信息只报所需变量前缀，不回显取值。
//! postgresx 没有 `connect_from_env`，故用 [`PostgresConfig::from_env`] + [`PostgresPool::connect`]。
//!
//! **共享库红线**：目标库 `market_binance` 为在用库——
//!
//! - 所有 DDL/DML 只落在 `e2e_<pid>_<nanos>` 前缀的自建表上，测试尾 `DROP TABLE` 清理
//!   （失败路径也尽力清理）；
//! - 迁移器用例会创建并最终删除 `infra_schema_migrations`（公开常量
//!   [`SCHEMA_MIGRATIONS_TABLE`] 固定表名，无法改名）：用例开始前先断言该表**不存在**，
//!   存在即拒绝执行（避免触碰他人数据）；结束时删除前再核对表内只剩本计划写入的行；
//! - 绝不 DROP / ALTER / TRUNCATE 既有表、绝不动他人数据。
//!
//! # 覆盖矩阵（公开接口 → 用例）
//!
//! | 公开项 | 用例 |
//! | --- | --- |
//! | `PostgresPool::connect/new/acquire/acquire_with/execute/query/query_one/query_opt/ping/health_check/stats/summary/close` | `live_connect_ping_health_and_close`、`live_unique_table_roundtrip_and_cleanup`、`live_pool_query_variants_and_error_kinds` |
//! | `copy_in_bytes` / `copy_out_bytes`（池级 + 连接级） | `live_copy_in_out_roundtrip` |
//! | `with_transaction` / `begin`（含 Err 回滚与 Drop 回滚） | `live_unique_table_roundtrip_and_cleanup`、`live_transaction_lifecycle` |
//! | `PgTransaction` 全 SQL 入口 + `TxStatus` 状态机 | `live_transaction_lifecycle` |
//! | `PgConnection` 全 SQL 入口 + `begin` | `live_pool_query_variants_and_error_kinds`、`live_transaction_lifecycle` |
//! | `PostgresConfig::from_env/from_toml/from_url/builder/validate/has_password` | `live_config_entries_toml_url_builder` |
//! | `SslMode::parse/as_str`、`host_is_local`、`ENV_*`/`DEFAULT_*` 常量 | `live_config_entries_toml_url_builder` |
//! | `Migrator::new/plan/ensure_table/list_applied/status/verify/apply`、`Migration/AppliedMigration/ChecksumMismatch/MigrationStatus/MigrationReport/ensure_boot_ok`、迁移常量 | `live_migrator_full_flow` |
//! | `with_retry_sync/with_retry_async/with_retry_async_no_wait`、`PgRetryConfig` 全构建器 | `live_retry_helpers_with_live_errors` |
//! | `build_client_config*`、`MakeRustlsConnect` 全入口、`ErrorKind/error_from_sqlstate/error_kind_from_sqlstate/map_pool_error/map_tokio_error` | `live_tls_and_error_surface`（真实 42P01/23505 分类另见 query 用例） |
//! | TLS require/prefer 在非公共 CA 证书下 fail-closed | `live_tls_require_fails_closed_on_untrusted_cert`、`live_tls_prefer_fails_closed_when_server_tls_on` |
//!
//! 未能 E2E 的项（现有覆盖）：`MakeRustlsConnect::with_ca_file` 成功路径需要受信 CA 文件
//! （失败路径已在本文件断言；成功路径见 `src/tls.rs` 内联测试）；SCRAM-PLUS channel binding
//! 未实现（`src/tls.rs` 常量锚定）；16 MiB COPY 上限的「真实超限传输」只做了本地拒绝断言。

use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use postgresx::{
    build_client_config, build_client_config_with_ca, build_client_config_with_options,
    ensure_boot_ok, error_from_sqlstate, error_kind_from_sqlstate, host_is_local, map_pool_error,
    map_tokio_error, with_retry_async, with_retry_async_no_wait, with_retry_sync, ErrorKind,
    MakeRustlsConnect, Migration, Migrator, PgRetryConfig, PostgresConfig, PostgresError,
    PostgresPool, SslMode, TxStatus, DEFAULT_COPY_IN_MAX_BYTES, DEFAULT_MAX_POOL_SIZE,
    DEFAULT_PORT, ENV_DATABASE, ENV_HOST, ENV_PASSWORD, ENV_PORT, ENV_SSLMODE, ENV_USER,
    MIGRATION_LOCK_KEY1, MIGRATION_LOCK_KEY2, SCHEMA_MIGRATIONS_TABLE,
};

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

/// 读取非秘密环境变量（缺失时回退默认值）。
fn env_or(key: &str, fallback: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| fallback.to_string())
}

/// URL userinfo 百分号编码（密码可能含 `@` / `:` / `/` 等保留字符）。
fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// 建连 + 结构化探活 + close 收尾。
#[tokio::test]
#[ignore = "需要真实 PostgreSQL 实例与 FOUNDATIONX_POSTGRESX_* 环境变量"]
async fn live_connect_ping_health_and_close() {
    tokio::time::timeout(LIVE_TIMEOUT, async {
        let pool = connect_pool().await;

        // 建连成功断言（E2）：池可用且摘要含端点信息（不含密码）。
        let summary = pool.summary().to_string();
        assert!(!summary.is_empty(), "池摘要不得为空");
        let password = std::env::var(ENV_PASSWORD).unwrap_or_default();
        if !password.is_empty() {
            assert!(
                !summary.contains(&password),
                "池摘要不得包含密码（脱敏红线）"
            );
            assert!(
                !format!("{:?}", connect_pool_config()).contains(&password),
                "配置 Debug 输出不得包含密码（脱敏红线）"
            );
        }

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

        // 池统计与 Debug（纯项顺带断言）。
        let stats = pool.stats();
        assert!(!stats.closed, "关闭前统计不得标记 closed");
        assert!(stats.max_size >= 1, "池上限应 >= 1");
        assert!(
            format!("{pool:?}").contains("PostgresPool"),
            "池 Debug 应可用"
        );

        // acquire / acquire_with（E2）：借出连接不执行 SQL 即归还（drop）。
        {
            let conn = pool.acquire().await.expect("acquire 应成功");
            assert!(
                format!("{conn:?}").contains("PgConnection"),
                "连接 Debug 应可用"
            );
        }
        {
            let _conn = pool
                .acquire_with(Duration::from_secs(5))
                .await
                .expect("acquire_with 应成功");
        }
        let zero = pool
            .acquire_with(Duration::ZERO)
            .await
            .expect_err("零 deadline 必须返回配置错误");
        assert!(matches!(zero, PostgresError::Config(_)));

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

/// 读取 from_env 配置（仅供脱敏断言复用，不建连）。
fn connect_pool_config() -> PostgresConfig {
    PostgresConfig::from_env().expect("from_env 应成功")
}

/// 配置四入口（env / TOML / URL / builder）等价性、validate 与密码脱敏。
#[tokio::test]
#[ignore = "需要真实 PostgreSQL 实例与 FOUNDATIONX_POSTGRESX_* 环境变量"]
async fn live_config_entries_toml_url_builder() {
    tokio::time::timeout(LIVE_TIMEOUT, async {
        let host = env_or(ENV_HOST, "127.0.0.1");
        let database = std::env::var(ENV_DATABASE).expect("live 测试需要 FOUNDATIONX_POSTGRESX_DATABASE");
        let user = std::env::var(ENV_USER).expect("live 测试需要 FOUNDATIONX_POSTGRESX_USER");
        let port: u16 = env_or(ENV_PORT, "5432").parse().expect("端口应为 u16");
        let sslmode = env_or(ENV_SSLMODE, "disable");
        let password = std::env::var(ENV_PASSWORD).unwrap_or_default();

        // from_env：字段与真实环境逐项对账（E2）。
        let config = connect_pool_config();
        assert_eq!(config.host, host);
        assert_eq!(config.database, database);
        assert_eq!(config.user, user);
        assert_eq!(config.port, port);
        assert_eq!(config.sslmode, SslMode::parse(&sslmode).expect("sslmode 应可解析"));
        assert!(config.validate().is_ok(), "live 配置应通过 validate");
        if password.is_empty() {
            assert!(!config.has_password(), "未设密码时 has_password 应为 false");
        } else {
            assert!(config.has_password(), "已设密码时 has_password 应为 true");
            let debug = format!("{config:?}");
            assert!(debug.contains("***"), "Debug 输出应脱敏密码");
            assert!(!debug.contains(&password), "Debug 输出不得泄露密码");
        }

        // from_toml：非秘密字段来自 TOML，密码经 ENV_PASSWORD 注入（E2 真连服验证注入有效）。
        let toml_text = format!(
            "host = \"{host}\"\nport = {port}\ndatabase = \"{database}\"\nuser = \"{user}\"\nsslmode = \"{sslmode}\"\n"
        );
        let from_toml = PostgresConfig::from_toml(&toml_text).expect("TOML 解析应成功");
        assert_eq!(from_toml.port, port);
        assert_eq!(from_toml.max_pool_size, DEFAULT_MAX_POOL_SIZE);
        assert_eq!(from_toml.database, database);
        if !password.is_empty() {
            assert!(from_toml.has_password(), "TOML 配置应经环境变量注入密码");
        }
        let toml_pool = PostgresPool::connect(from_toml)
            .await
            .expect("TOML 配置建连应成功（密码注入真实可用）");
        toml_pool.ping().await.expect("TOML 配置 ping 应成功");
        toml_pool.close();
        // fail-closed：TOML 携带 password 键 / 未知键必须拒绝（不回显源码行）。
        assert!(
            PostgresConfig::from_toml(&format!("{toml_text}password = \"leaked\"\n")).is_err(),
            "TOML 中的 password 键必须 fail-closed"
        );
        assert!(
            PostgresConfig::from_toml(&format!("{toml_text}acquire_timout_ms = 500\n")).is_err(),
            "拼写错误的未知键必须报错"
        );

        // from_url：由环境拼 URL（密码百分号编码，不落字面量），真连服验证（E2）。
        let url = format!(
            "postgres://{}:{}@{}:{}/{}?sslmode={}",
            percent_encode(&user),
            percent_encode(&password),
            host,
            port,
            database,
            sslmode
        );
        let from_url = PostgresConfig::from_url(&url).expect("URL 解析应成功");
        assert_eq!(from_url.host, host);
        assert_eq!(from_url.port, port);
        assert_eq!(from_url.database, database);
        assert_eq!(from_url.user, user);
        if !password.is_empty() {
            assert!(from_url.has_password(), "URL 应携带密码");
        }
        let url_pool = PostgresPool::connect(from_url)
            .await
            .expect("URL 配置建连应成功");
        url_pool.ping().await.expect("URL 配置 ping 应成功");
        url_pool.close();

        // builder：与 from_env 等价（E2）。
        let mut builder = PostgresConfig::builder()
            .host(host.clone())
            .port(port)
            .database(database.clone())
            .user(user.clone())
            .sslmode(SslMode::parse(&sslmode).expect("sslmode"));
        if !password.is_empty() {
            builder = builder.password(password.clone());
        }
        let built = builder.build().expect("builder 构建应成功");
        assert_eq!(built.host, config.host);
        assert_eq!(built.database, config.database);
        assert_eq!(built.user, config.user);
        assert_eq!(built.port, config.port);
        assert_eq!(built.sslmode, config.sslmode);
        // builder 缺 host 必须 fail-closed（纯项）。
        assert!(
            PostgresConfig::builder()
                .database("d")
                .user("u")
                .build()
                .is_err(),
            "缺 host 必须报错"
        );
        // validate：非 loopback + 非 require 必须 fail-closed（纯项）。
        let remote = PostgresConfig::builder()
            .host("db.example.com")
            .database("d")
            .user("u")
            .sslmode(SslMode::Disable)
            .build()
            .expect_err("远程 disable 必须失败");
        assert!(matches!(remote, PostgresError::Config(_)));
        PostgresConfig::builder()
            .host("db.example.com")
            .database("d")
            .user("u")
            .sslmode(SslMode::Require)
            .build()
            .expect("远程 require 应通过校验");

        // 纯项顺带断言：SslMode / host_is_local / 常量。
        assert_eq!(SslMode::Disable.as_str(), "disable");
        assert_eq!(SslMode::Prefer.as_str(), "prefer");
        assert_eq!(SslMode::Require.as_str(), "require");
        assert_eq!(SslMode::parse(" ALLOW ").expect("allow 别名"), SslMode::Prefer);
        assert_eq!(
            SslMode::parse("verify-full").expect("verify-full 别名"),
            SslMode::Require
        );
        assert!(SslMode::parse("wat").is_err());
        assert!(host_is_local("127.0.0.1"));
        assert!(host_is_local("localhost"));
        assert!(host_is_local("/var/run/postgresql"));
        assert!(!host_is_local("10.0.0.9"));
        assert_eq!(DEFAULT_PORT, 5432);
        assert_eq!(DEFAULT_MAX_POOL_SIZE, 16);
        assert_eq!(
            DEFAULT_COPY_IN_MAX_BYTES, 16 * 1024 * 1024,
            "COPY IN 默认上限应为 16 MiB"
        );
    })
    .await
    .expect("live 用例不得超时");
}

/// 池级/连接级 query 全变体、execute 行数语义与真实错误分类（23505/42P01）。
#[tokio::test]
#[ignore = "需要真实 PostgreSQL 实例与 FOUNDATIONX_POSTGRESX_* 环境变量"]
async fn live_pool_query_variants_and_error_kinds() {
    tokio::time::timeout(LIVE_TIMEOUT, async {
        let pool = connect_pool().await;
        let table = format!("e2e_{}", unique_suffix());
        let create = format!(
            "CREATE TABLE {table} (id BIGINT PRIMARY KEY, name TEXT NOT NULL, score BIGINT NOT NULL DEFAULT 0)"
        );
        pool.execute(&create, &[]).await.expect("建表应成功");
        let insert = format!("INSERT INTO {table} (id, name) VALUES ($1, $2)");
        let update = format!("UPDATE {table} SET score = $1 WHERE id <= $2");

        let outcome = async {
            // execute：影响行数语义（E2）。
            for (id, name) in [(1_i64, "one"), (2_i64, "two"), (3_i64, "three")] {
                let affected = pool.execute(&insert, &[&id, &name]).await?;
                assert_eq!(affected, 1, "每条 insert 应影响 1 行");
            }
            let updated = pool.execute(&update, &[&10_i64, &2_i64]).await?;
            assert_eq!(updated, 2, "UPDATE 应影响 2 行");
            let untouched = pool
                .execute(&update, &[&99_i64, &0_i64])
                .await?;
            assert_eq!(untouched, 0, "无匹配行的 UPDATE 应影响 0 行");

            // query：0..N 行（E2）。
            let rows = pool
                .query(&format!("SELECT id, name, score FROM {table} ORDER BY id"), &[])
                .await?;
            assert_eq!(rows.len(), 3, "应返回 3 行");
            let ids: Vec<i64> = rows.iter().map(|row| row.get(0)).collect();
            assert_eq!(ids, vec![1, 2, 3]);
            let names: Vec<String> = rows.iter().map(|row| row.get(1)).collect();
            assert_eq!(names, vec!["one", "two", "three"]);
            let scores: Vec<i64> = rows.iter().map(|row| row.get(2)).collect();
            assert_eq!(scores, vec![10, 10, 0]);
            let empty = pool
                .query(&format!("SELECT id FROM {table} WHERE id = $1"), &[&99_i64])
                .await?;
            assert!(empty.is_empty(), "无匹配时应返回 0 行");

            // query_one：恰好一行 / 0 行报错（E2）。
            let row = pool
                .query_one(&format!("SELECT name FROM {table} WHERE id = $1"), &[&2_i64])
                .await?;
            let name: String = row.get(0);
            assert_eq!(name, "two");
            assert!(
                pool.query_one(&format!("SELECT name FROM {table} WHERE id = $1"), &[&99_i64])
                    .await
                    .is_err(),
                "0 行时 query_one 应报错"
            );

            // query_opt：Some / None / 多行报错（E2）。
            let some = pool
                .query_opt(&format!("SELECT name FROM {table} WHERE id = $1"), &[&1_i64])
                .await?;
            assert!(some.is_some(), "命中时应返回 Some");
            let none = pool
                .query_opt(&format!("SELECT name FROM {table} WHERE id = $1"), &[&99_i64])
                .await?;
            assert!(none.is_none(), "未命中时应返回 None");
            assert!(
                pool.query_opt(&format!("SELECT id FROM {table} WHERE id <= $1"), &[&2_i64])
                    .await
                    .is_err(),
                "多行时 query_opt 应报错"
            );

            // 真实错误分类：23505 唯一键冲突 → Conflict 且不可重试（E2，经 map_tokio_error）。
            let conflict = pool
                .execute(&insert, &[&1_i64, &"dup"])
                .await
                .expect_err("重复主键必须失败");
            assert!(matches!(conflict, PostgresError::Conflict(_)), "23505 应映射为 Conflict: {conflict}");
            assert!(!conflict.is_retryable(), "唯一键冲突不值得重试");

            // 真实错误分类：42P01 缺表 → Missing 且不可重试（E2）。
            let missing = pool
                .query(&format!("SELECT * FROM {table}_absent"), &[])
                .await
                .expect_err("缺表必须失败");
            assert!(matches!(missing, PostgresError::Missing(_)), "42P01 应映射为 Missing: {missing}");
            assert!(!missing.is_retryable(), "缺表不值得重试");

            // 连接级入口：PgConnection 全 SQL 变体（E2）。
            let mut conn = pool.acquire().await?;
            let affected = conn.execute(&insert, &[&4_i64, &"via-conn"]).await?;
            assert_eq!(affected, 1);
            let row = conn
                .query_one(&format!("SELECT name FROM {table} WHERE id = $1"), &[&4_i64])
                .await?;
            let name: String = row.get(0);
            assert_eq!(name, "via-conn");
            let rows = conn.query(&format!("SELECT id FROM {table} ORDER BY id"), &[]).await?;
            assert_eq!(rows.len(), 4);
            let opt = conn
                .query_opt(&format!("SELECT name FROM {table} WHERE id = $1"), &[&99_i64])
                .await?;
            assert!(opt.is_none());
            assert!(
                format!("{conn:?}").contains("PgConnection"),
                "连接 Debug 应可用"
            );
            Ok::<(), postgresx::PostgresError>(())
        }
        .await;

        // 强制清理（E4）：无论成败都删除自建表。
        let cleaned = pool
            .execute(&format!("DROP TABLE IF EXISTS {table}"), &[])
            .await;

        outcome.expect("查询变体与错误分类应全部成功");
        cleaned.expect("临时表清理应成功");
        let after = pool.query(&format!("SELECT * FROM {table}"), &[]).await;
        assert!(after.is_err(), "清理后表不应仍可查询");

        pool.close();
        assert!(pool.ping().await.is_err(), "关闭后 ping 必须失败");
    })
    .await
    .expect("live 用例不得超时");
}

/// COPY IN / COPY OUT 原语（池级 + 连接级）与载荷上限 fail-closed。
#[tokio::test]
#[ignore = "需要真实 PostgreSQL 实例与 FOUNDATIONX_POSTGRESX_* 环境变量"]
async fn live_copy_in_out_roundtrip() {
    tokio::time::timeout(LIVE_TIMEOUT, async {
        let pool = connect_pool().await;
        let table = format!("e2e_{}", unique_suffix());
        pool.execute(&format!("CREATE TABLE {table} (id BIGINT, name TEXT)"), &[])
            .await
            .expect("建表应成功");
        let copy_in = format!("COPY {table} (id, name) FROM STDIN");
        let copy_out = format!("COPY {table} TO STDOUT");

        let outcome = async {
            // 池级 COPY IN（E2）：3 行文本载荷。
            let rows = pool
                .copy_in_bytes(&copy_in, b"1\talpha\n2\tbeta\n3\tgamma\n")
                .await?;
            assert_eq!(rows, 3, "COPY IN 应写入 3 行");
            let count = pool
                .query_one(&format!("SELECT count(*) FROM {table}"), &[])
                .await?;
            let total: i64 = count.get(0);
            assert_eq!(total, 3);

            // 池级 COPY OUT（E2）：默认上限（max_bytes=0 → 16 MiB）。
            let bytes = pool.copy_out_bytes(&copy_out, 0).await?;
            let text = String::from_utf8(bytes).expect("COPY OUT 应为 UTF-8 文本");
            assert!(text.contains("1\talpha\n"), "COPY OUT 应含首行: {text:?}");
            assert!(text.contains("3\tgamma\n"), "COPY OUT 应含末行");
            assert_eq!(text.lines().count(), 3, "COPY OUT 应为 3 行");

            // 连接级 COPY IN / OUT（E2）。
            let mut conn = pool.acquire().await?;
            let extra = conn.copy_in_bytes(&copy_in, b"4\tdelta\n").await?;
            assert_eq!(extra, 1, "连接级 COPY IN 应写入 1 行");
            let out = conn.copy_out_bytes(&copy_out, 0).await?;
            assert!(
                String::from_utf8_lossy(&out).contains("4\tdelta\n"),
                "连接级 COPY OUT 应含新行"
            );
            Ok::<(), postgresx::PostgresError>(())
        }
        .await;

        // 上限与空语句 fail-closed（E2 调用路径，本地拒绝）。
        let too_small = pool
            .copy_out_bytes(&copy_out, 4)
            .await
            .expect_err("聚合大小超上限必须失败");
        assert!(matches!(too_small, PostgresError::Config(_)));
        let empty_in = pool
            .copy_in_bytes("", b"1\tx\n")
            .await
            .expect_err("空 COPY IN 语句必须失败");
        assert!(matches!(empty_in, PostgresError::Config(_)));
        let empty_out = pool
            .copy_out_bytes("", 0)
            .await
            .expect_err("空 COPY OUT 语句必须失败");
        assert!(matches!(empty_out, PostgresError::Config(_)));
        let oversized = vec![b'x'; DEFAULT_COPY_IN_MAX_BYTES + 1];
        let too_big = pool
            .copy_in_bytes(&copy_in, &oversized)
            .await
            .expect_err("超过 16 MiB 的 COPY IN 载荷必须被本地拒绝");
        assert!(matches!(too_big, PostgresError::Config(_)));

        // 强制清理（E4）。
        let cleaned = pool
            .execute(&format!("DROP TABLE IF EXISTS {table}"), &[])
            .await;

        outcome.expect("COPY 往返应成功");
        cleaned.expect("临时表清理应成功");

        pool.close();
        assert!(pool.ping().await.is_err(), "关闭后 ping 必须失败");
    })
    .await
    .expect("live 用例不得超时");
}

/// 事务生命周期：begin/commit/rollback、Failed 状态机、Drop 服务端回滚与 with_transaction 双路径。
#[tokio::test]
#[ignore = "需要真实 PostgreSQL 实例与 FOUNDATIONX_POSTGRESX_* 环境变量"]
async fn live_transaction_lifecycle() {
    tokio::time::timeout(LIVE_TIMEOUT, async {
        let pool = connect_pool().await;
        let table = format!("e2e_{}", unique_suffix());
        pool.execute(&create_table_sql(&table), &[])
            .await
            .expect("建表应成功");
        let insert = insert_sql(&table);
        let count_sql = format!("SELECT count(*) FROM {table}");

        let outcome = async {
            // 显式事务：begin → 多语句 → commit（E2）。
            let mut tx = pool.begin().await?;
            assert_eq!(tx.status(), TxStatus::Active, "BEGIN 后应为 Active");
            assert!(tx.is_active());
            assert!(
                format!("{tx:?}").contains("PgTransaction"),
                "事务 Debug 应可用"
            );
            let affected = tx.execute(&insert, &[&10_i64, &"commit-path"]).await?;
            assert_eq!(affected, 1);
            // 事务内三查询入口（E2）。
            let row = tx.query_one(&count_sql, &[]).await?;
            let total: i64 = row.get(0);
            assert_eq!(total, 1, "事务内应可见未提交行");
            let rows = tx.query(&format!("SELECT id FROM {table}"), &[]).await?;
            assert_eq!(rows.len(), 1);
            let opt = tx
                .query_opt(&format!("SELECT id FROM {table} WHERE id = $1"), &[&10_i64])
                .await?;
            assert!(opt.is_some());
            tx.commit().await?;
            let committed = pool.query_one(&count_sql, &[]).await?;
            let total: i64 = committed.get(0);
            assert_eq!(total, 1, "COMMIT 后行应落库");

            // 显式回滚：begin → insert → rollback（E2）。
            let mut tx = pool.begin().await?;
            tx.execute(&insert, &[&11_i64, &"rollback-path"]).await?;
            tx.rollback().await?;
            let after = pool.query_one(&count_sql, &[]).await?;
            let total: i64 = after.get(0);
            assert_eq!(total, 1, "ROLLBACK 后行应缺席");

            // Failed 状态机：真实后端错误（23505）→ Failed → 拒绝再执行 → 允许回滚（E2）。
            let mut tx = pool.begin().await?;
            tx.execute(&insert, &[&12_i64, &"failed-path"]).await?;
            let dup = tx.execute(&insert, &[&12_i64, &"dup"]).await;
            assert!(dup.is_err(), "事务内重复主键必须失败");
            assert_eq!(tx.status(), TxStatus::Failed, "语句失败后应为 Failed");
            assert!(!tx.is_active());
            let rejected = tx.execute(&insert, &[&13_i64, &"after-fail"]).await;
            assert!(rejected.is_err(), "Failed 态应拒绝继续执行 SQL");
            tx.rollback().await?;
            let after = pool.query_one(&count_sql, &[]).await?;
            let total: i64 = after.get(0);
            assert_eq!(total, 1, "Failed 回滚后 12/13 均应缺席");

            // Drop 语义：不 commit 直接丢弃 → 连接脱池、服务端回滚（E2）。
            {
                let mut tx = pool.begin().await?;
                tx.execute(&insert, &[&14_i64, &"drop-path"]).await?;
            }
            // 等待服务端处理连接终止（backend 收到断开后回滚打开的事务）。
            tokio::time::sleep(Duration::from_millis(500)).await;
            let after = pool.query_one(&count_sql, &[]).await?;
            let total: i64 = after.get(0);
            assert_eq!(total, 1, "Drop 路径的行应由服务端回滚");

            // with_transaction：Ok 传播返回值 / Err 触发回滚（E2）。
            let value = pool
                .with_transaction(|tx| {
                    let insert = insert.clone();
                    Box::pin(async move {
                        tx.execute(&insert, &[&15_i64, &"wt-ok"]).await?;
                        Ok::<i64, postgresx::PostgresError>(15)
                    })
                })
                .await?;
            assert_eq!(value, 15, "with_transaction 应传播闭包返回值");
            let err = pool
                .with_transaction(|tx| {
                    let insert = insert.clone();
                    Box::pin(async move {
                        tx.execute(&insert, &[&16_i64, &"wt-err"]).await?;
                        Err::<(), postgresx::PostgresError>(postgresx::PostgresError::Backend(
                            "业务失败触发回滚".to_string(),
                        ))
                    })
                })
                .await
                .expect_err("闭包 Err 必须外显为错误");
            assert!(matches!(err, postgresx::PostgresError::Backend(_)));
            let final_count = pool.query_one(&count_sql, &[]).await?;
            let total: i64 = final_count.get(0);
            assert_eq!(total, 2, "仅 10 与 15 落库：11/12/13/14/16 均应回滚");

            // PgConnection::begin：从借出连接开启事务（E2）。
            let conn = pool.acquire().await?;
            let mut tx = conn.begin().await?;
            tx.execute(&insert, &[&17_i64, &"conn-begin"]).await?;
            tx.commit().await?;
            let final_count = pool.query_one(&count_sql, &[]).await?;
            let total: i64 = final_count.get(0);
            assert_eq!(total, 3, "conn.begin 路径的 17 应落库");
            Ok::<(), postgresx::PostgresError>(())
        }
        .await;

        // TxStatus 纯项顺带断言（状态机常量语义）。
        assert_eq!(TxStatus::Active.as_str(), "active");
        assert_eq!(TxStatus::Committed.as_str(), "committed");
        assert_eq!(TxStatus::RolledBack.as_str(), "rolled_back");
        assert_eq!(TxStatus::Failed.as_str(), "failed");
        assert!(TxStatus::Committed.is_finished());
        assert!(TxStatus::RolledBack.is_finished());
        assert!(!TxStatus::Active.is_finished());
        assert!(!TxStatus::Failed.is_finished());

        // 强制清理（E4）。
        let cleaned = pool
            .execute(&format!("DROP TABLE IF EXISTS {table}"), &[])
            .await;

        outcome.expect("事务生命周期应全部成功");
        cleaned.expect("临时表清理应成功");

        pool.close();
        assert!(pool.ping().await.is_err(), "关闭后 ping 必须失败");
    })
    .await
    .expect("live 用例不得超时");
}

/// 事务用例的建表 SQL。
fn create_table_sql(table: &str) -> String {
    format!("CREATE TABLE {table} (id BIGINT PRIMARY KEY, name TEXT NOT NULL)")
}

/// 事务用例的插入 SQL。
fn insert_sql(table: &str) -> String {
    format!("INSERT INTO {table} (id, name) VALUES ($1, $2)")
}

/// Migrator 全流程：建表 → pending → apply → 幂等 → checksum 不一致 / 未知版本 fail-closed → 清理。
///
/// `infra_schema_migrations` 是公开常量固定的表名：本用例开始前断言其不存在（存在即拒绝执行，
/// 避免触碰他人数据）；结束时核对表内只剩本计划写入的行后才删除。
#[tokio::test]
#[ignore = "需要真实 PostgreSQL 实例与 FOUNDATIONX_POSTGRESX_* 环境变量"]
async fn live_migrator_full_flow() {
    tokio::time::timeout(LIVE_TIMEOUT, async {
        let pool = connect_pool().await;
        let suffix = unique_suffix();
        let t1 = format!("e2e_{suffix}_m1");
        let t2 = format!("e2e_{suffix}_m2");

        // 前置：历史表必须不存在（本测试创建并负责删除）。
        let probe = pool
            .query_one(
                &format!("SELECT to_regclass('public.{SCHEMA_MIGRATIONS_TABLE}')::text"),
                &[],
            )
            .await
            .expect("查询历史表存在性应成功");
        let existed: Option<String> = probe.get(0);
        assert!(
            existed.is_none(),
            "共享库已存在 {SCHEMA_MIGRATIONS_TABLE}（非本测试创建）；为避免触碰他人数据，本用例拒绝执行"
        );

        let v1 = Migration::new(
            1,
            "create_m1",
            format!("CREATE TABLE {t1} (id BIGINT PRIMARY KEY);"),
        )
        .expect("迁移 v1");
        let v2 = Migration::new(
            2,
            "create_m2",
            format!("CREATE TABLE {t2} (id BIGINT PRIMARY KEY);"),
        )
        .expect("迁移 v2");
        let migrator = Migrator::new(pool.clone(), vec![v1.clone(), v2.clone()])
            .expect("构造 Migrator 应成功");

        let outcome = async {
            // 纯校验面（顺带断言）。
            assert!(Migration::new(0, "x", "SELECT 1;").is_err(), "version 必须 > 0");
            assert!(Migration::new(1, "", "SELECT 1;").is_err(), "name 不能为空");
            assert!(Migration::new(1, "x", "  ").is_err(), "sql 不能为空");
            assert_eq!(v1.checksum().len(), 64, "checksum 应为 64 位十六进制");
            assert!(
                Migrator::new(pool.clone(), vec![v1.clone(), v1.clone()]).is_err(),
                "重复 version 必须拒绝"
            );
            assert_eq!(migrator.plan().len(), 2);
            assert_eq!(migrator.plan()[0].version, 1, "计划应按 version 升序");
            assert_ne!(MIGRATION_LOCK_KEY1, 0);
            assert_ne!(MIGRATION_LOCK_KEY2, 0);

            // ensure_table + pending 状态（E2）。
            migrator.ensure_table().await?;
            let status = migrator.status().await?;
            assert_eq!(status.pending, vec![1, 2], "apply 前两版本均应 pending");
            assert!(status.is_boot_ok(), "pending 不阻塞启动（由运维显式 apply）");
            assert!(!status.is_clean(), "有 pending 时不是 clean");
            ensure_boot_ok(&status).expect("pending 状态应放行启动");

            // verify 是默认启动入口：不执行 DDL（E2）。
            let verified = migrator.verify().await?;
            assert_eq!(verified.pending, vec![1, 2]);
            let probe = pool
                .query_one(
                    &format!("SELECT to_regclass('public.{t1}')::text"),
                    &[],
                )
                .await?;
            let t1_absent: Option<String> = probe.get(0);
            assert!(t1_absent.is_none(), "verify 不得执行任何 DDL");

            // apply（E2）：advisory lock + DDL + 历史写入。
            let report = migrator.apply().await?;
            assert_eq!(report.applied_now, vec![1, 2]);
            assert!(report.status.is_clean(), "apply 后应 clean");
            let verified = migrator.verify().await?;
            assert!(verified.is_clean());
            ensure_boot_ok(&verified).expect("同步状态应放行");

            // list_applied 与 checksum 对账（E2）。
            let applied = migrator.list_applied().await?;
            assert_eq!(applied.len(), 2);
            assert_eq!(applied[0].version, 1);
            assert_eq!(applied[0].name, "create_m1");
            assert_eq!(applied[0].checksum, v1.checksum());
            assert_eq!(applied[1].version, 2);
            assert_eq!(applied[1].checksum, v2.checksum());

            // 幂等重放（E2）。
            let replay = migrator.apply().await?;
            assert!(replay.applied_now.is_empty(), "已应用版本不得重放");

            // checksum 不一致 fail-closed（E2，只读校验；apply 先 verify、不执行 DDL）。
            let mutated = Migration::new(
                1,
                "create_m1",
                format!("CREATE TABLE {t1} (id BIGINT PRIMARY KEY, note TEXT);"),
            )
            .expect("变异迁移");
            let bad = Migrator::new(pool.clone(), vec![mutated.clone(), v2.clone()])
                .expect("构造变异 Migrator");
            let bad_status = bad.status().await?;
            assert_eq!(bad_status.mismatches.len(), 1);
            assert_eq!(bad_status.mismatches[0].version, 1);
            assert_eq!(bad_status.mismatches[0].expected, mutated.checksum());
            assert_eq!(bad_status.mismatches[0].actual, v1.checksum());
            assert!(!bad_status.is_boot_ok());
            assert!(bad.verify().await.is_err(), "checksum 不一致必须 verify 失败");
            assert!(
                ensure_boot_ok(&bad_status).is_err(),
                "checksum 不一致必须拒绝启动"
            );
            assert!(bad.apply().await.is_err(), "checksum 不一致必须拒绝 apply");

            // 未知已应用版本 fail-closed（E2，只读）。
            let partial = Migrator::new(pool.clone(), vec![v2.clone()]).expect("部分计划");
            let partial_status = partial.status().await?;
            assert_eq!(partial_status.unknown_applied, vec![1], "计划外的 v1 应报未知版本");
            assert!(partial.verify().await.is_err(), "未知版本必须 verify 失败");

            // 迁移 DDL 真实落地（E2）。
            let affected = pool
                .execute(&format!("INSERT INTO {t1} (id) VALUES ($1)"), &[&1_i64])
                .await?;
            assert_eq!(affected, 1);
            Ok::<(), postgresx::PostgresError>(())
        }
        .await;

        // 强制清理（E4）：删自建表；历史表仅当仍只含本计划版本时才删除。
        let _ = pool.execute(&format!("DROP TABLE IF EXISTS {t1}"), &[]).await;
        let _ = pool.execute(&format!("DROP TABLE IF EXISTS {t2}"), &[]).await;
        let rows = pool
            .query(
                &format!("SELECT version FROM {SCHEMA_MIGRATIONS_TABLE}"),
                &[],
            )
            .await;
        if let Ok(rows) = rows {
            let versions: Vec<i64> = rows
                .iter()
                .map(|row| {
                    let version: i64 = row.get(0);
                    version
                })
                .collect();
            if versions.iter().all(|version| *version == 1 || *version == 2) {
                let _ = pool
                    .execute(&format!("DROP TABLE IF EXISTS {SCHEMA_MIGRATIONS_TABLE}"), &[])
                    .await;
            }
        }
        // 清理后历史表应不存在（本测试创建的表已删除）。
        let probe = pool
            .query_one(
                &format!("SELECT to_regclass('public.{SCHEMA_MIGRATIONS_TABLE}')::text"),
                &[],
            )
            .await
            .expect("清理后存在性查询应成功");
        let residue: Option<String> = probe.get(0);
        assert!(residue.is_none(), "清理后不得残留 {SCHEMA_MIGRATIONS_TABLE}");

        outcome.expect("migrator 全流程应成功");

        pool.close();
        assert!(pool.ping().await.is_err(), "关闭后 ping 必须失败");
    })
    .await
    .expect("live 用例不得超时");
}

/// 重试三入口与 PgRetryConfig 构建器：以真实「连接拒绝」与真实「缺表」错误驱动。
#[tokio::test]
#[ignore = "需要真实 PostgreSQL 实例与 FOUNDATIONX_POSTGRESX_* 环境变量"]
async fn live_retry_helpers_with_live_errors() {
    tokio::time::timeout(LIVE_TIMEOUT, async {
        let good = connect_pool().await;

        // 坏端口池：127.0.0.1:1 → 真实「连接拒绝」（可重试类）。
        let bad_config = PostgresConfig::builder()
            .host("127.0.0.1")
            .port(1)
            .database("x")
            .user("x")
            .sslmode(SslMode::Disable)
            .connect_timeout(Duration::from_millis(500))
            .acquire_timeout(Duration::from_millis(800))
            .build()
            .expect("坏端口配置应合法");
        let bad = PostgresPool::new(bad_config).expect("建池（不联网）应成功");

        // PgRetryConfig 纯面（顺带断言）。
        let fixed = PgRetryConfig::fixed(3, Duration::from_millis(2)).without_jitter();
        assert_eq!(fixed.max_attempts, 3);
        assert_eq!(fixed.initial_delay, Duration::from_millis(2));
        assert!(!fixed.jitter, "without_jitter 应关闭抖动");
        assert_eq!(fixed.delay_for_attempt(1), Duration::from_millis(2));
        let exponential =
            PgRetryConfig::exponential(4, Duration::from_millis(10), Duration::from_millis(30));
        assert_eq!(exponential.delay_for_attempt(1), Duration::from_millis(10));
        assert_eq!(exponential.delay_for_attempt(2), Duration::from_millis(20));
        assert_eq!(
            exponential.delay_for_attempt(9),
            Duration::from_millis(30),
            "退避应被上限截断"
        );
        assert_eq!(PgRetryConfig::default(), PgRetryConfig::new(3));

        // 真实瞬时错误池：连接拒绝 → Connection → is_retryable（E2）。
        // PostgresError 未实现 Clone，改为预先采集若干次真实失败，供同步重试闭包消费。
        let refused = bad.ping().await.expect_err("坏端口必须失败");
        assert!(refused.is_retryable(), "连接类错误应可重试: {refused}");
        let mut refused_batch: Vec<PostgresError> = Vec::new();
        for _ in 0..6 {
            refused_batch.push(bad.ping().await.expect_err("坏端口必须持续失败"));
        }
        // 全部属于可重试类（真实错误分类批量复核）。
        assert!(
            refused_batch.iter().all(PostgresError::is_retryable),
            "连接拒绝应全部映射为可重试错误"
        );

        // 真实非瞬时错误：缺表 → Missing → 不可重试（E2）。
        let missing = good
            .query("SELECT * FROM e2e_no_such_table_for_retry", &[])
            .await
            .expect_err("缺表必须失败");
        assert!(matches!(missing, PostgresError::Missing(_)));
        assert!(!missing.is_retryable(), "缺表不值得重试");

        // with_retry_sync：第三次恢复（错误源自真实连接拒绝）（E2）。
        let mut calls = 0_u32;
        let outcome = with_retry_sync(
            &PgRetryConfig::fixed(3, Duration::ZERO),
            "live.retry.sync",
            || {
                calls += 1;
                if calls < 3 {
                    Err(refused_batch.pop().expect("预采集错误应够用"))
                } else {
                    Ok(calls)
                }
            },
        )
        .expect("第三次应成功");
        assert_eq!(outcome, 3);
        assert_eq!(calls, 3);

        // with_retry_sync：不可重试错误立即返回（E2，真实 Missing 错误）。
        // 非可重试错误保证只调用一次，故用 Option::take 单次消费（PostgresError 无 Clone）。
        let mut missing_slot = Some(missing);
        let mut non_retryable_calls = 0_u32;
        let error = with_retry_sync(
            &PgRetryConfig::fixed(5, Duration::ZERO),
            "live.retry.nonretryable",
            || {
                non_retryable_calls += 1;
                Err::<(), _>(missing_slot.take().expect("不可重试路径只应调用一次"))
            },
        )
        .expect_err("不可重试错误必须直接返回");
        assert!(matches!(error, PostgresError::Missing(_)));
        assert_eq!(non_retryable_calls, 1, "不可重试错误只应调用一次");

        // with_retry_sync：总预算耗尽 → Timeout（E2）。
        let deadline_config = PgRetryConfig::fixed(10, Duration::from_millis(5))
            .with_deadline(Duration::from_millis(2));
        let error = with_retry_sync(&deadline_config, "live.retry.deadline", || {
            Err::<(), _>(refused_batch.pop().expect("预采集错误应够用"))
        })
        .expect_err("总预算耗尽必须报 Timeout");
        assert!(matches!(error, PostgresError::Timeout(_)));

        // with_retry_async：第一次坏池、第二次好池 → 恢复（E2，真实错误驱动）。
        let attempts = AtomicU32::new(0);
        with_retry_async(&fixed, "live.retry.async", || {
            let attempt = attempts.fetch_add(1, Ordering::SeqCst);
            let bad = bad.clone();
            let good = good.clone();
            async move {
                if attempt == 0 {
                    bad.ping().await
                } else {
                    good.ping().await
                }
            }
        })
        .await
        .expect("第一次坏池、第二次好池应恢复");
        assert_eq!(attempts.load(Ordering::SeqCst), 2);

        // with_retry_async：尝试次数耗尽返回最后一个可重试错误（E2）。
        let attempts = AtomicU32::new(0);
        let exhausted_config = PgRetryConfig::fixed(2, Duration::from_millis(5)).without_jitter();
        let error = with_retry_async(&exhausted_config, "live.retry.async.exhaust", || {
            attempts.fetch_add(1, Ordering::SeqCst);
            let bad = bad.clone();
            async move { bad.ping().await }
        })
        .await
        .expect_err("坏池两次都应失败");
        assert!(error.is_retryable(), "耗尽后返回的仍是可重试类错误");
        assert_eq!(attempts.load(Ordering::SeqCst), 2);

        // with_retry_async_no_wait：立即重试恢复（E2）。
        let attempts = AtomicU32::new(0);
        let no_wait_config = PgRetryConfig::fixed(3, Duration::from_secs(30)).without_jitter();
        with_retry_async_no_wait(&no_wait_config, "live.retry.nowait", || {
            let attempt = attempts.fetch_add(1, Ordering::SeqCst);
            let bad = bad.clone();
            let good = good.clone();
            async move {
                if attempt == 0 {
                    bad.ping().await
                } else {
                    good.ping().await
                }
            }
        })
        .await
        .expect("无等待重试应立即恢复");
        assert_eq!(attempts.load(Ordering::SeqCst), 2);

        good.close();
        assert!(good.ping().await.is_err(), "关闭后 ping 必须失败");
    })
    .await
    .expect("live 用例不得超时");
}

/// TLS 连接器与错误映射的公开面（纯项真实执行；握手路径另见两个 fail-closed 用例）。
#[tokio::test]
#[ignore = "需要真实 PostgreSQL 实例与 FOUNDATIONX_POSTGRESX_* 环境变量"]
async fn live_tls_and_error_surface() {
    tokio::time::timeout(LIVE_TIMEOUT, async {
        let pool = connect_pool().await;

        // TLS 配置构建器全入口（真实执行；不握手）。
        let config = build_client_config().expect("rustls 配置应构建成功");
        let _ = build_client_config_with_ca(None).expect("带 CA 槽位的配置应成功");
        let _ = build_client_config_with_options(None, None, None).expect("全参配置应成功");

        // MakeRustlsConnect 全入口。
        let make = MakeRustlsConnect::with_webpki_roots().expect("webpki 连接器应成功");
        let _ = MakeRustlsConnect::with_webpki_and_ca(None).expect("webpki+CA 连接器应成功");
        let _ = MakeRustlsConnect::with_options(None, None, None).expect("全参连接器应成功");
        let from_config = MakeRustlsConnect::from_config(config);
        let connect = from_config
            .for_domain("db.example.com")
            .expect("合法域名应可构造握手参数");
        assert!(
            format!("{connect:?}").contains("db.example.com"),
            "握手参数 Debug 应含域名"
        );
        assert!(
            MakeRustlsConnect::with_webpki_roots()
                .expect("连接器")
                .for_domain("")
                .is_err(),
            "空域名必须拒绝"
        );
        assert!(!MakeRustlsConnect::supports_extra_ca_path(None));
        let path = std::path::PathBuf::from("/tmp/pgx-ca.pem");
        assert!(MakeRustlsConnect::supports_extra_ca_path(Some(&path)));
        assert!(!format!("{make:?}").is_empty(), "连接器 Debug 应可用");

        // TLS fail-closed：缺失 CA 文件 / mTLS 单边（真实执行）。
        let missing_ca = build_client_config_with_ca(Some(Path::new("/no/such/pgx-ca.pem")))
            .expect_err("缺失 CA 必须失败");
        assert!(matches!(missing_ca, PostgresError::Config(_)));
        assert!(
            MakeRustlsConnect::with_ca_file("/no/such/pgx-ca.pem").is_err(),
            "缺失 CA 文件的连接器必须失败"
        );
        let half_mtls =
            build_client_config_with_options(None, Some(Path::new("/tmp/pgx-only-cert.pem")), None)
                .expect_err("仅证书必须失败");
        assert!(matches!(half_mtls, PostgresError::Config(_)));

        // ErrorKind / SQLSTATE 纯面（真实 42P01/23505 映射另见 query 用例）。
        assert_eq!(error_kind_from_sqlstate("42P01"), ErrorKind::Missing);
        assert_eq!(error_kind_from_sqlstate("23505"), ErrorKind::Conflict);
        assert_eq!(error_kind_from_sqlstate("40001"), ErrorKind::Serialization);
        assert_eq!(ErrorKind::Serialization.to_string(), "serialization");
        assert_eq!(ErrorKind::DeadlineExceeded.as_str(), "deadline_exceeded");
        assert!(ErrorKind::Unavailable.is_retryable());
        assert!(ErrorKind::Serialization.is_retryable());
        assert!(!ErrorKind::Conflict.is_retryable());
        assert!(!ErrorKind::Missing.is_retryable());
        assert!(matches!(
            ErrorKind::Missing.into_postgres_error("x".to_string()),
            PostgresError::Missing(_)
        ));
        let unavailable = error_from_sqlstate("08006", "connection failure");
        assert!(unavailable.is_retryable());
        assert!(
            unavailable.to_string().contains("08006"),
            "消息应携带 SQLSTATE"
        );

        // map_pool_error / map_tokio_error：签名可用 + 关键分支（真实执行）。
        let closed = map_pool_error(deadpool_postgres::PoolError::Closed);
        assert!(matches!(closed, PostgresError::Connection(_)));
        let _: fn(deadpool_postgres::PoolError) -> PostgresError = map_pool_error;
        let _: fn(tokio_postgres::Error) -> PostgresError = map_tokio_error;

        // 顺带：ping 仍可用（本用例建连证明池健康）。
        pool.ping().await.expect("ping 应成功");
        pool.close();
        assert!(pool.ping().await.is_err(), "关闭后 ping 必须失败");
    })
    .await
    .expect("live 用例不得超时");
}

/// TLS 探测配置：真实凭据 + 指定 sslmode（loopback 允许三种模式）。
fn tls_probe_config(mode: SslMode) -> PostgresConfig {
    let host = env_or(ENV_HOST, "127.0.0.1");
    let database =
        std::env::var(ENV_DATABASE).expect("live 测试需要 FOUNDATIONX_POSTGRESX_DATABASE");
    let user = std::env::var(ENV_USER).expect("live 测试需要 FOUNDATIONX_POSTGRESX_USER");
    let port: u16 = env_or(ENV_PORT, "5432").parse().expect("端口应为 u16");
    let mut builder = PostgresConfig::builder()
        .host(host)
        .port(port)
        .database(database)
        .user(user)
        .sslmode(mode)
        .max_pool_size(2)
        .connect_timeout(Duration::from_secs(3))
        .acquire_timeout(Duration::from_secs(3));
    if let Ok(password) = std::env::var(ENV_PASSWORD) {
        if !password.is_empty() {
            builder = builder.password(password);
        }
    }
    builder.build().expect("TLS 探测配置应合法")
}

/// sslmode=require：服务端证书非公共 CA 签发时必须 fail-closed（无 insecure 旁路）。
///
/// 环境前提：本机服务端 `ssl=on` 且证书非公共 CA / 无 `127.0.0.1` IP SAN。
/// 若服务端未来更换为受信证书（含 IP SAN），本用例应改为断言连接成功。
#[tokio::test]
#[ignore = "需要真实 PostgreSQL 实例（ssl=on，证书非公共 CA）"]
async fn live_tls_require_fails_closed_on_untrusted_cert() {
    tokio::time::timeout(LIVE_TIMEOUT, async {
        // 建池只做本地校验（构造 rustls 连接器），不联网 → 应成功。
        let pool = PostgresPool::new(tls_probe_config(SslMode::Require))
            .expect("本地校验与连接器构建应成功");
        let result = tokio::time::timeout(Duration::from_secs(10), pool.ping()).await;
        let ping = result.expect("TLS 握手失败也应及时返回，不得挂死");
        assert!(
            ping.is_err(),
            "非公共 CA 证书下 require 连接必须失败（fail-closed，无旁路）"
        );
        pool.close();
    })
    .await
    .expect("live 用例不得超时");
}

/// sslmode=prefer：服务端开启 TLS 时走 TLS 协商，证书不受信即失败（同为 fail-closed）。
///
/// 环境前提同上（`ssl=on`）：服务端对 SSLRequest 应答 'S'，rustls 校验不受信证书 → 失败。
/// 若服务端关闭 TLS（应答 'N'），prefer 会回落明文连接成功，届时本用例应改断言。
#[tokio::test]
#[ignore = "需要真实 PostgreSQL 实例（ssl=on，证书非公共 CA）"]
async fn live_tls_prefer_fails_closed_when_server_tls_on() {
    tokio::time::timeout(LIVE_TIMEOUT, async {
        let pool = PostgresPool::new(tls_probe_config(SslMode::Prefer))
            .expect("本地校验与连接器构建应成功");
        let result = tokio::time::timeout(Duration::from_secs(10), pool.ping()).await;
        let ping = result.expect("TLS 协商失败也应及时返回，不得挂死");
        assert!(
            ping.is_err(),
            "服务端 ssl=on 且证书不受信时，prefer 的 TLS 协商必须失败（fail-closed）"
        );
        pool.close();
    })
    .await
    .expect("live 用例不得超时");
}
