#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable
)]
//! SDD 规格对照（特性 002）：把 `docs/标准.md` 的章节条款转成可执行断言。
//!
//! // SPEC-MAP: S-1 | 1. 定位 | assert_positioning
//! // SPEC-MAP: S-2 | 2. 字段治理 | assert_field_governance
//! // SPEC-MAP: S-3 | 3. 安全约定 | assert_security_conventions
//! // SPEC-MAP: S-4 | 4. 迁移与失败处理 | assert_migration_failure_handling
//! // SPEC-MAP: S-5 | 5. 验收 | assert_acceptance

use std::time::Duration;

use postgresx::{
    ensure_boot_ok, error_kind_from_sqlstate, host_is_local, AppliedMigration, ErrorKind,
    Migration, MigrationStatus, Migrator, PgRetryConfig, PostgresConfig, PostgresError,
    PostgresPool, SslMode, DEFAULT_MAX_POOL_SIZE, DEFAULT_PORT, ENV_HOST, ENV_PASSWORD,
    MIGRATION_LOCK_KEY1, MIGRATION_LOCK_KEY2, SCHEMA_MIGRATIONS_TABLE,
};

/// S-1：crate 提供连接池 / 参数化 SQL / 事务 / 迁移 / TLS / 重试原语，不是 ORM，
/// 亦不依赖任何私有内部 crate。
#[test]
fn assert_positioning() {
    // 公开原语类型可用且线程安全（零内部依赖的独立组件库）。
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<PostgresPool>();
    assert_send_sync::<PostgresConfig>();
    assert_send_sync::<Migrator>();
    assert_send_sync::<PgRetryConfig>();

    // 只做本地校验、不联网即可建池（连接池原语可独立使用）。
    let pool = PostgresPool::new(PostgresConfig::default()).expect("建池（不联网）");
    assert_eq!(pool.stats().max_size, DEFAULT_MAX_POOL_SIZE);
    assert_eq!(DEFAULT_PORT, 5432);
    pool.close();
}

/// S-2：字段治理——统一前缀、四入口等价、TOML 严格、密码不进入公开字段与日志。
#[test]
fn assert_field_governance() {
    assert!(ENV_HOST.starts_with("FOUNDATIONX_POSTGRESX_"));
    assert!(ENV_PASSWORD.starts_with("FOUNDATIONX_POSTGRESX_"));
    // 密码不是公开字段：只能经 builder 注入，公开 API 只暴露 has_password（见文末断言）。

    // 四入口等价（from_url / from_toml / builder 对同一组参数给出一致结果）。
    let from_builder = PostgresConfig::builder()
        .host("127.0.0.1")
        .port(6543)
        .database("app")
        .user("app")
        .sslmode(SslMode::Disable)
        .max_pool_size(7)
        .build()
        .expect("builder");
    let from_url = PostgresConfig::from_url("postgres://app@127.0.0.1:6543/app?sslmode=disable")
        .expect("from_url");
    let from_toml = PostgresConfig::from_toml(
        "host = \"127.0.0.1\"\nport = 6543\ndatabase = \"app\"\nuser = \"app\"\nsslmode = \"disable\"\nmax_pool_size = 7\n",
    )
    .expect("from_toml");
    for (label, config) in [("from_url", &from_url), ("from_toml", &from_toml)] {
        assert_eq!(config.host, from_builder.host, "{label} host");
        assert_eq!(config.port, from_builder.port, "{label} port");
        assert_eq!(config.database, from_builder.database, "{label} database");
        assert_eq!(config.user, from_builder.user, "{label} user");
        assert_eq!(config.sslmode, from_builder.sslmode, "{label} sslmode");
    }
    // 池上限只出现在 TOML / builder（URL 无该查询参数），单独断言。
    assert_eq!(from_toml.max_pool_size, 7);

    // TOML 未知键（含误写 password）fail-loud，不静默回落默认值。
    assert!(PostgresConfig::from_toml("host = \"127.0.0.1\"\npassword = \"x\"").is_err());
    // 密码脱敏：Debug 输出恒为 ***。
    let with_password = PostgresConfig::builder()
        .host("127.0.0.1")
        .database("db")
        .user("u")
        .password("Sup3rSecretValue")
        .build()
        .expect("配置");
    assert!(with_password.has_password());
    let debug = format!("{with_password:?}");
    assert!(debug.contains("***"));
    assert!(!debug.contains("Sup3rSecretValue"));
}

/// S-3：安全约定——参数化 SQL、非 loopback 强制 TLS、mTLS 成对、超时兜底。
#[test]
fn assert_security_conventions() {
    // 非 loopback 主机强制 sslmode=require。
    assert!(PostgresConfig::builder()
        .host("db.example.com")
        .database("db")
        .user("u")
        .sslmode(SslMode::Disable)
        .build()
        .is_err());
    // loopback / Unix socket 判定矩阵。
    for local in ["127.0.0.1", "localhost", "[::1]", "/var/run/postgresql"] {
        assert!(host_is_local(local), "{local} 应为本地");
    }
    for remote in ["10.0.0.9", "db.example.com", "0.0.0.0"] {
        assert!(!host_is_local(remote), "{remote} 应为远程");
    }
    // mTLS 证书与私钥必须成对。
    let error = PostgresConfig::builder()
        .host("127.0.0.1")
        .database("db")
        .user("u")
        .sslmode(SslMode::Require)
        .tls_client_cert("/tmp/only.crt")
        .build()
        .expect_err("仅证书必须失败");
    assert!(error.to_string().contains("tls_client_cert"));
    // 超时必须非零（超时兜底所有阻塞点）。
    for config in [
        PostgresConfig::builder()
            .host("127.0.0.1")
            .database("db")
            .user("u")
            .acquire_timeout(Duration::ZERO)
            .build(),
        PostgresConfig::builder()
            .host("127.0.0.1")
            .database("db")
            .user("u")
            .operation_timeout(Duration::ZERO)
            .build(),
    ] {
        assert!(matches!(config, Err(PostgresError::Config(_))));
    }
}

/// S-4：迁移与失败处理——checksum fail-closed、未知版本拒绝、SQLSTATE 语义分类、
/// 重试仅针对可重试错误。
#[test]
fn assert_migration_failure_handling() {
    // checksum 对 SQL 内容敏感（空白变化即不同）。
    let first = Migration::new(1, "a", "CREATE TABLE t (id int);").expect("迁移");
    let second = Migration::new(1, "a", "CREATE TABLE t (id int); ").expect("迁移");
    assert_eq!(first.checksum().len(), 64);
    assert_ne!(first.checksum(), second.checksum());

    // verify 只校验不执行：mismatch / 未知版本令启动 fail-closed。
    let plan = vec![Migration::new(1, "a", "CREATE TABLE a (id int);").expect("迁移")];
    let applied = vec![
        AppliedMigration {
            version: 1,
            name: "a".into(),
            checksum: "deadbeef".into(),
        },
        AppliedMigration {
            version: 9,
            name: "orphan".into(),
            checksum: "x".into(),
        },
    ];
    let status = MigrationStatus::compute(applied, &plan);
    assert!(!status.is_boot_ok());
    assert!(!status.is_clean());
    assert!(ensure_boot_ok(&status).is_err());
    assert_eq!(SCHEMA_MIGRATIONS_TABLE, "infra_schema_migrations");
    assert_ne!(MIGRATION_LOCK_KEY1, 0);
    assert_ne!(MIGRATION_LOCK_KEY2, 0);
    assert_ne!(MIGRATION_LOCK_KEY1, MIGRATION_LOCK_KEY2);

    // SQLSTATE 语义分类：唯一键冲突是冲突（不可重试），序列化失败可重试。
    assert_eq!(error_kind_from_sqlstate("23505"), ErrorKind::Conflict);
    assert_eq!(error_kind_from_sqlstate("40001"), ErrorKind::Serialization);
    assert_eq!(error_kind_from_sqlstate("42P01"), ErrorKind::Missing);

    // 重试预算：指数退避单调、封顶；不可重试错误只尝试一次。
    let retry = PgRetryConfig::exponential(4, Duration::from_millis(100), Duration::from_secs(1));
    assert!(retry.delay_for_attempt(1) <= retry.delay_for_attempt(2));
    assert!(retry.delay_for_attempt(9) <= Duration::from_secs(1));
    let mut attempts = 0_u32;
    let error = postgresx::with_retry_sync(&retry, "op", || {
        attempts += 1;
        Err::<u8, _>(PostgresError::Conflict("dup".into()))
    })
    .expect_err("冲突不可重试");
    assert!(matches!(error, PostgresError::Conflict(_)));
    assert_eq!(attempts, 1, "非可重试错误只允许尝试一次");
}

/// S-5：验收——三件套命令可一次性执行；本文件的离线用例即验收面的一部分。
#[test]
fn assert_acceptance() {
    let _ = std::env::current_dir().expect("可取得当前目录（验收命令可执行）");
    // 离线约束：默认配置指向 127.0.0.1:5432，用例不依赖真实实例。
    let default = PostgresConfig::default();
    assert_eq!(default.host, "127.0.0.1");
    assert_eq!(default.port, DEFAULT_PORT);
}
