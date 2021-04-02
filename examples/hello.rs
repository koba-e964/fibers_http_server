use bytecodec::bytes::Utf8Encoder;
use bytecodec::null::NullDecoder;
use fibers_http_server::metrics::{MetricsHandler, WithMetrics};
use fibers_http_server::{HandleRequest, Reply, Req, Res, ServerBuilder, Status};
use httpcodec::{BodyDecoder, BodyEncoder};
use sloggers::terminal::TerminalLoggerBuilder;
use sloggers::types::Severity;
use sloggers::Build;
use trackable::{track_any_err, track_try_unwrap};

#[tokio::main]
async fn main() {
    let logger = track_try_unwrap!(TerminalLoggerBuilder::new().level(Severity::Debug).build());

    let addr = "0.0.0.0:3100".parse().unwrap();
    let mut builder = ServerBuilder::new(addr);
    builder.logger(logger);
    track_try_unwrap!(builder.add_handler(WithMetrics::new(Hello)));
    track_try_unwrap!(builder.add_handler(MetricsHandler));
    let server = builder.finish();

    track_try_unwrap!(track_any_err!(server.await));
}

struct Hello;
impl HandleRequest for Hello {
    const METHOD: &'static str = "GET";
    const PATH: &'static str = "/hello";

    type ReqBody = ();
    type ResBody = String;
    type Decoder = BodyDecoder<NullDecoder>;
    type Encoder = BodyEncoder<Utf8Encoder>;
    type Reply = Reply<Self::ResBody>;

    fn handle_request(&self, _req: Req<Self::ReqBody>) -> Self::Reply {
        Box::new(futures03::future::ready(Res::new(
            Status::Ok,
            "hello".to_owned(),
        )))
    }
}
