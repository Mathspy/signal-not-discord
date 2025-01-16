mod core;
mod http;
mod unix;

use core::CoreSender;
use std::path::PathBuf;

use futures_util::{Stream, StreamExt};
use http::HttpReceiver;
use presage::libsignal_service::prelude::Uuid;
use unix::{UnixReceiver, UnixSender};

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
    Receiver {
        port: u16,
        socket: PathBuf,
    },
    Sender {
        socket: PathBuf,
        signal_user: Uuid,
        signal_database: PathBuf,
    },
}

impl Mode {
    fn from_env() -> Mode {
        let signal = std::env::var("SIGNAL_USER_UUID")
            .and_then(|signal_user| std::env::var("DB_PATH").map(|db_path| (signal_user, db_path)));
        let socket = std::env::var("SOCKET_PATH");

        match (signal, socket) {
            (Ok((signal_user, db_path)), Ok(socket)) => Mode::Sender {
                socket: PathBuf::from(socket),
                signal_user: Uuid::try_parse(&signal_user)
                    .expect("SIGNAL_USER_UUID is not valid UUID"),
                signal_database: PathBuf::from(db_path),
            },
            (Ok((signal_user, db_path)), Err(_)) => Mode::Both {
                port: 3000,
                signal_user: Uuid::try_parse(&signal_user)
                    .expect("SIGNAL_USER_UUID is not valid UUID"),
                signal_database: PathBuf::from(db_path),
            },
            (Err(_), Ok(socket)) => Mode::Receiver {
                port: 3000,
                socket: PathBuf::from(socket),
            },
            (Err(_), Err(_)) => panic!("Neither SIGNAL_USER_UUID nor SOCKET_PATH was specified"),
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
            println!("Running with mode BOTH");
            let sender = CoreSender::new(signal_user, signal_database);
            let receiver = HttpReceiver::new(port);
            signal_pipe(sender, receiver).await
        }
        Mode::Receiver { port, socket } => {
            println!("Running with mode RECEIVER");
            let sender = UnixSender::new(socket);
            let receiver = HttpReceiver::new(port);
            signal_pipe(sender, receiver).await
        }
        Mode::Sender {
            socket,
            signal_user,
            signal_database,
        } => {
            println!("Running with mode SENDER");
            let sender = CoreSender::new(signal_user, signal_database);
            let receiver = UnixReceiver::new(socket);
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
