//! The single owner of stdout. Two lanes feed one writer task:
//! - reliable (unbounded): responses, state/device/media events — never dropped
//! - frames (bounded): spectrum/pcm — dropped under backpressure, `seq` gaps
//!   tell the host
//!
//! Both lanes accept sends from plain OS threads (audio, COM) without an
//! async context.

use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, oneshot};
use tracing::{error, warn};

use crate::protocol::Response;
use crate::protocol::events::Event;

enum WriterMsg {
    Line(String),
    Flush(oneshot::Sender<()>),
}

#[derive(Clone)]
pub struct EventTx {
    reliable: mpsc::UnboundedSender<WriterMsg>,
    frames: mpsc::Sender<String>,
}

impl EventTx {
    pub fn send_response(&self, resp: &Response) {
        match serde_json::to_string(resp) {
            Ok(line) => {
                let _ = self.reliable.send(WriterMsg::Line(line));
            }
            Err(e) => error!("failed to serialize response: {e}"),
        }
    }

    /// Reliable event lane (never dropped).
    pub fn send_event(&self, ev: &Event) {
        match serde_json::to_string(ev) {
            Ok(line) => {
                let _ = self.reliable.send(WriterMsg::Line(line));
            }
            Err(e) => error!("failed to serialize event: {e}"),
        }
    }

    /// Droppable frame lane; returns false when the frame was dropped.
    pub fn try_send_frame(&self, ev: &Event) -> bool {
        match serde_json::to_string(ev) {
            Ok(line) => self.frames.try_send(line).is_ok(),
            Err(e) => {
                error!("failed to serialize frame: {e}");
                false
            }
        }
    }

    /// Wait until everything queued before this call has been written.
    pub async fn flush(&self) {
        let (tx, rx) = oneshot::channel();
        if self.reliable.send(WriterMsg::Flush(tx)).is_ok() {
            let _ = rx.await;
        }
    }
}

pub fn spawn(frame_capacity: usize) -> (EventTx, tokio::task::JoinHandle<()>) {
    let (reliable_tx, mut reliable_rx) = mpsc::unbounded_channel::<WriterMsg>();
    let (frame_tx, mut frame_rx) = mpsc::channel::<String>(frame_capacity);

    let join = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        let mut reliable_open = true;
        let mut frames_open = true;
        loop {
            let msg: Option<WriterMsg> = tokio::select! {
                biased;
                m = reliable_rx.recv(), if reliable_open => match m {
                    Some(m) => Some(m),
                    None => { reliable_open = false; continue; }
                },
                f = frame_rx.recv(), if frames_open => match f {
                    Some(line) => Some(WriterMsg::Line(line)),
                    None => { frames_open = false; continue; }
                },
                else => break,
            };
            match msg {
                Some(WriterMsg::Line(line)) => {
                    if write_line(&mut stdout, &line).await.is_err() {
                        // Host side of the pipe is gone; stop consuming.
                        warn!("stdout closed; writer exiting");
                        break;
                    }
                }
                Some(WriterMsg::Flush(done)) => {
                    let _ = stdout.flush().await;
                    let _ = done.send(());
                }
                None => break,
            }
        }
    });

    (
        EventTx {
            reliable: reliable_tx,
            frames: frame_tx,
        },
        join,
    )
}

async fn write_line(stdout: &mut tokio::io::Stdout, line: &str) -> std::io::Result<()> {
    stdout.write_all(line.as_bytes()).await?;
    stdout.write_all(b"\n").await?;
    stdout.flush().await
}
