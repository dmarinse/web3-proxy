use axum::extract::State;
use axum::headers::HeaderName;
use axum::http::HeaderValue;
use axum::response::{IntoResponse, Response};
use axum::{routing::get, Router};
use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio::sync::broadcast;
use tracing::info;

use crate::app::App;
use crate::errors::Web3ProxyResult;

/// Run a prometheus metrics server on the given port.
pub async fn serve(
    app: Arc<App>,
    mut shutdown_receiver: broadcast::Receiver<()>,
) -> Web3ProxyResult<()> {
    // routes should be ordered most to least common
    let router = Router::new().route("/", get(root)).with_state(app.clone());

    // note: the port here might be 0
    let port = app.prometheus_port.load(Ordering::SeqCst);
    // TODO: config for the host?
    let addr = SocketAddr::from(([0, 0, 0, 0], port));

    let service = router.into_make_service();

    // `axum::Server` is a re-export of `hyper::Server`
    let server = axum::Server::bind(&addr).serve(service);

    let port = server.local_addr().port();
    info!("prometheus listening on port {}", port);

    app.prometheus_port.store(port, Ordering::SeqCst);

    server
        .with_graceful_shutdown(async move {
            let _ = shutdown_receiver.recv().await;
        })
        .await
        .map_err(Into::into)
}

async fn root(State(app): State<Arc<App>>) -> Response {
    let serialized = app.prometheus_metrics().await;

    let mut r = serialized.into_response();

    // // TODO: is there an easier way to do this?
    r.headers_mut().insert(
        HeaderName::from_static("content-type"),
        HeaderValue::from_static("application/openmetrics-text; version=1.0.0; charset=utf-8"),
    );

    r
}
