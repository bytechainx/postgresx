# postgresx 公开 API

**版本 / 角色**：`postgresx 0.1.0` · PostgreSQL 适配器（连接池 + 参数化 SQL + 事务 + 迁移 + rustls TLS + 重试）

## 公开消费面

| 入口 | 说明 |
| --- | --- |
| `PostgresConfig` / `PostgresConfigBuilder` / `SslMode` | 配置：`from_env` / `from_toml` / `from_url` / `validate` / `builder`；`SslMode::{Disable, Prefer, Require}` |
| `PostgresPool` | `connect` / `new` / `acquire` / `execute` / `query` / `query_one` / `query_opt` / `with_transaction` / `begin` / `copy_in_bytes` / `copy_out_bytes` / `ping` / `health_check` / `stats` / `close`；`Clone`（内部 `Arc`） |
| `PgConnection` | 连接句柄：参数化 SQL + `COPY` 原语 + `DEFAULT_COPY_IN_MAX_BYTES` / `DEFAULT_COPY_OUT_MAX_BYTES`（各 16 MiB） |
| `PgTransaction` / `TxStatus` | 事务句柄与准确状态机（`Active` / `Committed` / `RolledBack` / `Failed`） |
| `Migrator` / `Migration` / `MigrationStatus` / `MigrationReport` / `ensure_boot_ok` | 迁移：advisory lock + SHA-256 checksum；`verify()` 只校验，不自动执行 DDL |
| `MakeRustlsConnect` / `build_client_config` / `build_client_config_with_ca` / `build_client_config_with_options` | rustls TLS 连接器：webpki 公共根 + 系统信任库、自定义 CA、mTLS |
| `with_retry_sync` / `with_retry_async` / `PgRetryConfig` | 指数退避 + 抖动 + 总预算（deadline）的重试，自实现无外部可靠性框架依赖 |
| `PostgresError` / `PostgresResult` / `ErrorKind` | 统一错误、结果别名与 SQLSTATE 语义分类（`error_from_sqlstate` / `error_kind_from_sqlstate`） |
| `Row` / `ToSql` | 重新导出 `tokio_postgres` 的行类型与参数 trait |
| `ENV_*` / `DEFAULT_*` 常量 | 环境变量名（前缀 `FOUNDATIONX_POSTGRESX_`）与默认值 |

## 最小用法

```rust,no_run
use postgresx::{PostgresConfig, PostgresPool, PostgresResult, SslMode};

# async fn demo() -> PostgresResult<()> {
let config = PostgresConfig::builder()
    .host("127.0.0.1")
    .database("app")
    .user("app")
    .sslmode(SslMode::Disable)
    .build()?;

let pool = PostgresPool::connect(config).await?; // 会做一次 `SELECT 1` 冒烟验证

// 参数化 SQL：只接受 `$N` + ToSql，禁止拼接用户输入
pool.execute("CREATE TABLE IF NOT EXISTS demo (id BIGINT PRIMARY KEY, name TEXT NOT NULL)", &[])
    .await?;
let affected = pool
    .execute("INSERT INTO demo (id, name) VALUES ($1, $2)", &[&1_i64, &"alice"])
    .await?;
assert_eq!(affected, 1);

let row = pool.query_one("SELECT name FROM demo WHERE id = $1", &[&1_i64]).await?;
let name: String = row.get(0);
assert_eq!(name, "alice");

// 事务：闭包返回 Ok → COMMIT，返回 Err → ROLLBACK
pool.with_transaction(|tx| Box::pin(async move {
    tx.execute("INSERT INTO demo (id, name) VALUES ($1, $2)", &[&2_i64, &"bob"]).await?;
    Ok(())
})).await?;

pool.close();
# Ok(())
# }
```

## 能力边界

- **参数化 SQL 是唯一入口**：所有查询 API 只接受 `$N` 占位符 + `ToSql` 参数；不提供任何把
  字符串拼进 SQL 的公开 API。多语句 `batch_execute` 仅用于 crate 内部受信任迁移脚本，不对外。
- `COPY` 语句名由调用方提供，禁止拼接不可信标识符；载荷受 16 MiB 上限常量约束。
- 迁移：`verify()` 是启动默认入口，只校验 checksum 与未知版本，**绝不**自动执行 DDL；
  执行 pending 必须显式调用 `apply()`。含 `CREATE INDEX CONCURRENTLY` / `VACUUM` 等
  无法在事务块内执行的语句会被保守拒绝（`PostgresError::Unsupported`）。
- 当前未实现 SCRAM-PLUS channel binding（`ChannelBinding::none()`）；服务端若强制
  channel binding 会认证失败。
- `SslMode::Disable` / `Prefer` 仅限本机地址；非 loopback 主机 `validate()` 强制 `require`。
- 不提供 ORM、查询构建器或领域模型；重试策略经 `PgRetryConfig` 由调用方显式组合。
