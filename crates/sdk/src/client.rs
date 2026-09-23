//! A typed client for the `/v1` API.

use std::path::{Path, PathBuf};

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::{Method, Request, StatusCode};
use hyper_util::rt::TokioIo;
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::types::{
    ApiError, Event, FetchRequest, Fetched, File, Folder, KnownPeer, NewFolder, NewMember, Peer,
    Recycled, Status,
};

/// Client errors.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Could not reach the daemon.
    #[error("cannot connect to {target}: {source}")]
    Connect {
        /// What was dialed.
        target: String,
        /// The underlying error.
        source: std::io::Error,
    },
    /// The HTTP exchange failed.
    #[error("HTTP error: {0}")]
    Http(String),
    /// The server answered with an error.
    #[error("{} ({:?})", .0.message, .0.kind)]
    Api(ApiError),
    /// The response body was not what the API promises.
    #[error("unexpected response: {0}")]
    Decode(String),
}

#[derive(Debug, Clone)]
enum Target {
    Unix(PathBuf),
    Tcp { host: String },
}

/// A client for one daemon.
#[derive(Debug, Clone)]
pub struct Client {
    target: Target,
}

impl Client {
    /// Talks to a daemon over its Unix control socket (`<data_dir>/control.sock`).
    pub fn unix(socket: impl Into<PathBuf>) -> Self {
        Self {
            target: Target::Unix(socket.into()),
        }
    }

    /// Talks to a daemon over plain HTTP at `host:port`.
    pub fn tcp(host: impl Into<String>) -> Self {
        Self {
            target: Target::Tcp { host: host.into() },
        }
    }

    async fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<Vec<u8>>,
    ) -> Result<hyper::Response<hyper::body::Incoming>, Error> {
        let (io, host) = match &self.target {
            Target::Unix(socket) => {
                let stream = tokio::net::UnixStream::connect(socket)
                    .await
                    .map_err(|source| Error::Connect {
                        target: socket.display().to_string(),
                        source,
                    })?;
                (Io::Unix(TokioIo::new(stream)), "localhost".to_string())
            }
            Target::Tcp { host } => {
                let stream = tokio::net::TcpStream::connect(host)
                    .await
                    .map_err(|source| Error::Connect {
                        target: host.clone(),
                        source,
                    })?;
                (Io::Tcp(TokioIo::new(stream)), host.clone())
            }
        };
        let request = Request::builder()
            .method(method)
            .uri(path)
            .header(hyper::header::HOST, host)
            .header(hyper::header::CONTENT_TYPE, "application/json")
            .body(Full::new(Bytes::from(body.unwrap_or_default())))
            .map_err(|e| Error::Http(e.to_string()))?;
        match io {
            Io::Unix(io) => exchange(io, request).await,
            Io::Tcp(io) => exchange(io, request).await,
        }
    }

    async fn call<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<&impl Serialize>,
    ) -> Result<T, Error> {
        let body = body
            .map(serde_json::to_vec)
            .transpose()
            .map_err(|e| Error::Http(e.to_string()))?;
        let response = self.send(method, path, body).await?;
        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .map_err(|e| Error::Http(e.to_string()))?
            .to_bytes();
        if !status.is_success() {
            let error: ApiError = serde_json::from_slice(&bytes).map_err(|_| {
                Error::Decode(format!(
                    "HTTP {status}: {}",
                    String::from_utf8_lossy(&bytes)
                ))
            })?;
            return Err(Error::Api(error));
        }
        let bytes = if status == StatusCode::NO_CONTENT || bytes.is_empty() {
            Bytes::from_static(b"null")
        } else {
            bytes
        };
        serde_json::from_slice(&bytes).map_err(|e| Error::Decode(e.to_string()))
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, Error> {
        self.call(Method::GET, path, None::<&()>).await
    }

    /// This device.
    pub async fn status(&self) -> Result<Status, Error> {
        self.get("/v1/status").await
    }

    /// All folders.
    pub async fn folders(&self) -> Result<Vec<Folder>, Error> {
        self.get("/v1/folders").await
    }

    /// One folder.
    pub async fn folder(&self, id: &str) -> Result<Folder, Error> {
        self.get(&format!("/v1/folders/{id}")).await
    }

    /// Creates or joins a folder.
    pub async fn add_folder(&self, folder: &NewFolder) -> Result<Folder, Error> {
        self.call(Method::POST, "/v1/folders", Some(folder)).await
    }

    /// Stops syncing a folder added at runtime.
    pub async fn remove_folder(&self, id: &str) -> Result<(), Error> {
        self.call(Method::DELETE, &format!("/v1/folders/{id}"), None::<&()>)
            .await
    }

    /// Lets a user's devices sync a folder.
    pub async fn add_member(&self, folder: &str, user: &str) -> Result<Folder, Error> {
        let body = NewMember {
            user: user.to_string(),
        };
        self.call(
            Method::POST,
            &format!("/v1/folders/{folder}/members"),
            Some(&body),
        )
        .await
    }

    /// Removes a user from a folder.
    pub async fn remove_member(&self, folder: &str, user: &str) -> Result<Folder, Error> {
        self.call(
            Method::DELETE,
            &format!("/v1/folders/{folder}/members/{user}"),
            None::<&()>,
        )
        .await
    }

    /// Scans a folder for local changes now.
    pub async fn scan(&self, folder: &str) -> Result<(), Error> {
        self.call(
            Method::POST,
            &format!("/v1/folders/{folder}/scan"),
            None::<&()>,
        )
        .await
    }

    /// Live files in a folder.
    pub async fn files(&self, folder: &str) -> Result<Vec<File>, Error> {
        self.get(&format!("/v1/folders/{folder}/files")).await
    }

    /// Makes a file available on the daemon's disk, fetching it from peers if needed (a
    /// placeholder in a partial replica), and returns where it is.
    pub async fn fetch(&self, folder: &str, path: &str) -> Result<Fetched, Error> {
        self.call(
            Method::POST,
            &format!("/v1/folders/{folder}/fetch"),
            Some(&FetchRequest {
                path: path.to_owned(),
            }),
        )
        .await
    }

    /// A folder's recycle bin.
    pub async fn recycle_bin(&self, folder: &str) -> Result<Vec<Recycled>, Error> {
        self.get(&format!("/v1/folders/{folder}/recycle")).await
    }

    /// Restores a deleted entry.
    pub async fn restore(&self, folder: &str, entry: &str) -> Result<(), Error> {
        self.call(
            Method::POST,
            &format!("/v1/folders/{folder}/recycle/{entry}/restore"),
            None::<&()>,
        )
        .await
    }

    /// Peers with an active sync session.
    pub async fn peers(&self) -> Result<Vec<Peer>, Error> {
        self.get("/v1/peers").await
    }

    /// Peers this device keeps connected.
    pub async fn known_peers(&self) -> Result<Vec<KnownPeer>, Error> {
        self.get("/v1/peers/known").await
    }

    /// Remembers a peer to keep connected.
    pub async fn add_peer(&self, peer: &KnownPeer) -> Result<(), Error> {
        self.call(Method::POST, "/v1/peers/known", Some(peer)).await
    }

    /// Forgets a peer added at runtime.
    pub async fn remove_peer(&self, device: &str) -> Result<(), Error> {
        self.call(
            Method::DELETE,
            &format!("/v1/peers/known/{device}"),
            None::<&()>,
        )
        .await
    }

    /// The server's OpenAPI document.
    pub async fn openapi(&self) -> Result<serde_json::Value, Error> {
        self.get("/v1/openapi.json").await
    }

    /// Streams change events until the connection closes, calling `on_event` for each.
    /// Returning `false` from `on_event` stops the stream.
    pub async fn events(&self, mut on_event: impl FnMut(Event) -> bool) -> Result<(), Error> {
        let response = self.send(Method::GET, "/v1/events", None).await?;
        if !response.status().is_success() {
            return Err(Error::Http(format!("HTTP {}", response.status())));
        }
        let mut body = response.into_body();
        let mut buffer = String::new();
        while let Some(frame) = body.frame().await {
            let frame = frame.map_err(|e| Error::Http(e.to_string()))?;
            let Some(data) = frame.data_ref() else {
                continue;
            };
            buffer.push_str(&String::from_utf8_lossy(data));
            while let Some(end) = buffer.find("\n\n") {
                let block: String = buffer.drain(..end + 2).collect();
                for line in block.lines() {
                    if let Some(json) = line.strip_prefix("data:") {
                        let event: Event = serde_json::from_str(json.trim())
                            .map_err(|e| Error::Decode(e.to_string()))?;
                        if !on_event(event) {
                            return Ok(());
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// The control socket path under a daemon's data directory.
    pub fn socket_path(data_dir: &Path) -> PathBuf {
        data_dir.join("control.sock")
    }
}

enum Io {
    Unix(TokioIo<tokio::net::UnixStream>),
    Tcp(TokioIo<tokio::net::TcpStream>),
}

async fn exchange<T>(
    io: TokioIo<T>,
    request: Request<Full<Bytes>>,
) -> Result<hyper::Response<hyper::body::Incoming>, Error>
where
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (mut sender, connection) = hyper::client::conn::http1::handshake(io)
        .await
        .map_err(|e| Error::Http(e.to_string()))?;
    tokio::spawn(connection);
    sender
        .send_request(request)
        .await
        .map_err(|e| Error::Http(e.to_string()))
}
