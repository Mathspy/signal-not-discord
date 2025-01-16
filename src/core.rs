use std::{path::PathBuf, time::UNIX_EPOCH};

use futures_util::{pin_mut, StreamExt};
use presage::{
    libsignal_service::{prelude::Uuid, protocol::ServiceId},
    manager::ReceivingMode,
    model::identity::OnNewIdentity,
    proto::DataMessage,
    Manager,
};
use presage_store_sled::{MigrationConflictStrategy, SledStore};
use tokio::{
    runtime, select,
    signal::ctrl_c,
    sync::mpsc::{self, Sender},
    task::{self, yield_now, LocalSet},
};

use crate::SignalMessageSender;

#[derive(Clone)]
pub struct CoreSender {
    internal: Sender<String>,
}

impl CoreSender {
    pub fn new(user: Uuid, db_path: PathBuf) -> Self {
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
