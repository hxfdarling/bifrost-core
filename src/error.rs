use thiserror::Error;
#[derive(Error, Debug)]
pub enum ProxyError {
    #[error("证书错误: {0}")]
    CertificateError(String),

    #[error("TLS错误: {0}")]
    TlsError(String),

    #[error("IO错误: {0}")]
    IoError(#[from] std::io::Error),

    #[error("HTTP错误: {0}")]
    HttpError(#[from] hyper::Error),

    #[error("连接错误: {0}")]
    ConnectionError(String),
}

pub type Result<T> = std::result::Result<T, ProxyError>;
