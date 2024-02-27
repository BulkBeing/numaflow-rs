#![warn(missing_docs)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::shared::{self, prost_timestamp_from_utc};
use crate::source::sourcer::source_server::Source;
use crate::source::sourcer::{
    AckRequest, AckResponse, PendingResponse, ReadRequest, ReadResponse, ReadyResponse,
};
use chrono::{DateTime, Utc};
use tokio::sync::mpsc::{self, Sender};
use tokio::sync::oneshot;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{async_trait, Request, Response, Status};

use self::sourcer::{partitions_response, source_server, PartitionsResponse};

mod sourcer {
    tonic::include_proto!("source.v1");
}

struct SourceService<T> {
    handler: Arc<T>,
}

#[async_trait]
/// Trait representing a [user defined source](https://numaflow.numaproj.io/user-guide/sources/overview/).
///
/// ## Example
/// Please refer to [simple source](https://github.com/numaproj/numaflow-rs/tree/main/examples/simple-source) for an example.
///
/// ## NOTE
/// The standard convention for both [`Sourcer::read`] and [`Sourcer::ack`] is that they should be mutable,
/// since they have to update some state. Unfortunately the SDK provides only a shared reference of self and thus makes it immutable. This is because
/// gRPC [tonic] provides only a shared reference for its traits. This means, the implementer for trait will have to use [SharedState] pattern to mutate
/// the values as recommended in [issue-427]. This might change in future as async traits evolves.
///
/// [user-defined source]: https://numaflow.numaproj.io/user-guide/sources/overview/
/// [tonic]: https://github.com/hyperium/tonic/
/// [SharedState]: https://tokio.rs/tokio/tutorial/shared-state
/// [issue-427]: https://github.com/hyperium/tonic/issues/427
pub trait Sourcer {
    /// Reads the messages from the source and sends them to the transmitter.
    async fn read(&self, request: SourceReadRequest, transmitter: Sender<Message>);
    /// Acknowledges the messages that have been processed by the user-defined source.
    async fn ack(&self, offsets: Vec<Offset>);
    /// Returns the number of messages that are yet to be processed by the user-defined source.
    async fn pending(&self) -> usize;
    /// Returns the partitions associated with the source. This will be used by the platform to determine
    /// the partitions to which the watermark should be published. Some sources might not have the concept of partitions.
    /// Kafka is an example of source where a reader can read from multiple partitions.
    /// If None is returned, Numaflow replica-id will be returned as the partition.
    async fn partitions(&self) -> Option<Vec<i32>>;
}

/// A request from the gRPC client (numaflow) to the user's [`Sourcer::read`].
pub struct SourceReadRequest {
    /// The number of messages to be read.
    pub count: usize,
    /// Request timeout in milliseconds.
    pub timeout: Duration,
}

/// The offset of the message.
pub struct Offset {
    /// Offset value in bytes.
    pub offset: Vec<u8>,
    /// Partition ID of the message.
    pub partition_id: i32,
}

#[async_trait]
impl<T> Source for SourceService<T>
where
    T: Sourcer + Send + Sync + 'static,
{
    type ReadFnStream = ReceiverStream<Result<ReadResponse, Status>>;

    async fn read_fn(
        &self,
        request: Request<ReadRequest>,
    ) -> Result<Response<Self::ReadFnStream>, Status> {
        let sr = request.into_inner().request.unwrap();

        // tx,rx pair for sending data over to user-defined source
        let (stx, mut srx) = mpsc::channel::<Message>(1);
        // tx,rx pair for gRPC response
        let (tx, rx) = mpsc::channel::<Result<ReadResponse, Status>>(1);

        // start the ud-source rx asynchronously and start populating the gRPC response so it can be streamed to the gRPC client (numaflow).
        tokio::spawn(async move {
            while let Some(resp) = srx.recv().await {
                tx.send(Ok(ReadResponse {
                    result: Some(sourcer::read_response::Result {
                        payload: resp.value,
                        offset: Some(sourcer::Offset {
                            offset: resp.offset.offset,
                            partition_id: resp.offset.partition_id,
                        }),
                        event_time: prost_timestamp_from_utc(resp.event_time),
                        keys: resp.keys,
                    }),
                }))
                .await
                .expect("receiver dropped");
            }
        });

        let handler_fn = Arc::clone(&self.handler);
        // we want to start streaming to the server as soon as possible
        tokio::spawn(async move {
            // user-defined source read handler
            handler_fn
                .read(
                    SourceReadRequest {
                        count: sr.num_records as usize,
                        timeout: Duration::from_millis(sr.timeout_in_ms as u64),
                    },
                    stx,
                )
                .await
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }

    async fn ack_fn(&self, request: Request<AckRequest>) -> Result<Response<AckResponse>, Status> {
        let ar: AckRequest = request.into_inner();

        let success_response = Response::new(AckResponse {
            result: Some(sourcer::ack_response::Result { success: Some(()) }),
        });

        let Some(request) = ar.request else {
            return Ok(success_response);
        };

        // invoke the user-defined source's ack handler
        let offsets = request
            .offsets
            .into_iter()
            .map(|so| Offset {
                offset: so.offset,
                partition_id: so.partition_id,
            })
            .collect();

        self.handler.ack(offsets).await;

        Ok(success_response)
    }

    async fn pending_fn(&self, _: Request<()>) -> Result<Response<PendingResponse>, Status> {
        // invoke the user-defined source's pending handler
        let pending = self.handler.pending().await;

        Ok(Response::new(PendingResponse {
            result: Some(sourcer::pending_response::Result {
                count: pending as i64,
            }),
        }))
    }

    async fn partitions_fn(
        &self,
        _request: Request<()>,
    ) -> Result<Response<PartitionsResponse>, Status> {
        let partitions = match self.handler.partitions().await {
            Some(v) => v,
            None => vec![std::env::var("NUMAFLOW_REPLICA")
                .unwrap_or_default()
                .parse::<i32>()
                .unwrap_or_default()],
        };
        Ok(Response::new(PartitionsResponse {
            result: Some(partitions_response::Result { partitions }),
        }))
    }

    async fn is_ready(&self, _: Request<()>) -> Result<Response<ReadyResponse>, Status> {
        Ok(Response::new(ReadyResponse { ready: true }))
    }
}

/// Message is the response from the user's [`Sourcer::read`]
pub struct Message {
    /// The value passed to the next vertex.
    pub value: Vec<u8>,
    /// Offset of the message. When the message is acked, the offset is passed to the user's [`Sourcer::ack`].
    pub offset: Offset,
    /// The time at which the message was generated.
    pub event_time: DateTime<Utc>,
    /// Keys of the message.
    pub keys: Vec<String>,
}

/// gRPC server for starting a [`Sourcer`] service
#[derive(Debug)]
pub struct Server<T> {
    sock_addr: PathBuf,
    max_message_size: usize,
    server_info_file: PathBuf,
    svc: Option<T>,
}

impl<T> Server<T> {
    /// Creates a new gRPC `Server` instance
    pub fn new(source_svc: T) -> Self {
        let server_info_file = if std::env::var_os("NUMAFLOW_POD").is_some() {
            "/var/run/numaflow/server-info"
        } else {
            "/tmp/numaflow.server-info"
        };
        Server {
            sock_addr: "/var/run/numaflow/source.sock".into(),
            max_message_size: 64 * 1024 * 1024,
            server_info_file: server_info_file.into(),
            svc: Some(source_svc),
        }
    }

    /// Set the unix domain socket file path used by the gRPC server to listen for incoming connections.
    /// Default value is `/var/run/numaflow/source.sock`
    pub fn with_socket_file(mut self, file: impl Into<PathBuf>) -> Self {
        self.sock_addr = file.into();
        self
    }

    /// Get the unix domain socket file path where gRPC server listens for incoming connections. Default value is `/var/run/numaflow/source.sock`
    pub fn socket_file(&self) -> &std::path::Path {
        self.sock_addr.as_path()
    }

    /// Set the maximum size of an encoded and decoded gRPC message. The value of `message_size` is in bytes. Default value is 4MB.
    pub fn with_max_message_size(mut self, message_size: usize) -> Self {
        self.max_message_size = message_size;
        self
    }

    /// Get the maximum size of an encoded and decoded gRPC message in bytes. Default value is 64MB.
    pub fn max_message_size(&self) -> usize {
        self.max_message_size
    }

    /// Change the file in which numflow server information is stored on start up to the new value. Default value is `/tmp/numaflow.server-info`
    pub fn with_server_info_file(mut self, file: impl Into<PathBuf>) -> Self {
        self.server_info_file = file.into();
        self
    }

    /// Get the path to the file where numaflow server info is stored. Default value is `/tmp/numaflow.server-info`
    pub fn server_info_file(&self) -> &std::path::Path {
        self.server_info_file.as_path()
    }

    /// Starts the gRPC server. When message is received on the `shutdown` channel, graceful shutdown of the gRPC server will be initiated.
    pub async fn start_with_shutdown(
        &mut self,
        shutdown: oneshot::Receiver<()>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
    where
        T: Sourcer + Send + Sync + 'static,
    {
        let listener = shared::create_listener_stream(&self.sock_addr, &self.server_info_file)?;
        let handler = self.svc.take().unwrap();
        let source_service = SourceService {
            handler: Arc::new(handler),
        };

        let source_svc = source_server::SourceServer::new(source_service)
            .max_decoding_message_size(self.max_message_size)
            .max_decoding_message_size(self.max_message_size);

        let shutdown = async {
            shutdown
                .await
                .expect("Receiving message from shutdown channel");
        };
        tonic::transport::Server::builder()
            .add_service(source_svc)
            .serve_with_incoming_shutdown(listener, shutdown)
            .await
            .map_err(Into::into)
    }

    /// Starts the gRPC server. Automatically registers singal handlers for SIGINT and SIGTERM and initiates graceful shutdown of gRPC server when either one of the singal arrives.
    pub async fn start(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
    where
        T: Sourcer + Send + Sync + 'static,
    {
        let (tx, rx) = oneshot::channel::<()>();
        tokio::spawn(shared::wait_for_signal(tx));
        self.start_with_shutdown(rx).await
    }
}

#[cfg(test)]
mod tests {
    use super::sourcer;
    use chrono::Utc;
    use std::vec;
    use std::{error::Error, time::Duration};
    use tokio::sync::Mutex;
    use tokio_stream::StreamExt;
    use tower::service_fn;

    use crate::source::{self, Message, Offset, SourceReadRequest};
    use tempfile::TempDir;
    use tokio::sync::mpsc::Sender;
    use tokio::sync::oneshot;
    use tonic::transport::Uri;

    // A source that returns numbers in the range [0..end)
    struct Counter {
        end: usize,
        pos: Mutex<usize>,
        acked: Mutex<usize>,
    }

    impl Counter {
        fn new(end: usize) -> Self {
            Self {
                end,
                pos: Mutex::new(0),
                acked: Mutex::new(0),
            }
        }
    }

    #[tonic::async_trait]
    impl source::Sourcer for Counter {
        async fn read(&self, request: SourceReadRequest, transmitter: Sender<Message>) {
            let mut pos = self.pos.lock().await;
            if *pos >= self.end {
                return;
            }
            let mut count = 0;
            while *pos < self.end && count < request.count {
                transmitter
                    .send(Message {
                        value: pos.to_le_bytes().to_vec(),
                        event_time: Utc::now(),
                        offset: Offset {
                            offset: pos.to_string().into_bytes(),
                            partition_id: 0,
                        },
                        keys: vec![],
                    })
                    .await
                    .unwrap();
                count += 1;
                *pos += 1;
            }
        }

        async fn ack(&self, offsets: Vec<Offset>) {
            *self.acked.lock().await += offsets.len()
        }

        async fn pending(&self) -> usize {
            self.end - *self.acked.lock().await
        }

        async fn partitions(&self) -> Option<Vec<i32>> {
            Some(vec![2])
        }
    }

    #[tokio::test]
    async fn source_server() -> Result<(), Box<dyn Error>> {
        let tmp_dir = TempDir::new()?;
        let sock_file = tmp_dir.path().join("source.sock");
        let server_info_file = tmp_dir.path().join("server_info");

        let mut server = source::Server::new(Counter::new(8))
            .with_server_info_file(&server_info_file)
            .with_socket_file(&sock_file)
            .with_max_message_size(10240);

        assert_eq!(server.max_message_size(), 10240);
        assert_eq!(server.server_info_file(), server_info_file);
        assert_eq!(server.socket_file(), sock_file);

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let task = tokio::spawn(async move { server.start_with_shutdown(shutdown_rx).await });

        tokio::time::sleep(Duration::from_millis(50)).await;

        // https://github.com/hyperium/tonic/blob/master/examples/src/uds/client.rs
        let channel = tonic::transport::Endpoint::try_from("http://[::]:50051")?
            .connect_with_connector(service_fn(move |_: Uri| {
                // Connect to a Uds socket
                let sock_file = sock_file.clone();
                tokio::net::UnixStream::connect(sock_file)
            }))
            .await?;

        let mut client = sourcer::source_client::SourceClient::new(channel);
        let request = tonic::Request::new(sourcer::ReadRequest {
            request: Some(sourcer::read_request::Request {
                num_records: 5,
                timeout_in_ms: 500,
            }),
        });

        let resp = client.read_fn(request).await?;
        let resp = resp.into_inner();
        let result: Vec<sourcer::read_response::Result> = resp
            .map(|item| item.unwrap().result.unwrap())
            .collect()
            .await;
        let response_values: Vec<usize> = result
            .iter()
            .map(|item| {
                usize::from_le_bytes(
                    item.payload
                        .clone()
                        .try_into()
                        .expect("expected Vec length to be 8"),
                )
            })
            .collect();
        assert_eq!(response_values, vec![0, 1, 2, 3, 4]);

        let pending_before_ack = client
            .pending_fn(tonic::Request::new(()))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(
            pending_before_ack.result.unwrap().count,
            8,
            "Expected pending messages to be 8 before ACK"
        );

        let offsets_to_ack: Vec<sourcer::Offset> = result
            .iter()
            .map(|item| item.clone().offset.unwrap())
            .collect();
        let ack_request = tonic::Request::new(sourcer::AckRequest {
            request: Some(sourcer::ack_request::Request {
                offsets: offsets_to_ack,
            }),
        });
        let resp = client.ack_fn(ack_request).await.unwrap().into_inner();
        assert!(
            resp.result.unwrap().success.is_some(),
            "Expected acknowledgement request to be successful"
        );

        let pending_before_ack = client
            .pending_fn(tonic::Request::new(()))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(
            pending_before_ack.result.unwrap().count,
            3,
            "Expected pending messages to be 3 after ACK"
        );

        let partitions = client
            .partitions_fn(tonic::Request::new(()))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(
            partitions.result.unwrap().partitions,
            vec![2],
            "Expected number of partitions to be 2"
        );

        shutdown_tx
            .send(())
            .expect("Sending shutdown signal to gRPC server");
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(task.is_finished(), "gRPC server is still running");
        Ok(())
    }
}
