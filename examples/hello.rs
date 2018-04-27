extern crate bytecodec;
extern crate fibers;
extern crate fibers_http_server;
extern crate futures;
extern crate httpcodec;
extern crate sloggers;
#[macro_use]
extern crate trackable;

use bytecodec::bytes::Utf8Encoder;
use bytecodec::value::NullDecoder;
use fibers::{Executor, Spawn, ThreadPoolExecutor};
use fibers_http_server::{HandleRequest, Reply, Req, Res, ServerBuilder, Status};
use httpcodec::{BodyDecoder, BodyEncoder};
use sloggers::Build;
use sloggers::terminal::TerminalLoggerBuilder;

fn main() {
    let logger = track_try_unwrap!(TerminalLoggerBuilder::new().build());
    let mut executor = track_try_unwrap!(track_any_err!(ThreadPoolExecutor::new()));

    let mut builder = ServerBuilder::new("0.0.0.0:3000".parse().unwrap());
    builder.logger(logger);
    track_try_unwrap!(builder.add_handler(Hello));
    let server = builder.finish(executor.handle());

    let fiber = executor.spawn_monitor(server);
    let result = track_try_unwrap!(track_any_err!(executor.run_fiber(fiber)));
    track_try_unwrap!(track_any_err!(result));
}

struct Hello;
impl HandleRequest for Hello {
    const METHOD: &'static str = "GET";
    const PATH: &'static str = "/hello";

    type ReqBody = ();
    type ResBody = String;
    type Decoder = BodyDecoder<NullDecoder>;
    type Encoder = BodyEncoder<Utf8Encoder>;

    fn handle_request(&self, _req: Req<Self::ReqBody>) -> Reply<Self> {
        Reply::done(Res::new(Status::Ok, "hello".to_owned()))
    }
}