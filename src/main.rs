use std::{
    collections::BTreeMap,
    fmt::{self, Write},
    path::PathBuf,
    time::UNIX_EPOCH,
};

use axum::{
    body::Body,
    extract::{Query, State},
    response::{IntoResponse, NoContent},
    routing, Json, Router,
};
use futures_util::{pin_mut, Stream, StreamExt};
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
    runtime, select,
    signal::ctrl_c,
    sync::mpsc::{self, Receiver, Sender},
    task::{self, yield_now, LocalSet},
};
use tokio_stream::wrappers::ReceiverStream;

trait SignalMessageSender: Clone {
    async fn send(&mut self, msg: String) -> Result<(), Box<dyn std::error::Error>>;
}

trait SignalMessageReceiver {
    fn create_stream(self) -> Result<impl Stream<Item = String>, Box<dyn std::error::Error>>;
}

async fn signal_pipe<T, R>(sender: T, receiver: R)
where
    T: SignalMessageSender,
    R: SignalMessageReceiver,
{
    receiver
        .create_stream()
        .expect("creating stream failed")
        .for_each(|msg| {
            let mut sender = sender.clone();
            async move {
                sender
                    .send(msg)
                    .await
                    .expect("receiver failed to receive message");
            }
        })
        .await
}

#[derive(Clone)]
struct CoreSender {
    internal: Sender<String>,
}

impl CoreSender {
    fn new(user: Uuid, db_path: PathBuf) -> Self {
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
                let receiving_task = task::spawn_local(async move {
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

                loop {
                    select! {
                        Some(msg) = rx.recv() => {
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
                                .send_message(ServiceId::Aci(user.into()), message, timestamp)
                                .await
                                .expect("failed to send message");

                            eprintln!("Message sent");
                        }
                        _ = ctrl_c() => {
                            break
                        }
                    }
                }

                receiving_task.abort();
            });

            rt.block_on(local);
        });

        Self { internal: tx }
    }
}

impl SignalMessageSender for CoreSender {
    async fn send(&mut self, msg: String) -> Result<(), Box<dyn std::error::Error>> {
        self.internal
            .send(msg)
            .await
            .map_err(|error| -> Box<dyn std::error::Error> { Box::new(error) })
    }
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

struct HttpReceiver {
    internal: Receiver<String>,
}

impl HttpReceiver {
    fn new(port: u16) -> Self {
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

async fn wait_for_ctrl_c() {
    ctrl_c().await.expect("failed to install Ctrl+C handler");
}

impl SignalMessageReceiver for HttpReceiver {
    fn create_stream(self) -> Result<impl Stream<Item = String>, Box<dyn std::error::Error>> {
        Ok(ReceiverStream::new(self.internal))
    }
}

enum Mode {
    Both {
        port: u16,
        signal_user: Uuid,
        signal_database: PathBuf,
    },
}

impl Mode {
    fn from_env() -> Mode {
        Mode::Both {
            port: 3000,
            signal_user: Uuid::try_parse(
                &std::env::var("SIGNAL_USER_UUID").expect("SIGNAL_USER_UUID env variable not set"),
            )
            .expect("SIGNAL_USER_UUID is not valid UUID"),
            signal_database: PathBuf::from(
                std::env::var("DB_PATH").expect("DB_PATH env variable not set"),
            ),
        }
    }
}

async fn async_main(mode: Mode) {
    match mode {
        Mode::Both {
            port,
            signal_user,
            signal_database,
        } => {
            let sender = CoreSender::new(signal_user, signal_database);
            let receiver = HttpReceiver::new(port);
            signal_pipe(sender, receiver).await
        }
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

fn main() {
    let mode = Mode::from_env();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async { async_main(mode).await })
}
