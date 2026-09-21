//! `config` 模块单元测试：默认值与 Builder、URL/TOML 解析、TLS 与 fail-closed 校验。
//!
//! 由本模块的 `#[cfg(test)] mod tests;` 引入，仅在测试构建中编译。

#[cfg(test)]
use super::*;

#[test]
fn defaults_are_stable() {
    let config = PostgresConfig::default();
    assert_eq!(config.port, DEFAULT_PORT);
    assert_eq!(config.max_pool_size, DEFAULT_MAX_POOL_SIZE);
    assert_eq!(config.sslmode, SslMode::Disable);
    assert!(!config.has_password());
    assert_eq!(SslMode::Disable.as_str(), "disable");
    assert_eq!(SslMode::Prefer.as_str(), "prefer");
    assert_eq!(SslMode::Require.as_str(), "require");
}

#[test]
fn builder_roundtrip_and_redaction() {
    let config = PostgresConfig::builder()
        .host("127.0.0.1")
        .database("db")
        .user("u")
        .password("Sup3rSecret")
        .max_pool_size(4)
        .build()
        .expect("合法配置");
    assert!(config.has_password());
    assert_eq!(config.password(), "Sup3rSecret");
    let debug = format!("{config:?}");
    assert!(debug.contains("***"));
    assert!(!debug.contains("Sup3rSecret"));
}

#[test]
fn remote_requires_ssl() {
    for mode in [SslMode::Disable, SslMode::Prefer] {
        let error = PostgresConfig::builder()
            .host("db.example.com")
            .database("db")
            .user("user")
            .sslmode(mode)
            .build()
            .expect_err("远程非 require 必须失败");
        assert!(matches!(error, PostgresError::Config(_)));
    }
    PostgresConfig::builder()
        .host("db.example.com")
        .database("db")
        .user("user")
        .sslmode(SslMode::Require)
        .build()
        .expect("远程 require 应通过");
}

#[test]
fn local_hosts_skip_tls_requirement() {
    assert!(host_is_local("127.0.0.1"));
    assert!(host_is_local("localhost"));
    assert!(host_is_local("[::1]"));
    assert!(host_is_local("/var/run/postgresql"));
    assert!(!host_is_local("10.0.0.9"));
    assert!(!host_is_local("db.example.com"));
}

#[test]
fn invalid_configs_are_rejected() {
    assert!(PostgresConfig::builder()
        .database("db")
        .user("u")
        .build()
        .is_err());
    assert!(PostgresConfig::builder()
        .host(" ")
        .database("db")
        .user("u")
        .build()
        .is_err());
    assert!(PostgresConfig::builder()
        .host("127.0.0.1")
        .user("u")
        .build()
        .is_err());
    assert!(PostgresConfig::builder()
        .host("127.0.0.1")
        .database("db")
        .user("u")
        .port(0)
        .build()
        .is_err());
    assert!(PostgresConfig::builder()
        .host("127.0.0.1")
        .database("db")
        .user("u")
        .max_pool_size(0)
        .build()
        .is_err());
    assert!(PostgresConfig::builder()
        .host("127.0.0.1")
        .database("db")
        .user("u")
        .operation_timeout(Duration::ZERO)
        .build()
        .is_err());
}

#[test]
fn mtls_requires_pair() {
    let error = PostgresConfig::builder()
        .host("127.0.0.1")
        .database("db")
        .user("u")
        .sslmode(SslMode::Require)
        .tls_client_cert("/tmp/only.crt")
        .build()
        .expect_err("仅证书必须失败");
    assert!(error.to_string().contains("tls_client_cert"));
}

#[test]
fn url_sets_password_and_ssl() {
    let config = PostgresConfig::from_url(
        "postgres://user:p%40ss@db.example.com:6432/app?sslmode=require&application_name=svc",
    )
    .expect("URL 解析");
    assert_eq!(config.host, "db.example.com");
    assert_eq!(config.port, 6432);
    assert_eq!(config.database, "app");
    assert_eq!(config.user, "user");
    assert_eq!(config.password(), "p@ss");
    assert_eq!(config.sslmode, SslMode::Require);
    assert_eq!(config.application_name.as_deref(), Some("svc"));
}

#[test]
fn url_defaults_and_ipv6() {
    let config = PostgresConfig::from_url("postgresql://u@[::1]/db").expect("IPv6 解析");
    assert_eq!(config.host, "::1");
    assert_eq!(config.port, DEFAULT_PORT);
    assert_eq!(config.database, "db");
    assert!(config.password().is_empty());
}

#[test]
fn url_ipv6_parses_port_forms() {
    let with_port = PostgresConfig::from_url("postgres://u@[::1]:6543/db").expect("IPv6 带端口");
    assert_eq!(with_port.host, "::1");
    assert_eq!(with_port.port, 6543);

    // `[::1]:` 与主机名形式的 `host:` 行为一致：端口为空回落到默认端口。
    let empty_port = PostgresConfig::from_url("postgres://u@[::1]:/db").expect("空端口");
    assert_eq!(empty_port.host, "::1");
    assert_eq!(empty_port.port, DEFAULT_PORT);
}

/// `]` 之后只允许 `:port` 或直接结束。
///
/// 回归保护：此前该位置的非 `:` 文本会被**静默丢弃**——`postgres://u@[::1]junk/db`
/// 会被接受并解析成 `host=::1`（实测确认），而**同一个函数的 hostname 分支**对
/// `host:notaport` 会报错。两条分支行为不一致，且前者是静默的，属于「本地接受
/// 畸形输入、问题推迟到运行期」这一类。
#[test]
fn url_ipv6_rejects_trailing_junk() {
    assert!(PostgresConfig::from_url("postgres://u@[::1]junk/db").is_err());
    assert!(PostgresConfig::from_url("postgres://u@[::1]:notaport/db").is_err());
    // 缺少 `]` 同样必须报错。
    assert!(PostgresConfig::from_url("postgres://u@[::1/db").is_err());
}

#[test]
fn url_rejects_unknown_scheme() {
    assert!(PostgresConfig::from_url("mysql://host/db").is_err());
    assert!(PostgresConfig::from_url("postgres://host:notaport/db").is_err());
    assert!(PostgresConfig::from_url("postgres://host/db?sslmode=wat").is_err());
}

#[test]
fn toml_parses_non_secret_fields() {
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
    let config = PostgresConfig::from_toml(text).expect("TOML 解析");
    assert_eq!(config.port, 6543);
    assert_eq!(config.max_pool_size, 7);
    assert_eq!(config.acquire_timeout, Duration::from_millis(3000));
    assert_eq!(config.operation_timeout, Duration::from_millis(8000));
    assert!(!config.has_password());
}

#[test]
fn toml_rejects_password_key() {
    let text = r#"
host = "127.0.0.1"
database = "app"
user = "app"
password = "leaked"
"#;
    let error = PostgresConfig::from_toml(text).expect_err("TOML 中的 password 必须 fail-closed");
    assert!(matches!(error, PostgresError::Config(_)));
    assert!(error.to_string().contains("TOML 解析失败"));
}

#[test]
fn toml_rejects_unknown_keys() {
    let text = r#"
host = "127.0.0.1"
database = "app"
user = "app"
acquire_timout_ms = 500
"#;
    assert!(
        PostgresConfig::from_toml(text).is_err(),
        "拼写错误的键必须报错而不是静默使用默认值"
    );
}

#[test]
fn toml_invalid_values_fail_closed() {
    assert!(PostgresConfig::from_toml("host = ").is_err());
    assert!(PostgresConfig::from_toml("port = \"x\"").is_err());
    assert!(PostgresConfig::from_toml(
        "host = \"db.example.com\"\ndatabase = \"d\"\nuser = \"u\"\nsslmode = \"disable\""
    )
    .is_err());
}

#[test]
fn tls_server_name_with_ip_host_sets_hostaddr() {
    let config = PostgresConfig::builder()
        .host("84.247.154.45")
        .database("postgres")
        .user("postgres")
        .sslmode(SslMode::Require)
        .tls_server_name("db.internal")
        .build()
        .expect("配置");
    let deadpool_config = config.to_deadpool_config();
    assert_eq!(deadpool_config.host.as_deref(), Some("db.internal"));
    assert_eq!(
        deadpool_config.hostaddr,
        Some("84.247.154.45".parse().expect("ip")),
        "IP 必须走 hostaddr，SNI 走 host"
    );
}

#[test]
fn sslmode_parse_aliases() {
    assert_eq!(
        SslMode::parse(" DISABLE ").expect("disable"),
        SslMode::Disable
    );
    assert_eq!(SslMode::parse("allow").expect("allow"), SslMode::Prefer);
    assert_eq!(
        SslMode::parse("verify-full").expect("verify-full"),
        SslMode::Require
    );
    assert!(SslMode::parse("wat").is_err());
}
