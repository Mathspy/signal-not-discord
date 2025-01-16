mod core;
mod http;

use core::CoreSender;
use std::path::PathBuf;

use futures_util::{Stream, StreamExt};
use http::HttpReceiver;
use presage::libsignal_service::prelude::Uuid;

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

fn main() {
    let mode = Mode::from_env();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async { async_main(mode).await })
}
