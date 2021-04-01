use crate::dispatcher::Dispatcher;
use crate::handler::{BoxReply, HandleInput, RequestHandlerInstance};
use crate::metrics::ServerMetrics;
use crate::response::ResEncoder;
use crate::server::ServerOptions;
use crate::{Error, Req, Result, Status};
use bytecodec::combinator::MaybeEos;
use bytecodec::io::{BufferedIo, IoDecodeExt, IoEncodeExt};
use bytecodec::{Decode, DecodeExt, Encode};
use futures03::Future as Future03;
use httpcodec::{NoBodyDecoder, RequestDecoder};
use pin_project::pin_project;
use slog::Logger;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::{io::Read, mem};
use std::{
    io::Write,
    sync::atomic::{AtomicBool, Ordering},
};
use tokio::net::TcpStream;
use url::Url;

#[derive(Debug)]
pub struct Connection {
    logger: Logger,
    metrics: ServerMetrics,
    stream: BufferedIo<TcpStream>,
    req_head_decoder: MaybeEos<RequestDecoder<NoBodyDecoder>>,
    dispatcher: Dispatcher,
    is_server_alive: Arc<AtomicBool>,
    base_url: Url,
    phase: Phase,
    do_close: bool,
}
impl Connection {
    pub fn new(
        logger: Logger,
        metrics: ServerMetrics,
        stream: TcpStream,
        dispatcher: Dispatcher,
        is_server_alive: Arc<AtomicBool>,
        options: &ServerOptions,
    ) -> Result<Self> {
        let _ = stream.set_nodelay(true);
        let base_url = format!(
            "http://{}/",
            track!(stream.local_addr().map_err(Error::from))?
        );
        let base_url = track!(Url::parse(&base_url).map_err(Error::from))?;

        metrics.connected_tcp_clients.increment();
        let req_head_decoder =
            RequestDecoder::with_options(NoBodyDecoder, options.decode_options.clone());
        Ok(Connection {
            logger,
            metrics,
            stream: BufferedIo::new(stream, options.read_buffer_size, options.write_buffer_size),
            req_head_decoder: req_head_decoder.maybe_eos(),
            dispatcher,
            is_server_alive,
            base_url,
            phase: Phase::ReadRequestHead,
            do_close: false,
        })
    }

    fn is_closed(&self) -> bool {
        self.stream.is_eos()
            || (self.stream.write_buf_ref().is_empty() && self.phase.is_closed())
            || !self.is_server_alive.load(Ordering::SeqCst)
    }

    fn read_request_head(&mut self) -> Phase {
        let result = self
            .req_head_decoder
            .decode_from_read_buf(self.stream.read_buf_mut())
            .and_then(|()| {
                if self.req_head_decoder.is_idle() {
                    self.req_head_decoder.finish_decoding().map(Some)
                } else {
                    Ok(None)
                }
            });
        match result {
            Err(e) => {
                warn!(
                    self.logger,
                    "Cannot decode the head part of a HTTP request: {}", e
                );
                self.metrics.read_request_head_errors.increment();
                self.do_close = true;
                Phase::WriteResponse(ResEncoder::error(Status::BadRequest))
            }
            Ok(None) => Phase::ReadRequestHead,
            Ok(Some(head)) => match track!(Req::new(head, &self.base_url)) {
                Err(e) => {
                    warn!(
                        self.logger,
                        "Cannot parse the path of a HTTP request: {}", e
                    );
                    self.metrics.parse_request_path_errors.increment();
                    self.do_close = true;
                    Phase::WriteResponse(ResEncoder::error(Status::BadRequest))
                }
                Ok(head) => Phase::DispatchRequest(head),
            },
        }
    }

    fn dispatch_request(&mut self, head: Req<()>) -> Phase {
        match self.dispatcher.dispatch(&head) {
            Err(status) => {
                self.metrics.dispatch_request_errors.increment();
                self.do_close = true;
                Phase::WriteResponse(ResEncoder::error(status))
            }
            Ok(mut handler) => match track!(handler.init(head)) {
                Err(e) => {
                    warn!(self.logger, "Cannot initialize a request handler: {}", e);
                    self.metrics.initialize_handler_errors.increment();
                    self.do_close = true;
                    Phase::WriteResponse(ResEncoder::error(Status::InternalServerError))
                }
                Ok(()) => Phase::HandleRequest(handler),
            },
        }
    }

    fn handle_request(&mut self, mut handler: RequestHandlerInstance) -> Phase {
        match track!(handler.handle_input(self.stream.read_buf_mut())) {
            Err(e) => {
                warn!(
                    self.logger,
                    "Cannot decode the body of a HTTP request: {}", e
                );
                self.metrics.decode_request_body_errors.increment();
                self.do_close = true;
                Phase::WriteResponse(ResEncoder::error(Status::BadRequest))
            }
            Ok(None) => Phase::HandleRequest(handler),
            Ok(Some(reply)) => {
                self.do_close = handler.is_closed();
                Phase::PollReply(reply)
            }
        }
    }

    fn poll_reply(reply: Pin<&mut BoxReply>, cx: &mut Context) -> Option<ResEncoder> {
        if let Poll::Ready(res_encoder) = reply.poll(cx) {
            Some(res_encoder)
        } else {
            None
        }
    }

    fn write_response(&mut self, mut encoder: ResEncoder) -> Result<Phase> {
        track!(encoder.encode_to_write_buf(self.stream.write_buf_mut())).map_err(|e| {
            self.metrics.write_response_errors.increment();
            e
        })?;
        if encoder.is_idle() {
            if self.do_close {
                Ok(Phase::Closed)
            } else {
                Ok(Phase::ReadRequestHead)
            }
        } else {
            Ok(Phase::WriteResponse(encoder))
        }
    }

    fn poll_once(self: Pin<&mut Self>, cx: &mut Context) -> Result<bool> {
        let self_mut = self.get_mut();
        let poll_result = Pin::new(&mut self_mut.stream).execute_io_poll(cx);
        if let Poll::Ready(result) = poll_result {
            track!(result)?;
        }

        let old = mem::discriminant(&self_mut.phase);
        let next = match self_mut.phase.take() {
            Phase::ReadRequestHead => self_mut.read_request_head(),
            Phase::DispatchRequest(req) => self_mut.dispatch_request(req),
            Phase::HandleRequest(handler) => self_mut.handle_request(handler),
            Phase::PollReply(mut reply) => {
                if let Some(res_encoder) = Self::poll_reply(Pin::new(&mut reply), cx) {
                    Phase::WriteResponse(res_encoder)
                } else {
                    Phase::PollReply(reply)
                }
            }
            Phase::WriteResponse(res) => track!(self_mut.write_response(res))?,
            Phase::Closed => Phase::Closed,
        };
        self_mut.phase = next;
        let changed = mem::discriminant(&self_mut.phase) != old;
        Ok(changed || !self_mut.stream.would_block())
    }
}
impl Future03 for Connection {
    type Output = std::result::Result<(), ()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        while !self.as_ref().is_closed() {
            match track!(self.as_mut().poll_once(cx)) {
                Err(e) => {
                    warn!(self.logger, "Connection aborted: {}", e);
                    self.metrics.disconnected_tcp_clients.increment();
                    return Poll::Ready(Err(()));
                }
                Ok(do_continue) => {
                    if !do_continue {
                        if self.is_closed() {
                            break;
                        }
                        return Poll::Pending;
                    }
                }
            }
        }

        debug!(self.logger, "Connection closed");
        self.metrics.disconnected_tcp_clients.increment();
        Poll::Ready(Ok(()))
    }
}

#[pin_project(project = PhaseProj)]
#[derive(Debug)]
enum Phase {
    ReadRequestHead,
    DispatchRequest(Req<()>),
    HandleRequest(RequestHandlerInstance),
    PollReply(#[pin] BoxReply),
    WriteResponse(ResEncoder),
    Closed,
}
impl Phase {
    fn take(&mut self) -> Self {
        mem::replace(self, Phase::Closed)
    }

    fn is_closed(&self) -> bool {
        if let Phase::Closed = *self {
            true
        } else {
            false
        }
    }
}

#[derive(Debug)]
struct ReadWriteWrapper(TcpStream);

impl Read for ReadWriteWrapper {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let s = self.0.try_read(buf);
        eprintln!("result = {:?}", s);
        s
    }
}

impl Write for ReadWriteWrapper {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let s = self.0.try_write(buf);
        eprintln!("result w = {:?}", s);
        s
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
