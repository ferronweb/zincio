use std::convert::Infallible;

use bytes::Bytes;
use http_body_util::Full;
use hyper::service::service_fn;
use hyper::{Request, Response};
use zincio::RuntimeBuilder;
use zincio_hyper::ZincioIo;

async fn hello(_: Request<impl ::hyper::body::Body>) -> Result<Response<Full<Bytes>>, Infallible> {
    Ok(Response::new(Full::new(Bytes::from("Hello World!"))))
}

fn main() -> Result<(), std::io::Error> {
    let runtime = RuntimeBuilder::new().build()?;

    // A basic single-threaded HTTP server (using `hyper`) example
    // In production, you would typically use a multi-threaded runtime and offload blocking I/O.
    runtime.block_on(async {
        let listener = zincio::net::TcpListener::bind("127.0.0.1:8080")?;

        while let Ok((stream, _)) = listener.accept().await {
            let io = ZincioIo::new(stream.into_poll()?);
            zincio::spawn(async move {
                if let Err(err) = hyper::server::conn::http1::Builder::new()
                    .serve_connection(io, service_fn(hello))
                    .await
                {
                    println!("Error serving connection: {err:?}");
                }
            });
        }

        Ok(())
    })
}
