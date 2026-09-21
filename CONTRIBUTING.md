# CONTRIBUTING.md — 贡献指南（postgresx）

本文件面向贡献者，汇总本地门禁与提交约定。
AI Agent 的工作约定另见 [`AGENTS.md`](./AGENTS.md)；术语与领域语言见 [`CONTEXT.md`](./CONTEXT.md)。

## 开发流程

- 本仓库是**独立的单 crate 仓库**，不依赖 `xhyper.rs` 主工程及其内部 crate（`kernel` /
  `contracts` / `config` 等），只依赖 crates.io 公开包。
- substantial 变更走 feature branch → PR → review → merge，**禁止直接 push `main`**。
- `main` 已启用分支保护：要求 PR + 必需检查 `fmt / clippy / test`，
  `required_approving_review_count = 0`（单人也能合并），禁止强推与删除。
- 合并方式固定为 **create a merge commit**。注意仓库设置是
  `merge_commit_title = MERGE_MESSAGE` + `merge_commit_message = PR_TITLE`，因此
  `gh pr merge` 必须显式传 `--subject` 与 `--body`，否则会产出通用
  `Merge pull request #N from …` 标题。
- 提交信息遵循 Conventional Commits（`feat:` / `fix:` / `docs:` / `ci:` / `chore:` /
  `refactor:`），描述用简体中文。

## 本地门禁（P0 三件套）

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```

本仓库无可选 feature，上述命令原样执行即可（`--all-targets` 已覆盖 tests 与 benches）。
集成测试全部离线运行：不依赖真实 PostgreSQL 实例，失败路径统一使用 `127.0.0.1:1`。

元数据完整性门禁（**不发布 crates.io**，此命令只校验打包元数据）：

```bash
cargo package --no-verify --allow-dirty
```

## 复用口径（不发布 crates.io）

- 本 crate **不发布到 crates.io**，仅以 GitHub 源码 / git 依赖形式复用。
- 文档与元数据中不得出现「可独立发布」「可直接 `cargo publish`」等表述，
  也不得放置 crates.io / docs.rs 徽章与外链。
- `Cargo.toml` 的 `documentation` 指向 `https://github.com/bytechainx/postgresx#readme`。
- 消费方引入方式（README「安装」小节为准）：

  ```toml
  [dependencies]
  postgresx = { git = "https://github.com/bytechainx/postgresx" }
  ```

## 开发约定

- 注释、文档、错误消息使用**简体中文**；标识符保持英文。
- 错误类型：`thiserror` 枚举 + `#[non_exhaustive]` + `pub type PostgresResult<T>` 别名，
  并保留 `is_retryable()` 与 SQLSTATE 语义分类。
- 不在库代码里裸 `unwrap()`（`[lints.clippy]` 已 `deny` `unwrap_used` / `expect_used` / `panic`）。
- 所有 `pub` 项必须有中文 `///` 文档（`missing_docs` 已 `deny`），`unsafe_code` 已 `forbid`。
- 集成测试**必须离线运行**，不触碰真实网络。
- **SQL 入口只接受 `$N` + `ToSql`**：不新增任何把字符串拼进 SQL 的公开 API；多语句
  `batch_execute` 仅限 crate 内部受信任迁移脚本。
- **密码注入面固定**：`password` 不是公开字段，只能经环境变量 / URL / builder 注入；
  TOML 严格解析，未知键（含误写的 `password`）直接报错；`Debug` 输出 `***`。
- **TLS 准入条件**：非 loopback 主机在 `validate()` 阶段强制 `sslmode=require`，
  rustls 无 insecure 旁路；mTLS 客户端证书与私钥必须成对提供。
- **迁移默认只读**：`verify()` 绝不执行 DDL，执行 pending 必须显式 `apply()`；
  事务块内禁用语句保守拒绝为 `PostgresError::Unsupported`。
- edition 2021，MSRV `rust-version = "1.85"`（改动依赖时同步核对）。

## 提交前自检清单

- [ ] `cargo fmt --all -- --check` 通过
- [ ] `cargo clippy --all-targets -- -D warnings` 通过
- [ ] `cargo test --all-targets` 通过
- [ ] `cargo package --no-verify --allow-dirty` 通过
- [ ] 新增 `pub` 项都有中文 `///` 文档
- [ ] 文档中无「可独立发布」/ crates.io / docs.rs 表述
- [ ] 新增查询 API 只接受 `$N` + `ToSql`，未引入字符串拼接 SQL 的入口
- [ ] 未在库代码中引入裸 `unwrap()` / `expect()`，密码未进入日志
