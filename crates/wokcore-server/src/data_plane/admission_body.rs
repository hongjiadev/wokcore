use std::{
    pin::Pin,
    task::{Context, Poll},
};

use axum::{
    body::{Body, Bytes, HttpBody},
    response::Response,
};
use http_body::{Frame, SizeHint};

use crate::lifecycle::ActiveRequestGuard;

pub(crate) fn hold_admission_until_body_end(
    response: Response,
    guard: ActiveRequestGuard,
) -> Response {
    let (parts, body) = response.into_parts();
    Response::from_parts(parts, Body::new(AdmissionBody::new(body, guard)))
}

struct AdmissionBody {
    inner: Pin<Box<Body>>,
    guard: Option<ActiveRequestGuard>,
}

impl AdmissionBody {
    fn new(inner: Body, guard: ActiveRequestGuard) -> Self {
        Self {
            inner: Box::pin(inner),
            guard: Some(guard),
        }
    }
}

impl HttpBody for AdmissionBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        let frame = this.inner.as_mut().poll_frame(context);
        let finished = match &frame {
            Poll::Ready(None) | Poll::Ready(Some(Err(_))) => true,
            Poll::Ready(Some(Ok(_))) | Poll::Pending => false,
        };
        if finished {
            this.guard.take();
        }
        frame
    }

    fn is_end_stream(&self) -> bool {
        self.inner.as_ref().is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.as_ref().size_hint()
    }
}
