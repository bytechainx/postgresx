# postgresx checklist

本文件只复述本仓已有门禁，不新增义务。命令差异以 [README.md](README.md)「门禁」（若有）与 `.github/workflows/ci.yml` 为准。`[x]` 表示本轮已在 crate 根目录跑过且退出码为 0。

来源：工作区 README「质量门禁」默认四条（本仓 README 无「门禁」节）

## P0

- [ ] `cargo fmt --check`
- [ ] `cargo clippy --all-targets -- -D warnings`
- [ ] `cargo test --all-targets`
- [ ] `cargo package --no-verify`
- [ ] `cargo deny check`（CI `cargo deny` job 同口径）
- [ ] MSRV `cargo check --all-features`（CI `msrv 1.89` job，工具链 1.89）

## 说明

- 从 crate 根目录运行；执行前核对工作树归属。
- 验收命令口径与 `CONTRIBUTING.md` 门禁三件套及 `docs/标准.md` §5 一致：`cargo test --all-targets`。
- 不执行 `cargo publish`。README / rustdoc / 元数据不承诺 crates.io 或 docs.rs。
- 工作区级检查在元仓库执行，见工作区 `README.md`「质量门禁」。
- 机器全绿不代替已登记的人审，也不产生合入权。
