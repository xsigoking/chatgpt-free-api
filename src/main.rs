#[macro_use]
extern crate log;

use anyhow::{anyhow, bail, Result};
use bytes::Bytes;
use chrono::Utc;
use futures_util::StreamExt;
use http::{HeaderMap, HeaderValue, Response, StatusCode};
use http_body_util::{combinators::BoxBody, BodyExt, Full, StreamBody};
use hyper::{
    body::{Frame, Incoming},
    service::service_fn,
};
use hyper_util::rt::{TokioExecutor, TokioIo};
use rand::{seq::SliceRandom, thread_rng};
use reqwest::{Client, ClientBuilder, Method, Proxy};
use reqwest_eventsource::{Error as EventSourceError, Event, RequestBuilderExt};
use serde_json::{json, Value};
use std::{convert::Infallible, env, sync::Arc, time::Duration};
use tokio::{net::TcpListener, sync::oneshot};
use tokio_graceful::Shutdown;
use tokio_stream::wrappers::ReceiverStream;
use uuid::Uuid;

const PORT: u16 = 3040;
const BASE_URL: &str = "https://chat.openai.com";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

#[tokio::main]
async fn main() -> Result<()> {
    init_logger();

    let port = if let Ok(port) = env::var("PORT") {
        port.parse::<u16>()
            .map_err(|_| anyhow!("Invalid environment variable $PORT"))?
    } else {
        PORT
    };
    let mut client_builder = ClientBuilder::new().connect_timeout(CONNECT_TIMEOUT);
    if let Ok(proxy) = env::var("ALL_PROXY") {
        client_builder = client_builder.proxy(
            Proxy::all(proxy)
                .map_err(|err| anyhow!("Invalid environment variable $ALL_PROXY, {err}"))?,
        );
    };
    let listener = tokio::net::TcpListener::bind(&format!("0.0.0.0:{port}")).await?;

    let server = Arc::new(Server {
        client: client_builder.build()?,
    });
    let stop_server = server.run(listener).await?;
    println!(
        r#"Access the API server at: http://0.0.0.0:{port}/v1/chat/completions

Environment Variables:
  - PORT: change the listening port, defaulting to {PORT}
  - ALL_PROXY: set the proxy server

Please contact us at https://github.com/xsigoking/chatgpt-free-api if you encounter any issues.
"#
    );

    shutdown_signal().await;
    let _ = stop_server.send(());
    Ok(())
}

fn init_logger() {
    env_logger::builder()
        .parse_env(env_logger::Env::new().filter_or("RUST_LOG", "info"))
        .format_target(false)
        .format_module_path(false)
        .init();
}

type AppResponse = Response<BoxBody<Bytes, Infallible>>;

struct Server {
    client: Client,
}

impl Server {
    async fn run(self: Arc<Self>, listener: TcpListener) -> Result<oneshot::Sender<()>> {
        let (tx, rx) = oneshot::channel();
        tokio::spawn(async move {
            let shutdown = Shutdown::new(async { rx.await.unwrap_or_default() });
            let guard = shutdown.guard_weak();

            loop {
                tokio::select! {
                    res = listener.accept() => {
                        let Ok((cnx, _)) = res else {
                            continue;
                        };

                        let stream = TokioIo::new(cnx);
                        let server = self.clone();
                        shutdown.spawn_task(async move {
                            let hyper_service = service_fn(move |request: hyper::Request<Incoming>| {
                                server.clone().handle(request)
                            });
                            let _ = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new())
                                .serve_connection_with_upgrades(stream, hyper_service)
                                .await;
                        });
                    }
                    _ = guard.cancelled() => {
                        break;
                    }
                }
            }
        });
        Ok(tx)
    }

    async fn handle(
        self: Arc<Self>,
        req: hyper::Request<Incoming>,
    ) -> std::result::Result<AppResponse, hyper::Error> {
        let method = req.method();
        let uri = req.uri();
        let mut res = if method == Method::POST && uri == "/v1/chat/completions" {
            match self.chat_completion(req).await {
                Ok(res) => res,
                Err(err) => create_error_response(err),
            }
        } else {
            let mut res = create_error_response("The requested endpoint was not found.");
            *res.status_mut() = StatusCode::NOT_FOUND;
            res
        };
        set_cors_header(&mut res);
        Ok(res)
    }

    async fn chat_completion(&self, req: hyper::Request<Incoming>) -> Result<AppResponse> {
        let (oai_device_id, token) = self.refresh_session().await?;

        let req_body = req.collect().await?.to_bytes();
        let req_body: Value = serde_json::from_slice(&req_body)
            .map_err(|err| anyhow!("Invalid request body, {err}"))?;

        let is_stream = req_body["stream"].as_bool().unwrap_or_default();
        let mut new_messages = vec![];
        let mut invalid_messages = false;
        if let Some(messages) = req_body["messages"].as_array() {
            for v in messages {
                let role = match v["role"].as_str() {
                    Some(v) => v,
                    None => {
                        invalid_messages = true;
                        break;
                    }
                };
                let content = {
                    let text = match (v["content"].as_str(), v["content"].as_array()) {
                        (Some(v), None) => v,
                        (None, Some(arr)) => {
                            if arr.len() == 1 {
                                arr[0]["text"].as_str().unwrap_or_default()
                            } else {
                                ""
                            }
                        }
                        _ => "",
                    };
                    if text.is_empty() {
                        invalid_messages = true;
                        break;
                    }
                    text
                };
                new_messages.push(json!({
                    "id": random_id(),
                    "author": { "role": role },
                    "content": { "content_type": "text", "parts": [content] },
                    "metadata": {},
                }));
            }
        }

        if invalid_messages {
            bail!("Invalid request messages");
        }

        let req_body = json!({
            "action": "next",
            "messages": new_messages,
            "parent_message_id": random_id(),
            "model": "text-davinci-002-render-sha",
            "timezone_offset_min": -180,
            "suggestions": [],
            "history_and_training_disabled": true,
            "conversation_mode": { "kind": "primary_assistant" },
            "force_paragen": false,
            "force_paragen_model_slug": "",
            "force_nulligen": false,
            "force_rate_limit":false,
            "websocket_request_id": random_id(),
        });

        debug!("req body: {req_body}");

        let mut es = self
            .client
            .post(format!("{BASE_URL}/backend-api/conversation"))
            .headers(common_headers())
            .header("oai-device-id", oai_device_id)
            .header("openai-sentinel-chat-requirements-token", token)
            .json(&req_body)
            .eventsource()?;

        let (tx, mut rx) = tokio::sync::mpsc::channel(1);

        tokio::spawn(async move {
            let mut check = true;
            let mut prev_text_size = 0;
            while let Some(event) = es.next().await {
                match event {
                    Ok(Event::Open) => {}
                    Ok(Event::Message(message)) => {
                        if check {
                            let _ = tx.send(ResEvent::First(None)).await;
                            check = false;
                        }
                        if message.data == "[DONE]" {
                            let _ = tx.send(ResEvent::Done).await;
                            break;
                        }
                        if let Ok(data) = serde_json::from_str::<Value>(&message.data) {
                            if let (Some("assistant"), Some(text)) = (
                                data["message"]["author"]["role"].as_str(),
                                data["message"]["content"]["parts"][0].as_str(),
                            ) {
                                let trimed_text: String =
                                    text.chars().skip(prev_text_size).collect();
                                if trimed_text.is_empty() && prev_text_size > 0 {
                                    continue;
                                }
                                let _ = tx.send(ResEvent::Text(trimed_text)).await;
                                prev_text_size = text.chars().count();
                            }
                        };
                    }
                    Err(err) => {
                        match err {
                            EventSourceError::InvalidStatusCode(_, res) => {
                                let status = res.status().as_u16();
                                let data = match res.text().await {
                                    Ok(v) => format!("Invalid response code {status}, {v}"),
                                    Err(err) => format!("Invalid response, code {status}, {err}"),
                                };
                                if check {
                                    let _ = tx.send(ResEvent::First(Some(data))).await;
                                    check = false;
                                }
                            }
                            EventSourceError::StreamEnded => {}
                            _ => {
                                if check {
                                    let _ = tx.send(ResEvent::First(Some(err.to_string()))).await;
                                    check = false;
                                }
                            }
                        }
                        es.close();
                    }
                }
            }
        });

        let completion_id = generate_completion_id();
        let created = Utc::now().timestamp();

        let first_event = rx.recv().await;

        if let Some(ResEvent::First(Some(err))) = first_event {
            bail!("{err}");
        }

        if is_stream {
            let shared = Arc::new((completion_id, created));
            let stream = ReceiverStream::new(rx);
            let stream = stream.filter_map(move |v| {
                let shared = shared.clone();
                async move {
                    match v {
                        ResEvent::Text(text) => {
                            Some(Ok(create_frame(&shared.0, shared.1, &text, false)))
                        }
                        ResEvent::Done => Some(Ok(create_frame(&shared.0, shared.1, "", true))),
                        _ => None,
                    }
                }
            });
            let res = Response::builder()
                .status(StatusCode::OK)
                .header("Content-Type", "text/event-stream")
                .header("Cache-Control", "no-cache")
                .header("Connection", "keep-alive")
                .body(BodyExt::boxed(StreamBody::new(stream)))?;
            Ok(res)
        } else {
            let mut content_parts = vec![];
            while let Some(event) = rx.recv().await {
                match event {
                    ResEvent::Text(text) => {
                        content_parts.push(text);
                    }
                    ResEvent::Done => {
                        break;
                    }
                    _ => {}
                }
            }
            let content = content_parts.join("");

            let res = Response::builder()
                .header("Content-Type", "application/json")
                .body(Full::new(create_bytes_body(&completion_id, created, &content)).boxed())?;
            Ok(res)
        }
    }

    async fn refresh_session(&self) -> Result<(String, String)> {
        let oai_device_id = random_id();
        let res = self
            .client
            .post(format!(
                "{BASE_URL}/backend-anon/sentinel/chat-requirements"
            ))
            .headers(common_headers())
            .header("oai-device-id", oai_device_id.clone())
            .body("{}")
            .send()
            .await?;
        let data: Value = res.json().await?;
        let token = match data["token"].as_str() {
            Some(v) => v.to_string(),
            None => bail!("Failed to refresh sesseion, {data}"),
        };
        Ok((oai_device_id, token))
    }
}

#[derive(Debug)]
enum ResEvent {
    First(Option<String>),
    Text(String),
    Done,
}

async fn shutdown_signal() {
    tokio::signal::ctrl_c()
        .await
        .expect("Failed to install CTRL+C signal handler")
}

fn generate_completion_id() -> String {
    let mut rng = thread_rng();

    let id_charset: Vec<char> = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789"
        .chars()
        .collect();

    let random_id: String = (0..16)
        .map(|_| *id_charset.choose(&mut rng).unwrap())
        .collect();

    format!("chatcmpl-{}", random_id)
}

fn common_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();

    headers.insert("accept", HeaderValue::from_static("*/*"));
    headers.insert(
        "accept-language",
        HeaderValue::from_static("en-US,en;q=0.9"),
    );
    headers.insert("cache-control", HeaderValue::from_static("no-cache"));
    headers.insert("content-type", HeaderValue::from_static("application/json"));
    headers.insert("oai-language", HeaderValue::from_static("en-US"));
    headers.insert("origin", HeaderValue::from_static("baseUrl"));
    headers.insert("pragma", HeaderValue::from_static("no-cache"));
    headers.insert("referer", HeaderValue::from_static("baseUrl"));
    headers.insert(
        "sec-ch-ua",
        HeaderValue::from_static(
            r#""Google Chrome"; v="123", "Not:A-Brand"; v="8", "Chromium"; v="123""#,
        ),
    );
    headers.insert("sec-ch-ua-mobile", HeaderValue::from_static("?0"));
    headers.insert(
        "sec-ch-ua-platform",
        HeaderValue::from_static(r#""Windows""#),
    );
    headers.insert("sec-fetch-dest", HeaderValue::from_static("empty"));
    headers.insert("sec-fetch-mode", HeaderValue::from_static("cors"));
    headers.insert("sec-fetch-site", HeaderValue::from_static("same-origin"));
    headers.insert("user-agent", HeaderValue::from_static("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/123.0.0.0 Safari/537.36"));

    headers
}

fn set_cors_header(res: &mut AppResponse) {
    res.headers_mut().insert(
        hyper::header::ACCESS_CONTROL_ALLOW_ORIGIN,
        hyper::header::HeaderValue::from_static("*"),
    );
    res.headers_mut().insert(
        hyper::header::ACCESS_CONTROL_ALLOW_METHODS,
        hyper::header::HeaderValue::from_static("GET,POST,PUT,PATCH,DELETE"),
    );
    res.headers_mut().insert(
        hyper::header::ACCESS_CONTROL_ALLOW_HEADERS,
        hyper::header::HeaderValue::from_static("Content-Type,Authorization"),
    );
}

fn create_frame(id: &str, created: i64, content: &str, done: bool) -> Frame<Bytes> {
    let (delta, finish_reason) = if done {
        (json!({}), "stop".into())
    } else {
        let delta = if content.is_empty() {
            json!({ "role": "assistant", "content": content })
        } else {
            json!({ "content": content })
        };
        (delta, Value::Null)
    };
    let mut value = json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": "gpt-3.5-turbo",
        "choices": [
            {
                "index": 0,
                "delta": delta,
                "finish_reason": finish_reason,
            },
        ],
    });
    let output = if done {
        value["usage"] = json!({
            "prompt_tokens": 0,
            "completion_tokens": 0,
            "total_tokens": 0,
        });
        format!("data: {value}\n\ndata: [DONE]\n\n")
    } else {
        format!("data: {value}\n\n")
    };
    Frame::data(Bytes::from(output))
}

fn create_bytes_body(id: &str, created: i64, content: &str) -> Bytes {
    let res_body = json!({
        "id": id,
        "object": "chat.completion",
        "created": created,
        "model": "gpt-3.5-turbo",
        "choices": [
            {
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": content,
                },
                "finish_reason": "stop",
            },
        ],
        "usage": {
            "prompt_tokens": 0,
            "completion_tokens": 0,
            "total_tokens": 0,
        },
    });
    Bytes::from(res_body.to_string())
}

fn create_error_response<T: std::fmt::Display>(err: T) -> AppResponse {
    error!("api error: {err}");
    let data = json!({
        "status": false,
        "error": {
            "message": err.to_string(),
            "type": "invalid_request_error",
        },
    });
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/json")
        .body(Full::new(Bytes::from(data.to_string())).boxed())
        .unwrap()
}

fn random_id() -> String {
    Uuid::new_v4().to_string()
}
