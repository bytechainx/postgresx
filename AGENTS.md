# postgresx Agent 指南

> 本文件为 AI Agent 在本仓库工作时的入口指南。

## 项目定位

零内部依赖的 PostgreSQL 适配器 crate：连接池、参数化 SQL、事务、schema 迁移与 rustls TLS，只依赖 crates.io 公开包，可直接被任意 Rust 工程复用。

## 技术栈

- Rust edition 2021（rust-version 1.85）
- 关键依赖：`tokio-postgres` / `deadpool-postgres`、`tokio`、`tokio-rustls` / `rustls` / `rustls-native-certs` / `rustls-pemfile` / `webpki-roots`、`serde` / `serde_json`、`sha2`、`thiserror`、`toml`、`tracing`、`futures-util`、`bytes`
- 不依赖内部框架/私有 crate，零内部耦合

## 代码结构

```
src/
├── lib.rs          # 入口 + 公共 API re-export（#![deny(missing_docs)] / #![forbid(unsafe_code)]）
├── error.rs        # PostgresError（#[non_exhaustive]）+ ErrorKind + SQLSTATE 分类 + PostgresResult 别名
├── config.rs       # PostgresConfig + PostgresConfigBuilder + SslMode + from_env/from_toml/from_url + validate
├── pool.rs         # PostgresPool：acquire/execute/query/with_transaction/ping/health_check/stats/close
├── conn.rs         # PgConnection：参数化 SQL + COPY IN/OUT 原语与载荷上限常量
├── tx.rs           # PgTransaction + TxStatus 状态机
├── migration.rs    # Migrator / Migration：advisory lock + SHA-256 checksum，verify() 不自动 DDL
├── resilience.rs   # PgRetryConfig + with_retry_sync/with_retry_async：指数退避 + 抖动 + 总预算
└── tls.rs          # MakeRustlsConnect / build_client_config*：rustls 连接器（CA / mTLS）
tests/
├── api_surface.rs      # 公开 API 面
├── config_env.rs       # 环境变量 / TOML / URL 配置
├── ping_unreachable.rs # 失败路径（127.0.0.1:1）
└── pure_functions.rs   # SQLSTATE 分类等纯函数
benches/
└── hot_path.rs     # 热路径基准（harness = false，支持 --quick）
docs/
├── API.md          # 公开 API 一览与能力边界
└── 标准.md         # 定位、字段治理与验收标准
```

## 开发约定

- 注释与文档使用简体中文；标识符保持英文
- 错误类型：thiserror 枚举 + `#[non_exhaustive]` + `pub type PostgresResult<T> = ...`，含 SQLSTATE 语义分类与 `is_retryable()`
- 配置：`PostgresConfig` 结构体 + `builder()` + `from_env()` / `from_toml()` / `from_url()` + `validate()` + fail-fast
- 密码只能经环境变量 / URL / builder 注入；`Debug` 输出为 `***`；TOML 严格解析，出现未知键（含 `password`）直接报错
- 所有查询 API 只接受 `$N` 占位符 + `ToSql` 参数；不提供任何把字符串拼进 SQL 的公开入口
- 非 loopback 主机在 `validate()` 阶段强制 `sslmode=require`；rustls 始终校验服务端证书，无 insecure 旁路
- 禁止裸 `unwrap()`（库代码）/ 无注释 `expect()`
- 异步代码使用 tokio，禁止在 async 中做阻塞 I/O
- 迁移：`verify()` 只校验（checksum / 未知版本），绝不自动执行 DDL；`apply()` 须显式调用；事务块内禁用语句（`CREATE INDEX CONCURRENTLY` / `VACUUM`）保守拒绝

## 门禁三件套（P0）

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```

## 相关文档

- 组织 Rust 规范：`~/org-config/rulesets/rust/RULES.md`
- API 文档：`docs/API.md`
- 标准与验收：`docs/标准.md`
- 术语与领域语言：`CONTEXT.md`
- 贡献指南：`CONTRIBUTING.md`
- 变更记录：`CHANGELOG.md`
- 基准测试：`benches/hot_path.rs`
