//! 有界 schema 迁移执行器。
//!
//! # 合同
//!
//! - 每条迁移持有**事务级 advisory lock**（`pg_advisory_xact_lock`）串行执行，
//!   commit / rollback 自动释放，不存在会话级锁泄漏；
//! - 每条迁移记录 SHA-256 **checksum**，已应用版本的 SQL 一旦被修改即 fail-closed；
//! - [`Migrator::verify`] 是**默认启动入口**：只校验，不执行任何 DDL；
//! - [`Migrator::apply`] 必须显式调用才会执行 pending 迁移；
//! - 已应用版本不会被修改或重放。

use std::collections::BTreeMap;

use sha2::{Digest, Sha256};

use crate::error::{map_tokio_error, PostgresError, PostgresResult};
use crate::pool::PostgresPool;
use crate::tx::PgTransaction;

/// 迁移历史表名（固定字面量，非动态标识符）。
pub const SCHEMA_MIGRATIONS_TABLE: &str = "infra_schema_migrations";

/// advisory lock key1（稳定常量，避免与业务锁冲突）。
pub const MIGRATION_LOCK_KEY1: i32 = 0x7058_5f6d; // 'px_m'
/// advisory lock key2。
pub const MIGRATION_LOCK_KEY2: i32 = 0x6967_7261; // 'igra'

/// 事务级 advisory lock SQL（commit/rollback 自动释放）。
const ADVISORY_XACT_LOCK_SQL: &str = "SELECT pg_advisory_xact_lock($1, $2)";

/// PostgreSQL 无法在事务块内执行的 DDL/DML 模式（保守拒绝，不假装原子）。
const NON_TRANSACTIONAL_DDL: &[(&str, &str)] = &[
    ("CREATE INDEX CONCURRENTLY", "CREATE INDEX CONCURRENTLY"),
    ("DROP INDEX CONCURRENTLY", "DROP INDEX CONCURRENTLY"),
    ("REINDEX CONCURRENTLY", "REINDEX CONCURRENTLY"),
    (
        "REFRESH MATERIALIZED VIEW CONCURRENTLY",
        "REFRESH MATERIALIZED VIEW CONCURRENTLY",
    ),
    ("CREATE DATABASE", "CREATE DATABASE"),
    ("VACUUM", "VACUUM"),
    ("CLUSTER ", "CLUSTER"),
];

/// 单条迁移定义（仅 forward SQL）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Migration {
    /// 单调递增版本号（> 0）。
    pub version: i64,
    /// 人类可读短名（如 `create_records`）。
    pub name: String,
    /// 完整 SQL 脚本（可多语句；由调用方保证安全，禁止拼接用户输入）。
    pub sql: String,
}

impl Migration {
    /// 构造并做基础校验。
    pub fn new(
        version: i64,
        name: impl Into<String>,
        sql: impl Into<String>,
    ) -> PostgresResult<Self> {
        let name = name.into();
        let sql = sql.into();
        if version <= 0 {
            return Err(PostgresError::Config(
                "migration version 必须 > 0".to_string(),
            ));
        }
        if name.trim().is_empty() {
            return Err(PostgresError::Config("migration name 不能为空".to_string()));
        }
        if sql.trim().is_empty() {
            return Err(PostgresError::Config("migration sql 不能为空".to_string()));
        }
        if name.len() > 256 {
            return Err(PostgresError::Config(
                "migration name 过长（≤256）".to_string(),
            ));
        }
        Ok(Self { version, name, sql })
    }

    /// SQL 正文的 SHA-256 十六进制 checksum（小写，64 字符）。
    #[must_use]
    pub fn checksum(&self) -> String {
        let digest = Sha256::digest(self.sql.as_bytes());
        digest.iter().map(|byte| format!("{byte:02x}")).collect()
    }
}

/// 已落库的迁移行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedMigration {
    /// 版本。
    pub version: i64,
    /// 名称。
    pub name: String,
    /// 落库 checksum。
    pub checksum: String,
}

/// checksum 不一致（计划中的 SQL 与库中记录不符）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChecksumMismatch {
    /// 版本。
    pub version: i64,
    /// 计划中的 checksum。
    pub expected: String,
    /// 库中已记录的 checksum。
    pub actual: String,
}

/// [`Migrator::verify`] / [`Migrator::status`] 快照。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MigrationStatus {
    /// 已应用（按 version 升序）。
    pub applied: Vec<AppliedMigration>,
    /// 计划中尚未应用的版本。
    pub pending: Vec<i64>,
    /// 已应用但 checksum 与计划不符。
    pub mismatches: Vec<ChecksumMismatch>,
    /// 库中存在、计划中不存在的版本。
    pub unknown_applied: Vec<i64>,
}

impl MigrationStatus {
    /// 由「已应用行 + 计划」计算状态（纯函数，便于离线测试）。
    #[must_use]
    pub fn compute(applied: Vec<AppliedMigration>, plan: &[Migration]) -> Self {
        let applied_map: BTreeMap<i64, &AppliedMigration> =
            applied.iter().map(|row| (row.version, row)).collect();
        let plan_map: BTreeMap<i64, &Migration> = plan.iter().map(|m| (m.version, m)).collect();

        let mut pending = Vec::new();
        let mut mismatches = Vec::new();
        for migration in plan {
            match applied_map.get(&migration.version) {
                None => pending.push(migration.version),
                Some(row) => {
                    let expected = migration.checksum();
                    if row.checksum != expected {
                        mismatches.push(ChecksumMismatch {
                            version: migration.version,
                            expected,
                            actual: row.checksum.clone(),
                        });
                    }
                }
            }
        }
        let unknown_applied: Vec<i64> = applied
            .iter()
            .filter(|row| !plan_map.contains_key(&row.version))
            .map(|row| row.version)
            .collect();

        Self {
            applied,
            pending,
            mismatches,
            unknown_applied,
        }
    }

    /// 是否完全同步：无 mismatch、无未知版本、无 pending。
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.mismatches.is_empty() && self.unknown_applied.is_empty() && self.pending.is_empty()
    }

    /// 启动是否可放行：无 mismatch / 未知版本；pending 允许（由运维显式 apply）。
    #[must_use]
    pub fn is_boot_ok(&self) -> bool {
        self.mismatches.is_empty() && self.unknown_applied.is_empty()
    }
}

/// [`Migrator::apply`] 结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationReport {
    /// 本次新应用的版本。
    pub applied_now: Vec<i64>,
    /// apply 之后的状态。
    pub status: MigrationStatus,
}

/// schema 迁移执行器。
#[derive(Clone, Debug)]
pub struct Migrator {
    pool: PostgresPool,
    migrations: Vec<Migration>,
}

impl Migrator {
    /// 构造执行器：`migrations` 按 version 排序，重复 version 直接拒绝。
    pub fn new(pool: PostgresPool, migrations: Vec<Migration>) -> PostgresResult<Self> {
        let mut migrations = migrations;
        migrations.sort_by_key(|migration| migration.version);
        let mut seen = BTreeMap::new();
        for migration in &migrations {
            if seen.insert(migration.version, ()).is_some() {
                return Err(PostgresError::Config(format!(
                    "重复的 migration version: {}",
                    migration.version
                )));
            }
        }
        Ok(Self { pool, migrations })
    }

    /// 计划中的迁移（已按 version 升序）。
    #[must_use]
    pub fn plan(&self) -> &[Migration] {
        &self.migrations
    }

    /// 确保历史表存在。
    pub async fn ensure_table(&self) -> PostgresResult<()> {
        let sql = format!(
            "CREATE TABLE IF NOT EXISTS {SCHEMA_MIGRATIONS_TABLE} (\
               version BIGINT PRIMARY KEY, \
               name TEXT NOT NULL, \
               checksum TEXT NOT NULL, \
               applied_at TIMESTAMPTZ NOT NULL DEFAULT now()\
             )"
        );
        self.pool.execute(&sql, &[]).await?;
        Ok(())
    }

    /// 读取已应用行（不持锁；apply 路径的锁内重读由内部完成）。
    pub async fn list_applied(&self) -> PostgresResult<Vec<AppliedMigration>> {
        self.ensure_table().await?;
        let sql = format!(
            "SELECT version, name, checksum FROM {SCHEMA_MIGRATIONS_TABLE} ORDER BY version ASC"
        );
        let rows = self.pool.query(&sql, &[]).await?;
        let mut applied = Vec::with_capacity(rows.len());
        for row in rows {
            let version: i64 = row.try_get(0).map_err(map_tokio_error)?;
            let name: String = row.try_get(1).map_err(map_tokio_error)?;
            let checksum: String = row.try_get(2).map_err(map_tokio_error)?;
            applied.push(AppliedMigration {
                version,
                name,
                checksum,
            });
        }
        Ok(applied)
    }

    /// 计算状态（不持锁）。
    pub async fn status(&self) -> PostgresResult<MigrationStatus> {
        let applied = self.list_applied().await?;
        Ok(MigrationStatus::compute(applied, &self.migrations))
    }

    /// 默认启动路径：校验 checksum 与未知版本；**不**执行 pending DDL。
    pub async fn verify(&self) -> PostgresResult<MigrationStatus> {
        let status = self.status().await?;
        ensure_apply_allowed(&status)?;
        Ok(status)
    }

    /// 显式应用全部 pending（按 version 升序）。
    ///
    /// 先 [`Self::verify`] fail-closed；每条 pending 在 [`PgTransaction`] 内取得
    /// `pg_advisory_xact_lock`，锁内重读该版本：已存在且 checksum 一致则跳过，
    /// 否则执行 DDL 与历史写入。锁随 commit / rollback 自动释放。
    pub async fn apply(&self) -> PostgresResult<MigrationReport> {
        self.verify().await?;

        let applied = self.list_applied().await?;
        let mut applied_map: BTreeMap<i64, AppliedMigration> =
            applied.into_iter().map(|row| (row.version, row)).collect();

        let pre_status =
            MigrationStatus::compute(applied_map.values().cloned().collect(), &self.migrations);
        ensure_apply_allowed(&pre_status)?;

        let mut applied_now = Vec::new();
        for migration in &self.migrations {
            if applied_map.contains_key(&migration.version) {
                continue;
            }
            let newly_applied = apply_one_migration(&self.pool, migration).await?;
            applied_map.insert(
                migration.version,
                AppliedMigration {
                    version: migration.version,
                    name: migration.name.clone(),
                    checksum: migration.checksum(),
                },
            );
            if newly_applied {
                applied_now.push(migration.version);
            }
        }

        let status =
            MigrationStatus::compute(applied_map.into_values().collect(), &self.migrations);
        Ok(MigrationReport {
            applied_now,
            status,
        })
    }
}

fn ensure_apply_allowed(status: &MigrationStatus) -> PostgresResult<()> {
    if !status.mismatches.is_empty() {
        return Err(PostgresError::Conflict(format!(
            "migration checksum 不一致: {} 条",
            status.mismatches.len()
        )));
    }
    if !status.unknown_applied.is_empty() {
        return Err(PostgresError::Conflict(format!(
            "库中存在计划外 migration 版本: {:?}",
            status.unknown_applied
        )));
    }
    Ok(())
}

/// 锁内对「是否已应用」的决策（纯逻辑，便于单元测试）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LockedApplyAction {
    /// 尚未落库，应执行 DDL 与历史写入。
    Apply,
    /// 已落库且 checksum 一致，跳过（并发 runner 已完成）。
    Skip,
}

/// 依据锁内重读结果决定 apply / skip；checksum 不一致 fail-closed。
fn decide_locked_apply(
    migration: &Migration,
    existing: Option<&AppliedMigration>,
) -> PostgresResult<LockedApplyAction> {
    match existing {
        None => Ok(LockedApplyAction::Apply),
        Some(row) => {
            let expected = migration.checksum();
            if row.checksum == expected {
                Ok(LockedApplyAction::Skip)
            } else {
                Err(PostgresError::Conflict(format!(
                    "migration checksum 不一致: version={} expected={} actual={}",
                    migration.version, expected, row.checksum
                )))
            }
        }
    }
}

fn non_transactional_ddl_reason(sql: &str) -> Option<&'static str> {
    let upper = sql.to_ascii_uppercase();
    NON_TRANSACTIONAL_DDL
        .iter()
        .find(|(needle, _)| upper.contains(needle))
        .map(|(_, label)| *label)
}

/// 单条 pending：事务内 `pg_advisory_xact_lock` → 重读版本 → DDL + 历史 → commit。
async fn apply_one_migration(pool: &PostgresPool, migration: &Migration) -> PostgresResult<bool> {
    let conn = pool.acquire().await?;
    let mut tx = conn.begin().await?;
    match apply_one_under_xact_lock(&mut tx, migration).await {
        Ok(newly_applied) => {
            tx.commit().await?;
            Ok(newly_applied)
        }
        Err(apply_error) => match tx.rollback().await {
            Ok(()) => Err(apply_error),
            Err(rollback_error) => Err(apply_error.context_message(&format!(
                "migration v{} 失败且 ROLLBACK 也失败: {rollback_error}",
                migration.version
            ))),
        },
    }
}

async fn apply_one_under_xact_lock(
    tx: &mut PgTransaction,
    migration: &Migration,
) -> PostgresResult<bool> {
    tx.execute(
        ADVISORY_XACT_LOCK_SQL,
        &[&MIGRATION_LOCK_KEY1, &MIGRATION_LOCK_KEY2],
    )
    .await?;

    let existing = load_applied_version(tx, migration.version).await?;
    match decide_locked_apply(migration, existing.as_ref())? {
        LockedApplyAction::Skip => Ok(false),
        LockedApplyAction::Apply => {
            apply_migration_in_tx(tx, migration).await?;
            Ok(true)
        }
    }
}

async fn load_applied_version(
    tx: &mut PgTransaction,
    version: i64,
) -> PostgresResult<Option<AppliedMigration>> {
    let sql =
        format!("SELECT version, name, checksum FROM {SCHEMA_MIGRATIONS_TABLE} WHERE version = $1");
    match tx.query_opt(&sql, &[&version]).await? {
        None => Ok(None),
        Some(row) => {
            let version: i64 = row.try_get(0).map_err(map_tokio_error)?;
            let name: String = row.try_get(1).map_err(map_tokio_error)?;
            let checksum: String = row.try_get(2).map_err(map_tokio_error)?;
            Ok(Some(AppliedMigration {
                version,
                name,
                checksum,
            }))
        }
    }
}

/// 在已 `BEGIN` 的事务内执行单条迁移的 DDL 与历史写入。
///
/// DDL 走 `batch_execute`（simple query，支持多语句）；历史写入仍用参数化 `execute`。
async fn apply_migration_in_tx(
    tx: &mut PgTransaction,
    migration: &Migration,
) -> PostgresResult<()> {
    if let Some(reason) = non_transactional_ddl_reason(&migration.sql) {
        return Err(PostgresError::Unsupported(format!(
            "migration v{} 含不可事务化 DDL（{reason}）；请拆分或由运维显式执行",
            migration.version
        )));
    }

    if let Err(ddl_error) = tx.batch_execute(&migration.sql).await {
        return Err(
            ddl_error.context_message(&format!("migration v{} DDL 执行失败", migration.version))
        );
    }

    let checksum = migration.checksum();
    let insert = format!(
        "INSERT INTO {SCHEMA_MIGRATIONS_TABLE} (version, name, checksum) VALUES ($1, $2, $3)"
    );
    tx.execute(&insert, &[&migration.version, &migration.name, &checksum])
        .await?;
    Ok(())
}

/// 将 [`MigrationStatus::is_boot_ok`] 失败映射为错误（与 [`Migrator::verify`] 同一判定）。
pub fn ensure_boot_ok(status: &MigrationStatus) -> PostgresResult<()> {
    if status.is_boot_ok() {
        Ok(())
    } else {
        Err(PostgresError::Conflict(format!(
            "migration 启动校验失败: mismatches={}, unknown={:?}",
            status.mismatches.len(),
            status.unknown_applied
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_is_stable_and_sensitive() {
        let first = Migration::new(1, "a", "CREATE TABLE t (id int);").expect("迁移");
        assert_eq!(first.checksum(), first.checksum());
        assert_eq!(first.checksum().len(), 64);
        let second = Migration::new(1, "a", "CREATE TABLE t (id int); ").expect("迁移");
        assert_ne!(
            first.checksum(),
            second.checksum(),
            "空白变化必须改变 checksum"
        );
    }

    #[test]
    fn migration_rejects_bad_meta() {
        assert!(Migration::new(0, "a", "x").is_err());
        assert!(Migration::new(1, "", "x").is_err());
        assert!(Migration::new(1, "a", "  ").is_err());
        assert!(Migration::new(1, "n".repeat(257), "x").is_err());
    }

    #[test]
    fn compute_reports_mismatch_and_unknown() {
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
        assert_eq!(status.mismatches.len(), 1);
        assert_eq!(status.mismatches[0].version, 1);
        assert_eq!(status.unknown_applied, vec![99]);
        assert_eq!(status.pending, vec![2]);
        assert!(!status.is_boot_ok());
        assert!(!status.is_clean());
    }

    #[test]
    fn compute_clean_when_synced() {
        let plan = vec![Migration::new(1, "a", "CREATE TABLE a (id int);").expect("迁移")];
        let applied = vec![AppliedMigration {
            version: 1,
            name: "a".into(),
            checksum: plan[0].checksum(),
        }];
        let status = MigrationStatus::compute(applied, &plan);
        assert!(status.is_clean());
        assert!(status.is_boot_ok());
        ensure_boot_ok(&status).expect("同步状态应放行");
    }

    #[test]
    fn ensure_boot_ok_rejects_unknown() {
        let status = MigrationStatus {
            applied: vec![],
            pending: vec![],
            mismatches: vec![],
            unknown_applied: vec![99],
        };
        let error = ensure_boot_ok(&status).expect_err("未知版本必须拒绝");
        assert!(matches!(error, PostgresError::Conflict(_)));
        assert!(error.to_string().contains("unknown"));
    }

    #[test]
    fn non_transactional_ddl_is_rejected() {
        assert!(non_transactional_ddl_reason("CREATE INDEX CONCURRENTLY idx ON t(c)").is_some());
        assert!(non_transactional_ddl_reason("VACUUM ANALYZE t").is_some());
        assert!(non_transactional_ddl_reason("CREATE TABLE t (id int);").is_none());
    }

    #[test]
    fn locked_apply_decisions() {
        let migration = Migration::new(1, "a", "CREATE TABLE t (id int);").expect("迁移");
        assert_eq!(
            decide_locked_apply(&migration, None).expect("apply"),
            LockedApplyAction::Apply
        );
        let matching = AppliedMigration {
            version: 1,
            name: "a".into(),
            checksum: migration.checksum(),
        };
        assert_eq!(
            decide_locked_apply(&migration, Some(&matching)).expect("skip"),
            LockedApplyAction::Skip
        );
        let stale = AppliedMigration {
            version: 1,
            name: "a".into(),
            checksum: "deadbeef".into(),
        };
        let error = decide_locked_apply(&migration, Some(&stale)).expect_err("mismatch");
        assert!(matches!(error, PostgresError::Conflict(_)));
    }

    #[test]
    fn constants_are_stable() {
        assert_eq!(SCHEMA_MIGRATIONS_TABLE, "infra_schema_migrations");
        assert_ne!(MIGRATION_LOCK_KEY1, 0);
        assert_ne!(MIGRATION_LOCK_KEY2, 0);
        assert!(ADVISORY_XACT_LOCK_SQL.contains("pg_advisory_xact_lock"));
        assert!(
            !ADVISORY_XACT_LOCK_SQL.contains("pg_advisory_lock("),
            "禁止会话级 pg_advisory_lock（错误路径会泄漏）"
        );
    }
}
