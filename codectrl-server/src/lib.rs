#![warn(clippy::pedantic)]
#![feature(let_chains)]

use codectrl_protobuf_bindings::{
    data::Log,
    logs_service::{
        Connection, Empty, LogClientService, LogClientTrait, LogServerService,
        LogServerTrait, RequestResult, RequestStatus, ServerDetails,
    },
};
use dashmap::{DashMap, DashSet};
use futures::StreamExt;
use std::{collections::VecDeque, net::SocketAddr, sync::Arc, time::Instant};
use tokio::sync::{mpsc, RwLock};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{
    metadata::MetadataMap, transport::Server, Code, Request, Response, Status, Streaming,
};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct ConnectionState {
    last_update: Instant,
    sent_log_ids: DashSet<String>,
}

impl Default for ConnectionState {
    fn default() -> Self {
        Self {
            last_update: Instant::now(),
            sent_log_ids: DashSet::new(),
        }
    }
}

impl ConnectionState {
    pub fn add_log(&mut self, log: &Log) {
        self.sent_log_ids.insert(log.uuid.clone());
        self.last_update = Instant::now();
    }
}

#[derive(Debug, Clone)]
pub struct Service {
    logs: Arc<RwLock<VecDeque<Log>>>,
    connections: Arc<RwLock<DashMap<String, ConnectionState>>>,
    host: String,
    port: u32,
    uptime: Instant,
}

impl Service {
    #[allow(clippy::missing_panics_doc)]
    pub fn verify_log(
        log: &mut Log,
        remote_addr: Option<SocketAddr>,
        metadata: &MetadataMap,
    ) {
        log.uuid = Uuid::new_v4().hyphenated().to_string();

        if log.message.len() > 1000 {
            log.warnings.push("Message exceeds 1000 characters".into());
        }

        if log.message.is_empty() {
            log.warnings.push("No message was given".into());
            log.message = "<None>".into();
        }

        if log.message_type.is_empty() {
            log.warnings.push("Message type was not supplied".into());
        }

        if log.stack.is_empty() {
            log.warnings.push("Stacktrace is empty".into());
        }

        if log.file_name.is_empty() {
            log.warnings.push("No file name found".into());
            log.file_name = "<None>".into();
        }

        match metadata.get("x-host") {
            Some(host) if matches!(remote_addr, Some(_)) =>
                if let Ok(host) = host.to_str() {
                    log.address = format!("{host}:{}", remote_addr.unwrap().port());
                } else {
                    log.address = remote_addr.unwrap().to_string();
                },
            Some(host) =>
                if let Ok(host) = host.to_str() {
                    log.address = host.to_string();
                },
            None if matches!(remote_addr, Some(_)) =>
                log.address = remote_addr.unwrap().to_string(),

            None => log.address = "Unknown".into(),
        }
    }
}

#[tonic::async_trait]
impl LogServerTrait for Service {
    async fn register_client(
        &self,
        _: Request<Empty>,
    ) -> Result<Response<Connection>, Status> {
        let connection = Connection::new();

        self.connections
            .write()
            .await
            .insert(connection.uuid.clone(), ConnectionState::default());

        Ok(Response::new(connection))
    }

    async fn register_existing_client(
        &self,
        _connection: Request<Connection>,
    ) -> Result<Response<RequestResult>, Status> {
        todo!()
    }

    async fn get_server_details(
        &self,
        _: Request<Empty>,
    ) -> Result<Response<ServerDetails>, Status> {
        let host = std::env::var("HOST").unwrap_or_else(|_| self.host.clone());

        let res = Response::new(ServerDetails {
            host,
            port: self.port,
            uptime: self.uptime.elapsed().as_secs(),
        });

        Ok(res)
    }

    async fn get_log(
        &self,
        connection: Request<Connection>,
    ) -> Result<Response<Log>, Status> {
        let connection = connection.into_inner();

        if Uuid::try_parse(&connection.uuid).is_err() {
            return Err(Status::unauthenticated("No valid Connection was supplied."));
        }

        if !self.connections.read().await.contains_key(&connection.uuid) {
            return Err(Status::unauthenticated(
                "Invalid connection, please register.",
            ));
        }

        let mut ignore = DashSet::new();

        if self.connections.read().await.contains_key(&connection.uuid) {
            ignore = self
                .connections
                .read()
                .await
                .get(&connection.uuid)
                .unwrap()
                .clone()
                .sent_log_ids;
        }

        let logs = self.logs.read().await.clone();
        let mut logs = logs
            .iter()
            .cloned()
            .filter(|log| !ignore.contains(&log.uuid))
            .collect::<VecDeque<_>>();

        if let Some(log) = logs.pop_front()
            && !ignore.contains(&log.uuid)
        {
            let key = self.connections.write().await;
            let key = key.get_mut(&connection.uuid);

            if let Some(mut key) = key {
                key.add_log(&log);
            }
            return Ok(Response::new(log));
        }

        Err(Status::new(Code::ResourceExhausted, "No more logs"))
    }

    type GetLogsStream = ReceiverStream<Result<Log, Status>>;

    async fn get_logs(
        &self,
        connection: Request<Connection>,
    ) -> Result<Response<Self::GetLogsStream>, Status> {
        let (tx, rx) = mpsc::channel(1024);
        let connection = connection.into_inner();

        if Uuid::try_parse(&connection.uuid).is_err() {
            return Err(Status::unauthenticated("No valid Connection was supplied."));
        }

        if !self.connections.read().await.contains_key(&connection.uuid) {
            return Err(Status::unauthenticated(
                "Invalid connection, please register.",
            ));
        }

        let connections = Arc::clone(&self.connections);

        let mut ignore = DashSet::new();

        if connections.read().await.contains_key(&connection.uuid) {
            ignore = connections
                .read()
                .await
                .get(&connection.uuid)
                .unwrap()
                .clone()
                .sent_log_ids;
        }

        let logs = self.logs.read().await.clone();
        let mut logs = logs
            .iter()
            .cloned()
            .filter(|log| !ignore.contains(&log.uuid))
            .collect::<VecDeque<_>>();

        tokio::spawn(async move {
            let key = connections.write().await;
            let mut key = key.get_mut(&connection.uuid);

            while let Some(log) = logs.pop_front()
                && !ignore.contains(&log.uuid)
            {

                if let Err(e) = tx.send(Ok(log.clone())).await {
                    eprintln!("[ERROR] Occurred when writing to channel: {e:?}");
                } else if let Some(key) = key.as_mut() {
                    key.add_log(&log);
                }
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }
}

#[tonic::async_trait]
impl LogClientTrait for Service {
    async fn send_log(
        &self,
        request: Request<Log>,
    ) -> Result<Response<RequestResult>, Status> {
        let remote_addr = request.remote_addr();
        let metadata = request.metadata().clone();
        let mut log = request.into_inner();

        Self::verify_log(&mut log, remote_addr, &metadata);

        self.logs.write().await.push_back(log);

        Ok(Response::new(RequestResult {
            message: "Log added!".into(),
            status: RequestStatus::Confirmed.into(),
        }))
    }

    async fn send_logs(
        &self,
        request: Request<Streaming<Log>>,
    ) -> Result<Response<RequestResult>, Status> {
        let remote_addr = request.remote_addr();
        let metadata = request.metadata().clone();
        let mut stream = request.into_inner();

        let mut lock = self.logs.write().await;

        let mut amount = 0;
        while let Some(log) = stream.next().await {
            let mut log = log?;

            Self::verify_log(&mut log, remote_addr, &metadata);
            lock.push_back(log);

            amount += 1;
        }

        Ok(Response::new(RequestResult {
            message: format!("{amount} logs added!"),
            status: RequestStatus::Confirmed.into(),
        }))
    }
}

/// Runs the `gRPC` server to be used by the GUI or the standalone binary.
///
/// # Errors
///
/// This function could error under the following circumstances:
///
/// 1. Supplied host was taken or invalid.
/// 2. Supplied port was taken or invalid.
/// 3. The inner tonic server returns an error during runtime.
#[allow(clippy::missing_panics_doc)]
pub async fn run_server(
    run_legacy_server: Option<bool>,
    host: Option<String>,
    port: Option<u32>,
) -> anyhow::Result<()> {
    // TODO: Add the legacy server thread and manage it through the gPRC server.
    let run_legacy_server = if run_legacy_server.is_some() {
        run_legacy_server.unwrap()
    } else {
        false
    };
    let host = if host.is_some() {
        host.unwrap()
    } else {
        String::from("127.0.0.1")
    };
    let port = if port.is_some() { port.unwrap() } else { 3002 };

    let logs = Arc::new(RwLock::new(VecDeque::new()));

    let logs_service = Service {
        host: host.clone(),
        port,
        uptime: Instant::now(),
        logs: Arc::clone(&logs),
        connections: Arc::new(RwLock::new(DashMap::new())),
    };

    if run_legacy_server {}

    let server_service = LogServerService::new(logs_service.clone());
    let client_service = LogClientService::new(logs_service);

    let gprc_addr = format!("{host}:{port}").parse()?;

    println!("Starting gPRC server on {gprc_addr}...");

    Server::builder()
        .add_service(tonic_web::enable(server_service))
        .add_service(tonic_web::enable(client_service))
        .serve(gprc_addr)
        .await?;

    Ok(())
}