use crate::connection::Connection;
use crate::dispatcher::{Dispatcher, DispatcherBuilder};
use crate::metrics::ServerMetrics;
use crate::{Error, HandleRequest, HandlerOptions, Result};
use factory::Factory;
use fibers::{self, BoxSpawn, Spawn};
use futures03::TryFutureExt;
use futures03::{compat::Compat, ready};
use futures03::{Future, Stream};
use httpcodec::DecodeOptions;
use pin_project::{pin_project, pinned_drop};
use prometrics::metrics::MetricBuilder;
use slog::{Discard, Logger};
use std::io::Result as IOResult;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
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
        Server {
            logger,
            metrics: ServerMetrics::new(self.metrics),
            spawner: spawner.boxed(),
            listener: Listener::Binding(Box::new(fut03.map_err(Error::from)), self.bind_addr),
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
#[pin_project(PinnedDrop)]
#[must_use = "future do nothing unless polled"]
#[derive(Debug)]
pub struct Server {
    logger: Logger,
    metrics: ServerMetrics,
    spawner: BoxSpawn,
    #[pin]
    listener: Listener,
    dispatcher: Dispatcher,
    is_server_alive: Arc<AtomicBool>,
    options: ServerOptions,
    connected: Vec<(SocketAddr, Compat<TcpStream>)>,
}
impl Server {
    /// Returns a future that retrieves the address to which the server is bound.
    #[deprecated(note = "Please use local_addr_immediate instead")]
    pub fn local_addr(self) -> impl Future<Output = Result<(Self, SocketAddr)>> {
        let result = match self.local_addr_immediate() {
            Ok(local_addr) => Ok((self, local_addr)),
            Err(e) => Err(e),
        };
        futures03::future::ready(result)
    }

    pub fn local_addr_immediate(&self) -> Result<SocketAddr> {
        match self.listener {
            Listener::Listening { .. } => self.listener.local_addr().map_err(Error::from),
            Listener::Binding(_, local_addr) => Ok(local_addr),
        }
    }

    /// Returns the metrics of the server.
    pub fn metrics(&self) -> &ServerMetrics {
        &self.metrics
    }
}
impl Future for Server {
    type Output = Result<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        loop {
            let this = self.project();
            match track!(this.listener.poll_next(cx)) {
                Poll::Pending => {
                    break;
                }
                Poll::Ready(None) => {
                    warn!(this.logger, "The socket of the HTTP server has been closed");
                    return Poll::Ready(Ok(()));
                }
                Poll::Ready(Some(result)) => {
                    let (stream, client_addr) = result?;
                    // 元の実装では Connected (TcpStream へと解決される Future) を格納したが、
                    // tokio を使う場合は所望の TcpStream がそのまま格納されるため、Server 内部の poll でまた解決する必要がなくなる。
                    // それを受け、ここで Connection の spawn 処理をやる。
                    let logger = this.logger.new(o!("client" => client_addr.to_string()));
                    debug!(logger, "New client arrived");
                    let future = track!(Connection::new(
                        logger,
                        this.metrics.clone(),
                        stream,
                        this.dispatcher.clone(),
                        Arc::clone(&this.is_server_alive),
                        &this.options,
                    ))?;
                    this.spawner.spawn(future.compat());
                }
            }
        }

        Poll::Pending
    }
}
#[pinned_drop]
impl PinnedDrop for Server {
    fn drop(self: Pin<&mut Self>) {
        self.is_server_alive.store(false, Ordering::SeqCst);
    }
}

#[pin_project(project = ListenerProj)]
enum Listener {
    Binding(
        #[pin] Box<dyn Future<Output = Result<TcpListener>> + Send + Unpin + 'static>,
        SocketAddr,
    ),
    Listening(#[pin] TcpListener),
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
    type Item = Result<(TcpStream, SocketAddr)>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        loop {
            let next = match self.project() {
                ListenerProj::Binding(f, _) => {
                    if let Poll::Ready(listener) = track!(f.poll(cx).map_err(Error::from))? {
                        Listener::Listening(listener)
                    } else {
                        break;
                    }
                }
                ListenerProj::Listening(s) => {
                    return Poll::Ready(track!(Some(
                        ready!(s.poll_accept(cx)).map_err(Error::from)
                    )))
                }
            };
            *self = next;
        }
        Poll::Pending
    }
}

impl std::fmt::Debug for Listener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Listener {}",
            match self {
                Listener::Binding(..) => "Binding",
                Listener::Listening(..) => "Listening",
            }
        )
    }
}

#[derive(Debug)]
struct TcpListenerWrapper(TcpListener);

impl TcpListenerWrapper {
    fn local_addr(&self) -> IOResult<SocketAddr> {
        self.0.local_addr()
    }
}

impl Stream for TcpListenerWrapper {
    type Item = IOResult<TcpStream>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.0.poll_accept(cx) {
            Poll::Ready(Ok((socket, _))) => Poll::Ready(Some(Ok(socket))),
            Poll::Ready(Err(e)) => Poll::Ready(Some(Err(e))),
            Poll::Pending => Poll::Pending,
        }
    }
}

#[derive(Debug)]
pub struct ServerOptions {
    pub read_buffer_size: usize,
    pub write_buffer_size: usize,
    pub decode_options: DecodeOptions,
}
