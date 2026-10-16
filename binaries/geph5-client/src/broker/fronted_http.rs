use std::{net::SocketAddr, sync::LazyLock, time::Instant};

use super::http_client::{client_builder, egress_addrs, with_dns};
use anyhow::Context;
use async_trait::async_trait;
use base64::{Engine as _, prelude::BASE64_STANDARD_NO_PAD};
use geph5_misc_rpc::client_config::UpstreamProxy;
use nanorpc::{JrpcRequest, JrpcResponse, RpcTransport};
use rand::Rng as _;

// Kept here for the device-IP probe, which uses the same loopback routing.
pub(crate) use super::http_client::OverrideDnsResolve;

pub struct FrontedHttpTransport {
    pub url: String,
    pub host: Option<String>,
    pub dns: Option<Vec<SocketAddr>>,
    pub upstream: Option<UpstreamProxy>,
}

/// Build the reqwest proxy descriptor for an upstream proxy, if one is set.
pub(crate) fn reqwest_proxy(upstream: &UpstreamProxy) -> anyhow::Result<reqwest::Proxy> {
    let (url, auth) = match upstream {
        UpstreamProxy::Socks5 { addr, auth } => (format!("socks5h://{addr}"), auth),
        UpstreamProxy::Http { addr, auth } => (format!("http://{addr}"), auth),
    };
    let proxy = reqwest::Proxy::all(&url)
        .with_context(|| format!("could not build reqwest proxy for {url}"))?;
    Ok(match auth {
        Some(auth) => proxy.basic_auth(&auth.username, &auth.password),
        None => proxy,
    })
}

#[async_trait]
impl RpcTransport for FrontedHttpTransport {
    type Error = anyhow::Error;
    async fn call_raw(&self, req: JrpcRequest) -> Result<JrpcResponse, Self::Error> {
        static POOL: LazyLock<reqwest::Client> =
            LazyLock::new(|| client_builder().build().unwrap());

        tracing::debug!(
            method = req.method,
            url = self.url,
            host = debug(&self.host),
            "calling broker through http"
        );
        let start = Instant::now();
        let url = reqwest::Url::parse(&self.url).context("unparseable broker front URL")?;
        let client = match (&self.upstream, egress_addrs(&url, self.dns.as_deref()).await?) {
            // With an upstream proxy set, all egress goes through it: skip the
            // physical-NIC forwarder (the upstream proxy is the egress) and
            // build a proxied client per call.
            (Some(upstream), _) => client_builder()
                .proxy(reqwest_proxy(upstream)?)
                .build()
                .context("could not build proxied broker HTTP client")?,
            (None, Some(addrs)) => with_dns(client_builder(), addrs)
                .build()
                .context("could not build broker HTTP client")?,
            (None, None) => POOL.clone(),
        };
        let mut request_builder = client.post(url).header("content-type", "application/json");

        if let Some(host) = &self.host {
            request_builder = request_builder
                .header("Host", host)
                .header("X-Padding", random_padding_header());
        }

        let request_body = serde_json::to_vec(&req)?;
        let response = request_builder
            .body(request_body)
            .send()
            .await
            .context("cannot send request to front")?;

        let resp_bytes = response.bytes().await?;
        tracing::trace!(
            method = req.method,
            url = self.url,
            host = debug(&self.host),
            resp_len = resp_bytes.len(),
            elapsed = debug(start.elapsed()),
            "response received through http",
        );
        Ok(serde_json::from_slice(&resp_bytes)?)
    }
}

fn random_padding_header() -> String {
    let mut rng = rand::rng();
    let mut bytes = vec![0u8; rng.random_range(7..=375)];
    rng.fill(bytes.as_mut_slice());
    BASE64_STANDARD_NO_PAD.encode(bytes)
}
