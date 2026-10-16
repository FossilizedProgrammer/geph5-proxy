//! Upstream-proxy chaining for the engine's own outbound TCP.
//!
//! When `Config.upstream` is set, every base TCP dial (bridge/exit routes,
//! direct-TCP broker sources) is wrapped so the engine first connects to the
//! upstream proxy and establishes a CONNECT tunnel to the real destination.
//! Higher protocol layers (sosistab3, meeklike, TLS) sit above this and are
//! unaffected — they simply see a `Pipe` to the destination.

use std::net::SocketAddr;

use anyctx::AnyCtx;
use anyhow::Context;
use async_trait::async_trait;
use base64::{Engine as _, prelude::BASE64_STANDARD};
use geph5_misc_rpc::client_config::{UpstreamAuth, UpstreamProxy};
use sillad::dialer::{Dialer, DialerExt, DynDialer};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::{
    bound_dialer::{BoundTcpDialer, BoundTcpPipe},
    client::Config,
};

/// Wrap a raw TCP dial to `dest` with the configured upstream proxy, if any.
pub fn wrap_tcp(ctx: &AnyCtx<Config>, dest: SocketAddr) -> DynDialer {
    match &ctx.init().upstream {
        Some(upstream) => UpstreamDialer {
            upstream: upstream.clone(),
            dest,
        }
        .dynamic(),
        None => BoundTcpDialer { dest_addr: dest }.dynamic(),
    }
}

/// A dialer that reaches `dest` by CONNECTing through `upstream`.
pub struct UpstreamDialer {
    pub upstream: UpstreamProxy,
    pub dest: SocketAddr,
}

#[async_trait]
impl Dialer for UpstreamDialer {
    type P = BoundTcpPipe;

    async fn dial(&self) -> std::io::Result<Self::P> {
        let (proxy_addr, is_socks, auth) = match &self.upstream {
            UpstreamProxy::Socks5 { addr, auth } => (*addr, true, auth),
            UpstreamProxy::Http { addr, auth } => (*addr, false, auth),
        };
        let mut pipe = BoundTcpDialer {
            dest_addr: proxy_addr,
        }
        .dial()
        .await?;
        let result = if is_socks {
            socks5_connect(&mut pipe, self.dest, auth.as_ref()).await
        } else {
            http_connect(&mut pipe, self.dest, auth.as_ref()).await
        };
        result.map_err(|err| {
            tracing::warn!(%proxy_addr, dest = %self.dest, err = debug(&err), "upstream CONNECT failed");
            std::io::Error::other(format!("upstream proxy CONNECT to {} failed: {err:#}", self.dest))
        })?;
        pipe.set_remote(self.dest.to_string());
        Ok(pipe)
    }
}

async fn socks5_connect(
    pipe: &mut BoundTcpPipe,
    dest: SocketAddr,
    auth: Option<&UpstreamAuth>,
) -> anyhow::Result<()> {
    // Greeting: offer no-auth, plus user/pass when credentials are configured.
    let mut greeting = vec![0x05u8, 0x02, 0x00, 0x02];
    if auth.is_none() {
        greeting = vec![0x05, 0x01, 0x00];
    }
    pipe.write_all(&greeting).await?;
    let mut reply = [0u8; 2];
    pipe.read_exact(&mut reply).await?;
    anyhow::ensure!(reply[0] == 0x05, "upstream is not SOCKS5 (version {})", reply[0]);
    match reply[1] {
        0x00 => {}
        0x02 => {
            let auth = auth.context("upstream requires SOCKS5 auth but none was given")?;
            anyhow::ensure!(
                !auth.username.is_empty() && auth.username.len() <= 255 && auth.password.len() <= 255,
                "SOCKS5 auth must be 1..=255 bytes"
            );
            let mut req = vec![0x01, auth.username.len() as u8];
            req.extend_from_slice(auth.username.as_bytes());
            req.push(auth.password.len() as u8);
            req.extend_from_slice(auth.password.as_bytes());
            pipe.write_all(&req).await?;
            let mut sub = [0u8; 2];
            pipe.read_exact(&mut sub).await?;
            anyhow::ensure!(sub[0] == 0x01 && sub[1] == 0x00, "SOCKS5 auth rejected");
        }
        m => anyhow::bail!("SOCKS5 proxy offered no acceptable auth method ({m:#04x})"),
    }

    // CONNECT request with the destination address literally.
    let mut req = vec![0x05u8, 0x01, 0x00];
    match dest.ip() {
        std::net::IpAddr::V4(ip) => {
            req.push(0x01);
            req.extend_from_slice(&ip.octets());
        }
        std::net::IpAddr::V6(ip) => {
            req.push(0x04);
            req.extend_from_slice(&ip.octets());
        }
    }
    req.extend_from_slice(&dest.port().to_be_bytes());
    pipe.write_all(&req).await?;

    // Reply: VER REP RSV ATYP BIND.ADDR BIND.PORT
    let mut head = [0u8; 4];
    pipe.read_exact(&mut head).await?;
    anyhow::ensure!(head[0] == 0x05, "bad SOCKS5 reply version");
    anyhow::ensure!(head[1] == 0x00, "SOCKS5 CONNECT refused (code {})", head[1]);
    match head[3] {
        0x01 => {
            let mut skip = [0u8; 4 + 2];
            pipe.read_exact(&mut skip).await?;
        }
        0x04 => {
            let mut skip = [0u8; 16 + 2];
            pipe.read_exact(&mut skip).await?;
        }
        0x03 => {
            let mut len = [0u8; 1];
            pipe.read_exact(&mut len).await?;
            let mut skip = vec![0u8; len[0] as usize + 2];
            pipe.read_exact(&mut skip).await?;
        }
        atyp => anyhow::bail!("bad SOCKS5 reply ATYP {atyp}"),
    }
    Ok(())
}

async fn http_connect(
    pipe: &mut BoundTcpPipe,
    dest: SocketAddr,
    auth: Option<&UpstreamAuth>,
) -> anyhow::Result<()> {
    let mut req = format!("CONNECT {dest} HTTP/1.1\r\nHost: {dest}\r\n");
    if let Some(auth) = auth {
        let token = BASE64_STANDARD.encode(format!("{}:{}", auth.username, auth.password));
        req.push_str(&format!("Proxy-Authorization: Basic {token}\r\n"));
    }
    req.push_str("Proxy-Connection: Keep-Alive\r\n\r\n");
    pipe.write_all(req.as_bytes()).await?;

    // Read the status line + headers (capped so a hostile proxy can't OOM us).
    let mut buf = Vec::with_capacity(512);
    let mut byte = [0u8; 1];
    while !buf.ends_with(b"\r\n\r\n") {
        anyhow::ensure!(buf.len() < 8192, "HTTP CONNECT reply too large");
        let n = pipe.read(&mut byte).await?;
        anyhow::ensure!(n == 1, "upstream proxy closed during HTTP CONNECT");
        buf.push(byte[0]);
    }
    let text = String::from_utf8_lossy(&buf);
    let status_line = text.lines().next().unwrap_or_default();
    let code: u16 = status_line
        .split_whitespace()
        .nth(1)
        .context("malformed HTTP CONNECT status line")?
        .parse()
        .context("malformed HTTP CONNECT status code")?;
    anyhow::ensure!(code == 200, "HTTP CONNECT refused: {status_line}");
    Ok(())
}
