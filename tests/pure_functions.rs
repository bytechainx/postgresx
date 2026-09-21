#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable
)]
//! 纯函数行为：SQLSTATE 映射、重试判定、事务状态、迁移 checksum 与状态计算。

use std::time::Duration;

use postgresx::{
    ensure_boot_ok, error_from_sqlstate, error_kind_from_sqlstate, host_is_local,
    with_retry_async_no_wait, with_retry_sync, AppliedMigration, ChecksumMismatch, ErrorKind,
    Migration, MigrationStatus, PgRetryConfig, PostgresError, SslMode, TxStatus,
};

#[test]
fn sqlstate_covers_each_documented_class() {
    let cases: &[(&str, ErrorKind)] = &[
        // Class 08 — 连接类
        ("08000", ErrorKind::Unavailable),
        ("08006", ErrorKind::Unavailable),
        ("08003", ErrorKind::Unavailable),
        // Class 22 — 数据异常
        ("22012", ErrorKind::Invalid),
        ("22P02", ErrorKind::Invalid),
        // Class 23 — 完整性约束
        ("23505", ErrorKind::Conflict),
        ("23503", ErrorKind::Invalid),
        ("23502", ErrorKind::Invalid),
        ("23514", ErrorKind::Invalid),
        ("23000", ErrorKind::Conflict),
        // Class 25 — 事务状态
        ("25P02", ErrorKind::Invariant),
        // Class 28 / 3D — 认证 / 库名
        ("28P01", ErrorKind::Invalid),
        ("28000", ErrorKind::Invalid),
        ("3D000", ErrorKind::Invalid),
        // Class 40 — 序列化 / 死锁
        ("40001", ErrorKind::Serialization),
        ("40P01", ErrorKind::Serialization),
        // Class 42 — 语法与访问规则
        ("42P01", ErrorKind::Missing),
        ("42704", ErrorKind::Missing),
        ("42703", ErrorKind::Invalid),
        ("42601", ErrorKind::Invalid),
        ("42501", ErrorKind::Invalid),
        ("42000", ErrorKind::Invalid),
        // Class 53 / 55 — 资源与锁
        ("53300", ErrorKind::Transient),
        ("53200", ErrorKind::Transient),
        ("55P03", ErrorKind::Transient),
        ("55006", ErrorKind::Transient),
        // Class 57 — 运维干预
        ("57014", ErrorKind::Cancelled),
        ("57P01", ErrorKind::Unavailable),
        ("57P02", ErrorKind::Unavailable),
        ("57P03", ErrorKind::Unavailable),
        ("57000", ErrorKind::Unavailable),
        // Class 58 / XX / P0001
        ("58030", ErrorKind::Unavailable),
        ("XX000", ErrorKind::Internal),
        ("P0001", ErrorKind::Invalid),
        // 未知
        ("99999", ErrorKind::Internal),
        ("", ErrorKind::Internal),
    ];
    for (code, expected) in cases {
        assert_eq!(error_kind_from_sqlstate(code), *expected, "SQLSTATE {code}");
    }
}

#[test]
fn error_kind_retry_classification_is_documented() {
    assert!(ErrorKind::Serialization.is_retryable());
    assert!(ErrorKind::Transient.is_retryable());
    assert!(ErrorKind::Unavailable.is_retryable());
    assert!(ErrorKind::DeadlineExceeded.is_retryable());
    assert!(!ErrorKind::Invalid.is_retryable());
    assert!(!ErrorKind::Missing.is_retryable());
    assert!(!ErrorKind::Conflict.is_retryable());
    assert!(!ErrorKind::Cancelled.is_retryable());
    assert!(!ErrorKind::Invariant.is_retryable());
    assert!(!ErrorKind::Internal.is_retryable());
}

#[test]
fn postgres_error_mapping_and_retryability() {
    // 可重试：40001 / 40P01 / 08xxx / 57P01 / 53300 / 55P03
    for code in ["40001", "40P01", "08006", "57P01", "53300", "55P03"] {
        let error = error_from_sqlstate(code, "retry");
        assert!(error.is_retryable(), "{code} 应可重试: {error}");
    }
    // 不可重试：唯一键冲突 / 缺失 / 语法 / 取消 / 未知
    for code in ["23505", "42P01", "42601", "57014", "99999"] {
        let error = error_from_sqlstate(code, "no retry");
        assert!(!error.is_retryable(), "{code} 不应重试: {error}");
    }

    assert!(matches!(
        error_from_sqlstate("23505", "dup"),
        PostgresError::Conflict(_)
    ));
    assert!(matches!(
        error_from_sqlstate("42P01", "missing"),
        PostgresError::Missing(_)
    ));
    assert!(matches!(
        error_from_sqlstate("40001", "serialize"),
        PostgresError::Serialization(_)
    ));
    assert!(matches!(
        error_from_sqlstate("08006", "conn"),
        PostgresError::Connection(_)
    ));
    assert!(matches!(
        error_from_sqlstate("42601", "syntax"),
        PostgresError::Backend(_)
    ));

    // 分类本地错误的重试语义
    assert!(!PostgresError::Config("x".into()).is_retryable());
    assert!(!PostgresError::Unsupported("x".into()).is_retryable());
    assert!(PostgresError::Timeout("x".into()).is_retryable());
    assert!(
        PostgresError::Io(std::io::Error::new(std::io::ErrorKind::TimedOut, "io")).is_retryable()
    );
}

#[test]
fn tx_status_state_machine() {
    // 四种状态互不相等
    let states = [
        TxStatus::Active,
        TxStatus::Committed,
        TxStatus::RolledBack,
        TxStatus::Failed,
    ];
    for (index, left) in states.iter().enumerate() {
        for (other_index, right) in states.iter().enumerate() {
            assert_eq!(index == other_index, left == right);
        }
    }

    // 终结态：COMMIT / ROLLBACK 之后不可再操作
    assert!(TxStatus::Committed.is_finished());
    assert!(TxStatus::RolledBack.is_finished());
    assert!(!TxStatus::Active.is_finished());
    // Failed 是「rollback-only」：未终结，但不可继续执行 SQL
    assert!(!TxStatus::Failed.is_finished());

    assert_eq!(TxStatus::Active.as_str(), "active");
    assert_eq!(TxStatus::Committed.as_str(), "committed");
    assert_eq!(TxStatus::RolledBack.as_str(), "rolled_back");
    assert_eq!(TxStatus::Failed.as_str(), "failed");

    // Copy 语义：状态可自由快照
    let snapshot = TxStatus::Active;
    let moved = snapshot;
    assert_eq!(snapshot, moved);
}

#[test]
fn migration_checksum_is_sha256_of_sql() {
    let migration =
        Migration::new(1, "create_table", "CREATE TABLE t (id BIGINT PRIMARY KEY);").expect("迁移");
    let checksum = migration.checksum();
    assert_eq!(checksum.len(), 64);
    assert!(checksum.chars().all(|c| c.is_ascii_hexdigit()));
    assert_eq!(checksum, migration.checksum(), "同一条迁移 checksum 稳定");
    // SHA-256 已知值（`abc` 的十六进制摘要前 8 位）
    let abc = Migration::new(1, "abc", "abc").expect("迁移");
    assert!(abc.checksum().starts_with("ba7816bf"));

    let reworded =
        Migration::new(1, "create_table", "CREATE TABLE t (id INT PRIMARY KEY);").expect("迁移");
    assert_ne!(checksum, reworded.checksum());
    assert!(Migration::new(0, "v0", "SELECT 1").is_err());
    assert!(Migration::new(1, "", "SELECT 1").is_err());
    assert!(Migration::new(1, "n", "   ").is_err());
}

#[test]
fn migration_status_detects_checksum_mismatch_and_unknown_versions() {
    let plan = vec![
        Migration::new(1, "a", "CREATE TABLE a (id int);").expect("迁移"),
        Migration::new(2, "b", "CREATE TABLE b (id int);").expect("迁移"),
    ];
    let synced = vec![AppliedMigration {
        version: 1,
        name: "a".into(),
        checksum: plan[0].checksum(),
    }];

    let status = MigrationStatus::compute(synced.clone(), &plan);
    assert!(status.mismatches.is_empty());
    assert_eq!(status.pending, vec![2]);
    assert!(status.unknown_applied.is_empty());
    assert!(status.is_boot_ok(), "pending 不影响启动");
    assert!(!status.is_clean(), "有 pending 时不是完全同步");
    ensure_boot_ok(&status).expect("启动放行");

    let mut drifted = synced.clone();
    drifted[0].checksum = "deadbeef".to_string();
    let status = MigrationStatus::compute(drifted, &plan);
    assert_eq!(
        status.mismatches,
        vec![ChecksumMismatch {
            version: 1,
            expected: plan[0].checksum(),
            actual: "deadbeef".to_string(),
        }]
    );
    assert!(!status.is_boot_ok());
    let error = ensure_boot_ok(&status).expect_err("checksum 漂移必须 fail-closed");
    assert!(matches!(error, PostgresError::Conflict(_)));
    assert!(error.to_string().contains("mismatches"));

    let orphan = vec![
        AppliedMigration {
            version: 1,
            name: "a".into(),
            checksum: plan[0].checksum(),
        },
        AppliedMigration {
            version: 99,
            name: "orphan".into(),
            checksum: "x".into(),
        },
    ];
    let status = MigrationStatus::compute(orphan, &plan);
    assert_eq!(status.unknown_applied, vec![99]);
    assert!(!status.is_boot_ok());

    let complete = vec![
        AppliedMigration {
            version: 1,
            name: "a".into(),
            checksum: plan[0].checksum(),
        },
        AppliedMigration {
            version: 2,
            name: "b".into(),
            checksum: plan[1].checksum(),
        },
    ];
    let status = MigrationStatus::compute(complete, &plan);
    assert!(status.is_clean());
    assert!(status.is_boot_ok());
}

#[test]
fn retry_delay_and_attempts_are_deterministic() {
    let exponential =
        PgRetryConfig::exponential(4, Duration::from_millis(100), Duration::from_millis(300));
    assert_eq!(exponential.delay_for_attempt(1), Duration::from_millis(100));
    assert_eq!(exponential.delay_for_attempt(2), Duration::from_millis(200));
    assert_eq!(exponential.delay_for_attempt(3), Duration::from_millis(300));

    let fixed = PgRetryConfig::fixed(2, Duration::ZERO);
    let mut calls = 0_u32;
    let error = with_retry_sync(&fixed, "pg", || {
        calls += 1;
        Err::<(), _>(error_from_sqlstate("08006", "连接断开"))
    })
    .expect_err("重试耗尽");
    assert!(error.is_retryable());
    assert_eq!(calls, 2, "max_attempts 为总尝试次数");
}

#[test]
fn retry_stops_immediately_on_non_retryable_error() {
    let config = PgRetryConfig::fixed(5, Duration::ZERO);
    let mut calls = 0_u32;
    let error = with_retry_sync(&config, "pg", || {
        calls += 1;
        Err::<(), _>(error_from_sqlstate("23505", "唯一键冲突"))
    })
    .expect_err("不可重试");
    assert_eq!(calls, 1);
    assert!(matches!(error, PostgresError::Conflict(_)));
}

#[test]
fn retry_deadline_produces_timeout() {
    let config =
        PgRetryConfig::fixed(10, Duration::from_millis(50)).with_deadline(Duration::from_millis(5));
    let error = with_retry_sync(&config, "pg", || {
        Err::<(), _>(error_from_sqlstate("08006", "断连"))
    })
    .expect_err("总预算耗尽");
    assert!(matches!(error, PostgresError::Timeout(_)));
    assert!(error.is_retryable());
}

#[tokio::test]
async fn async_no_wait_retry_ignores_backoff() {
    // 退避 30s 但 no_wait 立即重试：若真的等待，本测试会超时
    let config = PgRetryConfig::fixed(4, Duration::from_secs(30));
    let mut calls = 0_u32;
    let value = tokio::time::timeout(
        Duration::from_secs(2),
        with_retry_async_no_wait(&config, "pg", || {
            let attempt = {
                calls += 1;
                calls
            };
            async move {
                if attempt < 3 {
                    Err(error_from_sqlstate("08006", "连接重置"))
                } else {
                    Ok(attempt)
                }
            }
        }),
    )
    .await
    .expect("no_wait 不应等待退避")
    .expect("第三次成功");
    assert_eq!(value, 3);
    assert_eq!(calls, 3);
}

#[test]
fn ssl_mode_parse_and_host_classification() {
    assert_eq!(
        SslMode::parse("disable").expect("disable"),
        SslMode::Disable
    );
    assert_eq!(SslMode::parse("0").expect("0"), SslMode::Disable);
    assert_eq!(SslMode::parse("allow").expect("allow"), SslMode::Prefer);
    assert_eq!(SslMode::parse("prefer").expect("prefer"), SslMode::Prefer);
    assert_eq!(
        SslMode::parse("verify-full").expect("verify-full"),
        SslMode::Require
    );
    assert_eq!(
        SslMode::parse("REQUIRE").expect("REQUIRE"),
        SslMode::Require
    );
    assert!(SslMode::parse("unknown").is_err());
    assert_eq!(SslMode::default(), SslMode::Disable);

    assert!(host_is_local("127.0.0.1"));
    assert!(host_is_local("127.0.0.53"));
    assert!(host_is_local("localhost"));
    assert!(host_is_local("LOCALHOST"));
    assert!(host_is_local("::1"));
    assert!(host_is_local("[::1]"));
    assert!(host_is_local("/var/run/postgresql"));
    assert!(!host_is_local("10.0.0.9"));
    assert!(!host_is_local("db.example.com"));
}
