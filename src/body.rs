//! Response body types shared by built-in, static and PHP responses.

use std::{
    pin::Pin,
    task::{Context, Poll},
};

use bytes::Bytes;
use http_body_util::{BodyExt, Empty, Full, combinators::BoxBody};
use hyper::body::{Body, Frame};
use tokio::sync::mpsc;

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// An error in a streamed body aborts the connection, so a client never
/// mistakes a truncated response for a complete one.
pub type ResponseBody = BoxBody<Bytes, BoxError>;

pub fn full(data: impl Into<Bytes>) -> ResponseBody {
    Full::new(data.into())
        .map_err(|never| match never {})
        .boxed()
}

pub fn empty() -> ResponseBody {
    Empty::new().map_err(|never| match never {}).boxed()
}

/// Sending half of a [`channel`] body. Bounded, so a slow client applies
/// backpressure to the producer instead of buffering the whole response.
pub type BodySender = mpsc::Sender<Result<Bytes, BoxError>>;

/// A body fed from another task, chunk by chunk.
pub fn channel(capacity: usize) -> (BodySender, ResponseBody) {
    let (tx, rx) = mpsc::channel(capacity);
    (tx, ChannelBody { rx }.boxed())
}

struct ChannelBody {
    rx: mpsc::Receiver<Result<Bytes, BoxError>>,
}

impl Body for ChannelBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        self.rx
            .poll_recv(cx)
            .map(|item| item.map(|chunk| chunk.map(Frame::data)))
    }
}
