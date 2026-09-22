//! 环境变量加载：`from_env` 与两个 env 读取辅助。
//!
//! 自 `src/config.rs` 下沉而来（模块名避开与 `std::env` 的同名遮蔽）。
//! `from_env` 是 `pub`，无需可见性调整；`env_optional` / `parse_env` 也被门面的 `from_toml`
//! 调用，故提为 `pub(super)`。`PostgresConfig` 的定义仍在门面，子模块可直接读写其私有字段。

use std::env;
use std::path::PathBuf;
use std::time::Duration;

use crate::error::{PostgresError, PostgresResult};

use super::url::parse_url;
use super::{
    PostgresConfig, SslMode, ENV_ACQUIRE_TIMEOUT_MS, ENV_APPLICATION_NAME, ENV_CONNECT_TIMEOUT_MS,
    ENV_DATABASE, ENV_HOST, ENV_MAX_POOL_SIZE, ENV_OPERATION_TIMEOUT_MS, ENV_PASSWORD, ENV_PORT,
    ENV_SSLMODE, ENV_TLS_CA_FILE, ENV_TLS_CLIENT_CERT, ENV_TLS_CLIENT_KEY, ENV_TLS_SERVER_NAME,
    ENV_URL, ENV_USER,
};

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
}

pub(super) fn env_optional(key: &str) -> Option<String> {
    env::var(key).ok().filter(|value| !value.trim().is_empty())
}

fn parse_env<T, F>(value: String, key: &str, parse: F) -> PostgresResult<T>
where
    F: FnOnce(&str) -> Option<T>,
{
    parse(&value)
        .ok_or_else(|| PostgresError::Config(format!("环境变量 {key} 不是合法数值: `{value}`")))
}
