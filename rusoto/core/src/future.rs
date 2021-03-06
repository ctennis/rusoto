use std::time::Duration;

use futures::{Future, IntoFuture, Poll, Async};
use futures::sync::oneshot::spawn;
use tokio::runtime::Runtime;

use super::credential::CredentialsError;
use super::client::{TimeoutFuture, SignAndDispatchError};
use super::request::{HttpResponse, HttpDispatchError};

lazy_static! {
    static ref FALLBACK_RUNTIME: Runtime = Runtime::new().unwrap();
}

/// Future that is returned from all rusoto service APIs.
///
/// ## Mocking
///
/// To mock service traits, you can use the `From` implementation to create `RusotoFuture`
/// instance.
///
/// ```rust,ignore
/// use rusoto_core::RusotoFuture;
/// use rusoto_s3::*;
///
/// pub struct S3Mock;
///
/// impl S3 for S3Mock {
///     fn abort_multipart_upload(
///         &self,
///         _input: AbortMultipartUploadRequest,
///     ) -> RusotoFuture<AbortMultipartUploadOutput, AbortMultipartUploadError> {
///         unimplemented!();
///     }
///
///     ...
///
///     fn put_object(&self, input: PutObjectRequest) -> RusotoFuture<PutObjectOutput, PutObjectError> {
///         if input.bucket == "foo" {
///             Ok(PutObjectOutput {
///                 ..Default::default()
///             }).into()
///         } else {
///             Err(PutObjectError::Validation("Invalid bucket".to_string())).into()
///         }
///     }
///
///     ...
/// }
/// ```
pub struct RusotoFuture<T, E> {
    state: Option<RusotoFutureState<T, E>>
}

pub fn new<T, E>(
        future: Box<TimeoutFuture<Item=HttpResponse, Error=SignAndDispatchError> + Send>,
        handler: fn(HttpResponse) -> Box<Future<Item=T, Error=E> + Send>
    ) -> RusotoFuture<T, E>
{
    RusotoFuture { state: Some(RusotoFutureState::SignAndDispatch { future, handler }) }
}

impl<T, E> RusotoFuture<T, E> {
    /// Set the timeout on the future to the provided duration.
    ///
    /// Unlike `set_timeout` this method can be easily chained:
    ///
    /// ```rust,ignore
    /// let future = s3.head_object(...)
    ///     .with_timeout(Duration::from_secs(10));
    /// ```
    ///
    /// This is only guaranteed to take effect when called before the future
    /// is polled for the first time.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.set_timeout(timeout);
        self
    }

    /// Set the timeout on the future to the provided duration.
    ///
    /// This is only guaranteed to take effect when called before the future
    /// is polled for the first time.
    pub fn set_timeout(&mut self, timeout: Duration) {
        if let Some(RusotoFutureState::SignAndDispatch { ref mut future, .. }) = self.state {
            future.set_timeout(timeout);
        }
    }

    /// Clear the timeout on the future.
    ///
    /// This is only guaranteed to take effect when called before the future
    /// is polled for the first time.
    pub fn clear_timeout(&mut self) {
        if let Some(RusotoFutureState::SignAndDispatch { ref mut future, .. }) = self.state {
            future.clear_timeout();
        }
    }

    /// Blocks the current thread until the future has resolved.
    ///
    /// This is meant to provide a simple way for non-async consumers
    /// to work with rusoto.
    pub fn sync(self) -> Result<T, E>
        where T: Send + 'static,
              E: From<CredentialsError> + From<HttpDispatchError> + Send + 'static
    {
        spawn(self, &FALLBACK_RUNTIME.executor()).wait()
    }
}

impl<T, E> Future for RusotoFuture<T, E>
    where E: From<CredentialsError> + From<HttpDispatchError>
{
    type Item = T;
    type Error = E;

    fn poll(&mut self) -> Poll<T, E> {
        match self.state.take().unwrap() {
            RusotoFutureState::SignAndDispatch { mut future, handler } => {
                match future.poll() {
                    Err(SignAndDispatchError::Credentials(err)) => Err(err.into()),
                    Err(SignAndDispatchError::Dispatch(err)) => Err(err.into()),
                    Ok(Async::Ready(response)) => {
                        self.state = Some(RusotoFutureState::RunningResponseHandler(handler(response)));
                        self.poll()
                    },
                    Ok(Async::NotReady) => {
                        self.state = Some(RusotoFutureState::SignAndDispatch { future, handler });
                        Ok(Async::NotReady)
                    }
                }
            },
            RusotoFutureState::RunningResponseHandler(mut future) => {
                match future.poll()? {
                    Async::Ready(value) => Ok(Async::Ready(value)),
                    Async::NotReady => {
                        self.state = Some(RusotoFutureState::RunningResponseHandler(future));
                        Ok(Async::NotReady)
                    }
                }
            }
        }
    }
}

enum RusotoFutureState<T, E> {
    SignAndDispatch {
        future: Box<TimeoutFuture<Item=HttpResponse, Error=SignAndDispatchError> + Send>,
        handler: fn(HttpResponse) -> Box<Future<Item=T, Error=E> + Send>
    },
    RunningResponseHandler(Box<Future<Item=T, Error=E> + Send>)
}

impl<T: Send + 'static, E: Send + 'static> From<Result<T, E>> for RusotoFuture<T, E> {
    fn from(value: Result<T, E>) -> Self {
        let fut = value.into_future();
        RusotoFuture {
            state: Some(RusotoFutureState::RunningResponseHandler(Box::new(fut))),
        }
    }
}

#[test]
fn rusoto_future_is_send() {
    fn is_send<T: Send>() {}

    is_send::<RusotoFuture<(), ()>>();
}

#[test]
fn rusuto_future_from_ok() {
    use std::error::Error;
    let fut: RusotoFuture<i32, Box<Error + Send + Sync>> = RusotoFuture::from(Ok(42));
    assert_eq!(fut.sync().unwrap(), 42);
}

#[test]
fn rusuto_future_from_err() {
    use std::error::Error;
    let fut: RusotoFuture<i32, Box<Error + Send + Sync>> =
        RusotoFuture::from("ab".parse::<i32>().map_err(|e| e.into()));
    assert!(fut.sync().is_err());
}
