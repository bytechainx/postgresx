# postgresx

`postgresx` 是一个**零内部依赖**的 PostgreSQL 适配器 crate：连接池、参数化 SQL、
事务、schema 迁移与 rustls TLS，只依赖 crates.io 公开包，可直接被任意 Rust 工程复用。

- 统一错误模型：`PostgresError` / `PostgresResult`，含 SQLSTATE 语义分类与 `is_retryable()`
- 配置四入口：`from_env()` / `from_toml()` / `from_url()` / `builder()`，密码永不进入日志
- `PostgresPool`：`acquire` / `execute` / `query` / `query_one` / `query_opt` / `with_transaction`
  / `ping` / `health_check` / `stats` / `close`，`Clone`（内部 `Arc`）
- `PgConnection`：参数化 SQL + `COPY IN/OUT` 原语与载荷上限常量
- `PgTransaction` + `TxStatus`：准确状态机（`Active` / `Committed` / `RolledBack` / `Failed`）
- `Migrator`：advisory lock + SHA-256 checksum 校验；`verify()` 只校验，**不**自动执行 DDL
- rustls TLS：`SslMode::{Disable, Prefer, Require}`、自定义 CA、`rustls-native-certs` 系统根
- 重试：`PgRetryConfig` + 指数退避 + 抖动 + 总预算（deadline），自实现无外部可靠性框架依赖

## 安装

本 crate **不发布到 crates.io**，通过 git 依赖引入：

```toml
[dependencies]
postgresx = { git = "https://github.com/bytechainx/postgresx" }
```

需要异步运行时（`tokio`）；`tokio-postgres` 的历史类型支持（`chrono` / `serde_json` / `uuid`）
已随本 crate 的依赖开启。

## 最小可运行示例

```rust,no_run
use postgresx::{PostgresConfig, PostgresPool, PostgresResult, SslMode};

#[tokio::main]
async fn main() -> PostgresResult<()> {
    // 1) 配置（等价入口：from_env / from_toml / from_url / builder）
    let config = PostgresConfig::builder()
        .host("127.0.0.1")
        .port(5432)
        .database("app")
        .user("app")
        .password(std::env::var("FOUNDATIONX_POSTGRESX_PASSWORD").unwrap_or_default())
        .sslmode(SslMode::Disable)
        .build()?;

    // 2) 建池：会做一次 `SELECT 1` 冒烟验证
    let pool = PostgresPool::connect(config).await?;

    // 3) 参数化 EXECUTE / QUERY：只接受 `$N` + ToSql
    pool.execute(
        "CREATE TABLE IF NOT EXISTS demo (id BIGINT PRIMARY KEY, name TEXT NOT NULL)",
        &[],
    )
    .await?;

    let affected = pool
        .execute("INSERT INTO demo (id, name) VALUES ($1, $2)", &[&1_i64, &"alice"])
        .await?;
    println!("插入 {affected} 行");

    let row = pool.query_one("SELECT name FROM demo WHERE id = $1", &[&1_i64]).await?;
    let name: String = row.get(0);
    println!("id=1 -> {name}");

    let rows = pool.query("SELECT id, name FROM demo WHERE id > $1 ORDER BY id", &[&0_i64]).await?;
    println!("命中 {} 行", rows.len());

    // 4) 事务：闭包返回 Ok → COMMIT，返回 Err → ROLLBACK
    pool.with_transaction(|tx| {
        Box::pin(async move {
            tx.execute("INSERT INTO demo (id, name) VALUES ($1, $2)", &[&2_i64, &"bob"])
                .await?;
            Ok(())
        })
    })
    .await?;

    // 5) 健康检查
    pool.ping().await?;
    let health = pool.health_check().await?;
    println!(
        "server_version={} latency={:?} pool={:?}",
        health.server_version, health.latency, health.pool
    );

    pool.close();
    Ok(())
}
```

## 参数化 SQL 安全说明

**所有查询 API 只接受 `$N` 占位符 + `ToSql` 参数**，本 crate 不提供任何把字符串拼进 SQL 的
公开入口。请始终这样写：

```rust,ignore
// 正确：值走参数绑定，SQL 文本是常量
pool.query_one("SELECT id FROM users WHERE email = $1", &[&email]).await?;

// 错误（不要这样做）：把用户输入拼进 SQL 字符串
// let sql = format!("SELECT id FROM users WHERE email = '{email}'");
```

多语句 SQL（`batch_execute`）仅用于 crate 内部的受信任迁移脚本，不对调用方公开。
`COPY` 语句名由调用方提供，禁止拼接不可信标识符；载荷受
`DEFAULT_COPY_IN_MAX_BYTES` / `DEFAULT_COPY_OUT_MAX_BYTES`（各 16 MiB）约束。

其他安全默认值：

- `PostgresConfig` 的 `Debug` 对 `password` 输出 `***`，且 `password` 不是公开字段，
  只能经环境变量 / URL / builder 注入（不应写入 TOML 或日志）；
- 非 loopback 主机在 `validate()` 阶段强制 `sslmode=require`（`disable` / `prefer` 仅限本机）；
- rustls 始终校验服务端证书，无 insecure 旁路；
- `acquire_timeout` / `operation_timeout` 覆盖所有阻塞点，超时或任务取消时连接脱池，
  不会把状态未知的连接归还池中。

## 配置项

`PostgresConfig` 字段、环境变量（前缀 `FOUNDATIONX_POSTGRESX_*`）与 TOML 键一一对应：

| 字段 | 环境变量 | TOML 键 | 默认值 | 说明 |
| --- | --- | --- | --- | --- |
| `host` | `_HOST` | `host` | `127.0.0.1` | 主机 / IP / Unix socket 目录（`/` 开头） |
| `port` | `_PORT` | `port` | `5432`（`DEFAULT_PORT`） | 端口，0 非法 |
| `database` | `_DATABASE` | `database` | `postgres` | 数据库名 |
| `user` | `_USER` | `user` | `postgres` | 用户名 |
| `password` | `_PASSWORD` | —（禁止） | 空 | 只能经环境变量 / URL / builder 注入 |
| `sslmode` | `_SSLMODE` | `sslmode` | `disable` | `disable` / `prefer` / `require` |
| `max_pool_size` | `_MAX_POOL_SIZE` | `max_pool_size` | `16`（`DEFAULT_MAX_POOL_SIZE`） | 池上限，0 非法 |
| `application_name` | `_APPLICATION_NAME` | `application_name` | 无 | 服务端侧来源标识 |
| `connect_timeout` | `_CONNECT_TIMEOUT_MS` | `connect_timeout_ms` | 10s | 建连超时 |
| `acquire_timeout` | `_ACQUIRE_TIMEOUT_MS` | `acquire_timeout_ms` | 5s | 等待池连接超时 |
| `operation_timeout` | `_OPERATION_TIMEOUT_MS` | `operation_timeout_ms` | 10s | 单次 SQL/事务超时，并下发 `statement_timeout` |
| `tls_ca_file` | `_TLS_CA_FILE` | `tls_ca_file` | 无 | 额外 PEM CA / 服务端证书 |
| `tls_server_name` | `_TLS_SERVER_NAME` | `tls_server_name` | 无 | host 为 IP 时的 SNI/校验名 |
| `tls_client_cert` | `_TLS_CLIENT_CERT` | `tls_client_cert` | 无 | mTLS 客户端证书（与私钥成对） |
| `tls_client_key` | `_TLS_CLIENT_KEY` | `tls_client_key` | 无 | mTLS 客户端私钥（与证书成对） |

另有一个整体入口：`FOUNDATIONX_POSTGRESX_URL`（如
`postgres://user:pass@host:5432/db?sslmode=require`），作为其余环境变量的基底；
`from_url()` 同样可直接解析该格式。

TOML 采用严格解析：出现未知键（包括误写的 `password`）会直接报错，不会被静默忽略；
密码只能经环境变量 / URL / `PostgresConfigBuilder` 注入。

```rust,no_run
# use postgresx::{PostgresConfig, PostgresResult};
# fn demo() -> PostgresResult<()> {
// 环境变量
let _ = PostgresConfig::from_env()?;

// TOML（password 不在 TOML 中，若环境存在 _PASSWORD 会自动注入）
let _ = PostgresConfig::from_toml(r#"
host = "127.0.0.1"
port = 5432
database = "app"
user = "app"
sslmode = "disable"
max_pool_size = 8
acquire_timeout_ms = 3000
"#)?;

// URL
let _ = PostgresConfig::from_url("postgres://app:secret@127.0.0.1:5432/app?sslmode=disable")?;
# Ok(())
# }
```

## TLS

```rust,no_run
# use postgresx::{MakeRustlsConnect, PostgresResult, build_client_config_with_options};
# fn demo() -> PostgresResult<()> {
// 默认：webpki 公共根 + 系统信任库（rustls-native-certs）
let _default = MakeRustlsConnect::with_webpki_roots()?;

// 企业自签 CA（叠加在公共根之上）
let _custom_ca = MakeRustlsConnect::with_ca_file("/etc/ssl/private/corp-ca.pem")?;

// mTLS：客户端证书与私钥必须成对提供
let _mtls = build_client_config_with_options(
    Some(std::path::Path::new("/etc/ssl/certs/ca.pem")),
    Some(std::path::Path::new("/etc/ssl/certs/client.crt")),
    Some(std::path::Path::new("/etc/ssl/private/client.key")),
)?;
# Ok(())
# }
```

`SslMode::Prefer` / `SslMode::Require` 时池会使用上述 rustls 连接器；
`Disable` 使用 `NoTls`（仅允许本机地址）。当前未实现 SCRAM-PLUS channel binding
（`ChannelBinding::none()`），服务端若强制 channel binding 会认证失败。

## 迁移

```rust,no_run
# use postgresx::{Migration, Migrator, PostgresPool, PostgresResult};
# async fn demo(pool: PostgresPool) -> PostgresResult<()> {
let migrations = vec![
    Migration::new(1, "create_records", "CREATE TABLE records (id BIGINT PRIMARY KEY);")?,
    Migration::new(2, "add_name", "ALTER TABLE records ADD COLUMN name TEXT;")?,
];
let migrator = Migrator::new(pool, migrations)?;

// 启动默认入口：只校验 checksum / 未知版本，不执行任何 DDL
let status = migrator.verify().await?;
postgresx::ensure_boot_ok(&status)?; // 有 mismatch / 未知版本即 fail-closed
println!("pending={:?}", status.pending);

// 需要真正执行 pending 时显式调用
let report = migrator.apply().await?;
println!("本次应用: {:?}", report.applied_now);
# Ok(())
# }
```

- 每条迁移记录 SHA-256 checksum（`Migration::checksum()`），已应用版本的 SQL 被修改即
  报 `ChecksumMismatch` 并 fail-closed；
- 每条迁移在独立事务内取 `pg_advisory_xact_lock(MIGRATION_LOCK_KEY1, MIGRATION_LOCK_KEY2)`，
  多实例并发启动串行执行、commit/rollback 自动释放；
- 含 `CREATE INDEX CONCURRENTLY` / `VACUUM` 等无法在事务块内执行的语句会被保守拒绝
  （`PostgresError::Unsupported`），需拆分或由运维显式执行。

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in
the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any
additional terms or conditions.
