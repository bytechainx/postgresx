# Changelog — postgresx

本文件记录 `postgresx` 的用户可见变更，遵循 [Keep a Changelog](https://keepachangelog.com/)
与 [Semantic Versioning](https://semver.org/)。

本仓库代码自 `xhyper.rs` 的 `crates/platform/drivers/postgres` 抽取而来（抽取时点为 `0.3.27`）。
该工程内的版本线不在本文件中延续，本仓库从 `0.1.0` 重新起算。

## [Unreleased]

### 变更

- `with_retry_sync` 文档补充阻塞语义说明：标注其使用 `std::thread::sleep`
  会阻塞当前线程，指引异步上下文应使用 `with_retry_async`，并列出典型调用场景
  （启动阶段/同步工具/测试辅助）；纯文档级改动，无行为变更。

## [0.1.2] - 2026-09-22

### 新增

- 三类合规测试（特性 002）：
  - `tests/tdd_contracts.rs`：公开接口契约全部 12 个入口的行为契约与 `// TDD-PROBE:` 红绿表（变异探测见 PR 描述）；
  - `tests/sdd_spec.rs`：`docs/标准.md` §1–§5 章节的 `// SPEC-MAP:` 1:1 可执行对照；
  - `tests/aidd_boundary.rs`：9 条对抗/边界用例与 `// AIDD:` 人工复核表。
- `tests/live_postgres.rs`：真实 PostgreSQL 的 live 用例（建连 / 结构化探活 / 唯一名临时表往返与清理 / close 收尾），默认 `#[ignore]`，凭据只读环境变量，运行方式见 `scripts/live/README.md`。

### 变更

- **内部结构改写（公开 API 与可观察契约均不变）**：按 `docs/module-rules.md` §5.5 的手法，把
  `src/config.rs` 的两块职责下沉为子模块 —— 环境变量加载（`from_env` 与 `env_optional` /
  `parse_env` 两个 env 读取辅助）→ `src/config/envvars.rs`；配置校验（`validate`）→
  `src/config/validate.rs`。门面 `src/config.rs` 保留模块文档、`DEFAULT_*` / `ENV_*` 常量、
  `SslMode`、`PostgresConfig` 定义与 `Default` / `Debug`、serde 辅助（`default_*` / `de_*`）、
  `from_toml` / `from_url` / `has_password` / 三个 `pub(crate)` 取值器 / `builder` /
  `to_deadpool_config`、`host_is_local` 与**原有内联测试**。
  `from_env` 与 `validate` 均为 `pub`，故**公开路径与签名一字未改**；`env_optional` 因同时被门面的
  `from_toml` 调用而提为 `pub(super)`，`parse_env` 只被同模块的 `from_env` 使用、**保持私有**。
  子模块名用 `envvars` 而非 `env`，避免 edition 2018 的 uniform path 遮蔽 `std::env`（同 `ossx` 的处理）。
  `src/config.rs` 生产段 **598 → 414** 行。
  动机：`module-rules` 是元仓库必需检查，且它审计各仓**默认分支**，故当 `config.rs` 生产段距
  `MR-STRUCT-007` 的 800 行 ERROR 阈值只剩 202 行时，任一仓的任意改动都可能卡住元仓库的全部 PR。
  属**纯搬移**（行多重集比对确认零代码行丢失），全部 120 项测试与 doctest 结果不变。

## [0.1.1] - 2026-09-22

### 修正

- `PostgresConfig::from_toml` 的解析错误不再包含 TOML 源码行：原实现把 `toml` 的 `Display`
  （带 span 与出错源码行）拼进 `PostgresError::Config`，配置里若误写 `password = "…"`，
  凭据明文会随错误消息进日志；现改用 `toml::de::Error::message()`（错误分类与消息语义不变，
  仅去掉源码行与位置标注）。

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
