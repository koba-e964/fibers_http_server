use crate::dispatcher::{Dispatcher, DispatcherBuilder};
use crate::metrics::ServerMetrics;
use crate::{connection::Connection, error};
use crate::{Error, HandleRequest, HandlerOptions, Result};
use core::task::{Context, Poll as Poll03};
use factory::Factory;
use fibers::net::futures::{Connected, TcpListenerBind};
use fibers::{self, BoxSpawn, Spawn};
use futures::future::{loop_fn, ok, Either, Loop};
use futures::{Async, Future, Poll, Stream};
use futures03::compat::Compat;
use futures03::Stream as Stream03;
use futures03::TryFutureExt;
use futures03::TryStreamExt;
use httpcodec::DecodeOptions;
use prometrics::metrics::MetricBuilder;
use slog::{Discard, Logger};
use std::io::Result as IOResult;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::net::TcpStream;

/// HTTP server builder.
#[derive(Debug)]
pub struct ServerBuilder {
    bind_addr: SocketAddr,
    logger: Logger,
    metrics: MetricBuilder,
    dispatcher: DispatcherBuilder,
    options: ServerOptions,
}
impl ServerBuilder {
    /// Makes a new `ServerBuilder` instance.
    pub fn new(bind_addr: SocketAddr) -> Self {
        ServerBuilder {
            bind_addr,
            logger: Logger::root(Discard, o!()),
            metrics: MetricBuilder::default(),
            dispatcher: DispatcherBuilder::new(),
            options: ServerOptions {
                read_buffer_size: 8192,
                write_buffer_size: 8192,
                decode_options: DecodeOptions::default(),
            },
        }
    }

    /// Adds a HTTP request handler.
    ///
    /// # Errors
    ///
    /// If the path and method of the handler conflicts with the already registered handlers,
    /// an `ErrorKind::InvalidInput` error will be returned.
    pub fn add_handler<H>(&mut self, handler: H) -> Result<&mut Self>
    where
        H: HandleRequest,
        H::Decoder: Default,
        H::Encoder: Default,
    {
        self.add_handler_with_options(handler, HandlerOptions::default())
    }

    /// Adds a HTTP request handler with the given options.
    ///
    /// # Errors
    ///
    /// If the path and method of the handler conflicts with the already registered handlers,
    /// an `ErrorKind::InvalidInput` error will be returned.
    pub fn add_handler_with_options<H, D, E>(
        &mut self,
        handler: H,
        options: HandlerOptions<H, D, E>,
    ) -> Result<&mut Self>
    where
        H: HandleRequest,
        D: Factory<Item = H::Decoder> + Send + Sync + 'static,
        E: Factory<Item = H::Encoder> + Send + Sync + 'static,
    {
        track!(self.dispatcher.register_handler(handler, options))?;
        Ok(self)
    }

    /// Sets the logger of the server.
    ///
    /// The default value is `Logger::root(Discard, o!())`.
    pub fn logger(&mut self, logger: Logger) -> &mut Self {
        self.logger = logger;
        self
    }

    /// Sets `MetricBuilder` used by the server.
    ///
    /// The default value is `MetricBuilder::default()`.
    pub fn metrics(&mut self, metrics: MetricBuilder) -> &mut Self {
        self.metrics = metrics;
        self
    }

    /// Sets the application level read buffer size of the server in bytes.
    ///
    /// The default value is `8192`.
    pub fn read_buffer_size(&mut self, n: usize) -> &mut Self {
        self.options.read_buffer_size = n;
        self
    }

    /// Sets the application level write buffer size of the server in bytes.
    ///
    /// The default value is `8192`.
    pub fn write_buffer_size(&mut self, n: usize) -> &mut Self {
        self.options.write_buffer_size = n;
        self
    }

    /// Sets the options of the request decoder of the server.
    ///
    /// The default value is `DecodeOptions::default()`.
    pub fn decode_options(&mut self, options: DecodeOptions) -> &mut Self {
        self.options.decode_options = options;
        self
    }

    /// Builds a HTTP server with the given settings.
    pub fn finish<S>(self, spawner: S) -> Server
    where
        S: Spawn + Send + 'static,
    {
        let logger = self.logger.new(o!("server" => self.bind_addr.to_string()));

        info!(logger, "Starts HTTP server");
        let fut03: Pin<_> = Box::pin(TcpListener::bind(self.bind_addr));
        let fut01 = fut03.compat();
        Server {
            logger,
            metrics: ServerMetrics::new(self.metrics),
            spawner: spawner.boxed(),
            listener: Listener::Binding(Box::new(fut01.map_err(Error::from)), self.bind_addr),
            dispatcher: self.dispatcher.finish(),
            is_server_alive: Arc::new(AtomicBool::new(true)),
            options: self.options,
            connected: Vec::new(),
        }
    }
}

/// HTTP server.
///
/// This is created via `ServerBuilder`.
#[must_use = "future do nothing unless polled"]
#[derive(Debug)]
pub struct Server {
    logger: Logger,
    metrics: ServerMetrics,
    spawner: BoxSpawn,
    listener: Listener,
    dispatcher: Dispatcher,
    is_server_alive: Arc<AtomicBool>,
    options: ServerOptions,
    connected: Vec<(SocketAddr, Compat<TcpStream>)>,
}
impl Server {
    /// Returns a future that retrieves the address to which the server is bound.
    pub fn local_addr(self) -> impl Future<Item = (Self, SocketAddr), Error = Error> {
        match self.listener {
            Listener::Listening { .. } => match self.listener.local_addr() {
                Ok(local_addr) => Either::A(ok((self, local_addr))),
                Err(e) => Either::A(futures::future::err(Error::from(e))),
            },
            Listener::Binding(_, _) => {
                let future = loop_fn(self, |mut this| {
                    if fibers::fiber::with_current_context(|_| ()).is_none() {
                        return Ok(Loop::Continue(this));
                    }

                    track!(this.listener.poll())?;
                    if let Listener::Listening { .. } = this.listener {
                        let local_addr = this.listener.local_addr()?;
                        Ok(Loop::Break((this, local_addr)))
                    } else {
                        Ok(Loop::Continue(this))
                    }
                });
                Either::B(future)
            }
        }
    }

    /// Returns the metrics of the server.
    pub fn metrics(&self) -> &ServerMetrics {
        &self.metrics
    }
}
impl Future for Server {
    type Item = ();
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            match track!(self.listener.poll())? {
                Async::NotReady => {
                    break;
                }
                Async::Ready(None) => {
                    warn!(self.logger, "The socket of the HTTP server has been closed");
                    return Ok(Async::Ready(()));
                }
                Async::Ready(Some((stream, client_addr))) => {
                    // 元の実装では Connected (TcpStream へと解決される Future) を格納したが、
                    // tokio を使う場合は所望の TcpStream がそのまま格納されるため、Server 内部の poll でまた解決する必要がなくなる。
                    // それを受け、ここで Connection の spawn 処理をやる。
                    let logger = self.logger.new(o!("client" => client_addr.to_string()));
                    debug!(logger, "New client arrived");
                    let future = track!(Connection::new(
                        logger,
                        self.metrics.clone(),
                        stream,
                        self.dispatcher.clone(),
                        Arc::clone(&self.is_server_alive),
                        &self.options,
                    ))?;
                    self.spawner.spawn(future);
                }
            }
        }

        Ok(Async::NotReady)
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.is_server_alive.store(false, Ordering::SeqCst);
    }
}

enum Listener {
    Binding(
        Box<dyn Future<Item = TcpListener, Error = Error> + Send + 'static>,
        SocketAddr,
    ),
    Listening(ListenerCompat),
}
impl Listener {
    fn local_addr(&self) -> IOResult<SocketAddr> {
        match &self {
            Listener::Binding(_, local_addr) => Ok(*local_addr),
            Listener::Listening(ref listener) => listener.local_addr(),
        }
    }
}
impl Stream for Listener {
    type Item = (TcpStream, SocketAddr);
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        loop {
            let next = match self {
                Listener::Binding(f, _) => {
                    if let Async::Ready(listener) = track!(f.poll().map_err(Error::from))? {
                        Listener::Listening(ListenerCompat {
                            inner: TcpListenerWrapper(listener).compat(),
                        })
                    } else {
                        break;
                    }
                }
                Listener::Listening(s) => return track!(s.poll()),
            };
            *self = next;
        }
        Ok(Async::NotReady)
    }
}

impl std::fmt::Debug for Listener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        todo!()
    }
}

#[derive(Debug)]
struct TcpListenerWrapper(TcpListener);

impl TcpListenerWrapper {
    fn local_addr(&self) -> IOResult<SocketAddr> {
        self.0.local_addr()
    }
}

impl Stream03 for TcpListenerWrapper {
    type Item = IOResult<TcpStream>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll03<Option<Self::Item>> {
        match self.0.poll_accept(cx) {
            Poll03::Ready(Ok((socket, _))) => Poll03::Ready(Some(Ok(socket))),
            Poll03::Ready(Err(e)) => Poll03::Ready(Some(Err(e))),
            Poll03::Pending => Poll03::Pending,
        }
    }
}

#[derive(Debug)]
struct ListenerCompat {
    inner: Compat<TcpListenerWrapper>,
}

impl Stream for ListenerCompat {
    type Item = (TcpStream, SocketAddr);

    type Error = Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        let local_addr = self.local_addr()?;
        match self.inner.poll()? {
            Async::Ready(Some(stream)) => Ok(Async::Ready(Some((stream, local_addr)))),
            Async::Ready(None) => Ok(Async::Ready(None)),
            Async::NotReady => Ok(Async::NotReady),
        }
    }
}

impl ListenerCompat {
    fn local_addr(&self) -> IOResult<SocketAddr> {
        self.inner.get_ref().local_addr()
    }
}

#[derive(Debug)]
pub struct ServerOptions {
    pub read_buffer_size: usize,
    pub write_buffer_size: usize,
    pub decode_options: DecodeOptions,
}
