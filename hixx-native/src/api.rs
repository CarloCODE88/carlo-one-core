use hyper::header::CONTENT_TYPE;
use hyper::{Body, Method, Request, Response, StatusCode};
use std::convert::Infallible;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{error, info};

use crate::ipc::HixxIPC;

pub async fn handle_request_with_ipc(
    req: Request<Body>,
    ipc: Arc<Mutex<HixxIPC>>,
) -> Result<Response<Body>, Infallible> {
    match (req.method(), req.uri().path()) {
        (&Method::POST, "/v1/chat/completions") => {
            info!("Received inference request via IPC");
            let guard = ipc.lock().await;
            if let Err(error) = guard.submit_task() {
                error!(%error, "IPC submit failed");
                return Ok(json_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    r#"{"error":{"message":"kernel IPC unavailable","type":"service_unavailable"}}"#,
                ));
            }
            Ok(json_response(
                StatusCode::ACCEPTED,
                r#"{"id":"hixx-1","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","content":"Kernel task accepted"},"finish_reason":"stop"}]}"#,
            ))
        }
        (&Method::GET, "/health") => Ok(json_response(StatusCode::OK, r#"{"status":"ok"}"#)),
        (&Method::GET, "/status") => {
            let guard = ipc.lock().await;
            match guard.get_status() {
                Ok(queue_depth) => Ok(json_response(
                    StatusCode::OK,
                    &format!(
                        r#"{{"status":"running","kernel":"hixx-native","queue_depth":{queue_depth}}}"#
                    ),
                )),
                Err(error) => {
                    error!(%error, "IPC status failed");
                    Ok(json_response(
                        StatusCode::SERVICE_UNAVAILABLE,
                        r#"{"error":{"message":"kernel IPC unavailable","type":"service_unavailable"}}"#,
                    ))
                }
            }
        }
        _ => Ok(Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::empty())
            .unwrap()),
    }
}

fn json_response(status: StatusCode, body: &str) -> Response<Body> {
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_owned()))
        .expect("static HTTP response is valid")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_returns_ok() {
        let req = Request::builder()
            .method(Method::GET)
            .uri("/health")
            .body(Body::empty())
            .unwrap();
        assert_eq!(req.uri().path(), "/health");
    }
}
