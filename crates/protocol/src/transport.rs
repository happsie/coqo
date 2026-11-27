// SPDX-License-Identifier: Apache-2.0

//! Shared plumbing for the client and server: split a byte stream into a
//! framed reader and a writer task fed by a channel.

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite, ReadHalf, WriteHalf};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::codec::{FramedRead, FramedWrite};

use crate::encoding::Encoding;
use crate::error::ProtocolError;
use crate::frame::FrameCodec;
use crate::message::{ErrorObject, Message, Response};

pub(crate) type Reader<IO> = FramedRead<ReadHalf<IO>, FrameCodec>;
pub(crate) type Writer<IO> = FramedWrite<WriteHalf<IO>, FrameCodec>;

pub(crate) fn split<IO>(io: IO, max_frame_len: usize) -> (Reader<IO>, Writer<IO>)
where
    IO: AsyncRead + AsyncWrite,
{
    let (r, w) = tokio::io::split(io);
    (
        FramedRead::new(r, FrameCodec::new(max_frame_len)),
        FramedWrite::new(w, FrameCodec::new(max_frame_len)),
    )
}

pub(crate) fn set_max_frame_len<IO>(reader: &mut Reader<IO>, writer: &mut Writer<IO>, max: usize) {
    reader.decoder_mut().set_max_frame_len(max);
    writer.encoder_mut().set_max_frame_len(max);
}

/// Encode and send one message.
pub(crate) async fn send<IO>(
    writer: &mut Writer<IO>,
    encoding: Encoding,
    message: &Message,
) -> Result<(), ProtocolError>
where
    IO: AsyncWrite,
{
    let bytes = encoding.encode(message)?;
    writer.send(Bytes::from(bytes)).await?;
    Ok(())
}

/// Read the next frame and decode it. `None` on a clean end of stream.
pub(crate) async fn recv<IO>(
    reader: &mut Reader<IO>,
    encoding: Encoding,
) -> Option<Result<Result<Message, crate::DecodeError>, ProtocolError>>
where
    IO: AsyncRead,
{
    match reader.next().await? {
        Ok(frame) => Some(Ok(encoding.decode(&frame))),
        Err(e) => Some(Err(e.into())),
    }
}

/// Drain `rx` onto the wire until every sender is gone or the transport
/// fails, then shut the write half down.
pub(crate) fn spawn_writer<IO>(
    mut writer: Writer<IO>,
    encoding: Encoding,
    mut rx: mpsc::Receiver<Message>,
) -> JoinHandle<()>
where
    IO: AsyncWrite + Send + 'static,
{
    tokio::spawn(async move {
        while let Some(message) = rx.recv().await {
            match send(&mut writer, encoding, &message).await {
                Ok(()) => {}
                Err(e @ (ProtocolError::FrameTooLarge { .. } | ProtocolError::Encode(_))) => {
                    if let Message::Response(r) = message {
                        // Never leave the peer waiting: turn an unsendable
                        // response into an error response.
                        tracing::warn!(error = %e, "replacing an unsendable response with an error");
                        let error = Response::err(
                            r.id,
                            ErrorObject::internal(format!("the response cannot be sent: {e}")),
                        );
                        if send(&mut writer, encoding, &error.into()).await.is_err() {
                            break;
                        }
                    } else {
                        tracing::warn!(error = %e, "dropping an unsendable outgoing message");
                    }
                }
                Err(e) => {
                    tracing::debug!(error = %e, "writer stopped");
                    break;
                }
            }
        }
        let _ = SinkExt::<Bytes>::close(&mut writer).await;
    })
}

/// Aborts the task when dropped, so a cancelled `serve` future does not
/// leave its writer running.
pub(crate) struct AbortOnDrop(pub(crate) JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}
