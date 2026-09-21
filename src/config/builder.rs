//! [`PostgresConfigBuilder`]：链式覆盖字段，[`PostgresConfigBuilder::build`] 执行完整校验。
//!
//! 与 [`PostgresConfig::from_env`] / [`PostgresConfig::from_toml`] /
//! [`PostgresConfig::from_url`] 同源同规，缺字段即编译期可见。

use std::path::PathBuf;
use std::time::Duration;

use super::{PostgresConfig, SslMode, DEFAULT_MAX_POOL_SIZE, DEFAULT_PORT};
use crate::error::{PostgresError, PostgresResult};

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
