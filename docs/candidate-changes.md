# postgresx 候选改动清单（T101）

> 来源：工作区规格根 `specs/repos/infra/postgresx/tasks.md` T101。
> 基线：crate `origin/main` `3a7c8d6`；行为正本 `docs/标准.md`（v0.1.2）。
> 本清单只登记候选项与逐条依据，**不含代码改动**；每项须经 crate 仓 MR 评审
> （对应 tasks.md T102）后才拆为可执行任务。不产生合入权或发布授权。

## 已核对一致、无需改动的面

- `docs/标准.md` §2–§4 声明与实现抽查一致：env 前缀 `FOUNDATIONX_POSTGRESX_`、
  `ENV_*`/`DEFAULT_*` 常量、`deny_unknown_fields` 严格 TOML（`password` 键报错）、
  `Debug` 脱敏、非 loopback 强制 `sslmode=require`、`statement_timeout` 下发
  （`src/config.rs:381`）、advisory xact lock（`src/migration.rs:29`）、
  `verify()`/`ensure_boot_ok()`/`apply()` 语义、事务内禁用语句拒绝
  （`src/migration.rs:33-41`）、`ChannelBinding::none()`（`src/tls.rs:327`）、
  连接脱池（`src/guard.rs:14`）、`PgRetryConfig` 预算退避。
- `docs/API.md` 与 `src/lib.rs` re-export 面抽查一致（未逐条全量核对，见 C2）。

## 候选项

### C1 `AGENTS.md` MSRV 读数漂移

- **观察**：`AGENTS.md` 技术栈写「rust-version 1.85」；`Cargo.toml` 为
  `rust-version = "1.89"`，`CONTRIBUTING.md` 与 CI `msrv 1.89` job 一致，
  `CHANGELOG.md` Unreleased 已登记 MSRV 上调（uuid 1.27.0 依赖链要求）。
- **依据**：`AGENTS.md:11` vs `Cargo.toml:5`、`CONTRIBUTING.md:66`、
  `.github/workflows/ci.yml` msrv job。
- **建议**：把 `AGENTS.md` 的 1.85 改为 1.89（纯文档修正）。

### C2 `docs/API.md` 公开消费面覆盖口径待确认

- **观察**：API.md 称「公开 API 一览」，但 `src/lib.rs` 还导出以下条目未在表内：
  `with_retry_async_no_wait`、`PostgresPool::acquire_with`、`PostgresPool::new`、
  `PostgresPool::summary`、`RustlsConnect` / `RustlsStream`、`PoolStats`、
  `PostgresHealth`、`BoxFuture`、`AppliedMigration`、`SCHEMA_MIGRATIONS_TABLE`、
  `MIGRATION_LOCK_KEY1` / `MIGRATION_LOCK_KEY2`、`map_pool_error` /
  `map_tokio_error`；`PgRetryConfig` 的 `new` / `fixed` / `exponential` /
  `without_jitter` / `with_deadline` 构造面亦未逐条列出。
- **依据**：`src/lib.rs:102-127` pub use 清单 vs `docs/API.md` 表。
- **建议**：评审后二选一——补全表格，或在 API.md 写明收录口径
  （例如「只列消费方典型入口，完整面以 `cargo doc` 为准」）。不改运行时行为。

### C3 `checklist.md` 门禁命令口径与 CONTRIBUTING 不一致

- **观察**：`checklist.md` P0 写 `cargo test`，`CONTRIBUTING.md` 门禁三件套与
  `docs/标准.md` §5 均为 `cargo test --all-targets`；checklist 自身注释又提示
  「不要用会漏 doctest 的 `--all-targets` 替代」，两处口径需澄清
  `cargo test` 与 `--all-targets` 对 doctest 的实际覆盖差异。
- **依据**：`checklist.md` P0 表 vs `CONTRIBUTING.md:23-25`、
  `docs/标准.md` §5 验收块。
- **建议**：统一为一份命令口径；若 `--all-targets` 确实漏 doctest，
  修正 `docs/标准.md` §5 与 `CONTRIBUTING.md` 的验收命令（属行为正本变更，
  需单独评审）。

### C4 `checklist.md` P0 未覆盖 CI 其余必需 job

- **观察**：CI 另有 `cargo deny` 与 `msrv 1.89` 两个 job，`checklist.md` P0
  只列 fmt / clippy / test / package 四条，未复述 deny 与 MSRV 检查。
- **依据**：`.github/workflows/ci.yml` job 列表 vs `checklist.md` P0 表。
- **建议**：确认 checklist 的复述范围是否应含全部 CI 必需检查，或注明
  「P0 只列本地三件套 + 打包元数据，CI 扩展项见 workflow」。

## 评审结论（2026-11-15 Owner 裁定）

| 项 | 裁定 | 落实 |
| --- | --- | --- |
| C1 | 采纳修正 | `AGENTS.md` rust-version 1.85 → 1.89 |
| C2 | 补全表格 | `docs/API.md` 公开消费面补齐全部导出项 |
| C3 | 统一为 `--all-targets` | `checklist.md` P0 与 `docs/标准.md` §5、`CONTRIBUTING.md` 口径一致；doctest 覆盖由 CI `cargo test` 承担（`--all-targets` 不含 doctest，已实测） |
| C4 | 补全 CI 项 | `checklist.md` P0 增列 `cargo deny check` 与 MSRV `cargo check --all-features` |

## 不列为候选的已排除项

- `docs/标准.md` 自称版本 `v0.1.2` 与 `Cargo.toml` `0.1.2` 一致（此前工作区侧
  观察的「自称 0.1.0」漂移已消解，crate `3a7c8d6` 时点复核）。
- SCRAM-PLUS channel binding 未实现：`docs/API.md` 能力边界已明示，属登记在案
  的能力边界而非缺陷；是否立项实现另行决策。
- `graft/` 目录未入库（`.gitignore` 口径内），不产生仓内改动。
