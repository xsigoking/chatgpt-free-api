#[macro_use]
extern crate log;

use anyhow::{anyhow, bail, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
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
use rand::{seq::SliceRandom, thread_rng, Rng};
use reqwest::{Client, ClientBuilder, Method, Proxy};
use reqwest_eventsource::{Error as EventSourceError, Event, RequestBuilderExt};
use serde_json::{json, Value};
use sha3::{Digest, Sha3_512};
use std::{convert::Infallible, env, sync::Arc, time::Duration};
use tokio::{
    net::TcpListener,
    sync::{
        mpsc::{self, Sender},
        oneshot,
    },
};
use tokio_graceful::Shutdown;
use tokio_stream::wrappers::ReceiverStream;
use uuid::Uuid;

const PORT: u16 = 3040;
const CONVERSATION_URL: &str = "https://chat.openai.com/backend-anon/conversation";
const CHAT_REQUIREMENTS_URL: &str =
    "https://chat.openai.com/backend-anon/sentinel/chat-requirements";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/123.0.0.0 Safari/537.36";

lazy_static::lazy_static! {
    static ref PROOF_V1: u32 = {
        let mut rng = rand::thread_rng();
        rng.gen_range(2000..8000)
    };
}

#[tokio::main]
async fn main() -> Result<()> {
    init_logger();

    let mut has_envs = [false; 3];

    let port = if let Ok(port) = env::var("PORT") {
        has_envs[0] = true;
        port.parse::<u16>()
            .map_err(|_| anyhow!("Invalid environment variable $PORT"))?
    } else {
        PORT
    };
    let mut client_builder = ClientBuilder::new().connect_timeout(CONNECT_TIMEOUT);
    if let Ok(proxy) = env::var("ALL_PROXY") {
        has_envs[1] = true;
        client_builder = client_builder.proxy(
            Proxy::all(proxy)
                .map_err(|err| anyhow!("Invalid environment variable $ALL_PROXY, {err}"))?,
        );
    };
    let listener = tokio::net::TcpListener::bind(&format!("0.0.0.0:{port}")).await?;

    let authorization = env::var("AUTHORIZATION").ok().and_then(|v| {
        if v.is_empty() {
            None
        } else {
            has_envs[2] = true;
            Some(v)
        }
    });
    let server = Arc::new(Server {
        client: client_builder.build()?,
        authorization,
    });
    let [port_has_env, all_proxy_has_env, authorization_has_env] =
        has_envs.map(|v| if v { " ✅" } else { "" });
    let stop_server = server.run(listener).await?;
    println!(
        r#"Access the API server at: http://0.0.0.0:{port}/v1/chat/completions

Environment Variables:
  - PORT: change the listening port, defaulting to {PORT}{port_has_env}
  - ALL_PROXY: configure the proxy server, supporting HTTP, HTTPS, and SOCKS5 protocols{all_proxy_has_env}
  - AUTHORIZATION: only for internal use to protect the API and will not be sent to OpenAI{authorization_has_env}

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
    authorization: Option<String>,
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
        let method = req.method().clone();
        let uri = req.uri().clone();
        let mut auth_failed = false;
        if let Some(expect_authorization) = &self.authorization {
            if let Some(authorization) = req.headers().get("authorization") {
                if authorization.as_bytes() != expect_authorization.as_bytes() {
                    auth_failed = true;
                }
            } else {
                auth_failed = true;
            }
        }
        let mut status = StatusCode::OK;
        let res = if auth_failed {
            status = StatusCode::UNAUTHORIZED;
            Err(anyhow!(
                "No authorization header or invalid authorization value."
            ))
        } else if method == Method::POST && uri == "/v1/chat/completions" {
            self.chat_completion(req).await
        } else if method == Method::GET && uri == "/v1/models" {
            self.models(req).await
        } else if method == Method::OPTIONS
            && (uri == "/v1/chat/completions" || uri == "/v1/models")
        {
            status = StatusCode::NO_CONTENT;
            Ok(Response::default())
        } else {
            status = StatusCode::NOT_FOUND;
            Err(anyhow!("The requested endpoint was not found."))
        };
        let mut res = match res {
            Ok(res) => {
                info!("{method} {uri} {}", status.as_u16());
                res
            }
            Err(err) => {
                error!("{method} {uri} {} {err}", status.as_u16());
                create_error_response(err)
            }
        };
        *res.status_mut() = status;
        set_cors_header(&mut res);
        Ok(res)
    }

    async fn chat_completion(&self, req: hyper::Request<Incoming>) -> Result<AppResponse> {
        let requirements = self
            .chat_requirements()
            .await
            .map_err(|err| anyhow!("Failed to meet chat requirements, {err}"))?;

        let req_body = req.collect().await?.to_bytes();
        let req_body: Value = serde_json::from_slice(&req_body)
            .map_err(|err| anyhow!("Invalid request body, {err}"))?;

        let is_stream = req_body["stream"].as_bool().unwrap_or_default();
        let mut invalid = false;
        let mut new_messages = vec![];
        let mut system_prompt = None;
        if let Some(messages) = req_body["messages"].as_array() {
            let has_history = messages.len() > 2;
            for v in messages {
                let role = match v["role"].as_str() {
                    Some(v) => v,
                    None => {
                        invalid = true;
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
                        invalid = true;
                        break;
                    }
                    text
                };
                if role == "system" {
                    if system_prompt.is_some() {
                        invalid = true;
                        break;
                    }
                    system_prompt = Some(content.to_string());
                } else if role == "user" && has_history {
                    new_messages.push(format!("[INST]{content}[/INST]"));
                } else {
                    new_messages.push(content.to_string());
                }
            }
        }

        if invalid {
            bail!("Invalid request messages");
        }

        let mut messages = vec![];
        if let Some(system_prompt) = system_prompt {
            messages.push(json!({
                "id": random_id(),
                "author": { "role": "system" },
                "content": { "content_type": "text", "parts": [system_prompt] },
                "metadata": {},
            }))
        }

        let combine_message = new_messages.join("\n");
        messages.push(json!({
            "id": random_id(),
            "author": { "role": "user" },
            "content": { "content_type": "text", "parts": [combine_message] },
            "metadata": {},
        }));

        let req_body = json!({
            "action": "next",
            "messages": messages,
            "parent_message_id": random_id(),
            "model": "text-davinci-002-render-sha",
            "timezone_offset_min": 0,
            "suggestions": [],
            "history_and_training_disabled": true,
            "conversation_mode": { "kind": "primary_assistant" },
            "force_paragen": false,
            "force_paragen_model_slug": "",
            "force_nulligen": false,
            "force_rate_limit":false,
            "websocket_request_id": random_id(),
        });

        let proof_token = calculate_proof_token(&requirements.seed, &requirements.difficulty);
        debug!("headers: oai_device_id {}; openai-sentinel-chat-requirements-token {}; openai-sentinel-proof-token {proof_token}", requirements.oai_device_id, requirements.token);
        debug!("req body: {req_body}");

        let mut es = self
            .client
            .post(CONVERSATION_URL)
            .headers(common_headers())
            .header("oai-device-id", requirements.oai_device_id)
            .header(
                "openai-sentinel-chat-requirements-token",
                requirements.token,
            )
            .header("openai-sentinel-proof-token", proof_token)
            .json(&req_body)
            .eventsource()?;

        let (tx, mut rx) = mpsc::channel(1);

        tokio::spawn(async move {
            let mut check = true;
            let mut prev_text_size = 0;
            while let Some(event) = es.next().await {
                match event {
                    Ok(Event::Open) => {}
                    Ok(Event::Message(message)) => {
                        send_first_event(tx.clone(), None, &mut check).await;
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
                            EventSourceError::StreamEnded => {}
                            EventSourceError::InvalidStatusCode(_, res) => {
                                let status = res.status().as_u16();
                                let data = match res.text().await {
                                    Ok(v) => format!("Invalid response code {status}, {v}"),
                                    Err(err) => format!("Invalid response, code {status}, {err}"),
                                };
                                send_first_event(tx.clone(), Some(data), &mut check).await;
                            }
                            EventSourceError::InvalidContentType(_, res) => {
                                let text = res.text().await.unwrap_or_default();
                                let err = format!("The chatgpt api should return data as 'text/event-stream', but it isn't. {text}");
                                send_first_event(tx.clone(), Some(err), &mut check).await;
                            }
                            _ => {
                                send_first_event(tx.clone(), Some(err.to_string()), &mut check)
                                    .await;
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

    async fn models(&self, _req: hyper::Request<Incoming>) -> Result<AppResponse> {
        let body = json!({
            "object": "list",
            "data": [
                {
                    "id": "gpt-3.5-turbo",
                    "object": "model",
                    "created": 1626777600,
                    "owned_by": "openai",
                    "permission": [
                        {
                            "id": "modelperm-001",
                            "object": "model_permission",
                            "created": 1626777600,
                            "allow_create_engine": true,
                            "allow_sampling": true,
                            "allow_logprobs": true,
                            "allow_search_indices": false,
                            "allow_view": true,
                            "allow_fine_tuning": false,
                            "organization": "*",
                            "group": null,
                            "is_blocking": false
                        }
                    ],
                    "root": "gpt-3.5-turbo",
                    "parent": null
                }
            ]
        });
        let res = Response::builder()
            .header("Content-Type", "application/json")
            .body(Full::new(Bytes::from(body.to_string())).boxed())?;
        Ok(res)
    }

    async fn chat_requirements(&self) -> Result<Requirements> {
        let oai_device_id = random_id();
        let res = self
            .client
            .post(CHAT_REQUIREMENTS_URL)
            .headers(common_headers())
            .header("oai-device-id", oai_device_id.clone())
            .body("{}")
            .send()
            .await?;
        let data: Value = res.json().await?;
        if let (Some(token), Some((seed, difficulty))) = (
            data["token"].as_str(),
            data["proofofwork"].as_object().and_then(|v| {
                if let (Some(seed), Some(difficulty)) =
                    (v["seed"].as_str(), v["difficulty"].as_str())
                {
                    Some((seed, difficulty))
                } else {
                    None
                }
            }),
        ) {
            Ok(Requirements {
                oai_device_id,
                token: token.to_string(),
                seed: seed.to_string(),
                difficulty: difficulty.to_string(),
            })
        } else {
            bail!("Invalid data, {data}");
        }
    }
}

#[derive(Debug)]
enum ResEvent {
    First(Option<String>),
    Text(String),
    Done,
}

#[derive(Debug)]
struct Requirements {
    oai_device_id: String,
    token: String,
    seed: String,
    difficulty: String,
}

async fn send_first_event(tx: Sender<ResEvent>, data: Option<String>, check: &mut bool) {
    if *check {
        let _ = tx.send(ResEvent::First(data)).await;
        *check = false;
    }
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
    headers.insert("accept-language", HeaderValue::from_static("en"));
    headers.insert("cache-control", HeaderValue::from_static("no-cache"));
    headers.insert("content-type", HeaderValue::from_static("application/json"));
    headers.insert("oai-language", HeaderValue::from_static("en-US"));
    headers.insert(
        "origin",
        HeaderValue::from_static("https://chat.openai.com"),
    );
    headers.insert("pragma", HeaderValue::from_static("no-cache"));
    headers.insert("priority", HeaderValue::from_static("u=1, i"));
    headers.insert(
        "referer",
        HeaderValue::from_static("https://chat.openai.com/"),
    );
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
    headers.insert("user-agent", HeaderValue::from_static(USER_AGENT));

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

fn calculate_proof_token(seed: &str, diff: &str) -> String {
    let now = Utc::now();
    let datetime = now.format("%a %b %d %Y %H:%M:%S GMT%z (Coordinated Universal Time)");

    let diff_len = diff.len() / 2;
    let mut hasher = Sha3_512::new();

    for i in 0..100000 {
        let value = format!(
            r#"[{},"{datetime}",4294705152,{},"{USER_AGENT}"]"#,
            *PROOF_V1, i
        );
        let base = STANDARD.encode(value);
        hasher.update(format!("{}{}", seed, base).as_bytes());
        let hash = hasher.finalize_reset();
        let hash_hex = hex_encode(&hash[..diff_len]);

        if hash_hex.as_str() <= diff {
            return format!("gAAAAAB{}", base);
        }
    }

    format!(
        "gAAAAABwQ8Lk5FbGpA2NcR9dShT6gYjU7VxZ4D{}",
        STANDARD.encode(format!("\"{}\"", seed))
    )
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes
        .iter()
        .fold(String::new(), |acc, b| acc + &format!("{:02x}", b))
}
