# Changelog — postgresx

本文件记录 `postgresx` 的用户可见变更，遵循 [Keep a Changelog](https://keepachangelog.com/)
与 [Semantic Versioning](https://semver.org/)。

本仓库代码自 `xhyper.rs` 的 `crates/platform/drivers/postgres` 抽取而来（抽取时点为 `0.3.27`）。
该工程内的版本线不在本文件中延续，本仓库从 `0.1.0` 重新起算。

## [Unreleased]

### 新增

- 三类合规测试（特性 002）：
  - `tests/tdd_contracts.rs`：公开接口契约全部 12 个入口的行为契约与 `// TDD-PROBE:` 红绿表（变异探测见 PR 描述）；
  - `tests/sdd_spec.rs`：`docs/标准.md` §1–§5 章节的 `// SPEC-MAP:` 1:1 可执行对照；
  - `tests/aidd_boundary.rs`：8 条对抗/边界用例与 `// AIDD:` 人工复核表。
- `tests/live_postgres.rs`：真实 PostgreSQL 的 live 用例（建连 / 结构化探活 / 唯一名临时表往返与清理 / close 收尾），默认 `#[ignore]`，凭据只读环境变量，运行方式见 `scripts/live/README.md`。

## [0.1.0] - 2026-09-21

### 新增

- 以独立 crate 形式提供 PostgreSQL 连接池 `PostgresPool`：`connect` / `new` / `acquire` /
  `execute` / `query` / `query_one` / `query_opt` / `with_transaction` / `begin` /
  `copy_in_bytes` / `copy_out_bytes` / `ping` / `health_check` / `stats` / `close`，
  句柄 `Clone`（内部 `Arc`）。
- 配置 `PostgresConfig` / `PostgresConfigBuilder` / `SslMode`：`from_env` / `from_toml` /
  `from_url` / `builder` 四入口等价，构造期 `validate()` fail-fast，公开 `ENV_*` / `DEFAULT_*`
  常量（环境变量前缀 `FOUNDATIONX_POSTGRESX_`）。
- 参数化 SQL 消费面：所有查询入口只接受 `$N` 占位符 + `ToSql` 参数；`PgConnection` 提供
  `COPY IN` / `COPY OUT` 原语，载荷受 `DEFAULT_COPY_IN_MAX_BYTES` / `DEFAULT_COPY_OUT_MAX_BYTES`
  （各 16 MiB）约束。
- 事务 `PgTransaction` 与状态机 `TxStatus`（`Active` / `Committed` / `RolledBack` / `Failed`）；
  `with_transaction` 闭包返回 `Ok` 即 `COMMIT`、返回 `Err` 即 `ROLLBACK`。
- 迁移 `Migrator` / `Migration` / `MigrationStatus` / `MigrationReport` / `ensure_boot_ok`：
  每条迁移记录 SHA-256 checksum，经 `pg_advisory_xact_lock` 串行化；`verify()` 只校验、
  **不**自动执行 DDL，执行 pending 需显式 `apply()`。
- rustls TLS：`MakeRustlsConnect` / `build_client_config` / `build_client_config_with_ca` /
  `build_client_config_with_options`，覆盖 webpki 公共根、系统信任库、自定义 CA 与 mTLS；
  `SslMode::{Disable, Prefer, Require}`。
- 重试原语 `PgRetryConfig` / `with_retry_sync` / `with_retry_async`：指数退避 + 抖动 + 总预算
  （deadline）；crate 内独立实现，不再依赖外部可靠性框架。
- 统一错误 `PostgresError` / `PostgresResult` / `ErrorKind`：SQLSTATE 语义分类
  （`error_from_sqlstate` / `error_kind_from_sqlstate`）与 `is_retryable()`。
- 重新导出 `Row` 与 `ToSql`，便于调用方直接书写参数化查询。

### 变更

- 相对 `xhyper.rs` 源模块的破坏性变更：
  - 移除对内部 crate `kernel`、`config`、`resiliencx` 的依赖；错误模型从
    `kernel::ErrorKind` / `xerror_from_sqlstate` 下沉为 crate 内 `src/error.rs` 的
    `ErrorKind` / `PostgresError`。
  - 源模块的 `with_budget*` 重试族未迁移，统一收敛为 `PgRetryConfig` + `with_retry_sync` /
    `with_retry_async`（两者可组合出带预算的调用）。
  - 基准从源模块的 `query_hot_path` 更名为 `benches/hot_path.rs`。
- 不再随 crate 提供 `scaffold` / mock 适配器（源模块的 `PostgresAdapter` /
  `ObservingPostgresAdapter`）、`selfcheck` 与 `time_storage` 模块；本仓库只保留真实 PostgreSQL
  适配原语。

### 说明

- 本 crate **不发布到 crates.io**，仅以 GitHub 源码 / git 依赖形式复用；`cargo package`
  只作元数据完整性校验。
- `verify()` 与 `apply()` 的职责严格分离：启动默认入口 `verify()` 绝不执行 DDL。
- 事务块内无法执行的语句（`CREATE INDEX CONCURRENTLY` / `VACUUM` 等）被保守拒绝为
  `PostgresError::Unsupported`。
- 当前未实现 SCRAM-PLUS channel binding（`ChannelBinding::none()`），服务端强制
  channel binding 时会认证失败。
