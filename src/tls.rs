//! rustls TLS 连接器：为 `tokio-postgres` / `deadpool-postgres` 提供
//! [`MakeTlsConnect`] / [`TlsConnect`] 实现。
//!
//! 默认信任根 = webpki 公共根 + 当前主机系统信任库；可叠加额外 PEM CA
//! （企业根或自签服务端证书）与 mTLS 客户端身份。
//!
//! **始终**校验服务端证书：本模块不提供任何 insecure / 跳过校验的旁路。

use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::OnceLock;
use std::task::{Context, Poll};

use rustls::ClientConfig;
use rustls_pki_types::ServerName;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_postgres::tls::{ChannelBinding, MakeTlsConnect, TlsConnect, TlsStream};
use tokio_rustls::TlsConnector;

use crate::error::{PostgresError, PostgresResult};

/// PEM 文件大小上限（1 MiB），避免误把大文件当作证书读入。
const PEM_FILE_MAX_BYTES: u64 = 1024 * 1024;

/// 是否启用 TLS channel binding（SCRAM-PLUS / `tls-server-end-point`）。
///
/// 固定为 `false`：仅普通 SCRAM-SHA-256。服务端强制 channel binding 时认证会失败；
/// 若将来打开该能力，必须同时实现握手材料导出并更新本常量与测试。
const CHANNEL_BINDING_ENABLED: bool = false;

// 编译期锚定：禁止静默改写该常量却没有实现。
const _: () = assert!(
    !CHANNEL_BINDING_ENABLED,
    "SCRAM-PLUS / channel binding 未实现"
);

/// 确保 rustls 进程级默认 crypto provider（ring）已安装。
fn ensure_crypto_provider() {
    static INIT: OnceLock<()> = OnceLock::new();
    let _ = INIT.get_or_init(|| {
        // 已安装时忽略错误（多个适配器共存属正常）。
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// 构建带 webpki 公共根与系统信任库的 rustls [`ClientConfig`]。
pub fn build_client_config() -> PostgresResult<ClientConfig> {
    build_client_config_with_options(None, None, None)
}

/// 构建 rustls [`ClientConfig`]：webpki + 系统根 + 可选额外 PEM CA。
///
/// `extra_ca_pem` 可含一张或多张 PEM 证书；文件上限 1 MiB，解析失败 fail-closed。
pub fn build_client_config_with_ca(extra_ca_pem: Option<&Path>) -> PostgresResult<ClientConfig> {
    build_client_config_with_options(extra_ca_pem, None, None)
}

/// 构建 rustls [`ClientConfig`]：webpki + 系统根 + 可选 CA + 可选 mTLS 身份。
///
/// - `client_cert` 与 `client_key` 必须同时提供或同时缺省；
/// - 私钥支持 PKCS#8 / RSA / EC PEM，解析失败 fail-closed；
/// - 无跳过服务端校验的旁路。
pub fn build_client_config_with_options(
    extra_ca_pem: Option<&Path>,
    client_cert: Option<&Path>,
    client_key: Option<&Path>,
) -> PostgresResult<ClientConfig> {
    ensure_crypto_provider();
    let mut roots =
        rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    add_native_roots(&mut roots);
    if let Some(path) = extra_ca_pem {
        load_extra_ca_pem(path, &mut roots)?;
    }
    let builder = ClientConfig::builder().with_root_certificates(roots);
    match (client_cert, client_key) {
        (None, None) => Ok(builder.with_no_client_auth()),
        (Some(cert_path), Some(key_path)) => {
            let certs = load_client_certs(cert_path)?;
            let key = load_client_private_key(key_path)?;
            builder.with_client_auth_cert(certs, key).map_err(|error| {
                PostgresError::Config(format!(
                    "postgresx: 客户端证书/私钥与 rustls 不兼容: {error}"
                ))
            })
        }
        _ => Err(PostgresError::Config(
            "postgresx: mTLS 需要同时设置 tls_client_cert 与 tls_client_key".to_string(),
        )),
    }
}

/// 叠加主机系统信任库（best-effort，不覆盖已有 webpki 根）。
fn add_native_roots(roots: &mut rustls::RootCertStore) {
    let result = rustls_native_certs::load_native_certs();
    let _ = roots.add_parsable_certificates(result.certs);
}

fn read_pem_file(path: &Path, label: &str) -> PostgresResult<std::io::BufReader<std::fs::File>> {
    let metadata = std::fs::metadata(path).map_err(|error| {
        PostgresError::Config(format!("postgresx: 无法检查{label}文件: {error}"))
    })?;
    if !metadata.is_file() {
        return Err(PostgresError::Config(format!(
            "postgresx: {label}路径必须是普通文件"
        )));
    }
    if metadata.len() > PEM_FILE_MAX_BYTES {
        return Err(PostgresError::Config(format!(
            "postgresx: {label}文件不得超过 1 MiB"
        )));
    }
    let file = std::fs::File::open(path)
        .map_err(|error| PostgresError::Config(format!("postgresx: 无法读取{label}: {error}")))?;
    Ok(std::io::BufReader::new(file))
}

fn load_client_certs(
    path: &Path,
) -> PostgresResult<Vec<rustls_pki_types::CertificateDer<'static>>> {
    let mut reader = read_pem_file(path, "客户端证书")?;
    let certs = rustls_pemfile::certs(&mut reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| {
            PostgresError::Config(format!("postgresx: 客户端证书 PEM 解析失败: {error}"))
        })?;
    if certs.is_empty() {
        return Err(PostgresError::Config(
            "postgresx: 客户端证书文件中没有证书".to_string(),
        ));
    }
    Ok(certs)
}

fn load_client_private_key(
    path: &Path,
) -> PostgresResult<rustls_pki_types::PrivateKeyDer<'static>> {
    let mut reader = read_pem_file(path, "客户端私钥")?;
    rustls_pemfile::private_key(&mut reader)
        .map_err(|error| {
            PostgresError::Config(format!("postgresx: 客户端私钥 PEM 解析失败: {error}"))
        })?
        .ok_or_else(|| PostgresError::Config("postgresx: 客户端私钥文件中没有私钥".to_string()))
}

fn load_extra_ca_pem(path: &Path, roots: &mut rustls::RootCertStore) -> PostgresResult<()> {
    let mut reader = read_pem_file(path, "TLS CA ")?;
    let certificates = rustls_pemfile::certs(&mut reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| {
            PostgresError::Config(format!("postgresx: TLS CA PEM 解析失败: {error}"))
        })?;
    if certificates.is_empty() {
        return Err(PostgresError::Config(
            "postgresx: TLS CA 文件中没有证书".to_string(),
        ));
    }
    for certificate in certificates {
        roots.add(certificate).map_err(|error| {
            PostgresError::Config(format!("postgresx: TLS CA 证书无效: {error}"))
        })?;
    }
    Ok(())
}

/// `deadpool-postgres` / `tokio-postgres` 可用的 rustls [`MakeTlsConnect`] 实现。
#[derive(Clone)]
pub struct MakeRustlsConnect {
    connector: TlsConnector,
}

impl MakeRustlsConnect {
    /// 使用 webpki 公共根与系统信任库构建连接器。
    pub fn with_webpki_roots() -> PostgresResult<Self> {
        Ok(Self::from_config(build_client_config()?))
    }

    /// webpki + 系统根 + 可选额外 PEM CA。
    pub fn with_webpki_and_ca(extra_ca_pem: Option<&Path>) -> PostgresResult<Self> {
        Ok(Self::from_config(build_client_config_with_ca(
            extra_ca_pem,
        )?))
    }

    /// webpki + 系统根 + 可选 CA + 可选 mTLS 客户端身份。
    pub fn with_options(
        extra_ca_pem: Option<&Path>,
        client_cert: Option<&Path>,
        client_key: Option<&Path>,
    ) -> PostgresResult<Self> {
        Ok(Self::from_config(build_client_config_with_options(
            extra_ca_pem,
            client_cert,
            client_key,
        )?))
    }

    /// 从 CA 文件路径构建（便捷封装）。
    pub fn with_ca_file(path: impl AsRef<Path>) -> PostgresResult<Self> {
        Self::with_webpki_and_ca(Some(path.as_ref()))
    }

    /// 从既有 [`ClientConfig`] 构建。
    #[must_use]
    pub fn from_config(config: ClientConfig) -> Self {
        ensure_crypto_provider();
        Self {
            connector: TlsConnector::from(Arc::new(config)),
        }
    }

    /// 为指定域名构造一次 [`RustlsConnect`]（SNI / 证书校验名）。
    pub fn for_domain(&self, domain: &str) -> PostgresResult<RustlsConnect> {
        let server_name = ServerName::try_from(domain.to_owned())
            .map_err(|_| PostgresError::Config(format!("非法 TLS 域名（SNI）: {domain}")))?;
        Ok(RustlsConnect {
            connector: self.connector.clone(),
            domain: server_name,
        })
    }

    /// 诊断辅助：是否提供了额外 CA 路径（不暴露任何密钥材料）。
    #[must_use]
    pub fn supports_extra_ca_path(path: Option<&PathBuf>) -> bool {
        path.is_some()
    }
}

impl std::fmt::Debug for MakeRustlsConnect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MakeRustlsConnect").finish_non_exhaustive()
    }
}

/// 单次 TLS 握手参数。
pub struct RustlsConnect {
    connector: TlsConnector,
    domain: ServerName<'static>,
}

impl std::fmt::Debug for RustlsConnect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RustlsConnect")
            .field("domain", &self.domain.to_str())
            .finish_non_exhaustive()
    }
}

impl<S> MakeTlsConnect<S> for MakeRustlsConnect
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    type Stream = RustlsStream<S>;
    type TlsConnect = RustlsConnect;
    type Error = PostgresError;

    fn make_tls_connect(&mut self, domain: &str) -> Result<RustlsConnect, Self::Error> {
        self.for_domain(domain)
    }
}

impl<S> TlsConnect<S> for RustlsConnect
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    type Stream = RustlsStream<S>;
    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = Result<RustlsStream<S>, io::Error>> + Send>>;

    fn connect(self, stream: S) -> Self::Future {
        let Self { connector, domain } = self;
        Box::pin(async move {
            let tls = connector.connect(domain, stream).await?;
            Ok(RustlsStream { inner: tls })
        })
    }
}

/// rustls 包装流（实现 tokio-postgres [`TlsStream`]）。
pub struct RustlsStream<S> {
    inner: tokio_rustls::client::TlsStream<S>,
}

impl<S> AsyncRead for RustlsStream<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S> AsyncWrite for RustlsStream<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

impl<S> TlsStream for RustlsStream<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    fn channel_binding(&self) -> ChannelBinding {
        // 当前未实现 SCRAM-PLUS 材料导出，固定返回 none。
        ChannelBinding::none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_config_builds() {
        let _config = build_client_config().expect("rustls client config");
    }

    #[test]
    fn make_tls_connect_accepts_domain() {
        let make = MakeRustlsConnect::with_webpki_roots().expect("连接器");
        let connect = make.for_domain("db.example.com").expect("SNI");
        assert!(format!("{connect:?}").contains("RustlsConnect"));
    }

    #[test]
    fn make_tls_connect_rejects_empty_domain() {
        let make = MakeRustlsConnect::with_webpki_roots().expect("连接器");
        assert!(make.for_domain("").is_err());
    }

    #[test]
    fn extra_ca_missing_file_fails_closed() {
        let error = build_client_config_with_ca(Some(Path::new("/no/such/postgresx-ca.pem")))
            .expect_err("缺失 CA 必须失败");
        assert!(matches!(error, PostgresError::Config(_)));
    }

    #[test]
    fn extra_ca_empty_file_fails_closed() {
        let dir = std::env::temp_dir().join(format!("postgresx-empty-ca-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("empty.pem");
        std::fs::write(&path, b"").expect("写入空文件");
        let error = build_client_config_with_ca(Some(&path)).expect_err("空 CA 必须失败");
        assert!(matches!(error, PostgresError::Config(_)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mtls_requires_both_cert_and_key() {
        let error =
            build_client_config_with_options(None, Some(Path::new("/tmp/only-cert.pem")), None)
                .expect_err("仅证书必须失败");
        assert!(error.to_string().contains("同时设置"));
    }

    #[test]
    fn mtls_missing_files_fail_closed() {
        let error = build_client_config_with_options(
            None,
            Some(Path::new("/no/such/client.crt")),
            Some(Path::new("/no/such/client.key")),
        )
        .expect_err("缺失文件必须失败");
        assert!(matches!(error, PostgresError::Config(_)));
    }
}
