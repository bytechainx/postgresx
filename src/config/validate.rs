//! `PostgresConfig::validate`：配置合法性校验。
//!
//! 自 `src/config.rs` 下沉而来。`validate` 是 `pub`，无需可见性调整；`PostgresConfig` 的定义
//! 仍在门面，子模块可直接读写其私有字段；`host_is_local` 是门面的 `pub fn`，子模块可直接调用。

use crate::error::{PostgresError, PostgresResult};

use super::{host_is_local, PostgresConfig, SslMode};

impl PostgresConfig {
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
}
