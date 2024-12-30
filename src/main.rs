use std::{
    collections::BTreeMap,
    fmt::{self, Write},
    time::UNIX_EPOCH,
};

use axum::{
    body::Body,
    extract::{Query, State},
    response::{IntoResponse, NoContent},
    routing, Json, Router,
};
use futures_util::{pin_mut, StreamExt};
use presage::{
    libsignal_service::{prelude::Uuid, protocol::ServiceId},
    manager::ReceivingMode,
    model::identity::OnNewIdentity,
    proto::DataMessage,
    Manager,
};
use presage_store_sled::{MigrationConflictStrategy, SledStore};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::{
    runtime,
    sync::mpsc::{self, Sender},
    task::{self, yield_now, LocalSet},
};

async fn manager_runtime() -> Sender<String> {
    let db_path = std::env::var("DB_PATH").expect("DB_PATH env variable to be set");
    let signal_user_uuid =
        std::env::var("SIGNAL_USER_UUID").expect("DB_PATH env variable to be set");

    let (tx, mut rx) = mpsc::channel(8);
    let rt = runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    std::thread::spawn(move || {
        let local = LocalSet::new();

        local.spawn_local(async move {
            let store = SledStore::open_with_passphrase(
                db_path,
                None::<&str>,
                MigrationConflictStrategy::Raise,
                OnNewIdentity::Reject,
            )
            .await
            .expect("Database to exist and be accessible");
            let mut manager = Manager::load_registered(store)
                .await
                .expect("manager is able to boot up from database correctly");

            let mut receiving_manager = manager.clone();
            task::spawn_local(async move {
                let messages = receiving_manager
                    .receive_messages(ReceivingMode::Forever)
                    .await
                    .expect("failed to initialize messages stream");
                pin_mut!(messages);

                while (messages.next().await).is_some() {
                    yield_now().await;
                }
            });

            eprintln!("Ready to send messages out!");

            while let Some(msg) = rx.recv().await {
                let timestamp = std::time::SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .expect("Time went backwards")
                    .as_millis() as u64;

                let message = DataMessage {
                    body: Some(msg),
                    timestamp: Some(timestamp),
                    ..Default::default()
                };

                manager
                    .send_message(
                        ServiceId::Aci(
                            Uuid::try_parse(&signal_user_uuid)
                                .expect("is valid UUID")
                                .into(),
                        ),
                        message,
                        timestamp,
                    )
                    .await
                    .expect("failed to send message");

                eprintln!("Message sent");
            }
        });

        rt.block_on(local);
    });

    tx
}

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

async fn async_main() {
    let channel = manager_runtime().await;

    let app = Router::new()
        .route("/webhooks/:id/:token", routing::post(handler))
        .with_state(channel);
    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    axum::serve(listener, app).await.unwrap();
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

fn main() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async { async_main().await })
}
