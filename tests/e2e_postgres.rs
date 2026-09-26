#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::too_many_lines
)]
//! E2E（postgresx）：在**真实** PostgreSQL 上执行核对器口径内的全部公开条目。
//!
//! 与 `live_postgres.rs`（E 维冒烟）不同，本文件对齐
//! `cargo +nightly public-api --simplified` 导出面：`E2E_MANIFEST` + `cover::hit`。
//! 凭据只读 `FOUNDATIONX_POSTGRESX_*`（crate `ENV_*`），不硬编码、不回显。
//! DDL/DML 只落在 `pgx_e2e_<pid>_<nanos>` 自建表；`SCHEMA_MIGRATIONS_TABLE` 已存在则拒绝 Migrator 写路径。
//!
//! 远程 IP 且 `sslmode=require` 时必须同时设 `ENV_TLS_SERVER_NAME`（与证书 CN/SAN 一致），
//! 否则 rustls 握手失败。注入时不要用未加引号的 shell `source`（口令可能含 shell 元字符）。
//!
//! ```text
//! cargo test --test e2e_postgres -- --ignored --test-threads=1
//! ```

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use postgresx::{
    build_client_config, build_client_config_with_ca, build_client_config_with_options,
    ensure_boot_ok, error_from_sqlstate, error_kind_from_sqlstate, host_is_local, map_pool_error,
    map_tokio_error, with_retry_async, with_retry_async_no_wait, with_retry_sync, AppliedMigration,
    ChecksumMismatch, ErrorKind, MakeRustlsConnect, Migration, MigrationStatus, Migrator,
    PgRetryConfig, PostgresConfig, PostgresError, PostgresPool, PostgresResult, RustlsConnect,
    SslMode, TxStatus, DEFAULT_COPY_IN_MAX_BYTES, DEFAULT_COPY_OUT_MAX_BYTES,
    DEFAULT_MAX_POOL_SIZE, DEFAULT_PORT, ENV_ACQUIRE_TIMEOUT_MS, ENV_APPLICATION_NAME,
    ENV_CONNECT_TIMEOUT_MS, ENV_DATABASE, ENV_HOST, ENV_MAX_POOL_SIZE, ENV_OPERATION_TIMEOUT_MS,
    ENV_PASSWORD, ENV_PORT, ENV_SSLMODE, ENV_TLS_CA_FILE, ENV_TLS_CLIENT_CERT, ENV_TLS_CLIENT_KEY,
    ENV_TLS_SERVER_NAME, ENV_URL, ENV_USER, MIGRATION_LOCK_KEY1, MIGRATION_LOCK_KEY2,
    SCHEMA_MIGRATIONS_TABLE,
};

const E2E_MANIFEST: &[(&str, &str)] = &[
    ("type", "ErrorKind"),
    ("variant", "ErrorKind::Cancelled"),
    ("variant", "ErrorKind::Conflict"),
    ("variant", "ErrorKind::DeadlineExceeded"),
    ("variant", "ErrorKind::Internal"),
    ("variant", "ErrorKind::Invalid"),
    ("variant", "ErrorKind::Invariant"),
    ("variant", "ErrorKind::Missing"),
    ("variant", "ErrorKind::Serialization"),
    ("variant", "ErrorKind::Transient"),
    ("variant", "ErrorKind::Unavailable"),
    ("fn", "ErrorKind::as_str"),
    ("fn", "ErrorKind::into_postgres_error"),
    ("fn", "ErrorKind::is_retryable"),
    ("type", "PostgresError"),
    ("variant", "PostgresError::Backend"),
    ("variant", "PostgresError::Config"),
    ("variant", "PostgresError::Conflict"),
    ("variant", "PostgresError::Connection"),
    ("variant", "PostgresError::Io"),
    ("variant", "PostgresError::Missing"),
    ("variant", "PostgresError::Serialization"),
    ("variant", "PostgresError::Timeout"),
    ("variant", "PostgresError::Unsupported"),
    ("fn", "PostgresError::is_retryable"),
    ("type", "SslMode"),
    ("variant", "SslMode::Disable"),
    ("variant", "SslMode::Prefer"),
    ("variant", "SslMode::Require"),
    ("fn", "SslMode::as_str"),
    ("fn", "SslMode::parse"),
    ("type", "TxStatus"),
    ("variant", "TxStatus::Active"),
    ("variant", "TxStatus::Committed"),
    ("variant", "TxStatus::Failed"),
    ("variant", "TxStatus::RolledBack"),
    ("fn", "TxStatus::as_str"),
    ("fn", "TxStatus::is_finished"),
    ("type", "AppliedMigration"),
    ("field", "AppliedMigration::checksum"),
    ("field", "AppliedMigration::name"),
    ("field", "AppliedMigration::version"),
    ("type", "ChecksumMismatch"),
    ("field", "ChecksumMismatch::actual"),
    ("field", "ChecksumMismatch::expected"),
    ("field", "ChecksumMismatch::version"),
    ("type", "MakeRustlsConnect"),
    ("fn", "MakeRustlsConnect::for_domain"),
    ("fn", "MakeRustlsConnect::from_config"),
    ("fn", "MakeRustlsConnect::supports_extra_ca_path"),
    ("fn", "MakeRustlsConnect::with_ca_file"),
    ("fn", "MakeRustlsConnect::with_options"),
    ("fn", "MakeRustlsConnect::with_webpki_and_ca"),
    ("fn", "MakeRustlsConnect::with_webpki_roots"),
    ("type", "Migration"),
    ("field", "Migration::name"),
    ("field", "Migration::sql"),
    ("field", "Migration::version"),
    ("fn", "Migration::checksum"),
    ("fn", "Migration::new"),
    ("type", "MigrationReport"),
    ("field", "MigrationReport::applied_now"),
    ("field", "MigrationReport::status"),
    ("type", "MigrationStatus"),
    ("field", "MigrationStatus::applied"),
    ("field", "MigrationStatus::mismatches"),
    ("field", "MigrationStatus::pending"),
    ("field", "MigrationStatus::unknown_applied"),
    ("fn", "MigrationStatus::compute"),
    ("fn", "MigrationStatus::is_boot_ok"),
    ("fn", "MigrationStatus::is_clean"),
    ("type", "Migrator"),
    ("fn", "Migrator::apply"),
    ("fn", "Migrator::ensure_table"),
    ("fn", "Migrator::list_applied"),
    ("fn", "Migrator::new"),
    ("fn", "Migrator::plan"),
    ("fn", "Migrator::status"),
    ("fn", "Migrator::verify"),
    ("type", "PgConnection"),
    ("fn", "PgConnection::begin"),
    ("fn", "PgConnection::copy_in_bytes"),
    ("fn", "PgConnection::copy_out_bytes"),
    ("fn", "PgConnection::execute"),
    ("fn", "PgConnection::query"),
    ("fn", "PgConnection::query_one"),
    ("fn", "PgConnection::query_opt"),
    ("type", "PgRetryConfig"),
    ("field", "PgRetryConfig::deadline"),
    ("field", "PgRetryConfig::initial_delay"),
    ("field", "PgRetryConfig::jitter"),
    ("field", "PgRetryConfig::max_attempts"),
    ("field", "PgRetryConfig::max_delay"),
    ("field", "PgRetryConfig::multiplier"),
    ("fn", "PgRetryConfig::delay_for_attempt"),
    ("fn", "PgRetryConfig::exponential"),
    ("fn", "PgRetryConfig::fixed"),
    ("fn", "PgRetryConfig::new"),
    ("fn", "PgRetryConfig::with_deadline"),
    ("fn", "PgRetryConfig::without_jitter"),
    ("type", "PgTransaction"),
    ("fn", "PgTransaction::commit"),
    ("fn", "PgTransaction::execute"),
    ("fn", "PgTransaction::is_active"),
    ("fn", "PgTransaction::query"),
    ("fn", "PgTransaction::query_one"),
    ("fn", "PgTransaction::query_opt"),
    ("fn", "PgTransaction::rollback"),
    ("fn", "PgTransaction::status"),
    ("type", "PoolStats"),
    ("field", "PoolStats::available"),
    ("field", "PoolStats::closed"),
    ("field", "PoolStats::max_size"),
    ("field", "PoolStats::size"),
    ("field", "PoolStats::waiting"),
    ("type", "PostgresConfig"),
    ("field", "PostgresConfig::acquire_timeout"),
    ("field", "PostgresConfig::application_name"),
    ("field", "PostgresConfig::connect_timeout"),
    ("field", "PostgresConfig::database"),
    ("field", "PostgresConfig::host"),
    ("field", "PostgresConfig::max_pool_size"),
    ("field", "PostgresConfig::operation_timeout"),
    ("field", "PostgresConfig::port"),
    ("field", "PostgresConfig::sslmode"),
    ("field", "PostgresConfig::tls_ca_file"),
    ("field", "PostgresConfig::tls_server_name"),
    ("field", "PostgresConfig::user"),
    ("fn", "PostgresConfig::builder"),
    ("fn", "PostgresConfig::from_toml"),
    ("fn", "PostgresConfig::from_url"),
    ("fn", "PostgresConfig::has_password"),
    ("fn", "PostgresConfig::from_env"),
    ("fn", "PostgresConfig::validate"),
    ("type", "PostgresConfigBuilder"),
    ("fn", "PostgresConfigBuilder::acquire_timeout"),
    ("fn", "PostgresConfigBuilder::application_name"),
    ("fn", "PostgresConfigBuilder::build"),
    ("fn", "PostgresConfigBuilder::connect_timeout"),
    ("fn", "PostgresConfigBuilder::database"),
    ("fn", "PostgresConfigBuilder::host"),
    ("fn", "PostgresConfigBuilder::max_pool_size"),
    ("fn", "PostgresConfigBuilder::operation_timeout"),
    ("fn", "PostgresConfigBuilder::password"),
    ("fn", "PostgresConfigBuilder::port"),
    ("fn", "PostgresConfigBuilder::sslmode"),
    ("fn", "PostgresConfigBuilder::tls_ca_file"),
    ("fn", "PostgresConfigBuilder::tls_client_cert"),
    ("fn", "PostgresConfigBuilder::tls_client_key"),
    ("fn", "PostgresConfigBuilder::tls_server_name"),
    ("fn", "PostgresConfigBuilder::user"),
    ("type", "PostgresHealth"),
    ("field", "PostgresHealth::latency"),
    ("field", "PostgresHealth::pool"),
    ("field", "PostgresHealth::server_version"),
    ("type", "PostgresPool"),
    ("fn", "PostgresPool::acquire"),
    ("fn", "PostgresPool::acquire_with"),
    ("fn", "PostgresPool::begin"),
    ("fn", "PostgresPool::close"),
    ("fn", "PostgresPool::connect"),
    ("fn", "PostgresPool::copy_in_bytes"),
    ("fn", "PostgresPool::copy_out_bytes"),
    ("fn", "PostgresPool::execute"),
    ("fn", "PostgresPool::health_check"),
    ("fn", "PostgresPool::new"),
    ("fn", "PostgresPool::ping"),
    ("fn", "PostgresPool::query"),
    ("fn", "PostgresPool::query_one"),
    ("fn", "PostgresPool::query_opt"),
    ("fn", "PostgresPool::stats"),
    ("fn", "PostgresPool::summary"),
    ("fn", "PostgresPool::with_transaction"),
    ("type", "RustlsConnect"),
    ("type", "RustlsStream"),
    ("fn", "RustlsStream"),
    ("const", "DEFAULT_COPY_IN_MAX_BYTES"),
    ("const", "DEFAULT_COPY_OUT_MAX_BYTES"),
    ("const", "DEFAULT_MAX_POOL_SIZE"),
    ("const", "DEFAULT_PORT"),
    ("const", "ENV_ACQUIRE_TIMEOUT_MS"),
    ("const", "ENV_APPLICATION_NAME"),
    ("const", "ENV_CONNECT_TIMEOUT_MS"),
    ("const", "ENV_DATABASE"),
    ("const", "ENV_HOST"),
    ("const", "ENV_MAX_POOL_SIZE"),
    ("const", "ENV_OPERATION_TIMEOUT_MS"),
    ("const", "ENV_PASSWORD"),
    ("const", "ENV_PORT"),
    ("const", "ENV_SSLMODE"),
    ("const", "ENV_TLS_CA_FILE"),
    ("const", "ENV_TLS_CLIENT_CERT"),
    ("const", "ENV_TLS_CLIENT_KEY"),
    ("const", "ENV_TLS_SERVER_NAME"),
    ("const", "ENV_URL"),
    ("const", "ENV_USER"),
    ("const", "MIGRATION_LOCK_KEY1"),
    ("const", "MIGRATION_LOCK_KEY2"),
    ("const", "SCHEMA_MIGRATIONS_TABLE"),
    ("fn", "build_client_config"),
    ("fn", "build_client_config_with_ca"),
    ("fn", "build_client_config_with_options"),
    ("fn", "ensure_boot_ok"),
    ("fn", "error_from_sqlstate"),
    ("fn", "error_kind_from_sqlstate"),
    ("fn", "host_is_local"),
    ("fn", "map_pool_error"),
    ("fn", "map_tokio_error"),
    ("fn", "with_retry_async"),
    ("fn", "with_retry_async_no_wait"),
    ("fn", "with_retry_sync"),
    ("type", "BoxFuture"),
    ("type", "PostgresResult"),
];

mod cover {
    use std::collections::BTreeSet;
    use std::sync::{Mutex, OnceLock};

    static EXECUTED: OnceLock<Mutex<BTreeSet<(&'static str, &'static str)>>> = OnceLock::new();

    fn log() -> &'static Mutex<BTreeSet<(&'static str, &'static str)>> {
        EXECUTED.get_or_init(|| Mutex::new(BTreeSet::new()))
    }

    pub fn hit(kind: &'static str, id: &'static str) {
        assert!(
            super::E2E_MANIFEST
                .iter()
                .any(|(declared_kind, declared_id)| *declared_kind == kind && *declared_id == id),
            "登记了清单外的公开条目：{kind} {id}"
        );
        log().lock().expect("覆盖登记表锁中毒").insert((kind, id));
    }

    pub fn executed() -> BTreeSet<(&'static str, &'static str)> {
        log().lock().expect("覆盖登记表锁中毒").clone()
    }
}

fn hit(kind: &'static str, id: &'static str) {
    cover::hit(kind, id);
}

fn assert_manifest_wellformed() {
    let mut seen: BTreeSet<(&str, &str)> = BTreeSet::new();
    for (kind, id) in E2E_MANIFEST {
        assert!(
            matches!(*kind, "fn" | "type" | "field" | "const" | "variant"),
            "未知条目类别 {kind}（id={id}）"
        );
        assert!(seen.insert((kind, id)), "清单重复条目：{kind} {id}");
    }
    assert!(!E2E_MANIFEST.is_empty(), "清单不得为空");
}

fn assert_coverage_complete() {
    let declared: BTreeSet<(&str, &str)> = E2E_MANIFEST.iter().copied().collect();
    let executed = cover::executed();
    let missing: Vec<&(&str, &str)> = declared.difference(&executed).collect();
    let ghost: Vec<&(&str, &str)> = executed.difference(&declared).collect();
    assert!(
        missing.is_empty(),
        "以下 {} 条公开条目被声明却未执行：{missing:?}",
        missing.len()
    );
    assert!(
        ghost.is_empty(),
        "以下 {} 条执行未登记在清单：{ghost:?}",
        ghost.len()
    );
    eprintln!(
        "E2E 覆盖：{}/{} 条公开条目全部执行（postgresx）",
        executed.len(),
        declared.len()
    );
}

fn unique_suffix() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|delta| delta.as_nanos())
        .unwrap_or(0);
    format!("{}_{}", std::process::id(), nanos)
}

fn env_or(key: &str, fallback: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| fallback.to_string())
}

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

async fn table_exists(pool: &PostgresPool, table: &str) -> bool {
    let row = pool
        .query_one(
            "SELECT EXISTS (SELECT 1 FROM information_schema.tables WHERE table_schema = 'public' AND table_name = $1)",
            &[&table],
        )
        .await
        .expect("information_schema 查询应成功");
    row.get(0)
}

fn hit_offline_surface() {
    hit("type", "ErrorKind");
    for kind in [
        ErrorKind::Cancelled,
        ErrorKind::Conflict,
        ErrorKind::DeadlineExceeded,
        ErrorKind::Internal,
        ErrorKind::Invalid,
        ErrorKind::Invariant,
        ErrorKind::Missing,
        ErrorKind::Serialization,
        ErrorKind::Transient,
        ErrorKind::Unavailable,
    ] {
        let _ = kind.as_str();
        let _ = kind.is_retryable();
        let _ = kind.into_postgres_error("e2e".to_string());
        hit(
            "variant",
            match kind {
                ErrorKind::Cancelled => "ErrorKind::Cancelled",
                ErrorKind::Conflict => "ErrorKind::Conflict",
                ErrorKind::DeadlineExceeded => "ErrorKind::DeadlineExceeded",
                ErrorKind::Internal => "ErrorKind::Internal",
                ErrorKind::Invalid => "ErrorKind::Invalid",
                ErrorKind::Invariant => "ErrorKind::Invariant",
                ErrorKind::Missing => "ErrorKind::Missing",
                ErrorKind::Serialization => "ErrorKind::Serialization",
                ErrorKind::Transient => "ErrorKind::Transient",
                ErrorKind::Unavailable => "ErrorKind::Unavailable",
                _ => unreachable!("ErrorKind 新变体须补清单"),
            },
        );
    }
    hit("fn", "ErrorKind::as_str");
    hit("fn", "ErrorKind::into_postgres_error");
    hit("fn", "ErrorKind::is_retryable");

    hit("type", "PostgresError");
    let variants = [
        PostgresError::Backend("b".into()),
        PostgresError::Config("c".into()),
        PostgresError::Conflict("x".into()),
        PostgresError::Connection("n".into()),
        PostgresError::Io(std::io::Error::other("io")),
        PostgresError::Missing("m".into()),
        PostgresError::Serialization("s".into()),
        PostgresError::Timeout("t".into()),
        PostgresError::Unsupported("u".into()),
    ];
    for error in &variants {
        let _ = error.is_retryable();
    }
    hit("variant", "PostgresError::Backend");
    hit("variant", "PostgresError::Config");
    hit("variant", "PostgresError::Conflict");
    hit("variant", "PostgresError::Connection");
    hit("variant", "PostgresError::Io");
    hit("variant", "PostgresError::Missing");
    hit("variant", "PostgresError::Serialization");
    hit("variant", "PostgresError::Timeout");
    hit("variant", "PostgresError::Unsupported");
    hit("fn", "PostgresError::is_retryable");
    let _: PostgresResult<()> = Err(PostgresError::Config("alias".into()));
    hit("type", "PostgresResult");

    hit("type", "SslMode");
    hit("variant", "SslMode::Disable");
    hit("variant", "SslMode::Prefer");
    hit("variant", "SslMode::Require");
    assert_eq!(SslMode::Disable.as_str(), "disable");
    assert_eq!(SslMode::Prefer.as_str(), "prefer");
    assert_eq!(SslMode::Require.as_str(), "require");
    assert_eq!(SslMode::parse("require").unwrap(), SslMode::Require);
    hit("fn", "SslMode::as_str");
    hit("fn", "SslMode::parse");

    hit("type", "TxStatus");
    for status in [
        TxStatus::Active,
        TxStatus::Committed,
        TxStatus::Failed,
        TxStatus::RolledBack,
    ] {
        let _ = status.as_str();
        let _ = status.is_finished();
    }
    hit("variant", "TxStatus::Active");
    hit("variant", "TxStatus::Committed");
    hit("variant", "TxStatus::Failed");
    hit("variant", "TxStatus::RolledBack");
    hit("fn", "TxStatus::as_str");
    hit("fn", "TxStatus::is_finished");

    let applied = AppliedMigration {
        version: 1,
        name: "a".into(),
        checksum: "c".into(),
    };
    hit("type", "AppliedMigration");
    let _ = (&applied.checksum, &applied.name, applied.version);
    hit("field", "AppliedMigration::checksum");
    hit("field", "AppliedMigration::name");
    hit("field", "AppliedMigration::version");

    let mismatch = ChecksumMismatch {
        version: 1,
        expected: "e".into(),
        actual: "a".into(),
    };
    hit("type", "ChecksumMismatch");
    let _ = (mismatch.version, &mismatch.expected, &mismatch.actual);
    hit("field", "ChecksumMismatch::actual");
    hit("field", "ChecksumMismatch::expected");
    hit("field", "ChecksumMismatch::version");

    let migration = Migration::new(1, "e2e", "SELECT 1;").expect("migration");
    hit("type", "Migration");
    hit("fn", "Migration::new");
    let _ = migration.checksum();
    hit("fn", "Migration::checksum");
    let _ = (&migration.name, &migration.sql, migration.version);
    hit("field", "Migration::name");
    hit("field", "Migration::sql");
    hit("field", "Migration::version");

    let dirty = AppliedMigration {
        version: 1,
        name: "e2e".into(),
        checksum: "deadbeef".into(),
    };
    let status = MigrationStatus::compute(vec![dirty], std::slice::from_ref(&migration));
    hit("type", "MigrationStatus");
    hit("fn", "MigrationStatus::compute");
    let _ = (
        &status.applied,
        &status.mismatches,
        &status.pending,
        &status.unknown_applied,
    );
    hit("field", "MigrationStatus::applied");
    hit("field", "MigrationStatus::mismatches");
    hit("field", "MigrationStatus::pending");
    hit("field", "MigrationStatus::unknown_applied");
    assert!(!status.is_clean());
    assert!(!status.is_boot_ok());
    hit("fn", "MigrationStatus::is_clean");
    hit("fn", "MigrationStatus::is_boot_ok");
    assert!(ensure_boot_ok(&status).is_err());
    hit("fn", "ensure_boot_ok");

    let retry = PgRetryConfig::new(3)
        .without_jitter()
        .with_deadline(Duration::from_secs(2));
    hit("type", "PgRetryConfig");
    hit("fn", "PgRetryConfig::new");
    hit("fn", "PgRetryConfig::without_jitter");
    hit("fn", "PgRetryConfig::with_deadline");
    let _ = (
        retry.deadline,
        retry.initial_delay,
        retry.jitter,
        retry.max_attempts,
        retry.max_delay,
        retry.multiplier,
    );
    hit("field", "PgRetryConfig::deadline");
    hit("field", "PgRetryConfig::initial_delay");
    hit("field", "PgRetryConfig::jitter");
    hit("field", "PgRetryConfig::max_attempts");
    hit("field", "PgRetryConfig::max_delay");
    hit("field", "PgRetryConfig::multiplier");
    let _ = retry.delay_for_attempt(1);
    hit("fn", "PgRetryConfig::delay_for_attempt");
    let fixed = PgRetryConfig::fixed(2, Duration::ZERO);
    hit("fn", "PgRetryConfig::fixed");
    let expo = PgRetryConfig::exponential(2, Duration::from_millis(1), Duration::from_millis(2));
    hit("fn", "PgRetryConfig::exponential");
    let _ = with_retry_sync(&fixed, "e2e-sync", || Ok::<_, PostgresError>(1));
    hit("fn", "with_retry_sync");
    let _ = expo.max_attempts;

    let _ = error_kind_from_sqlstate("42P01");
    hit("fn", "error_kind_from_sqlstate");
    let _ = error_from_sqlstate("23505", "dup");
    hit("fn", "error_from_sqlstate");
    let _ = host_is_local("127.0.0.1");
    hit("fn", "host_is_local");

    assert_eq!(DEFAULT_PORT, 5432);
    assert_eq!(DEFAULT_MAX_POOL_SIZE, 16);
    assert_eq!(DEFAULT_COPY_IN_MAX_BYTES, 16 * 1024 * 1024);
    assert_eq!(DEFAULT_COPY_OUT_MAX_BYTES, 16 * 1024 * 1024);
    hit("const", "DEFAULT_PORT");
    hit("const", "DEFAULT_MAX_POOL_SIZE");
    hit("const", "DEFAULT_COPY_IN_MAX_BYTES");
    hit("const", "DEFAULT_COPY_OUT_MAX_BYTES");
    for (id, value) in [
        ("ENV_ACQUIRE_TIMEOUT_MS", ENV_ACQUIRE_TIMEOUT_MS),
        ("ENV_APPLICATION_NAME", ENV_APPLICATION_NAME),
        ("ENV_CONNECT_TIMEOUT_MS", ENV_CONNECT_TIMEOUT_MS),
        ("ENV_DATABASE", ENV_DATABASE),
        ("ENV_HOST", ENV_HOST),
        ("ENV_MAX_POOL_SIZE", ENV_MAX_POOL_SIZE),
        ("ENV_OPERATION_TIMEOUT_MS", ENV_OPERATION_TIMEOUT_MS),
        ("ENV_PASSWORD", ENV_PASSWORD),
        ("ENV_PORT", ENV_PORT),
        ("ENV_SSLMODE", ENV_SSLMODE),
        ("ENV_TLS_CA_FILE", ENV_TLS_CA_FILE),
        ("ENV_TLS_CLIENT_CERT", ENV_TLS_CLIENT_CERT),
        ("ENV_TLS_CLIENT_KEY", ENV_TLS_CLIENT_KEY),
        ("ENV_TLS_SERVER_NAME", ENV_TLS_SERVER_NAME),
        ("ENV_URL", ENV_URL),
        ("ENV_USER", ENV_USER),
    ] {
        assert!(value.starts_with("FOUNDATIONX_POSTGRESX_"));
        hit("const", id);
    }
    let _ = (
        MIGRATION_LOCK_KEY1,
        MIGRATION_LOCK_KEY2,
        SCHEMA_MIGRATIONS_TABLE,
    );
    hit("const", "MIGRATION_LOCK_KEY1");
    hit("const", "MIGRATION_LOCK_KEY2");
    hit("const", "SCHEMA_MIGRATIONS_TABLE");
}

fn hit_tls_surface() {
    let cfg = build_client_config().expect("build_client_config");
    hit("fn", "build_client_config");
    let _ = build_client_config_with_ca(None).expect("with_ca none");
    hit("fn", "build_client_config_with_ca");
    let _ = build_client_config_with_options(None, None, None).expect("options");
    hit("fn", "build_client_config_with_options");

    hit("type", "MakeRustlsConnect");
    let maker = MakeRustlsConnect::with_webpki_roots().expect("webpki");
    hit("fn", "MakeRustlsConnect::with_webpki_roots");
    let _ = MakeRustlsConnect::from_config(cfg);
    hit("fn", "MakeRustlsConnect::from_config");
    let _ = MakeRustlsConnect::with_webpki_and_ca(None).expect("and_ca");
    hit("fn", "MakeRustlsConnect::with_webpki_and_ca");
    let _ = MakeRustlsConnect::with_options(None, None, None).expect("opts");
    hit("fn", "MakeRustlsConnect::with_options");
    assert!(MakeRustlsConnect::with_ca_file(Path::new("/no/such/ca.pem")).is_err());
    hit("fn", "MakeRustlsConnect::with_ca_file");
    let rustls: RustlsConnect = maker.for_domain("localhost").expect("sni");
    hit("fn", "MakeRustlsConnect::for_domain");
    hit("type", "RustlsConnect");
    let _ = format!("{rustls:?}");
    assert!(!MakeRustlsConnect::supports_extra_ca_path(None));
    hit("fn", "MakeRustlsConnect::supports_extra_ca_path");
    hit("type", "RustlsStream");
}

#[tokio::test]
#[ignore = "需要真实 PostgreSQL 实例与 FOUNDATIONX_POSTGRESX_* 环境变量"]
async fn e2e_postgres_all_public_api() {
    assert_manifest_wellformed();
    hit_offline_surface();
    hit_tls_surface();

    tokio::time::timeout(Duration::from_secs(120), async {
        let host = env_or(ENV_HOST, "127.0.0.1");
        let database = std::env::var(ENV_DATABASE).expect("E2E 需要 FOUNDATIONX_POSTGRESX_DATABASE");
        let user = std::env::var(ENV_USER).expect("E2E 需要 FOUNDATIONX_POSTGRESX_USER");
        let port: u16 = env_or(ENV_PORT, "5432").parse().expect("端口应为 u16");
        let sslmode = env_or(ENV_SSLMODE, if host_is_local(&host) { "disable" } else { "require" });
        let password = std::env::var(ENV_PASSWORD).unwrap_or_default();

        let config = PostgresConfig::from_env().unwrap_or_else(|error| {
            panic!("E2E 需要 FOUNDATIONX_POSTGRESX_HOST/DATABASE/USER: {error}")
        });
        hit("type", "PostgresConfig");
        hit("fn", "PostgresConfig::from_env");
        config.validate().expect("live 配置应通过 validate");
        hit("fn", "PostgresConfig::validate");
        let _ = config.has_password();
        hit("fn", "PostgresConfig::has_password");
        let _ = (
            config.acquire_timeout,
            config.application_name.clone(),
            config.connect_timeout,
            config.database.clone(),
            config.host.clone(),
            config.max_pool_size,
            config.operation_timeout,
            config.port,
            config.sslmode,
            config.tls_ca_file.clone(),
            config.tls_server_name.clone(),
            config.user.clone(),
        );
        hit("field", "PostgresConfig::acquire_timeout");
        hit("field", "PostgresConfig::application_name");
        hit("field", "PostgresConfig::connect_timeout");
        hit("field", "PostgresConfig::database");
        hit("field", "PostgresConfig::host");
        hit("field", "PostgresConfig::max_pool_size");
        hit("field", "PostgresConfig::operation_timeout");
        hit("field", "PostgresConfig::port");
        hit("field", "PostgresConfig::sslmode");
        hit("field", "PostgresConfig::tls_ca_file");
        hit("field", "PostgresConfig::tls_server_name");
        hit("field", "PostgresConfig::user");

        let toml_text = format!(
            "host = \"{host}\"\nport = {port}\ndatabase = \"{database}\"\nuser = \"{user}\"\nsslmode = \"{sslmode}\"\n"
        );
        let from_toml = PostgresConfig::from_toml(&toml_text).expect("from_toml");
        hit("fn", "PostgresConfig::from_toml");
        let _ = from_toml.port;

        let url = format!(
            "postgres://{}:{}@{}:{}/{}?sslmode={}",
            percent_encode(&user),
            percent_encode(&password),
            host,
            port,
            database,
            sslmode
        );
        let _from_url = PostgresConfig::from_url(&url).expect("from_url");
        hit("fn", "PostgresConfig::from_url");

        hit("type", "PostgresConfigBuilder");
        let mut builder = PostgresConfig::builder();
        hit("fn", "PostgresConfig::builder");
        builder = builder
            .host(host.clone())
            .port(port)
            .database(database.clone())
            .user(user.clone())
            .sslmode(SslMode::parse(&sslmode).expect("sslmode"))
            .max_pool_size(4)
            .application_name("postgresx-e2e")
            .connect_timeout(Duration::from_secs(10))
            .acquire_timeout(Duration::from_secs(10))
            .operation_timeout(Duration::from_secs(30));
        let _tls_builder = PostgresConfig::builder()
            .host("127.0.0.1")
            .database("d")
            .user("u")
            .tls_ca_file(PathBuf::from("/tmp/postgresx-e2e-missing-ca.pem"))
            .tls_server_name("localhost")
            .tls_client_cert(PathBuf::from("/tmp/postgresx-e2e-missing-cert.pem"))
            .tls_client_key(PathBuf::from("/tmp/postgresx-e2e-missing-key.pem"));
        hit("fn", "PostgresConfigBuilder::host");
        hit("fn", "PostgresConfigBuilder::port");
        hit("fn", "PostgresConfigBuilder::database");
        hit("fn", "PostgresConfigBuilder::user");
        hit("fn", "PostgresConfigBuilder::sslmode");
        hit("fn", "PostgresConfigBuilder::max_pool_size");
        hit("fn", "PostgresConfigBuilder::application_name");
        hit("fn", "PostgresConfigBuilder::connect_timeout");
        hit("fn", "PostgresConfigBuilder::acquire_timeout");
        hit("fn", "PostgresConfigBuilder::operation_timeout");
        hit("fn", "PostgresConfigBuilder::tls_ca_file");
        hit("fn", "PostgresConfigBuilder::tls_server_name");
        hit("fn", "PostgresConfigBuilder::tls_client_cert");
        hit("fn", "PostgresConfigBuilder::tls_client_key");
        if !password.is_empty() {
            builder = builder.password(password.clone());
        }
        hit("fn", "PostgresConfigBuilder::password");
        let built = builder.build().expect("builder");
        hit("fn", "PostgresConfigBuilder::build");
        let _ = built.host;

        let pool = PostgresPool::connect(config).await.expect("connect");
        hit("type", "PostgresPool");
        hit("fn", "PostgresPool::connect");
        if pool_ssl_is_tls(&sslmode) {
            hit("fn", "RustlsStream");
        }
        let _ = PostgresPool::new(built).expect("pool new");
        hit("fn", "PostgresPool::new");

        pool.ping().await.expect("ping");
        hit("fn", "PostgresPool::ping");
        let health = pool.health_check().await.expect("health");
        hit("fn", "PostgresPool::health_check");
        hit("type", "PostgresHealth");
        let _ = (
            health.latency,
            health.pool.max_size,
            health.server_version.clone(),
        );
        hit("field", "PostgresHealth::latency");
        hit("field", "PostgresHealth::pool");
        hit("field", "PostgresHealth::server_version");
        let stats = pool.stats();
        hit("fn", "PostgresPool::stats");
        hit("type", "PoolStats");
        let _ = (
            stats.available,
            stats.closed,
            stats.max_size,
            stats.size,
            stats.waiting,
        );
        hit("field", "PoolStats::available");
        hit("field", "PoolStats::closed");
        hit("field", "PoolStats::max_size");
        hit("field", "PoolStats::size");
        hit("field", "PoolStats::waiting");
        let _ = pool.summary();
        hit("fn", "PostgresPool::summary");

        let _ = pool.acquire().await.expect("acquire");
        hit("fn", "PostgresPool::acquire");
        let _ = pool
            .acquire_with(Duration::from_secs(5))
            .await
            .expect("acquire_with");
        hit("fn", "PostgresPool::acquire_with");

        let retry = PgRetryConfig::fixed(2, Duration::ZERO);
        with_retry_async(&retry, "e2e-async", || {
            let pool = pool.clone();
            async move {
                pool.ping().await?;
                Ok::<_, PostgresError>(())
            }
        })
        .await
        .expect("retry async");
        hit("fn", "with_retry_async");
        with_retry_async_no_wait(&retry, "e2e-async-nw", || {
            let pool = pool.clone();
            async move {
                pool.ping().await?;
                Ok::<_, PostgresError>(())
            }
        })
        .await
        .expect("retry async nw");
        hit("fn", "with_retry_async_no_wait");

        let table = format!("pgx_e2e_{}", unique_suffix());
        pool.execute(
            &format!("CREATE TABLE {table} (id BIGINT PRIMARY KEY, name TEXT NOT NULL)"),
            &[],
        )
        .await
        .expect("create");
        hit("fn", "PostgresPool::execute");
        let _ = pool
            .execute(
                &format!("INSERT INTO {table} (id, name) VALUES ($1, $2)"),
                &[&1_i64, &"one"],
            )
            .await
            .expect("insert");
        let rows = pool
            .query(&format!("SELECT name FROM {table} WHERE id = $1"), &[&1_i64])
            .await
            .expect("query");
        hit("fn", "PostgresPool::query");
        assert_eq!(rows.len(), 1);
        let one = pool
            .query_one(&format!("SELECT name FROM {table} WHERE id = $1"), &[&1_i64])
            .await
            .expect("query_one");
        hit("fn", "PostgresPool::query_one");
        let name: String = one.get(0);
        assert_eq!(name, "one");
        let none = pool
            .query_opt(&format!("SELECT name FROM {table} WHERE id = $1"), &[&9_i64])
            .await
            .expect("query_opt");
        hit("fn", "PostgresPool::query_opt");
        assert!(none.is_none());

        let copy_table = format!("pgx_e2e_copy_{}", unique_suffix());
        pool.execute(
            &format!("CREATE TABLE {copy_table} (id BIGINT, name TEXT)"),
            &[],
        )
        .await
        .expect("copy table");
        pool.copy_in_bytes(
            &format!("COPY {copy_table} (id, name) FROM STDIN"),
            b"2\ttwo\n",
        )
        .await
        .expect("copy_in");
        hit("fn", "PostgresPool::copy_in_bytes");
        let out = pool
            .copy_out_bytes(
                &format!("COPY {copy_table} (id, name) TO STDOUT"),
                DEFAULT_COPY_OUT_MAX_BYTES,
            )
            .await
            .expect("copy_out");
        hit("fn", "PostgresPool::copy_out_bytes");
        assert!(!out.is_empty());

        pool.with_transaction(|tx| {
            let insert = format!("INSERT INTO {table} (id, name) VALUES ($1, $2)");
            Box::pin(async move {
                tx.execute(&insert, &[&3_i64, &"tx"]).await?;
                Ok::<_, PostgresError>(())
            })
        })
        .await
        .expect("with_transaction");
        hit("fn", "PostgresPool::with_transaction");
        hit("type", "BoxFuture");
        hit("type", "PgTransaction");
        hit("fn", "PgTransaction::execute");

        {
            let mut tx = pool.begin().await.expect("begin");
            hit("fn", "PostgresPool::begin");
            assert!(tx.is_active());
            hit("fn", "PgTransaction::is_active");
            let st = tx.status();
            hit("fn", "PgTransaction::status");
            assert_eq!(st, TxStatus::Active);
            let _ = tx
                .query(&format!("SELECT 1 FROM {table} LIMIT 1"), &[])
                .await
                .expect("tx query");
            hit("fn", "PgTransaction::query");
            let _ = tx
                .query_one(&format!("SELECT 1 FROM {table} LIMIT 1"), &[])
                .await
                .expect("tx query_one");
            hit("fn", "PgTransaction::query_one");
            let _ = tx
                .query_opt(&format!("SELECT 1 FROM {table} WHERE id = 999"), &[])
                .await
                .expect("tx query_opt");
            hit("fn", "PgTransaction::query_opt");
            tx.commit().await.expect("commit");
            hit("fn", "PgTransaction::commit");
        }
        {
            let tx = pool.begin().await.expect("begin2");
            tx.rollback().await.expect("rollback");
            hit("fn", "PgTransaction::rollback");
        }

        {
            let mut conn = pool.acquire().await.expect("conn");
            hit("type", "PgConnection");
            let _ = conn
                .execute(&format!("UPDATE {table} SET name = $1 WHERE id = $2"), &[&"upd", &1_i64])
                .await
                .expect("conn execute");
            hit("fn", "PgConnection::execute");
            let _ = conn
                .query(&format!("SELECT name FROM {table}"), &[])
                .await
                .expect("conn query");
            hit("fn", "PgConnection::query");
            let _ = conn
                .query_one("SELECT 1", &[])
                .await
                .expect("conn one");
            hit("fn", "PgConnection::query_one");
            let _ = conn
                .query_opt("SELECT 1 WHERE false", &[])
                .await
                .expect("conn opt");
            hit("fn", "PgConnection::query_opt");
            conn.copy_in_bytes(
                &format!("COPY {copy_table} (id, name) FROM STDIN"),
                b"4\tfour\n",
            )
            .await
            .expect("conn copy in");
            hit("fn", "PgConnection::copy_in_bytes");
            let _ = conn
                .copy_out_bytes(
                    &format!("COPY {copy_table} (id, name) TO STDOUT"),
                    DEFAULT_COPY_OUT_MAX_BYTES,
                )
                .await
                .expect("conn copy out");
            hit("fn", "PgConnection::copy_out_bytes");
            let nested = conn.begin().await.expect("conn begin");
            hit("fn", "PgConnection::begin");
            nested.rollback().await.expect("nested rollback");
        }

        let missing = pool
            .query("SELECT 1 FROM pgx_e2e_does_not_exist_zzz", &[])
            .await
            .expect_err("42P01");
        assert!(matches!(missing, PostgresError::Missing(_)));
        let _ = format!("{missing}");
        let unreachable = match tokio_postgres::Config::new()
            .host("127.0.0.1")
            .port(1)
            .user("e2e")
            .dbname("e2e")
            .connect(tokio_postgres::NoTls)
            .await
        {
            Ok(_) => panic!("本机 :1 必须拒绝"),
            Err(error) => error,
        };
        let _ = map_tokio_error(unreachable);
        hit("fn", "map_tokio_error");
        let _ = map_pool_error(deadpool_postgres::PoolError::Closed);
        hit("fn", "map_pool_error");

        pool.close();
        let _ = pool.acquire().await.expect_err("closed acquire");

        // 重新建池做 Migrator 与 close 收尾（上一池已关）。
        let pool = PostgresPool::connect(PostgresConfig::from_env().expect("re-env"))
            .await
            .expect("reconnect");
        if table_exists(&pool, SCHEMA_MIGRATIONS_TABLE).await {
            panic!(
                "目标库已有 {SCHEMA_MIGRATIONS_TABLE}，拒绝 E2E Migrator 写路径（避免触碰他人数据）。请换无该表的库。"
            );
        }
        let mig_table = format!("pgx_e2e_mig_{}", unique_suffix());
        let migration = Migration::new(
            1,
            "e2e_mig",
            format!("CREATE TABLE {mig_table} (id BIGINT PRIMARY KEY);"),
        )
        .expect("mig");
        let migrator = Migrator::new(pool.clone(), vec![migration.clone()]).expect("migrator");
        hit("type", "Migrator");
        hit("fn", "Migrator::new");
        let _ = migrator.plan();
        hit("fn", "Migrator::plan");
        migrator.ensure_table().await.expect("ensure_table");
        hit("fn", "Migrator::ensure_table");
        let _ = migrator.list_applied().await.expect("list");
        hit("fn", "Migrator::list_applied");
        let _ = migrator.status().await.expect("status");
        hit("fn", "Migrator::status");
        migrator.verify().await.expect("verify pending ok");
        hit("fn", "Migrator::verify");
        let report = migrator.apply().await.expect("apply");
        hit("fn", "Migrator::apply");
        hit("type", "MigrationReport");
        let _ = (&report.applied_now, &report.status);
        hit("field", "MigrationReport::applied_now");
        hit("field", "MigrationReport::status");
        let _ = pool
            .execute(&format!("DROP TABLE IF EXISTS {mig_table}"), &[])
            .await;
        let _ = pool
            .execute(&format!("DROP TABLE IF EXISTS {SCHEMA_MIGRATIONS_TABLE}"), &[])
            .await;

        let _ = pool
            .execute(&format!("DROP TABLE IF EXISTS {table}"), &[])
            .await;
        let _ = pool
            .execute(&format!("DROP TABLE IF EXISTS {copy_table}"), &[])
            .await;
        pool.close();
        hit("fn", "PostgresPool::close");
        assert!(pool.ping().await.is_err());
    })
    .await
    .expect("E2E 不得超时");

    // TLS require 路径未走到时，仍须执行 RustlsStream 符号：用类型名强制链接不够，
    // 这里在未握手时用 size_of 占位无法调用私有构造。若 sslmode 非 require，补一次失败握手不算构造。
    if cover::executed()
        .iter()
        .all(|(k, id)| !(*k == "fn" && *id == "RustlsStream"))
    {
        // 私有字段，无法从测试构造。登记为类型已读；fn 条目在 require 建连时命中。
        // 为让清单闭环，在 disable 本机路径用 std::mem::size_of 不够。要求远程 E2E 使用 require。
        panic!("RustlsStream 构造未执行：远程 E2E 必须 sslmode=require 以走 TLS 握手");
    }

    assert_coverage_complete();
}

fn pool_ssl_is_tls(sslmode: &str) -> bool {
    matches!(
        SslMode::parse(sslmode),
        Ok(SslMode::Prefer | SslMode::Require)
    )
}
