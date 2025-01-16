use std::{io::ErrorKind, path::PathBuf};

use serde::{Deserialize, Serialize};
use tokio::{
    fs,
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    select,
    sync::mpsc::{self, Receiver, Sender},
};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::{sync::CancellationToken, task::TaskTracker};

use crate::{shutdown_signal, SignalMessageReceiver, SignalMessageSender};

#[derive(Serialize, Deserialize)]
struct JsonRpcMessage {
    #[serde(rename = "i")]
    internal: String,
}

pub struct UnixReceiver {
    internal: Receiver<String>,
}

impl UnixReceiver {
    pub fn new(file: PathBuf) -> Self {
        async fn stream_task(stream: UnixStream, tx: Sender<String>, token: CancellationToken) {
            let mut reader = BufReader::new(stream);

            loop {
                let mut buffer = String::new();

                select! {
                    result = reader.read_line(&mut buffer) => {
                        match result {
                            Ok(_) => {
                                let msg = serde_json::from_str::<JsonRpcMessage>(&buffer)
                                    .expect("messages can be deserialized");
                                if tx.send(msg.internal).await.is_err() {
                                    break;
                                }
                            }
                            Err(error) => {
                                if error.kind() == ErrorKind::BrokenPipe {
                                    break;
                                }
                                eprintln!("Read line error: {error}");
                            }
                        }
                    }
                    _ = token.cancelled() => {
                        println!("Task instructed to exit, winding down unix stream...");
                        if let Err(error) = reader.into_inner().shutdown().await {
                            eprintln!("Error winding down stream: {error}");
                        }
                        break;
                    }
                }
            }
        }

        let token = CancellationToken::new();
        let (tx, rx) = mpsc::channel(8);
        let tasks = TaskTracker::new();
        let socket = UnixListener::bind(&file).expect("Unable to create socket stream");
        println!("Created new server on socket {}...", file.display());

        tokio::spawn(async move {
            let token = token.clone();
            loop {
                select! {
                    result = socket.accept() => {
                        match result {
                            Ok((stream, _)) => tasks.spawn(stream_task(stream, tx.clone(), token.clone())),
                            Err(error) => {
                                eprintln!("Socket open error: {error}");
                                continue;
                            },
                        }
                    }
                    signal = shutdown_signal() => {
                        println!("Exit signal {signal} received, winding down all stream tasks...");
                        token.cancel();

                        break
                    },
                };
            }

            tasks.close();

            tasks.wait().await;
            println!("All stream tasks have exited");

            let _ = fs::remove_file(file).await.map_err(|error| {
                eprintln!("Socket delete error: {error}");
            });

            println!("Freed socket");
        });

        UnixReceiver { internal: rx }
    }
}

impl SignalMessageReceiver for UnixReceiver {
    fn create_stream(
        self,
    ) -> Result<impl futures_util::Stream<Item = String>, Box<dyn std::error::Error>> {
        Ok(ReceiverStream::new(self.internal))
    }
}

#[derive(Clone)]
pub struct UnixSender {
    internal: Sender<String>,
}

impl UnixSender {
    pub fn new(file: PathBuf) -> Self {
        let (tx, mut rx) = mpsc::channel::<String>(8);
        tokio::spawn(async move {
            let mut stream = UnixStream::connect(&file)
                .await
                .expect("Unable to open socket stream");
            println!("Connected to server on socket {}...", file.display());

            loop {
                select! {
                    Some(msg) = rx.recv() => {
                        let msg = serde_json::to_vec(&JsonRpcMessage { internal: msg })
                            .expect("messages can be serialized");
                        if let Err(error) = stream.write_all(&msg).await {
                            if error.kind() == ErrorKind::BrokenPipe {
                                println!("Server socket has been closed... shutting down stream...");
                                break;
                            }
                        }
                    }
                    signal = shutdown_signal() => {
                        println!("Exit signal {signal} received, turning off socket client connect...");
                        break
                    }
                }
            }
        });

        Self { internal: tx }
    }
}

impl SignalMessageSender for UnixSender {
    async fn send(&mut self, msg: String) -> Result<(), Box<dyn std::error::Error>> {
        self.internal
            .send(msg)
            .await
            .map_err(|error| -> Box<dyn std::error::Error> { Box::new(error) })
    }
}
