use std::{collections::BTreeMap, time::UNIX_EPOCH};

use axum::{
    extract::{self, State},
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
            }
        });

        rt.block_on(local);
    });

    tx
}

#[derive(Deserialize, Debug)]
struct Payload {
    content: String,
    #[serde(flatten)]
    other: BTreeMap<String, Value>,
}

#[derive(Serialize)]
struct Response {}

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
    Json(payload): extract::Json<Payload>,
) -> Json<Response> {
    eprintln!("{payload:?}");
    // let _ = channel.send(payload.content).await.map_err(|err| {
    //     println!("Error couldn't send message: {err}");
    // });

    Json(Response {})
}

fn main() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async { async_main().await })
}
