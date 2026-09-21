//! `postgres://` / `postgresql://` URL 解析：authority、userinfo、IPv6 与 `%XX` 转义。
//!
//! 纯函数实现，不做配置校验——校验统一由 [`PostgresConfig::validate`] 负责。

use std::time::Duration;

use super::{PostgresConfig, SslMode, DEFAULT_PORT};
use crate::error::{PostgresError, PostgresResult};

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
pub(super) fn parse_url(url: &str) -> PostgresResult<PostgresConfig> {
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
