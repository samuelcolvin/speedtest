//! Measurement against Measurement Lab's NDT7 servers, the platform behind the speed tests in
//! Google Search and Google Fiber. See <https://github.com/m-lab/ndt-server/blob/main/spec/ndt7-protocol.md>.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use reqwest::{Client, Url};
use serde::Deserialize;
use tokio::net::TcpStream;
use tokio::task::JoinSet;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

const LOCATE_URL: &str = "https://locate.measurementlab.net/v2/nearest/ndt/ndt7";
const NDT7_PROTOCOL: &str = "net.measurementlab.ndt.v7";
const DOWNLOAD_KEY: &str = "wss:///ndt/v7/download";
const UPLOAD_KEY: &str = "wss:///ndt/v7/upload";

/// Upload message sizing per the NDT7 spec: start at 8 KiB and double while the message is
/// smaller than 1/16 of what has been sent so far, never exceeding 16 MiB.
const UP_INITIAL: usize = 1 << 13;
const UP_MAX: usize = 1 << 24;

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

#[derive(Deserialize)]
struct LocateResponse {
    results: Vec<LocateResult>,
}

#[derive(Deserialize)]
struct LocateResult {
    #[serde(default)]
    location: Location,
    urls: HashMap<String, String>,
}

#[derive(Deserialize, Default)]
struct Location {
    city: String,
    country: String,
}

/// The nearest NDT7 server as chosen by M-Lab's locate service.
pub struct Server {
    pub city: String,
    pub country: String,
    /// Plain HTTPS URL on the server, used for latency round trips.
    pub ping_url: String,
    pub download_url: String,
    pub upload_url: String,
}

/// Ask M-Lab for the nearest servers and take the first, which it ranks closest.
pub async fn locate(client: &Client) -> Result<Server> {
    let resp: LocateResponse = client
        .get(LOCATE_URL)
        .send()
        .await
        .context("contacting locate.measurementlab.net")?
        .error_for_status()
        .context("locate response")?
        .json()
        .await
        .context("parsing locate response")?;
    let Some(mut result) = resp.results.into_iter().next() else {
        bail!("locate.measurementlab.net returned no servers");
    };
    let download_url = result
        .urls
        .remove(DOWNLOAD_KEY)
        .context("locate response has no wss download url")?;
    let upload_url = result
        .urls
        .remove(UPLOAD_KEY)
        .context("locate response has no wss upload url")?;
    let host = Url::parse(&download_url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
        .context("download url has no host")?;
    Ok(Server {
        city: result.location.city,
        country: result.location.country,
        ping_url: format!("https://{host}/"),
        download_url,
        upload_url,
    })
}

async fn connect(url: &str) -> Result<WsStream> {
    let mut request = url.into_client_request().context("websocket url")?;
    request.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        HeaderValue::from_static(NDT7_PROTOCOL),
    );
    let (ws, _) = tokio_tungstenite::connect_async(request)
        .await
        .context("connecting to NDT7 server")?;
    Ok(ws)
}

/// Spawns the download: the server streams binary frames until it ends the test (about ten
/// seconds) or `deadline` passes, and every received byte is added to `counter`.
pub fn download(url: String, deadline: Instant, counter: &Arc<AtomicU64>) -> JoinSet<Result<()>> {
    let counter = counter.clone();
    let mut set = JoinSet::new();
    set.spawn(async move {
        let mut ws = connect(&url).await?;
        while let Some(msg) = ws.next().await {
            let msg = msg.context("download stream")?;
            match &msg {
                // Text frames are the server's periodic measurements; they count as transferred
                // bytes too, and we have no further use for their contents.
                Message::Binary(_) | Message::Text(_) => {
                    counter.fetch_add(msg.len() as u64, Ordering::Relaxed);
                }
                Message::Close(_) => break,
                _ => {}
            }
            if Instant::now() >= deadline {
                break;
            }
        }
        Ok(())
    });
    set
}

/// Spawns the upload: binary frames are sent until `deadline` or until the server ends the
/// test, counting each frame once the socket has accepted it. The server's measurement
/// frames are drained in the background so the connection never backs up.
pub fn upload(url: String, deadline: Instant, counter: &Arc<AtomicU64>) -> JoinSet<Result<()>> {
    let counter = counter.clone();
    let mut set = JoinSet::new();
    set.spawn(async move {
        let ws = connect(&url).await?;
        let (mut sink, mut stream) = ws.split();
        let reader = tokio::spawn(async move { while let Some(Ok(_)) = stream.next().await {} });

        let zeros = Bytes::from(vec![0u8; UP_MAX]);
        let mut size = UP_INITIAL;
        let mut sent = 0u64;
        while Instant::now() < deadline {
            if size < UP_MAX && (size as u64) < sent / 16 {
                size *= 2;
            }
            if let Err(e) = sink.send(Message::Binary(zeros.slice(..size))).await {
                // The server closes the socket when its ten seconds are up; that is the end of
                // the test, not a failure, provided the test actually got going.
                if sent > 0 {
                    break;
                }
                return Err(anyhow::Error::new(e).context("upload send"));
            }
            sent += size as u64;
            counter.fetch_add(size as u64, Ordering::Relaxed);
        }
        let _ = sink.send(Message::Close(None)).await;
        reader.abort();
        Ok(())
    });
    set
}
