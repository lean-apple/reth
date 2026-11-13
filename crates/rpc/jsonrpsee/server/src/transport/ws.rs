use crate::{
    server::{ConnectionState, ServerConfig},
    transport::stream::{serve_stream, MessageWriter, ReadError, StreamConnection},
    LOG_TARGET,
};

use futures_util::io::{BufReader, BufWriter};
use hyper::upgrade::Upgraded;
use hyper_util::rt::TokioIo;
use jsonrpsee_core::{middleware::RpcServiceT, server::MethodSink};
use soketto::connection::Error as SokettoError;
use tokio::sync::mpsc;
use tokio_util::compat::Compat;

pub(crate) type Sender = soketto::Sender<BufReader<BufWriter<Compat<TokioIo<Upgraded>>>>>;
pub(crate) type Receiver = soketto::Receiver<BufReader<BufWriter<Compat<TokioIo<Upgraded>>>>>;

pub use soketto::handshake::http::is_upgrade_request;

pub(crate) struct BackgroundTaskParams<S> {
    pub(crate) server_cfg: ServerConfig,
    pub(crate) conn: ConnectionState,
    pub(crate) ws_sender: Sender,
    pub(crate) ws_receiver: Receiver,
    pub(crate) rpc_service: S,
    pub(crate) sink: MethodSink,
    pub(crate) rx: mpsc::Receiver<String>,
}

pub(crate) async fn background_task<S>(params: BackgroundTaskParams<S>)
where
    S: RpcServiceT + Send + Sync + 'static,
{
    let BackgroundTaskParams { server_cfg, conn, ws_sender, ws_receiver, rpc_service, sink, rx } =
        params;
    let ServerConfig {
        batch_requests_config,
        max_request_body_size,
        write_timeout,
        message_buffer_capacity,
        ..
    } = server_cfg;

    let conn_id = conn.conn_id;
    let incoming = futures_util::stream::unfold(ws_receiver, move |mut receiver| async move {
        let mut data = Vec::new();
        loop {
            match receiver.receive(&mut data).await {
                Ok(soketto::Incoming::Data(_)) => return Some((Ok(data), receiver)),
                // The server sends no pings, so pongs are unsolicited and ignored.
                Ok(soketto::Incoming::Pong(_)) => data.clear(),
                Ok(soketto::Incoming::Closed(_)) | Err(SokettoError::Closed) => return None,
                Err(SokettoError::MessageTooLarge { current, maximum }) => {
                    tracing::debug!(
                        target: LOG_TARGET,
                        "WS recv error: message too large current={}/max={}",
                        current,
                        maximum
                    );
                    return Some((Err(ReadError::TooLarge), receiver));
                }
                Err(e) => {
                    tracing::debug!(target: LOG_TARGET, "WS error: {}; terminate connection: {}", e, conn_id);
                    return Some((Err(ReadError::Fatal(e)), receiver));
                }
            }
        }
    });
    tokio::pin!(incoming);

    serve_stream(StreamConnection {
        incoming,
        outgoing: WsWriter(ws_sender),
        rpc_service,
        sink,
        rx,
        stopped: conn.stop_handle.clone().shutdown(),
        batch_requests_config,
        max_request_body_size,
        max_concurrent_calls: message_buffer_capacity as usize,
        write_timeout: Some(write_timeout),
    })
    .await;

    drop(conn);
}

/// Writes text messages to a WebSocket.
struct WsWriter(Sender);

impl MessageWriter for WsWriter {
    type Error = SokettoError;

    async fn write(&mut self, message: String) -> Result<(), Self::Error> {
        self.0.send_text_owned(message).await
    }

    async fn flush(&mut self) -> Result<(), Self::Error> {
        self.0.flush().await
    }

    async fn close(&mut self) -> Result<(), Self::Error> {
        self.0.close().await
    }
}
