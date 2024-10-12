use std::convert::Infallible;
use std::net::SocketAddr;

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{HeaderMap, Request, Response};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

const PROXY_HOST_HEADER: &str = "Proxy-Host";

async fn handle_proxy_request(
    mut request: Request<hyper::body::Incoming>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    // 1. get 'Proxy-Host' header from request
    let headers = request.headers_mut();

    let Some(proxy_target) = headers.remove(PROXY_HOST_HEADER) else {
        return Ok(Response::builder()
            .status(400)
            .body(Full::new(Bytes::from("Proxy-Host header is missing")))
            .unwrap());
    };

    let Ok(proxy_target) = proxy_target.to_str() else {
        return Ok(Response::builder()
            .status(400)
            .body(Full::new(Bytes::from(
                "Proxy-Host header is not a valid string",
            )))
            .unwrap());
    };

    // 2. prepare request

    // 2.1. get request method
    let method = request.method().to_owned();

    // 2.2. get request headers
    let mut request_headers = HeaderMap::new();
    std::mem::swap(&mut request_headers, request.headers_mut());

    // 2.3 generate request URI for proxy
    let request_uri = {
        let uri = request.uri();
        let path = uri.path();
        let raw_query = uri.query();
        let mut request_uri =
            String::with_capacity(proxy_target.len() + path.len() + raw_query.unwrap_or("").len());

        request_uri.push_str(proxy_target);
        request_uri.push_str(path);

        if let Some(raw_query) = raw_query {
            request_uri.push('?');
            request_uri.push_str(raw_query);
        }

        request_uri
    };

    // 2.4. get request body
    let Ok(request_body) = request.into_body().collect().await.map(|body| {
        let bytes = body.to_bytes().to_vec();
        unsafe { String::from_utf8_unchecked(bytes) }
    }) else {
        return Ok(Response::builder()
            .status(400)
            .body(Full::new(Bytes::from("Failed to read request body")))
            .unwrap());
    };

    // 3. send request to proxy
    let Ok(client) = reqwest::ClientBuilder::new().build() else {
        return Ok(Response::builder()
            .status(400)
            .body(Full::new(Bytes::from("Failed to create a reqwest client")))
            .unwrap());
    };

    let proxy_request = client
        .request(method, request_uri)
        .body(request_body)
        .headers(request_headers);

    let proxy_result = proxy_request.send().await;

    // 4. return response from proxy to client
    match proxy_result {
        Ok(response) => {
            let mut response_builder = Response::builder().status(response.status());

            let headers = response_builder.headers_mut().unwrap();

            for (key, value) in response.headers() {
                headers.insert(key, value.clone());
            }

            let body = response.bytes().await.unwrap();

            Ok(response_builder.body(Full::new(body)).unwrap())
        }
        Err(err) => Ok(Response::builder()
            .status(500)
            .body(Full::new(Bytes::from(format!(
                "Failed to send request: {}",
                err
            ))))
            .unwrap()),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let addr = SocketAddr::from(([0, 0, 0, 0], 3000));

    // We create a TcpListener and bind it to the address we want to listen on
    let listener = TcpListener::bind(addr).await?;

    println!("Listening on http://{}", addr);

    // We start a loop to continuously accept incoming connections
    loop {
        let (stream, _) = listener.accept().await?;

        // Use an adapter to access something implementing `tokio::io` traits as if they implement
        // `hyper::rt` IO traits.
        let io = TokioIo::new(stream);

        // Spawn a tokio task to serve multiple connections concurrently
        tokio::task::spawn(async move {
            // Finally, we bind the incoming connection to our `hello` service
            if let Err(err) = http1::Builder::new()
                // `service_fn` converts our function in a `Service`
                .serve_connection(io, service_fn(handle_proxy_request))
                .await
            {
                eprintln!("Error serving connection: {:?}", err);
            }
        });
    }
}
