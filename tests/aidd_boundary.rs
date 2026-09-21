#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable
)]
//! AIDD 对抗 / 边界用例（特性 002）。
//!
//! 候选由 AI 生成，逐条人工复核后仅保留「结论=保留」项；丢弃项登记于 PR 描述。
//! 全部离线，不依赖真实 PostgreSQL。
//!
//! // AIDD: toml_password_key_rejected | 来源=AI | 复核=ZoneCNH/2026-09-22 | 依据=标准.md §2 密码不是公开字段（TOML 拒绝） | 结论=保留
//! // AIDD: url_ipv6_trailing_junk_rejected | 来源=AI | 复核=ZoneCNH/2026-09-22 | 依据=标准.md §2 URL 解析 fail-loud | 结论=保留
//! // AIDD: url_percent_encoding_and_unicode | 来源=AI | 复核=ZoneCNH/2026-09-22 | 依据=标准.md §2 四入口等价与转义 | 结论=保留
//! // AIDD: migration_name_length_boundary | 来源=AI | 复核=ZoneCNH/2026-09-22 | 依据=标准.md §4 迁移元数据校验 | 结论=保留
//! // AIDD: unknown_sqlstate_never_retryable | 来源=AI | 复核=ZoneCNH/2026-09-22 | 依据=标准.md §4 未识别 SQLSTATE 不猜测可重试 | 结论=保留
//! // AIDD: sqlstate_message_with_control_chars | 来源=AI | 复核=ZoneCNH/2026-09-22 | 依据=标准.md §3 不因不可信输入 panic | 结论=保留
//! // AIDD: host_whitespace_and_unicode | 来源=AI | 复核=ZoneCNH/2026-09-22 | 依据=标准.md §3 主机/必填字段校验 | 结论=保留
//! // AIDD: double_close_is_idempotent | 来源=AI | 复核=ZoneCNH/2026-09-22 | 依据=标准.md §3 超时/关闭时连接脱池 | 结论=保留

use std::time::Duration;

use postgresx::{
    error_from_sqlstate, error_kind_from_sqlstate, AppliedMigration, ErrorKind, Migration,
    MigrationStatus, PostgresConfig, PostgresError, PostgresPool, SslMode,
};

/// 边界：TOML 里夹带 `password`——凭据走私必须 fail-closed，不得构造出配置。
///
/// 已知偏差（2026-09-22 实测并已上报，按「测试不改生产代码」口径保留）：`toml` crate 的
/// 解析错误 `Display` 会回显出错源码行，故本用例不断言「错误消息不含凭据」——
/// 当前 `PostgresConfig::from_toml` 会把该行原文带进 `PostgresError::Config` 消息。
#[test]
fn toml_password_key_rejected() {
    let text = r#"
host = "127.0.0.1"
database = "app"
user = "app"
password = "smuggled-secret"
"#;
    let error = PostgresConfig::from_toml(text).expect_err("TOML 中的 password 必须被拒");
    assert!(matches!(error, PostgresError::Config(_)));

    // 去掉凭据键后同一份 TOML 正常解析：拒绝来自 `password` 键本身，
    // 且 TOML 通道无法把密码注入配置（密码只能经 env / URL / builder）。
    let clean = r#"
host = "127.0.0.1"
database = "app"
user = "app"
sslmode = "disable"
"#;
    let config = PostgresConfig::from_toml(clean).expect("无凭据键的 TOML");
    assert_eq!(config.database, "app");
    assert!(!format!("{config:?}").contains("smuggled-secret"));
}

/// 边界：URL 的 IPv6 段后夹带垃圾（`]junk` / 非数字端口 / 缺 `]`）——一律 fail-loud。
#[test]
fn url_ipv6_trailing_junk_rejected() {
    for bad in [
        "postgres://u@[::1]junk/db",
        "postgres://u@[::1]:notaport/db",
        "postgres://u@[::1/db",
        "postgres://host:notaport/db",
    ] {
        assert!(
            PostgresConfig::from_url(bad).is_err(),
            "畸形 URL 必须拒绝: {bad}"
        );
    }
    let ok = PostgresConfig::from_url("postgres://u@[::1]:6543/db").expect("合法 IPv6");
    assert_eq!(ok.host, "::1");
    assert_eq!(ok.port, 6543);
}

/// 边界：URL 百分号转义（含越界 `%`、非法十六进制）与内嵌凭据不得泄漏。
#[test]
fn url_percent_encoding_and_unicode() {
    let config = PostgresConfig::from_url(
        "postgres://%E7%94%A8%E6%88%B7:p%40ss@db.example.com:5432/%E5%BA%93?sslmode=require",
    )
    .expect("转义 URL");
    assert_eq!(config.user, "用户");
    assert_eq!(config.database, "库");
    assert!(config.has_password());
    assert!(!format!("{config:?}").contains("p@ss"), "密码不得外泄");

    for bad in ["postgres://u:p%ZZ@h/db", "postgres://u:p%@h/db"] {
        assert!(PostgresConfig::from_url(bad).is_err(), "非法转义必须拒绝");
    }
}

/// 边界：迁移名长度阈值（≤256 通过，257 拒绝）与空/零版本。
#[test]
fn migration_name_length_boundary() {
    assert!(Migration::new(1, "n".repeat(256), "SELECT 1").is_ok());
    assert!(Migration::new(1, "n".repeat(257), "SELECT 1").is_err());
    assert!(Migration::new(0, "n", "SELECT 1").is_err());
    assert!(Migration::new(1, "   ", "SELECT 1").is_err());
    assert!(Migration::new(1, "n", "   ").is_err());
    // 超长迁移名同时不得 panic。
    let error = Migration::new(1, "n".repeat(10_000), "SELECT 1").expect_err("过长");
    assert!(error.to_string().contains("过长"));
}

/// 边界：未知 / 空 SQLSTATE 一律回落 Internal，绝不猜测为可重试。
#[test]
fn unknown_sqlstate_never_retryable() {
    for code in ["", "99999", "ZZZZZ", "abc", "XX000"] {
        let kind = error_kind_from_sqlstate(code);
        assert!(
            matches!(kind, ErrorKind::Internal),
            "{code} 应为 Internal，实际 {kind:?}"
        );
        assert!(!error_from_sqlstate(code, "unknown").is_retryable());
    }
}

/// 边界：SQLSTATE 消息含控制字符 / 换行 / 超长——分类与构造不得 panic、不得丢失码字面量。
#[test]
fn sqlstate_message_with_control_chars() {
    let error = error_from_sqlstate("23505", "dup\n\r\x00\x1b[31m\u{202e}evil".to_string());
    assert!(matches!(error, PostgresError::Conflict(_)));
    assert!(error.to_string().contains("23505"));
    let long = error_from_sqlstate("08006", "x".repeat(100_000));
    assert!(long.is_retryable());
}

/// 边界：主机为空白 / Unicode / 超长——空白 fail-closed；非 ASCII 与超长不 panic。
#[test]
fn host_whitespace_and_unicode() {
    for blank in [" ", "   ", "\t\n"] {
        assert!(
            PostgresConfig::builder()
                .host(blank)
                .database("d")
                .user("u")
                .sslmode(SslMode::Require)
                .build()
                .is_err(),
            "空白主机必须拒绝"
        );
    }
    // 非 ASCII 主机：本地校验不做 DNS 解析，带 require 时可通过（连通性由运行期承担）。
    PostgresConfig::builder()
        .host("数据库.example.com")
        .database("d")
        .user("u")
        .sslmode(SslMode::Require)
        .build()
        .expect("非 ASCII 主机本地校验不 panic");
    // 超长主机名：不得 panic。
    let long_host = format!("{}.example.com", "a".repeat(5_000));
    let _ = PostgresConfig::builder()
        .host(long_host)
        .database("d")
        .user("u")
        .sslmode(SslMode::Require)
        .build();
}

/// 边界：重复 `close()` 幂等，关闭后所有入口持续拒绝（不得因二次关闭改变语义）。
#[tokio::test]
async fn double_close_is_idempotent() {
    let pool = PostgresPool::new(
        PostgresConfig::builder()
            .host("127.0.0.1")
            .port(1)
            .database("d")
            .user("u")
            .sslmode(SslMode::Disable)
            .connect_timeout(Duration::from_millis(200))
            .acquire_timeout(Duration::from_millis(200))
            .build()
            .expect("配置"),
    )
    .expect("建池（不联网）");
    pool.close();
    pool.close();
    assert!(pool.stats().closed);
    assert!(pool.ping().await.is_err());
    assert!(pool.execute("SELECT 1", &[]).await.is_err());
}

/// 边界：checksum 对不可见字符敏感——尾随空白 / 换行不得被视为同一版本。
#[test]
fn checksum_sensitive_to_invisible_chars() {
    let base = Migration::new(1, "a", "CREATE TABLE t (id int);").expect("迁移");
    for variant in [
        "CREATE TABLE t (id int); ",
        "CREATE TABLE t (id int);\n",
        "CREATE  TABLE t (id int);",
    ] {
        let other = Migration::new(1, "a", variant).expect("迁移");
        assert_ne!(base.checksum(), other.checksum(), "变体={variant:?}");
    }
    // 同步状态与 mismatch 判定与 checksum 一致。
    let status = MigrationStatus::compute(
        vec![AppliedMigration {
            version: 1,
            name: "a".into(),
            checksum: base.checksum(),
        }],
        std::slice::from_ref(&base),
    );
    assert!(status.is_clean());
}
