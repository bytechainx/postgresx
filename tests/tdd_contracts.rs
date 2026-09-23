#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable
)]
//! TDD 行为契约（特性 002）。
//!
//! 入口集合 = `specs/features/002-public-api-compliance-and-test-tiers/contracts/public-api-contract.md`
//! 登记的 12 个 postgresx 入口。每行描述「对该入口施加的最小语义变异 + 观测到红的用例 +
//! 本树观测到绿的用例」；本树绿为常态，变异红在 `/tmp/mut-postgresx` 副本上复现（见 PR 描述）。
//!
//! 全部用例离线：失败路径统一使用 `127.0.0.1:1`（必然拒绝连接）与关闭后的池，
//! 不依赖真实 PostgreSQL 实例。
//!
//! // TDD-PROBE: PostgresConfig::from_env | 变异：忽略 FOUNDATIONX_POSTGRESX_URL 基底，只看散字段 | 红=from_env_url_base_priority | 绿=from_env_url_base_priority
//! // TDD-PROBE: PostgresConfig::from_toml | 变异：放行未知键（去掉 deny_unknown_fields） | 红=from_toml_strict_keys | 绿=from_toml_strict_keys
//! // TDD-PROBE: PostgresConfig::validate | 变异：去掉「非 loopback 强制 ssl」判定 | 红=validate_remote_requires_ssl | 绿=validate_remote_requires_ssl
//! // TDD-PROBE: PostgresPool::connect | 变异：connect 忽略 ping 冒烟步骤直接返回 Ok | 红=connect_refused_is_retryable | 绿=connect_refused_is_retryable
//! // TDD-PROBE: PostgresPool::acquire | 变异：acquire_with 接受零 deadline | 红=acquire_rejects_zero_deadline | 绿=acquire_rejects_zero_deadline
//! // TDD-PROBE: PostgresPool::execute | 变异：关闭后 ensure_open 不再拒绝 | 红=execute_on_closed_pool_fails | 绿=execute_on_closed_pool_fails
//! // TDD-PROBE: PostgresPool::query | 变异：query 在未建连时返回空集而非错误 | 红=query_on_closed_pool_fails | 绿=query_on_closed_pool_fails
//! // TDD-PROBE: PostgresPool::with_transaction | 变异：取连接失败时仍执行闭包 | 红=with_transaction_on_closed_pool_fails | 绿=with_transaction_on_closed_pool_fails
//! // TDD-PROBE: PostgresPool::ping | 变异：ping 恒返回 Ok | 红=ping_on_closed_pool_fails | 绿=ping_on_closed_pool_fails
//! // TDD-PROBE: PostgresPool::health_check | 变异：health_check 关闭后仍返回快照 | 红=health_check_on_closed_pool_fails | 绿=health_check_on_closed_pool_fails
//! // TDD-PROBE: Migrator::verify | 变异：verify 对 checksum mismatch 放行 | 红=migrator_verify_fails_closed_on_mismatch | 绿=migrator_verify_fails_closed_on_mismatch
//! // TDD-PROBE: PostgresError::is_retryable | 变异：Serialization 不再计入可重试 | 红=error_is_retryable_classification | 绿=error_is_retryable_classification

use std::sync::Mutex;
use std::time::Duration;

use postgresx::{
    ensure_boot_ok, error_from_sqlstate, AppliedMigration, Migration, MigrationStatus, Migrator,
    PostgresConfig, PostgresError, PostgresPool, SslMode, ENV_DATABASE, ENV_HOST, ENV_PASSWORD,
    ENV_PORT, ENV_SSLMODE, ENV_URL, ENV_USER,
};

/// 环境变量是进程级共享状态；本文件内串行化修改，避免并行用例互相干扰。
static ENV_LOCK: Mutex<()> = Mutex::new(());

const MANAGED_ENV: &[&str] = &[
    ENV_URL,
    ENV_HOST,
    ENV_PORT,
    ENV_DATABASE,
    ENV_USER,
    ENV_PASSWORD,
    ENV_SSLMODE,
];

struct EnvGuard<'a> {
    _lock: std::sync::MutexGuard<'a, ()>,
}

impl<'a> EnvGuard<'a> {
    fn new(vars: &[(&str, &str)]) -> Self {
        let lock = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for key in MANAGED_ENV {
            std::env::remove_var(key);
        }
        for (key, value) in vars {
            std::env::set_var(key, value);
        }
        Self { _lock: lock }
    }
}

impl Drop for EnvGuard<'_> {
    fn drop(&mut self) {
        for key in MANAGED_ENV {
            std::env::remove_var(key);
        }
    }
}

/// 必然拒绝连接的配置（`127.0.0.1:1`），带短超时以免拖慢用例。
fn refused_config() -> PostgresConfig {
    PostgresConfig::builder()
        .host("127.0.0.1")
        .port(1)
        .database("x")
        .user("x")
        .sslmode(SslMode::Disable)
        .connect_timeout(Duration::from_millis(300))
        .acquire_timeout(Duration::from_millis(500))
        .operation_timeout(Duration::from_secs(1))
        .build()
        .expect("不可达配置本身应合法")
}

/// `PostgresConfig::from_env`：`_URL` 作为基底、散字段覆盖、缺失必填项 fail-closed。
#[test]
fn from_env_url_base_priority() {
    {
        let _guard = EnvGuard::new(&[]);
        let error = PostgresConfig::from_env().expect_err("缺少必填环境变量必须失败");
        assert!(
            matches!(error, PostgresError::Config(_)),
            "应为 Config 错误: {error}"
        );
        assert!(error.to_string().contains(ENV_HOST), "错误应指出缺失项");
    }
    {
        // 仅给 _URL：其余散字段缺省也应成功解析（证明 _URL 承担基底作用）。
        let _guard = EnvGuard::new(&[(
            ENV_URL,
            "postgres://urluser:urlsecret@127.0.0.1:5433/urldb?sslmode=disable",
        )]);
        let config = PostgresConfig::from_env().expect("URL 基底");
        assert_eq!(config.host, "127.0.0.1");
        assert_eq!(config.port, 5433);
        assert_eq!(config.database, "urldb");
        assert_eq!(config.user, "urluser");
        assert!(config.has_password());
    }
    {
        // 散字段覆盖 _URL 对应项。
        let _guard = EnvGuard::new(&[
            (
                ENV_URL,
                "postgres://urluser:urlsecret@127.0.0.1:5433/urldb?sslmode=disable",
            ),
            (ENV_DATABASE, "override_db"),
            (ENV_PORT, "6000"),
        ]);
        let config = PostgresConfig::from_env().expect("URL + 覆盖");
        assert_eq!(config.database, "override_db");
        assert_eq!(config.port, 6000);
        assert_eq!(config.user, "urluser", "未覆盖的字段仍取自 _URL");
        assert!(!format!("{config:?}").contains("urlsecret"), "密码不得外泄");
    }
}

/// `PostgresConfig::from_toml`：严格解析，未知键与 `password` 一律 fail-closed。
#[test]
fn from_toml_strict_keys() {
    let text = r#"
host = "127.0.0.1"
database = "app"
user = "app"
sslmode = "disable"
acquire_timeout_ms = 3000
operation_timeout_ms = 8000
"#;
    let config = PostgresConfig::from_toml(text).expect("合法 TOML");
    assert_eq!(config.acquire_timeout, Duration::from_millis(3000));
    assert_eq!(config.operation_timeout, Duration::from_millis(8000));

    // 误写的键名不得静默回落默认值。
    let typo = r#"
host = "127.0.0.1"
database = "app"
user = "app"
acquire_timout_ms = 500
"#;
    let error = PostgresConfig::from_toml(typo).expect_err("未知键必须报错");
    assert!(matches!(error, PostgresError::Config(_)));

    // TOML 中夹带 password 必须拒绝（密码只能经 env / URL / builder 注入）。
    let with_password = r#"
host = "127.0.0.1"
database = "app"
user = "app"
password = "leaked"
"#;
    let error = PostgresConfig::from_toml(with_password).expect_err("password 必须拒绝");
    assert!(error.to_string().contains("TOML 解析失败"));
}

/// `PostgresConfig::validate`：非 loopback 主机强制 `sslmode=require`。
#[test]
fn validate_remote_requires_ssl() {
    for mode in [SslMode::Disable, SslMode::Prefer] {
        let error = PostgresConfig::builder()
            .host("db.example.com")
            .database("db")
            .user("u")
            .sslmode(mode)
            .build()
            .expect_err("远程明文必须失败");
        assert!(matches!(error, PostgresError::Config(_)));
    }
    PostgresConfig::builder()
        .host("db.example.com")
        .database("db")
        .user("u")
        .sslmode(SslMode::Require)
        .build()
        .expect("远程 require 放行");
    // 本机 loopback / Unix socket 不强制 TLS。
    for host in ["127.0.0.1", "localhost", "[::1]", "/var/run/postgresql"] {
        PostgresConfig::builder()
            .host(host)
            .database("db")
            .user("u")
            .sslmode(SslMode::Disable)
            .build()
            .unwrap_or_else(|error| panic!("本机地址不应要求 TLS: {host} -> {error}"));
    }
}

/// `PostgresPool::connect`：不可达地址必须报错，且分类为可重试。
#[tokio::test]
async fn connect_refused_is_retryable() {
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        PostgresPool::connect(refused_config()),
    )
    .await
    .expect("connect 必须受内部超时约束，不得悬挂");
    let error = result.expect_err("不可达地址不得连接成功");
    assert!(error.is_retryable(), "连接失败应可重试: {error}");
}

/// `PostgresPool::acquire`：零 deadline 是本地配置错误，不触碰网络。
#[tokio::test]
async fn acquire_rejects_zero_deadline() {
    let pool = PostgresPool::new(refused_config()).expect("建池（不联网）");
    let error = pool
        .acquire_with(Duration::ZERO)
        .await
        .expect_err("零 deadline 必须拒绝");
    assert!(matches!(error, PostgresError::Config(_)), "{error}");
    pool.close();
}

/// `PostgresPool::execute`：池关闭后所有 SQL 入口返回连接类错误。
#[tokio::test]
async fn execute_on_closed_pool_fails() {
    let pool = PostgresPool::new(refused_config()).expect("建池（不联网）");
    pool.close();
    let error = pool
        .execute("SELECT 1", &[])
        .await
        .expect_err("关闭后不得执行");
    assert!(matches!(error, PostgresError::Connection(_)), "{error}");
}

/// `PostgresPool::query`：池关闭后查询同样拒绝，而不是返回空结果集。
#[tokio::test]
async fn query_on_closed_pool_fails() {
    let pool = PostgresPool::new(refused_config()).expect("建池（不联网）");
    pool.close();
    let error = pool
        .query("SELECT 1", &[])
        .await
        .expect_err("关闭后不得查询");
    assert!(matches!(error, PostgresError::Connection(_)), "{error}");
}

/// `PostgresPool::with_transaction`：取连接失败时不得执行闭包。
#[tokio::test]
async fn with_transaction_on_closed_pool_fails() {
    let pool = PostgresPool::new(refused_config()).expect("建池（不联网）");
    pool.close();
    let error = pool
        .with_transaction(|_tx| Box::pin(async move { Ok::<(), PostgresError>(()) }))
        .await
        .expect_err("关闭后不得开启事务");
    assert!(matches!(error, PostgresError::Connection(_)), "{error}");
}

/// `PostgresPool::ping`：探活失败必须报错，不得伪装成功。
#[tokio::test]
async fn ping_on_closed_pool_fails() {
    let pool = PostgresPool::new(refused_config()).expect("建池（不联网）");
    pool.close();
    let error = pool.ping().await.expect_err("关闭后 ping 必须失败");
    assert!(matches!(error, PostgresError::Connection(_)), "{error}");
}

/// `PostgresPool::health_check`：关闭后必须报错，而不是返回陈旧快照。
#[tokio::test]
async fn health_check_on_closed_pool_fails() {
    let pool = PostgresPool::new(refused_config()).expect("建池（不联网）");
    pool.close();
    let error = pool
        .health_check()
        .await
        .expect_err("关闭后 health_check 必须失败");
    assert!(matches!(error, PostgresError::Connection(_)), "{error}");
}

/// `Migrator::verify`：checksum mismatch / 未知版本 fail-closed；不可达库直接报错，
/// 不会静默「校验通过」。
#[tokio::test]
async fn migrator_verify_fails_closed_on_mismatch() {
    // 纯逻辑面：mismatch 与未知版本都必须令启动校验失败。
    let plan = vec![
        Migration::new(1, "a", "CREATE TABLE a (id int);").expect("迁移"),
        Migration::new(2, "b", "CREATE TABLE b (id int);").expect("迁移"),
    ];
    let applied = vec![
        AppliedMigration {
            version: 1,
            name: "a".into(),
            checksum: "deadbeef".into(),
        },
        AppliedMigration {
            version: 99,
            name: "orphan".into(),
            checksum: "x".into(),
        },
    ];
    let status = MigrationStatus::compute(applied, &plan);
    assert!(!status.is_boot_ok(), "mismatch/未知版本不得放行启动");
    assert!(
        ensure_boot_ok(&status).is_err(),
        "ensure_boot_ok 必须与 verify 同一判定"
    );

    // 网络面：不可达库上 verify 必须报错，绝不返回「已同步」。
    let pool = PostgresPool::new(refused_config()).expect("建池（不联网）");
    let migrator = Migrator::new(pool.clone(), plan).expect("执行器");
    assert_eq!(migrator.plan()[0].version, 1, "计划应按 version 升序");
    let result = tokio::time::timeout(Duration::from_secs(10), migrator.verify())
        .await
        .expect("verify 必须受内部超时约束");
    let error = result.expect_err("不可达库上 verify 必须失败");
    assert!(error.is_retryable() || matches!(error, PostgresError::Connection(_)));
    pool.close();
}

/// `PostgresError::is_retryable`：只有瞬时类（连接 / 序列化 / 超时 / I/O）可重试。
#[test]
fn error_is_retryable_classification() {
    for code in ["08006", "40001", "40P01", "53300", "55P03", "57P01"] {
        assert!(
            error_from_sqlstate(code, "transient").is_retryable(),
            "{code} 应可重试"
        );
    }
    for code in ["23505", "42P01", "57014", "99999", "23503"] {
        assert!(
            !error_from_sqlstate(code, "permanent").is_retryable(),
            "{code} 不得自动重试"
        );
    }
    assert!(!PostgresError::Config(String::new()).is_retryable());
    assert!(PostgresError::Timeout(String::new()).is_retryable());
}
