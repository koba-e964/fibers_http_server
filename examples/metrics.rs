use fibers_http_server::metrics::MetricsHandler;
use fibers_http_server::ServerBuilder;
use trackable::result::MainResult;
use trackable::track;

#[tokio::main]
async fn main() -> MainResult {
    let addr = "0.0.0.0:9090".parse().unwrap();
    let mut builder = ServerBuilder::new(addr);
    track!(builder.add_handler(MetricsHandler))?;

    let server = builder.finish();
    track!(server.await)?;
    Ok(())
}
