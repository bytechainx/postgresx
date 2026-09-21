#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable
)]
//! 配置：校验正/反用例、环境变量与 URL 解析、密码脱敏、默认值。

use std::sync::Mutex;
use std::time::Duration;

use postgresx::{
    PostgresConfig, PostgresError, SslMode, DEFAULT_MAX_POOL_SIZE, DEFAULT_PORT,
    ENV_ACQUIRE_TIMEOUT_MS, ENV_APPLICATION_NAME, ENV_CONNECT_TIMEOUT_MS, ENV_DATABASE, ENV_HOST,
    ENV_MAX_POOL_SIZE, ENV_OPERATION_TIMEOUT_MS, ENV_PASSWORD, ENV_PORT, ENV_SSLMODE,
    ENV_TLS_CA_FILE, ENV_TLS_CLIENT_CERT, ENV_TLS_CLIENT_KEY, ENV_TLS_SERVER_NAME, ENV_URL,
    ENV_USER,
};

/// 环境变量是进程级共享状态，用互斥锁串行化修改。
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// 所有本 crate 会读取的环境变量；每个用例进入与退出时都清空，避免相互串味。
const MANAGED: &[&str] = &[
    ENV_URL,
    ENV_HOST,
    ENV_PORT,
    ENV_DATABASE,
    ENV_USER,
    ENV_PASSWORD,
    ENV_SSLMODE,
    ENV_MAX_POOL_SIZE,
    ENV_APPLICATION_NAME,
    ENV_CONNECT_TIMEOUT_MS,
    ENV_ACQUIRE_TIMEOUT_MS,
    ENV_OPERATION_TIMEOUT_MS,
    ENV_TLS_CA_FILE,
    ENV_TLS_SERVER_NAME,
    ENV_TLS_CLIENT_CERT,
    ENV_TLS_CLIENT_KEY,
];

struct EnvGuard<'a> {
    _lock: std::sync::MutexGuard<'a, ()>,
}

impl<'a> EnvGuard<'a> {
    fn new(vars: &[(&str, &str)]) -> Self {
        let lock = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        clear_managed();
        for (key, value) in vars {
            std::env::set_var(key, value);
        }
        Self { _lock: lock }
    }
}

impl Drop for EnvGuard<'_> {
    fn drop(&mut self) {
        clear_managed();
    }
}

fn clear_managed() {
    for key in MANAGED {
        std::env::remove_var(key);
    }
}

#[test]
fn defaults_match_documented_values() {
    let config = PostgresConfig::default();
    assert_eq!(config.port, DEFAULT_PORT);
    assert_eq!(config.max_pool_size, DEFAULT_MAX_POOL_SIZE);
    assert_eq!(config.sslmode, SslMode::Disable);
    assert_eq!(config.host, "127.0.0.1");
    assert_eq!(config.acquire_timeout, Duration::from_secs(5));
    assert_eq!(config.operation_timeout, Duration::from_secs(10));
    assert_eq!(config.connect_timeout, Some(Duration::from_secs(10)));
    assert!(!config.has_password());
}

#[test]
fn builder_defaults_port_and_pool_size() {
    let config = PostgresConfig::builder()
        .host("127.0.0.1")
        .database("db")
        .user("u")
        .build()
        .expect("最小配置");
    assert_eq!(config.port, DEFAULT_PORT);
    assert_eq!(config.max_pool_size, DEFAULT_MAX_POOL_SIZE);
}

#[test]
fn validate_rejects_invalid_combinations() {
    let cases = [
        (
            "缺少 host",
            PostgresConfig::builder().database("db").user("u").build(),
        ),
        (
            "空 host",
            PostgresConfig::builder()
                .host("   ")
                .database("db")
                .user("u")
                .build(),
        ),
        (
            "缺少 database",
            PostgresConfig::builder()
                .host("127.0.0.1")
                .user("u")
                .build(),
        ),
        (
            "端口为 0",
            PostgresConfig::builder()
                .host("127.0.0.1")
                .database("db")
                .user("u")
                .port(0)
                .build(),
        ),
        (
            "池上限为 0",
            PostgresConfig::builder()
                .host("127.0.0.1")
                .database("db")
                .user("u")
                .max_pool_size(0)
                .build(),
        ),
        (
            "操作超时为 0",
            PostgresConfig::builder()
                .host("127.0.0.1")
                .database("db")
                .user("u")
                .operation_timeout(Duration::ZERO)
                .build(),
        ),
        (
            "mTLS 仅证书",
            PostgresConfig::builder()
                .host("127.0.0.1")
                .database("db")
                .user("u")
                .sslmode(SslMode::Require)
                .tls_client_cert("/tmp/only.crt")
                .build(),
        ),
    ];
    for (label, result) in cases {
        let error = result.expect_err(label);
        assert!(
            matches!(error, PostgresError::Config(_)),
            "{label}: {error}"
        );
    }
}

#[test]
fn validate_is_fail_closed_for_remote_plaintext() {
    for mode in [SslMode::Disable, SslMode::Prefer] {
        let error = PostgresConfig::builder()
            .host("db.example.com")
            .database("db")
            .user("u")
            .sslmode(mode)
            .build()
            .expect_err("远程非 TLS 必须拒绝");
        assert!(matches!(error, PostgresError::Config(_)));
    }
    let config = PostgresConfig::builder()
        .host("db.example.com")
        .database("db")
        .user("u")
        .sslmode(SslMode::Require)
        .build()
        .expect("远程 require 放行");
    assert_eq!(config.sslmode, SslMode::Require);
}

#[test]
fn password_is_redacted_in_debug() {
    let config = PostgresConfig::builder()
        .host("127.0.0.1")
        .database("db")
        .user("u")
        .password("SuperSecretValue")
        .build()
        .expect("配置");
    let debug = format!("{config:?}");
    assert!(debug.contains("***"));
    assert!(!debug.contains("SuperSecretValue"));
    assert!(config.has_password());

    let from_url =
        PostgresConfig::from_url("postgres://u:SuperSecretValue@127.0.0.1/db").expect("URL 配置");
    let debug = format!("{from_url:?}");
    assert!(!debug.contains("SuperSecretValue"));
}

#[test]
fn url_parsing_covers_common_forms() {
    let full = PostgresConfig::from_url(
        "postgres://app:p%40ssw0rd@db.example.com:6432/appdb?sslmode=require&application_name=svc&connect_timeout=7",
    )
    .expect("完整 URL");
    assert_eq!(full.host, "db.example.com");
    assert_eq!(full.port, 6432);
    assert_eq!(full.database, "appdb");
    assert_eq!(full.user, "app");
    assert!(full.has_password());
    assert_eq!(full.sslmode, SslMode::Require);
    assert_eq!(full.application_name.as_deref(), Some("svc"));
    assert_eq!(full.connect_timeout, Some(Duration::from_secs(7)));

    let minimal = PostgresConfig::from_url("postgresql://u@127.0.0.1/db").expect("最简 URL");
    assert_eq!(minimal.port, DEFAULT_PORT);
    assert_eq!(minimal.sslmode, SslMode::Disable);

    let ipv6 = PostgresConfig::from_url("postgres://u:p@[::1]:5433/db").expect("IPv6 URL");
    assert_eq!(ipv6.host, "::1");
    assert_eq!(ipv6.port, 5433);

    for bad in [
        "mysql://host/db",
        "postgres://host:not-a-port/db",
        "postgres://host/db?sslmode=wat",
        "postgres://host:5432/db?connect_timeout=abc",
    ] {
        assert!(
            PostgresConfig::from_url(bad).is_err(),
            "非法 URL 必须拒绝: {bad}"
        );
    }
}

#[test]
fn toml_parsing_and_env_password_injection() {
    let text = r#"
host = "127.0.0.1"
port = 6543
database = "app"
user = "app"
sslmode = "disable"
max_pool_size = 7
acquire_timeout_ms = 3000
operation_timeout_ms = 8000
"#;

    {
        let _guard = EnvGuard::new(&[]);
        let config = PostgresConfig::from_toml(text).expect("TOML");
        assert_eq!(config.port, 6543);
        assert_eq!(config.max_pool_size, 7);
        assert_eq!(config.acquire_timeout, Duration::from_millis(3000));
        assert_eq!(config.operation_timeout, Duration::from_millis(8000));
        assert!(!config.has_password(), "环境无密码时不应有密码");
    }

    {
        let _guard = EnvGuard::new(&[(ENV_PASSWORD, "from-env-secret")]);
        let config = PostgresConfig::from_toml(text).expect("TOML + env 密码");
        assert!(config.has_password());
        assert!(!format!("{config:?}").contains("from-env-secret"));
    }

    assert!(PostgresConfig::from_toml("host = \"127.0.0.1\"\nport = \"x\"").is_err());
    assert!(PostgresConfig::from_toml("not toml >>>").is_err());
}

#[test]
fn from_env_requires_core_fields() {
    {
        let _guard = EnvGuard::new(&[]);
        let error = PostgresConfig::from_env().expect_err("缺少必填环境变量");
        assert!(matches!(error, PostgresError::Config(_)));
        assert!(error.to_string().contains(ENV_HOST));
    }

    {
        let _guard = EnvGuard::new(&[
            (ENV_HOST, "127.0.0.1"),
            (ENV_DATABASE, "appdb"),
            (ENV_PORT, "6000"),
            (ENV_MAX_POOL_SIZE, "9"),
            (ENV_SSLMODE, "disable"),
        ]);
        // 缺少 user
        let error = PostgresConfig::from_env().expect_err("缺少 user");
        assert!(error.to_string().contains(ENV_USER));

        std::env::set_var(ENV_USER, "appuser");
        std::env::set_var(ENV_PASSWORD, "env-secret");
        let config = PostgresConfig::from_env().expect("完整环境变量");
        assert_eq!(config.host, "127.0.0.1");
        assert_eq!(config.database, "appdb");
        assert_eq!(config.user, "appuser");
        assert_eq!(config.port, 6000);
        assert_eq!(config.max_pool_size, 9);
        assert!(config.has_password());
        assert!(!format!("{config:?}").contains("env-secret"));
    }
}

#[test]
fn from_env_supports_url_base_with_overrides() {
    let _guard = EnvGuard::new(&[
        (
            ENV_URL,
            "postgres://urluser:urlsecret@127.0.0.1:5432/urldb?sslmode=disable",
        ),
        (ENV_DATABASE, "override_db"),
    ]);
    let config = PostgresConfig::from_env().expect("URL 基底 + 覆盖");
    assert_eq!(config.user, "urluser");
    assert_eq!(config.database, "override_db");
    assert_eq!(config.host, "127.0.0.1");
    assert!(config.has_password());
}
