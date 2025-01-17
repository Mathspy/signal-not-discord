use std::{
    io::{self, BufWriter, ErrorKind, Write},
    ops::ControlFlow,
    path::{Path, PathBuf},
    time::Duration,
};

use backoff::ExponentialBackoffBuilder;
use serde::{Deserialize, Serialize};
use serde_json::error;
use tokio::{
    fs,
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{unix::WriteHalf, UnixListener, UnixStream},
    select,
    sync::mpsc::{self, Receiver, Sender},
};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::{sync::CancellationToken, task::TaskTracker};

use crate::{shutdown_signal, SignalMessageReceiver, SignalMessageSender};

#[derive(Serialize, Deserialize, Debug)]
enum JsonRpc {
    Message {
        #[serde(rename = "c")]
        content: String,
    },
    Ping,
    Pong,
}

pub struct UnixReceiver {
    internal: Receiver<String>,
}

impl UnixReceiver {
    async fn handle_incoming_message<'a>(
        mut writer: WriteHalf<'a>,
        tx: &Sender<String>,
        buffer: &str,
    ) -> ControlFlow<(), WriteHalf<'a>> {
        let msg = match serde_json::from_str::<JsonRpc>(buffer) {
            Ok(msg) => msg,
            Err(error) => {
                if error.classify() == error::Category::Eof {
                    println!("Client socket disconnected by sending EOF, winding down stream...");
                    return ControlFlow::Break(());
                }

                eprintln!("Unexpected error while deserializing message: {error}");
                return ControlFlow::Continue(writer);
            }
        };

        match msg {
            JsonRpc::Message { content } => {
                if tx.send(content).await.is_err() {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(writer)
                }
            }
            JsonRpc::Ping => {
                let mut buf = BufWriter::new(Vec::new());
                serde_json::to_writer(&mut buf, &JsonRpc::Pong)
                    .expect("messages can be serialized");
                buf.write_all(b"\n")
                    .expect("writing into a vec shouldn't fail");
                buf.flush().expect("flushing to vector buffer failed");
                writer
                    .write_all(&buf.into_inner().unwrap())
                    .await
                    .expect("responding with pong failed");

                ControlFlow::Continue(writer)
            }
            JsonRpc::Pong => ControlFlow::Continue(writer),
        }
    }

    pub fn new(file: PathBuf) -> Self {
        async fn stream_task(mut stream: UnixStream, tx: Sender<String>, token: CancellationToken) {
            println!(
                "Established client connection from task {}",
                tokio::task::id()
            );

            let (reader, mut writer) = stream.split();
            let mut reader = BufReader::new(reader);

            loop {
                let mut buffer = String::new();

                select! {
                    result = reader.read_line(&mut buffer) => {
                        match result {
                            Ok(_) => {
                                match UnixReceiver::handle_incoming_message(writer, &tx, &buffer).await {
                                    ControlFlow::Continue(w) => {
                                        writer = w;
                                        continue
                                    },
                                    ControlFlow::Break(_) => break,
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
                        drop(reader);
                        println!("Task instructed to exit, winding down unix stream...");
                        if let Err(error) = stream.shutdown().await {
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
    async fn connect_to_stream(file: &Path) -> UnixStream {
        let backoff = ExponentialBackoffBuilder::new()
            .with_max_elapsed_time(None)
            .with_max_interval(Duration::from_secs(60))
            .build();

        backoff::future::retry_notify(
            backoff,
            || async {
                UnixStream::connect(&file)
                    .await
                    .map_err(|error| match error.kind() {
                        ErrorKind::NotFound => backoff::Error::transient(error),
                        _ => backoff::Error::permanent(error),
                    })
            },
            |error, duration| {
                println!(
                    "Failed to open socket stream, will retry again in {}: {error}",
                    fancy_duration::FancyDuration::new(duration).truncate(2)
                )
            },
        )
        .await
        .expect("Unable to open socket stream")
    }

    fn fill_buffer_with_message(buf: &mut Vec<u8>, msg: &JsonRpc) {
        buf.clear();
        let mut buf = BufWriter::new(buf);
        serde_json::to_writer(&mut buf, &msg).expect("messages can be serialized");
        buf.write_all(b"\n")
            .expect("writing into a vec shouldn't fail");
        buf.flush().expect("flushing to vector buffer failed");
    }

    async fn handle_io_error(error: io::Error, original_path: &Path) -> UnixStream {
        if error.kind() == ErrorKind::BrokenPipe {
            println!("Server socket has been closed... restarting stream...");

            let stream = Self::connect_to_stream(original_path).await;
            println!(
                "Succesfully restablished connection to stream {}",
                original_path.display()
            );

            stream
        } else {
            panic!("Stream writing returned unexpected error: {error}");
        }
    }

    async fn stream_send(stream: UnixStream, buf: &[u8], original_path: &Path) -> UnixStream {
        let mut stream = stream;

        loop {
            if let Err(error) = stream.write_all(buf).await {
                stream = Self::handle_io_error(error, original_path).await;
                continue;
            }

            break stream;
        }
    }

    async fn wait_for_pong(
        stream: UnixStream,
        buf: &mut Vec<u8>,
        original_path: &Path,
    ) -> UnixStream {
        buf.clear();
        let mut stream = stream;

        loop {
            let mut reader = BufReader::new(&mut stream);
            match reader.read_until(b'\n', buf).await {
                Ok(_) => {}
                Err(error) => {
                    stream = Self::handle_io_error(error, original_path).await;
                    continue;
                }
            }

            let msg = match serde_json::from_slice::<JsonRpc>(buf) {
                Ok(msg) => msg,
                Err(error) => {
                    if error.classify() == error::Category::Eof {
                        stream = Self::connect_to_stream(original_path).await;
                        continue;
                    }

                    panic!("Unexpected error while deserializing message: {error}");
                }
            };

            match msg {
                JsonRpc::Pong => {
                    // success...
                }
                weird => {
                    eprintln!("Unexpected message {weird:?} while waiting for pong");
                }
            }

            break stream;
        }
    }

    pub fn new(file: PathBuf) -> Self {
        let (tx, mut rx) = mpsc::channel::<String>(8);
        tokio::spawn(async move {
            let mut stream = Self::connect_to_stream(&file).await;

            println!("Connected to server on socket {}...", file.display());

            let mut ping_interval = tokio::time::interval(Duration::from_secs(5));

            let mut buf = Vec::new();

            loop {
                select! {
                    Some(msg) = rx.recv() => {
                        Self::fill_buffer_with_message(&mut buf, &JsonRpc::Message { content: msg });
                        stream = Self::stream_send(stream, &buf, &file).await;
                    }
                    _ = ping_interval.tick() => {
                        Self::fill_buffer_with_message(&mut buf, &JsonRpc::Ping);
                        stream = Self::stream_send(stream, &buf, &file).await;
                        stream = Self::wait_for_pong(stream, &mut buf, &file).await;
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
