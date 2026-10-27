//! 测试专用的本地 TLS mock：测试 CA + 指定 SAN 的叶子证书 + HTTPS 服务。
//!
//! 生产代码不引用（整个模块 `#[cfg(test)]`）。抽成独立文件是因为两处都要用：
//!
//! - `doh` 的测试验证 DoH over TLS 的 Host/SNI/证书校验；
//! - `upgrade` 的测试验证 `--relay-insecure` 下自签 relay 的兜底下载能否握手。
//!
//! CA 只通过客户端的「额外信任根」注入，不写进程级信任库——否则一个测试放行的
//! CA 会让另一个测试的证书校验断言变成假阳性。

pub struct TestPki {
    pub ca_pem: Vec<u8>,
    pub leaf_pem: Vec<u8>,
    pub key_pem: Vec<u8>,
}

/// 生成「测试 CA + 指定 SAN 的叶子证书」。
pub fn make_pki(san: &str) -> TestPki {
    use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};

    let ca_key = KeyPair::generate().unwrap();
    let mut ca_params = CertificateParams::new(vec!["test-ca".to_string()]).unwrap();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let ca_cert = ca_params.self_signed(&ca_key).unwrap();

    let leaf_key = KeyPair::generate().unwrap();
    let leaf_params = CertificateParams::new(vec![san.to_string()]).unwrap();
    let leaf_cert = leaf_params.signed_by(&leaf_key, &ca_cert, &ca_key).unwrap();
    TestPki {
        ca_pem: ca_cert.pem().into_bytes(),
        leaf_pem: leaf_cert.pem().into_bytes(),
        key_pem: leaf_key.serialize_pem().into_bytes(),
    }
}

/// 起一个 HTTPS mock：每个请求交给 `handler` 处理。返回监听端口与任务句柄。
pub async fn spawn_tls_server<F, Fut>(
    pki: &TestPki,
    handler: F,
) -> (u16, tokio::task::JoinHandle<()>)
where
    F: Fn(axum::extract::Request) -> Fut + Clone + Send + Sync + 'static,
    Fut: std::future::Future<Output = axum::response::Response> + Send + 'static,
{
    use axum::Router;
    crate::tlsutil::install_rustls_provider();
    let app = Router::new().fallback(move |req: axum::extract::Request| {
        let handler = handler.clone();
        async move { handler(req).await }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let config =
        axum_server::tls_rustls::RustlsConfig::from_pem(pki.leaf_pem.clone(), pki.key_pem.clone())
            .await
            .unwrap();
    let listener = listener.into_std().unwrap();
    let handle = tokio::spawn(async move {
        let _ = axum_server::from_tcp_rustls(listener, config)
            .serve(app.into_make_service())
            .await;
    });
    (port, handle)
}
