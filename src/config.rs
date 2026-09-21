//! Postgres 连接配置：环境变量、TOML 与 URL 三种加载入口。
//!
//! # 加载入口
//!
//! | 入口 | 用途 | 说明 |
//! |------|------|------|
//! | [`PostgresConfig::from_env`] | 容器/编排注入 | 读取 `FOUNDATIONX_POSTGRESX_*`；可选 `_URL` 作为基底 |
//! | [`PostgresConfig::from_toml`] | 静态配置文件 | 解析 TOML 后注入环境变量密码 |
//! | [`PostgresConfig::from_url`] | 单串快速接入 | `postgres://user:pass@host:5432/db?sslmode=require` |
//! | [`PostgresConfig::builder`] | 代码内构造 | 类型安全，缺字段即编译期可见 |
//!
//! # 安全约束
//!
//! - `password` 私有字段，[`std::fmt::Debug`] 输出 `***`，且被 `serde` 跳过：
//!   密码只能经环境变量、URL 或 [`PostgresConfigBuilder::password`] 注入；
//! - [`PostgresConfig::validate`] 对**非 loopback** 主机强制 `sslmode=require`，
//!   `disable` / `prefer` 仅允许本机地址（fail-closed）；
//! - mTLS 证书与私钥必须成对提供。
//!
//! # 环境变量列表
//!
//! 见模块内 `ENV_*` 常量（[`ENV_HOST`] ... [`ENV_TLS_CLIENT_KEY`]）。

use std::env;
use std::fmt;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize as _;

use crate::error::{PostgresError, PostgresResult};

/// 默认端口。
pub const DEFAULT_PORT: u16 = 5432;

/// 默认连接池上限。
pub const DEFAULT_MAX_POOL_SIZE: usize = 16;

/// 环境变量：完整连接 URL（可选，作为其余变量的基底）。
pub const ENV_URL: &str = "FOUNDATIONX_POSTGRESX_URL";
/// 环境变量：主机名或 IP。
pub const ENV_HOST: &str = "FOUNDATIONX_POSTGRESX_HOST";
/// 环境变量：端口。
pub const ENV_PORT: &str = "FOUNDATIONX_POSTGRESX_PORT";
/// 环境变量：数据库名。
pub const ENV_DATABASE: &str = "FOUNDATIONX_POSTGRESX_DATABASE";
/// 环境变量：用户名。
pub const ENV_USER: &str = "FOUNDATIONX_POSTGRESX_USER";
/// 环境变量：密码（唯一推荐的 secret 注入方式）。
pub const ENV_PASSWORD: &str = "FOUNDATIONX_POSTGRESX_PASSWORD";
/// 环境变量：`disable` / `prefer` / `require`。
pub const ENV_SSLMODE: &str = "FOUNDATIONX_POSTGRESX_SSLMODE";
/// 环境变量：连接池上限。
pub const ENV_MAX_POOL_SIZE: &str = "FOUNDATIONX_POSTGRESX_MAX_POOL_SIZE";
/// 环境变量：`application_name`。
pub const ENV_APPLICATION_NAME: &str = "FOUNDATIONX_POSTGRESX_APPLICATION_NAME";
/// 环境变量：连接建立超时（毫秒）。
pub const ENV_CONNECT_TIMEOUT_MS: &str = "FOUNDATIONX_POSTGRESX_CONNECT_TIMEOUT_MS";
/// 环境变量：等待池连接超时（毫秒）。
pub const ENV_ACQUIRE_TIMEOUT_MS: &str = "FOUNDATIONX_POSTGRESX_ACQUIRE_TIMEOUT_MS";
/// 环境变量：单次 SQL / 事务操作超时（毫秒）。
pub const ENV_OPERATION_TIMEOUT_MS: &str = "FOUNDATIONX_POSTGRESX_OPERATION_TIMEOUT_MS";
/// 环境变量：额外 PEM CA 文件路径。
pub const ENV_TLS_CA_FILE: &str = "FOUNDATIONX_POSTGRESX_TLS_CA_FILE";
/// 环境变量：TLS SNI / 证书校验名（连接地址为 IP 时使用）。
pub const ENV_TLS_SERVER_NAME: &str = "FOUNDATIONX_POSTGRESX_TLS_SERVER_NAME";
/// 环境变量：mTLS 客户端证书 PEM 路径。
pub const ENV_TLS_CLIENT_CERT: &str = "FOUNDATIONX_POSTGRESX_TLS_CLIENT_CERT";
/// 环境变量：mTLS 客户端私钥 PEM 路径。
pub const ENV_TLS_CLIENT_KEY: &str = "FOUNDATIONX_POSTGRESX_TLS_CLIENT_KEY";

/// TLS / SSL 模式。
///
/// - [`SslMode::Disable`]：`NoTls`，明文连接（仅允许本机地址）；
/// - [`SslMode::Prefer`]：优先 TLS，协商失败可回落明文（仅允许本机地址）；
/// - [`SslMode::Require`]：强制 TLS 并校验服务端证书。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub enum SslMode {
    /// 不使用 TLS。
    #[default]
    Disable,
    /// 优先 TLS（协商失败时可回退明文）。
    Prefer,
    /// 要求 TLS（证书校验）。
    Require,
}

impl SslMode {
    /// 解析 `sslmode` 字符串（大小写不敏感）。
    ///
    /// 兼容 PostgreSQL 官方别名：`allow` → `prefer`，`verify-ca` / `verify-full` → `require`。
    pub fn parse(value: &str) -> PostgresResult<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "disable" | "false" | "0" => Ok(Self::Disable),
            "prefer" | "allow" => Ok(Self::Prefer),
            "require" | "verify-ca" | "verify-full" => Ok(Self::Require),
            other => Err(PostgresError::Config(format!(
                "未知 sslmode `{other}`（期望 disable|prefer|require）"
            ))),
        }
    }

    /// 作为连接串 / 日志片段的字面量。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Disable => "disable",
            Self::Prefer => "prefer",
            Self::Require => "require",
        }
    }
}

impl<'de> serde::Deserialize<'de> for SslMode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).map_err(serde::de::Error::custom)
    }
}

/// 生产用 Postgres 配置。
///
/// `Debug` 输出对密码脱敏；`password` 字段本身不出现在公开 API 中，
/// 只能经环境变量 / URL / builder 注入，避免误把 secret 写进 TOML 或日志。
#[derive(Clone, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PostgresConfig {
    /// 主机名、IP 或 Unix socket 目录（以 `/` 开头）。
    pub host: String,
    /// 端口。
    pub port: u16,
    /// 数据库名。
    pub database: String,
    /// 用户名。
    pub user: String,
    /// 密码（脱敏输出，绝不写入日志）。
    #[serde(skip)]
    password: String,
    /// SSL 模式。
    pub sslmode: SslMode,
    /// 连接池上限。
    pub max_pool_size: usize,
    /// `application_name`（可选，便于服务端侧定位来源）。
    pub application_name: Option<String>,
    /// 连接建立超时（可选，`None` 表示交给系统默认）。
    #[serde(
        rename = "connect_timeout_ms",
        default = "default_connect_timeout",
        deserialize_with = "de_opt_duration_ms"
    )]
    pub connect_timeout: Option<Duration>,
    /// 等待池内连接的截止时间。
    #[serde(
        rename = "acquire_timeout_ms",
        default = "default_acquire_timeout",
        deserialize_with = "de_duration_ms"
    )]
    pub acquire_timeout: Duration,
    /// 单次 SQL 与事务终结操作的截止时间；同时下发为服务端 `statement_timeout`。
    #[serde(
        rename = "operation_timeout_ms",
        default = "default_operation_timeout",
        deserialize_with = "de_duration_ms"
    )]
    pub operation_timeout: Duration,
    /// 额外 PEM CA / 服务端证书文件（叠加 webpki 公共根与系统信任库）。
    pub tls_ca_file: Option<PathBuf>,
    /// TLS SNI / 证书校验名。
    ///
    /// 当 [`Self::host`] 为 IP 而证书 CN/SAN 为 DNS 名时设置：建池时以
    /// `hostaddr=IP` + `host=server_name` 分离 TCP 目标与校验名。
    pub tls_server_name: Option<String>,
    /// mTLS 客户端证书 PEM 路径（须与 [`Self::tls_client_key`] 成对）。
    tls_client_cert: Option<PathBuf>,
    /// mTLS 客户端私钥 PEM 路径（须与 [`Self::tls_client_cert`] 成对）。
    tls_client_key: Option<PathBuf>,
}

fn default_connect_timeout() -> Option<Duration> {
    Some(Duration::from_secs(10))
}

fn default_acquire_timeout() -> Duration {
    Duration::from_secs(5)
}

fn default_operation_timeout() -> Duration {
    Duration::from_secs(10)
}

fn de_duration_ms<'de, D>(deserializer: D) -> Result<Duration, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let millis = u64::deserialize(deserializer)?;
    Ok(Duration::from_millis(millis))
}

fn de_opt_duration_ms<'de, D>(deserializer: D) -> Result<Option<Duration>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let millis = Option::<u64>::deserialize(deserializer)?;
    Ok(millis.map(Duration::from_millis))
}

impl Default for PostgresConfig {
    /// 本机默认配置：`127.0.0.1:5432/postgres`，`sslmode=disable`，池上限 16。
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: DEFAULT_PORT,
            database: "postgres".to_string(),
            user: "postgres".to_string(),
            password: String::new(),
            sslmode: SslMode::Disable,
            max_pool_size: DEFAULT_MAX_POOL_SIZE,
            application_name: None,
            connect_timeout: default_connect_timeout(),
            acquire_timeout: default_acquire_timeout(),
            operation_timeout: default_operation_timeout(),
            tls_ca_file: None,
            tls_server_name: None,
            tls_client_cert: None,
            tls_client_key: None,
        }
    }
}

impl fmt::Debug for PostgresConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PostgresConfig")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("database", &self.database)
            .field("user", &self.user)
            .field("password", &"***")
            .field("sslmode", &self.sslmode)
            .field("max_pool_size", &self.max_pool_size)
            .field("application_name", &self.application_name)
            .field("connect_timeout", &self.connect_timeout)
            .field("acquire_timeout", &self.acquire_timeout)
            .field("operation_timeout", &self.operation_timeout)
            .field("tls_ca_file", &self.tls_ca_file)
            .field("tls_server_name", &self.tls_server_name)
            .field("tls_client_cert", &self.tls_client_cert)
            .field(
                "tls_client_key",
                &self
                    .tls_client_key
                    .as_ref()
                    .map(|_| PathBuf::from("<redacted>")),
            )
            .finish()
    }
}

impl PostgresConfig {
    /// 从 `FOUNDATIONX_POSTGRESX_*` 加载。
    ///
    /// - 若 [`ENV_URL`] 非空，先按 URL 解析作为基底，再逐字段覆盖；
    /// - 否则 [`ENV_HOST`] / [`ENV_DATABASE`] / [`ENV_USER`] 必须显式提供；
    /// - 末尾执行 [`Self::validate`]。
    pub fn from_env() -> PostgresResult<Self> {
        let url = env_optional(ENV_URL);
        let mut config = match url.as_deref() {
            Some(raw) => parse_url(raw)?,
            None => Self::default(),
        };

        if url.is_none() {
            let mut missing = Vec::new();
            match env_optional(ENV_HOST) {
                Some(value) => config.host = value,
                None => missing.push(ENV_HOST),
            }
            match env_optional(ENV_DATABASE) {
                Some(value) => config.database = value,
                None => missing.push(ENV_DATABASE),
            }
            match env_optional(ENV_USER) {
                Some(value) => config.user = value,
                None => missing.push(ENV_USER),
            }
            if !missing.is_empty() {
                return Err(PostgresError::Config(format!(
                    "缺少环境变量: {}（或改用 {ENV_URL}）",
                    missing.join(", ")
                )));
            }
        } else {
            if let Some(value) = env_optional(ENV_HOST) {
                config.host = value;
            }
            if let Some(value) = env_optional(ENV_DATABASE) {
                config.database = value;
            }
            if let Some(value) = env_optional(ENV_USER) {
                config.user = value;
            }
        }

        if let Some(value) = env_optional(ENV_PASSWORD) {
            config.password = value;
        }
        if let Some(value) = env_optional(ENV_SSLMODE) {
            config.sslmode = SslMode::parse(&value)?;
        }
        if let Some(value) = env_optional(ENV_PORT) {
            config.port = parse_env(value, ENV_PORT, |raw| raw.parse::<u16>().ok())?;
        }
        if let Some(value) = env_optional(ENV_MAX_POOL_SIZE) {
            config.max_pool_size =
                parse_env(value, ENV_MAX_POOL_SIZE, |raw| raw.parse::<usize>().ok())?;
        }
        if let Some(value) = env_optional(ENV_APPLICATION_NAME) {
            config.application_name = Some(value);
        }
        if let Some(value) = env_optional(ENV_CONNECT_TIMEOUT_MS) {
            config.connect_timeout = Some(Duration::from_millis(parse_env(
                value,
                ENV_CONNECT_TIMEOUT_MS,
                |raw| raw.parse::<u64>().ok(),
            )?));
        }
        if let Some(value) = env_optional(ENV_ACQUIRE_TIMEOUT_MS) {
            config.acquire_timeout =
                Duration::from_millis(parse_env(value, ENV_ACQUIRE_TIMEOUT_MS, |raw| {
                    raw.parse::<u64>().ok()
                })?);
        }
        if let Some(value) = env_optional(ENV_OPERATION_TIMEOUT_MS) {
            config.operation_timeout =
                Duration::from_millis(parse_env(value, ENV_OPERATION_TIMEOUT_MS, |raw| {
                    raw.parse::<u64>().ok()
                })?);
        }
        if let Some(value) = env_optional(ENV_TLS_CA_FILE) {
            config.tls_ca_file = Some(PathBuf::from(value));
        }
        if let Some(value) = env_optional(ENV_TLS_SERVER_NAME) {
            config.tls_server_name = Some(value);
        }
        if let Some(value) = env_optional(ENV_TLS_CLIENT_CERT) {
            config.tls_client_cert = Some(PathBuf::from(value));
        }
        if let Some(value) = env_optional(ENV_TLS_CLIENT_KEY) {
            config.tls_client_key = Some(PathBuf::from(value));
        }

        config.validate()?;
        Ok(config)
    }

    /// 从 TOML 字符串解析并校验。
    ///
    /// 字段名与结构体字段一致；超时字段为毫秒整数：
    ///
    /// ```toml
    /// host = "127.0.0.1"
    /// port = 5432
    /// database = "app"
    /// user = "app"
    /// sslmode = "require"
    /// max_pool_size = 8
    /// acquire_timeout_ms = 3000
    /// ```
    ///
    /// `password` 被 `serde` 跳过，**不**从 TOML 读取；若进程环境存在
    /// [`ENV_PASSWORD`]，解析后自动注入。未知键（含误写的 `password`）一律报错，
    /// 避免拼写错误被静默忽略。
    pub fn from_toml(text: &str) -> PostgresResult<Self> {
        let mut config: Self = toml::from_str(text)
            .map_err(|error| PostgresError::Config(format!("TOML 解析失败: {error}")))?;
        if let Some(password) = env_optional(ENV_PASSWORD) {
            config.password = password;
        }
        config.validate()?;
        Ok(config)
    }

    /// 从 `postgres://` / `postgresql://` URL 解析并校验。
    ///
    /// 识别 `user:password@host:port/database` 与查询参数 `sslmode`、
    /// `application_name`、`connect_timeout`（秒）；未知参数忽略（向前兼容）。
    /// `user` / `password` / `database` 支持 `%XX` 转义。
    pub fn from_url(url: &str) -> PostgresResult<Self> {
        let config = parse_url(url)?;
        config.validate()?;
        Ok(config)
    }

    /// 是否配置了非空密码（不暴露明文）。
    #[must_use]
    pub fn has_password(&self) -> bool {
        !self.password.is_empty()
    }

    /// 密码明文，仅供单元测试断言（生产代码只经 [`Self::to_deadpool_config`] 使用）。
    #[cfg(test)]
    pub(crate) fn password(&self) -> &str {
        &self.password
    }

    /// mTLS 客户端证书路径。
    pub(crate) fn tls_client_cert_path(&self) -> Option<&Path> {
        self.tls_client_cert.as_deref()
    }

    /// mTLS 客户端私钥路径。
    pub(crate) fn tls_client_key_path(&self) -> Option<&Path> {
        self.tls_client_key.as_deref()
    }

    /// 构建器入口。
    #[must_use]
    pub fn builder() -> PostgresConfigBuilder {
        PostgresConfigBuilder::default()
    }

    /// 校验配置合法性。
    ///
    /// 覆盖：必填字段、端口/池上限非零、超时非零、mTLS 成对、
    /// 以及「非 loopback 必须 `sslmode=require`」。
    pub fn validate(&self) -> PostgresResult<()> {
        if self.host.trim().is_empty() {
            return Err(PostgresError::Config(
                "PostgresConfig.host 不能为空".to_string(),
            ));
        }
        if self.database.trim().is_empty() {
            return Err(PostgresError::Config(
                "PostgresConfig.database 不能为空".to_string(),
            ));
        }
        if self.user.trim().is_empty() {
            return Err(PostgresError::Config(
                "PostgresConfig.user 不能为空".to_string(),
            ));
        }
        if self.port == 0 {
            return Err(PostgresError::Config(
                "PostgresConfig.port 不能为 0".to_string(),
            ));
        }
        if self.max_pool_size == 0 {
            return Err(PostgresError::Config(
                "PostgresConfig.max_pool_size 不能为 0".to_string(),
            ));
        }
        if self
            .connect_timeout
            .is_some_and(|timeout| timeout.is_zero())
            || self.acquire_timeout.is_zero()
            || self.operation_timeout.is_zero()
        {
            return Err(PostgresError::Config(
                "PostgresConfig timeout 必须大于零".to_string(),
            ));
        }
        if self.sslmode != SslMode::Require && !host_is_local(&self.host) {
            return Err(PostgresError::Config(
                "远程 PostgreSQL 必须使用 sslmode=require；disable/prefer 仅允许本机".to_string(),
            ));
        }
        if let Some(path) = &self.tls_ca_file {
            if path.as_os_str().is_empty() {
                return Err(PostgresError::Config(
                    "PostgresConfig.tls_ca_file 不能为空路径".to_string(),
                ));
            }
        }
        if let Some(name) = &self.tls_server_name {
            if name.trim().is_empty() {
                return Err(PostgresError::Config(
                    "PostgresConfig.tls_server_name 不能为空".to_string(),
                ));
            }
        }
        match (&self.tls_client_cert, &self.tls_client_key) {
            (None, None) => {}
            (Some(cert), Some(key)) => {
                if cert.as_os_str().is_empty() || key.as_os_str().is_empty() {
                    return Err(PostgresError::Config(
                        "PostgresConfig.tls_client_cert/key 不能为空路径".to_string(),
                    ));
                }
            }
            _ => {
                return Err(PostgresError::Config(
                    "PostgresConfig mTLS 需要同时设置 tls_client_cert 与 tls_client_key"
                        .to_string(),
                ));
            }
        }
        Ok(())
    }

    /// 转换为 deadpool-postgres 配置（不含 TLS 握手，握手见 [`crate::pool`]）。
    pub(crate) fn to_deadpool_config(&self) -> deadpool_postgres::Config {
        let mut config = deadpool_postgres::Config::new();
        // tls_server_name 存在且 host 为 IP 时，hostaddr=IP、host=server_name，
        // 使 SNI/证书校验名与 TCP 目标分离（企业自签证书常见场景）。
        if let Some(server_name) = self
            .tls_server_name
            .as_deref()
            .filter(|name| !name.is_empty())
        {
            match self.host.parse::<IpAddr>() {
                Ok(ip) => {
                    config.hostaddr = Some(ip);
                    config.host = Some(server_name.to_owned());
                }
                Err(_) => config.host = Some(server_name.to_owned()),
            }
        } else {
            config.host = Some(self.host.clone());
        }
        config.port = Some(self.port);
        config.dbname = Some(self.database.clone());
        config.user = Some(self.user.clone());
        if !self.password.is_empty() {
            config.password = Some(self.password.clone());
        }
        config.ssl_mode = Some(match self.sslmode {
            SslMode::Disable => deadpool_postgres::SslMode::Disable,
            SslMode::Prefer => deadpool_postgres::SslMode::Prefer,
            SslMode::Require => deadpool_postgres::SslMode::Require,
        });
        if let Some(name) = &self.application_name {
            config.application_name = Some(name.clone());
        }
        if let Some(timeout) = self.connect_timeout {
            config.connect_timeout = Some(timeout);
        }
        config.options = Some(format!(
            "-c statement_timeout={}",
            self.operation_timeout.as_millis()
        ));
        config.manager = Some(deadpool_postgres::ManagerConfig {
            // Clean：归还时校验并清空 session 状态，避免脏事务污染下一个借用者。
            recycling_method: deadpool_postgres::RecyclingMethod::Clean,
        });
        let mut pool = deadpool_postgres::PoolConfig::new(self.max_pool_size);
        pool.timeouts = deadpool_postgres::Timeouts {
            wait: Some(self.acquire_timeout),
            create: self.connect_timeout,
            recycle: Some(self.acquire_timeout),
        };
        config.pool = Some(pool);
        config
    }
}

/// [`PostgresConfig`] 构建器。
#[derive(Debug, Clone, Default)]
pub struct PostgresConfigBuilder {
    host: Option<String>,
    port: Option<u16>,
    database: Option<String>,
    user: Option<String>,
    password: Option<String>,
    sslmode: Option<SslMode>,
    max_pool_size: Option<usize>,
    application_name: Option<String>,
    connect_timeout: Option<Duration>,
    acquire_timeout: Option<Duration>,
    operation_timeout: Option<Duration>,
    tls_ca_file: Option<PathBuf>,
    tls_server_name: Option<String>,
    tls_client_cert: Option<PathBuf>,
    tls_client_key: Option<PathBuf>,
}

impl PostgresConfigBuilder {
    /// 主机。
    #[must_use]
    pub fn host(mut self, host: impl Into<String>) -> Self {
        self.host = Some(host.into());
        self
    }

    /// 端口。
    #[must_use]
    pub fn port(mut self, port: u16) -> Self {
        self.port = Some(port);
        self
    }

    /// 数据库名。
    #[must_use]
    pub fn database(mut self, database: impl Into<String>) -> Self {
        self.database = Some(database.into());
        self
    }

    /// 用户名。
    #[must_use]
    pub fn user(mut self, user: impl Into<String>) -> Self {
        self.user = Some(user.into());
        self
    }

    /// 密码（不进入 `Debug` 输出）。
    #[must_use]
    pub fn password(mut self, password: impl Into<String>) -> Self {
        self.password = Some(password.into());
        self
    }

    /// SSL 模式。
    #[must_use]
    pub fn sslmode(mut self, mode: SslMode) -> Self {
        self.sslmode = Some(mode);
        self
    }

    /// 连接池上限。
    #[must_use]
    pub fn max_pool_size(mut self, size: usize) -> Self {
        self.max_pool_size = Some(size);
        self
    }

    /// `application_name`。
    #[must_use]
    pub fn application_name(mut self, name: impl Into<String>) -> Self {
        self.application_name = Some(name.into());
        self
    }

    /// 连接建立超时。
    #[must_use]
    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = Some(timeout);
        self
    }

    /// 等待池连接的截止时间。
    #[must_use]
    pub fn acquire_timeout(mut self, timeout: Duration) -> Self {
        self.acquire_timeout = Some(timeout);
        self
    }

    /// 单次 SQL / 事务操作截止时间。
    #[must_use]
    pub fn operation_timeout(mut self, timeout: Duration) -> Self {
        self.operation_timeout = Some(timeout);
        self
    }

    /// 额外 PEM CA 文件路径。
    #[must_use]
    pub fn tls_ca_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.tls_ca_file = Some(path.into());
        self
    }

    /// TLS SNI / 证书校验服务器名。
    #[must_use]
    pub fn tls_server_name(mut self, name: impl Into<String>) -> Self {
        self.tls_server_name = Some(name.into());
        self
    }

    /// mTLS 客户端证书 PEM。
    #[must_use]
    pub fn tls_client_cert(mut self, path: impl Into<PathBuf>) -> Self {
        self.tls_client_cert = Some(path.into());
        self
    }

    /// mTLS 客户端私钥 PEM。
    #[must_use]
    pub fn tls_client_key(mut self, path: impl Into<PathBuf>) -> Self {
        self.tls_client_key = Some(path.into());
        self
    }

    /// 完成构建并校验。
    pub fn build(self) -> PostgresResult<PostgresConfig> {
        let defaults = PostgresConfig::default();
        let config = PostgresConfig {
            host: self.host.ok_or_else(|| {
                PostgresError::Config("PostgresConfigBuilder: 缺少 host".to_string())
            })?,
            port: self.port.unwrap_or(DEFAULT_PORT),
            database: self.database.ok_or_else(|| {
                PostgresError::Config("PostgresConfigBuilder: 缺少 database".to_string())
            })?,
            user: self.user.ok_or_else(|| {
                PostgresError::Config("PostgresConfigBuilder: 缺少 user".to_string())
            })?,
            password: self.password.unwrap_or_default(),
            sslmode: self.sslmode.unwrap_or_default(),
            max_pool_size: self.max_pool_size.unwrap_or(DEFAULT_MAX_POOL_SIZE),
            application_name: self.application_name,
            connect_timeout: self.connect_timeout.or(defaults.connect_timeout),
            acquire_timeout: self.acquire_timeout.unwrap_or(defaults.acquire_timeout),
            operation_timeout: self.operation_timeout.unwrap_or(defaults.operation_timeout),
            tls_ca_file: self.tls_ca_file,
            tls_server_name: self.tls_server_name,
            tls_client_cert: self.tls_client_cert,
            tls_client_key: self.tls_client_key,
        };
        config.validate()?;
        Ok(config)
    }
}

/// 主机是否为本地地址（`localhost`、loopback IP 或 Unix socket 路径）。
///
/// Unix socket 目录以 `/` 开头，不经 TCP，因此不要求 TLS。
#[must_use]
pub fn host_is_local(host: &str) -> bool {
    if host.starts_with('/') {
        return true;
    }
    let bare = host
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(host);
    bare.eq_ignore_ascii_case("localhost")
        || bare.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

fn env_optional(key: &str) -> Option<String> {
    env::var(key).ok().filter(|value| !value.trim().is_empty())
}

fn parse_env<T, F>(value: String, key: &str, parse: F) -> PostgresResult<T>
where
    F: FnOnce(&str) -> Option<T>,
{
    parse(&value)
        .ok_or_else(|| PostgresError::Config(format!("环境变量 {key} 不是合法数值: `{value}`")))
}

/// 已解析的 URL 片段（内部表示）。
struct UrlParts {
    user: String,
    password: String,
    host: String,
    port: u16,
    database: String,
    query: Vec<(String, String)>,
}

/// 解析 `postgres://` / `postgresql://` URL（不做校验）。
fn parse_url(url: &str) -> PostgresResult<PostgresConfig> {
    let parts = split_url(url)?;
    let mut config = PostgresConfig::default();
    if !parts.host.is_empty() {
        config.host = parts.host;
    }
    config.port = parts.port;
    if !parts.user.is_empty() {
        config.user = parts.user;
    }
    if !parts.database.is_empty() {
        config.database = parts.database;
    }
    config.password = parts.password;

    for (key, value) in &parts.query {
        match key.as_str() {
            "sslmode" | "ssl_mode" => config.sslmode = SslMode::parse(value)?,
            "application_name" => config.application_name = Some(value.clone()),
            "connect_timeout" => {
                let seconds: u64 = value.parse().map_err(|_| {
                    PostgresError::Config(format!("URL connect_timeout 不是合法秒数: `{value}`"))
                })?;
                config.connect_timeout = Some(Duration::from_secs(seconds));
            }
            _ => {}
        }
    }
    Ok(config)
}

fn split_url(url: &str) -> PostgresResult<UrlParts> {
    let rest = url
        .strip_prefix("postgres://")
        .or_else(|| url.strip_prefix("postgresql://"))
        .ok_or_else(|| {
            PostgresError::Config("URL 必须以 postgres:// 或 postgresql:// 开头".to_string())
        })?;

    let (authority, tail) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index + 1..]),
        None => (rest, ""),
    };
    let (path, query) = match tail.find('?') {
        Some(index) => (tail[..index].to_owned(), tail[index + 1..].to_owned()),
        None => (tail.to_owned(), String::new()),
    };
    let (user, password, hostport) = split_userinfo(authority);
    let (host, port) = split_host_port(hostport)?;

    Ok(UrlParts {
        user: percent_decode(&user)?,
        password: percent_decode(&password)?,
        host,
        port,
        database: percent_decode(&path)?,
        query: parse_query(&query)?,
    })
}

/// 拆出 `userinfo@host:port` 三段；无 userinfo 时 user/password 为空。
fn split_userinfo(authority: &str) -> (String, String, &str) {
    match authority.rfind('@') {
        Some(index) => {
            let userinfo = &authority[..index];
            let hostport = &authority[index + 1..];
            match userinfo.find(':') {
                Some(colon) => (
                    userinfo[..colon].to_owned(),
                    userinfo[colon + 1..].to_owned(),
                    hostport,
                ),
                None => (userinfo.to_owned(), String::new(), hostport),
            }
        }
        None => (String::new(), String::new(), authority),
    }
}

fn split_host_port(hostport: &str) -> PostgresResult<(String, u16)> {
    if let Some(rest) = hostport.strip_prefix('[') {
        // IPv6 字面量：`[::1]:5432`
        let (host, tail) = rest
            .split_once(']')
            .ok_or_else(|| PostgresError::Config("URL IPv6 主机缺少 `]`".to_string()))?;
        // `]` 之后只允许 `:port` 或直接结束；其余内容一律报错，不静默丢弃——
        // 与下方 hostname 分支对非法端口 fail-loud 的行为保持一致。
        let port = match tail {
            "" => DEFAULT_PORT,
            _ => parse_url_port(tail.strip_prefix(':').ok_or_else(|| {
                PostgresError::Config(format!("URL IPv6 主机后存在多余内容: `{tail}`"))
            })?)?,
        };
        return Ok((host.to_owned(), port));
    }
    match hostport.rsplit_once(':') {
        Some((host, port)) => Ok((host.to_owned(), parse_url_port(port)?)),
        None => Ok((hostport.to_owned(), DEFAULT_PORT)),
    }
}

fn parse_url_port(raw: &str) -> PostgresResult<u16> {
    if raw.is_empty() {
        return Ok(DEFAULT_PORT);
    }
    raw.parse::<u16>()
        .map_err(|_| PostgresError::Config(format!("URL 端口不是合法 u16: `{raw}`")))
}

fn parse_query(query: &str) -> PostgresResult<Vec<(String, String)>> {
    let mut pairs = Vec::new();
    for item in query.split('&').filter(|item| !item.is_empty()) {
        let (key, value) = item.split_once('=').unwrap_or((item, ""));
        pairs.push((percent_decode(key)?, percent_decode(value)?));
    }
    Ok(pairs)
}

/// `%XX` 转义解码（`+` 保持字面量，不回退为空格）。
fn percent_decode(raw: &str) -> PostgresResult<String> {
    if !raw.contains('%') {
        return Ok(raw.to_owned());
    }
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = bytes
                .get(index + 1..index + 3)
                .ok_or_else(|| PostgresError::Config("URL 百分号转义不完整".to_string()))?;
            let text = std::str::from_utf8(hex)
                .map_err(|_| PostgresError::Config("URL 百分号转义非法".to_string()))?;
            let byte = u8::from_str_radix(text, 16)
                .map_err(|_| PostgresError::Config(format!("URL 非法转义 `%{text}`")))?;
            out.push(byte);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).map_err(|_| PostgresError::Config("URL 含非法 UTF-8 字节".to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_stable() {
        let config = PostgresConfig::default();
        assert_eq!(config.port, DEFAULT_PORT);
        assert_eq!(config.max_pool_size, DEFAULT_MAX_POOL_SIZE);
        assert_eq!(config.sslmode, SslMode::Disable);
        assert!(!config.has_password());
        assert_eq!(SslMode::Disable.as_str(), "disable");
        assert_eq!(SslMode::Prefer.as_str(), "prefer");
        assert_eq!(SslMode::Require.as_str(), "require");
    }

    #[test]
    fn builder_roundtrip_and_redaction() {
        let config = PostgresConfig::builder()
            .host("127.0.0.1")
            .database("db")
            .user("u")
            .password("Sup3rSecret")
            .max_pool_size(4)
            .build()
            .expect("合法配置");
        assert!(config.has_password());
        assert_eq!(config.password(), "Sup3rSecret");
        let debug = format!("{config:?}");
        assert!(debug.contains("***"));
        assert!(!debug.contains("Sup3rSecret"));
    }

    #[test]
    fn remote_requires_ssl() {
        for mode in [SslMode::Disable, SslMode::Prefer] {
            let error = PostgresConfig::builder()
                .host("db.example.com")
                .database("db")
                .user("user")
                .sslmode(mode)
                .build()
                .expect_err("远程非 require 必须失败");
            assert!(matches!(error, PostgresError::Config(_)));
        }
        PostgresConfig::builder()
            .host("db.example.com")
            .database("db")
            .user("user")
            .sslmode(SslMode::Require)
            .build()
            .expect("远程 require 应通过");
    }

    #[test]
    fn local_hosts_skip_tls_requirement() {
        assert!(host_is_local("127.0.0.1"));
        assert!(host_is_local("localhost"));
        assert!(host_is_local("[::1]"));
        assert!(host_is_local("/var/run/postgresql"));
        assert!(!host_is_local("10.0.0.9"));
        assert!(!host_is_local("db.example.com"));
    }

    #[test]
    fn invalid_configs_are_rejected() {
        assert!(PostgresConfig::builder()
            .database("db")
            .user("u")
            .build()
            .is_err());
        assert!(PostgresConfig::builder()
            .host(" ")
            .database("db")
            .user("u")
            .build()
            .is_err());
        assert!(PostgresConfig::builder()
            .host("127.0.0.1")
            .user("u")
            .build()
            .is_err());
        assert!(PostgresConfig::builder()
            .host("127.0.0.1")
            .database("db")
            .user("u")
            .port(0)
            .build()
            .is_err());
        assert!(PostgresConfig::builder()
            .host("127.0.0.1")
            .database("db")
            .user("u")
            .max_pool_size(0)
            .build()
            .is_err());
        assert!(PostgresConfig::builder()
            .host("127.0.0.1")
            .database("db")
            .user("u")
            .operation_timeout(Duration::ZERO)
            .build()
            .is_err());
    }

    #[test]
    fn mtls_requires_pair() {
        let error = PostgresConfig::builder()
            .host("127.0.0.1")
            .database("db")
            .user("u")
            .sslmode(SslMode::Require)
            .tls_client_cert("/tmp/only.crt")
            .build()
            .expect_err("仅证书必须失败");
        assert!(error.to_string().contains("tls_client_cert"));
    }

    #[test]
    fn url_sets_password_and_ssl() {
        let config = PostgresConfig::from_url(
            "postgres://user:p%40ss@db.example.com:6432/app?sslmode=require&application_name=svc",
        )
        .expect("URL 解析");
        assert_eq!(config.host, "db.example.com");
        assert_eq!(config.port, 6432);
        assert_eq!(config.database, "app");
        assert_eq!(config.user, "user");
        assert_eq!(config.password(), "p@ss");
        assert_eq!(config.sslmode, SslMode::Require);
        assert_eq!(config.application_name.as_deref(), Some("svc"));
    }

    #[test]
    fn url_defaults_and_ipv6() {
        let config = PostgresConfig::from_url("postgresql://u@[::1]/db").expect("IPv6 解析");
        assert_eq!(config.host, "::1");
        assert_eq!(config.port, DEFAULT_PORT);
        assert_eq!(config.database, "db");
        assert!(config.password().is_empty());
    }

    #[test]
    fn url_ipv6_parses_port_forms() {
        let with_port =
            PostgresConfig::from_url("postgres://u@[::1]:6543/db").expect("IPv6 带端口");
        assert_eq!(with_port.host, "::1");
        assert_eq!(with_port.port, 6543);

        // `[::1]:` 与主机名形式的 `host:` 行为一致：端口为空回落到默认端口。
        let empty_port = PostgresConfig::from_url("postgres://u@[::1]:/db").expect("空端口");
        assert_eq!(empty_port.host, "::1");
        assert_eq!(empty_port.port, DEFAULT_PORT);
    }

    /// `]` 之后只允许 `:port` 或直接结束。
    ///
    /// 回归保护：此前该位置的非 `:` 文本会被**静默丢弃**——`postgres://u@[::1]junk/db`
    /// 会被接受并解析成 `host=::1`（实测确认），而**同一个函数的 hostname 分支**对
    /// `host:notaport` 会报错。两条分支行为不一致，且前者是静默的，属于「本地接受
    /// 畸形输入、问题推迟到运行期」这一类。
    #[test]
    fn url_ipv6_rejects_trailing_junk() {
        assert!(PostgresConfig::from_url("postgres://u@[::1]junk/db").is_err());
        assert!(PostgresConfig::from_url("postgres://u@[::1]:notaport/db").is_err());
        // 缺少 `]` 同样必须报错。
        assert!(PostgresConfig::from_url("postgres://u@[::1/db").is_err());
    }

    #[test]
    fn url_rejects_unknown_scheme() {
        assert!(PostgresConfig::from_url("mysql://host/db").is_err());
        assert!(PostgresConfig::from_url("postgres://host:notaport/db").is_err());
        assert!(PostgresConfig::from_url("postgres://host/db?sslmode=wat").is_err());
    }

    #[test]
    fn toml_parses_non_secret_fields() {
        let text = r#"
host = "127.0.0.1"
port = 6543
database = "app"
user = "app"
sslmode = "disable"
max_pool_size = 7
acquire_timeout_ms = 3000
operation_timeout_ms = 8000
"#;
        // `from_toml` 在进程环境存在 ENV_PASSWORD 时会注入密码（live 门禁会导出凭据），
        // 因此本用例先临时摘除该变量，确保断言的是「TOML 自身不携带密码」。
        let saved = env::var(ENV_PASSWORD).ok();
        env::remove_var(ENV_PASSWORD);
        let config = PostgresConfig::from_toml(text).expect("TOML 解析");
        if let Some(value) = saved {
            env::set_var(ENV_PASSWORD, value);
        }
        assert_eq!(config.port, 6543);
        assert_eq!(config.max_pool_size, 7);
        assert_eq!(config.acquire_timeout, Duration::from_millis(3000));
        assert_eq!(config.operation_timeout, Duration::from_millis(8000));
        assert!(!config.has_password());
    }

    #[test]
    fn toml_rejects_password_key() {
        let text = r#"
host = "127.0.0.1"
database = "app"
user = "app"
password = "leaked"
"#;
        let error =
            PostgresConfig::from_toml(text).expect_err("TOML 中的 password 必须 fail-closed");
        assert!(matches!(error, PostgresError::Config(_)));
        assert!(error.to_string().contains("TOML 解析失败"));
    }

    #[test]
    fn toml_rejects_unknown_keys() {
        let text = r#"
host = "127.0.0.1"
database = "app"
user = "app"
acquire_timout_ms = 500
"#;
        assert!(
            PostgresConfig::from_toml(text).is_err(),
            "拼写错误的键必须报错而不是静默使用默认值"
        );
    }

    #[test]
    fn toml_invalid_values_fail_closed() {
        assert!(PostgresConfig::from_toml("host = ").is_err());
        assert!(PostgresConfig::from_toml("port = \"x\"").is_err());
        assert!(PostgresConfig::from_toml(
            "host = \"db.example.com\"\ndatabase = \"d\"\nuser = \"u\"\nsslmode = \"disable\""
        )
        .is_err());
    }

    #[test]
    fn tls_server_name_with_ip_host_sets_hostaddr() {
        let config = PostgresConfig::builder()
            .host("84.247.154.45")
            .database("postgres")
            .user("postgres")
            .sslmode(SslMode::Require)
            .tls_server_name("db.internal")
            .build()
            .expect("配置");
        let deadpool_config = config.to_deadpool_config();
        assert_eq!(deadpool_config.host.as_deref(), Some("db.internal"));
        assert_eq!(
            deadpool_config.hostaddr,
            Some("84.247.154.45".parse().expect("ip")),
            "IP 必须走 hostaddr，SNI 走 host"
        );
    }

    #[test]
    fn sslmode_parse_aliases() {
        assert_eq!(
            SslMode::parse(" DISABLE ").expect("disable"),
            SslMode::Disable
        );
        assert_eq!(SslMode::parse("allow").expect("allow"), SslMode::Prefer);
        assert_eq!(
            SslMode::parse("verify-full").expect("verify-full"),
            SslMode::Require
        );
        assert!(SslMode::parse("wat").is_err());
    }
}
