use std::{
    collections::BTreeMap,
    fmt::{self, Write},
};

use axum::{
    body::Body,
    extract::{Query, State},
    response::{IntoResponse, NoContent},
    routing, Json, Router,
};
use futures_util::Stream;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::{
    signal::ctrl_c,
    sync::mpsc::{self, Receiver, Sender},
};
use tokio_stream::wrappers::ReceiverStream;

use crate::SignalMessageReceiver;

#[derive(Deserialize, Debug)]
struct EmbedField {
    name: String,
    value: String,
}

#[derive(Deserialize, Debug)]
struct Embed {
    title: Option<String>,
    description: Option<String>,
    #[serde(default)]
    fields: Vec<EmbedField>,
}

#[derive(Deserialize, Debug)]
struct Payload {
    content: Option<String>,
    #[serde(default)]
    embeds: Vec<Embed>,
    #[serde(flatten)]
    _other: BTreeMap<String, Value>,
}

impl fmt::Display for Payload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(content) = &self.content {
            f.write_str(content)?;
            f.write_char('\n')?;
            f.write_char('\n')?;
        }

        for embed in &self.embeds {
            if let Some(title) = &embed.title {
                f.write_str(title)?;
                f.write_char('\n')?;
            }

            if let Some(description) = &embed.description {
                f.write_str(description)?;
                f.write_char('\n')?;
                f.write_char('\n')?;
            }

            for field in &embed.fields {
                f.write_str(&field.name)?;
                f.write_str(": ")?;
                f.write_str(&field.value)?;
                f.write_char('\n')?;
            }
        }

        Ok(())
    }
}

#[derive(Deserialize)]
struct Options {
    #[serde(default)]
    wait: bool,
}

#[derive(Serialize)]
struct Response {
    id: &'static str,
}

pub struct HttpReceiver {
    internal: Receiver<String>,
}

impl HttpReceiver {
    pub fn new(port: u16) -> Self {
        let (tx, rx) = mpsc::channel(8);
        tokio::spawn(async move {
            let app = Router::new()
                .route("/webhooks/:id/:token", routing::post(handler))
                .with_state(tx);
            let listener = tokio::net::TcpListener::bind(("0.0.0.0", port))
                .await
                .unwrap();
            axum::serve(listener, app)
                .with_graceful_shutdown(wait_for_ctrl_c())
                .await
                .unwrap();
        });

        Self { internal: rx }
    }
}

async fn handler(
    State(channel): State<Sender<String>>,
    Query(query): Query<Options>,
    Json(payload): Json<Payload>,
) -> axum::http::Response<Body> {
    eprintln!("Sending: {payload:?}");
    let _ = channel.send(payload.to_string()).await.map_err(|err| {
        println!("Error couldn't send message: {err}");
    });

    if query.wait {
        Json(Response {
            id: "1322948599578890240",
        })
        .into_response()
    } else {
        NoContent.into_response()
    }
}

async fn wait_for_ctrl_c() {
    ctrl_c().await.expect("failed to install Ctrl+C handler");
}

impl SignalMessageReceiver for HttpReceiver {
    fn create_stream(self) -> Result<impl Stream<Item = String>, Box<dyn std::error::Error>> {
        Ok(ReceiverStream::new(self.internal))
    }
}
