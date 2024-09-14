use std::{
    fmt::Display, future::Future, io::Cursor, os::unix::net::UnixStream, sync::Arc, time::Duration,
};

use daemonize::{Daemonize, Stdio};
use fork::Fork;
use log::{debug, error, info, warn};
use thiserror::Error as ThisError;
use tokio::{
    fs,
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream as TokioUnixStream},
    select,
    sync::{
        mpsc::{self, Sender},
        oneshot,
    },
};

mod wait_token;

/// Run and await an async function that requires communication with a daemon, abstracting away
/// daemon creation and some communication logic.
///
/// The function that requires communication with a daemon is referred to as the client. It
/// receives a bi-directional async stream that is directly connected to one instance of the
/// daemon's client handler.
///
/// The client is awaited in the current process and may exchange information with the handler
/// using the provided stream to take advantage of the common state available to all instances of
/// the handler.
///
/// The handler is spawned on the daemon process for each client and is given the shared state and
/// a bi-directional stream whose other end is connected to the client. Multiple handlers are
/// awaited in separate async tasks, running in parallel.
///
/// The common state is produced by awaiting the `init` function in the daemon process and remains
/// in existence throughout the life of the daemon process.
///
/// The daemon process is spawned when necessary (i.e. when it is not detected as running) and
/// is kept running forever for further clients to connect to it and take advantage of its state.
///
/// # Arguments
///
/// * `pid_filename` - the name of the PID file
/// * `socket_filename` - the name of the UNIX socket file for communication with the daemon
/// * `init` - a async function which returns the state shared between client handlers on the daemon
///   side; it is given a control handle as an argument that can be used to request daemon shutdown
/// * `handler` - a function spawned for each client that connects to the daemon; it can use the
///   shared state and communicate with its client using a stream
/// * `client` - a function that implements the daemon's client instance by communicating with the
///   daemon
pub fn with_daemon<S, SE, I, IFut, H, HFut, R, C, CFut>(
    pid_filename: &str,
    socket_filename: &str,
    init: I,
    handler: H,
    client: C,
) -> Result<R, Error>
where
    I: FnOnce(DaemonControl) -> IFut + Send + 'static,
    IFut: Future<Output = Result<S, SE>> + Send,
    S: Send + Sync + 'static,
    SE: Send + std::fmt::Debug + Display + 'static,
    H: Fn(Arc<S>, TokioUnixStream) -> HFut + Send + 'static,
    HFut: Future<Output = ()> + Send + 'static,
    C: FnOnce(TokioUnixStream) -> CFut,
    CFut: Future<Output = R>,
{
    let (child_stream, parent_stream) = UnixStream::pair()
        .inspect_err(|e| error!("could not create UnixStream pair: {e}"))
        .map_err(|_| Error::FatalIO)?;
    parent_stream
        .set_nonblocking(true)
        .inspect_err(|e| error!("could not set UnixStream nonblocking: {e}"))
        .map_err(|_| Error::FatalIO)?;
    match fork::fork()
        .inspect_err(|e| error!("couldn't fork: {e}"))
        .map_err(|_| Error::FatalFork)?
    {
        Fork::Child => {
            debug!("in child process");
            drop(parent_stream);
            if do_child(pid_filename, socket_filename, child_stream, init, handler).is_err() {
                error!("daemon failed");
            }
            std::process::exit(0)
        }
        Fork::Parent(_) => {
            debug!("in parent process");
            drop(child_stream);
            run_client(socket_filename, client, parent_stream)
        }
    }
}

/// A handle to control the daemon.
pub struct DaemonControl(Sender<DaemonControlMessage>);

/// The error returned from [`with_daemon`].
#[derive(ThisError, Debug)]
pub enum Error {
    /// Could not perform basic I/O before any daemon detection/spawning. This is a fatal error.
    #[error("fatal I/O error")]
    FatalIO,
    /// Could not fork, this is a fatal error.
    #[error("fork() error")]
    FatalFork,
    /// The status of the daemon could not be determined because of a fatal error.
    #[error("cannot determine spawned daemon status")]
    DaemonStatusUnknown,
    /// Daemon failed to start.
    ///
    /// The daemon was not running so it was spawned and failed for an unknown reason.
    #[error("daemon failed to start")]
    DaemonFailed,
    /// Daemon failed to start.
    ///
    /// The daemon was not running so it was spawned but failed to produce initial state.
    /// This is a result of the `init` argument to [`with_daemon`] producing an error.
    ///
    /// The error from `init` is stored here as a string.
    #[error("daemon failed to start")]
    StateFailed(String),
    /// Could not connect to a running daemon.
    ///
    /// An I/O error occured.
    #[error("could not connect to daemon: {0}")]
    ConnectionError(std::io::Error),
}

/// Implementation of the child process.
///
/// This process either spawns the daemon or assumes it is already spawned on [`daemonize`] error.
/// If the daemon has already been running, [`DaemonReadyToken::Running`] is sent down the socket
/// pair. Otherwise, one side of the socket is inherited by the forked daemon which sends one of
/// the other possible variants back to the spawning client.
fn do_child<S, SE, I, IFut, H, HFut>(
    pid_filename: &str,
    socket_filename: &str,
    parent: UnixStream,
    init: I,
    handler: H,
) -> Result<(), ()>
where
    I: FnOnce(DaemonControl) -> IFut + Send + 'static,
    IFut: Future<Output = Result<S, SE>> + Send,
    S: Send + Sync + 'static,
    SE: Send + std::fmt::Debug + Display + 'static,
    H: Fn(Arc<S>, TokioUnixStream) -> HFut + Send + 'static,
    HFut: Future<Output = ()> + Send + 'static,
{
    let daemonize = Daemonize::new()
        .pid_file(pid_filename)
        .stderr(Stdio::keep())
        .stdout(Stdio::keep());
    parent
        .set_nonblocking(true)
        .map_err(|e| error!("could not set UnixStream nonblocking: {e}"))?;
    match daemonize.start() {
        Ok(_) => {
            info!("daemonized");
            run_daemon(socket_filename, init, parent, handler)
        }
        Err(e) => {
            info!("error daemonizing: {}, assuming daemon running", e);
            notify_daemon_running(parent)
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn notify_daemon_running(parent: UnixStream) -> Result<(), ()> {
    let parent = TokioUnixStream::from_std(parent)
        .map_err(|e| error!("error tokioing UnixStream to fork: {e}"))?;
    DaemonReadyToken::Running
        .write_to(parent)
        .await
        .map_err(|e| warn!("error writing to parent: {e}"))
}

impl DaemonControl {
    /// Request daemon shutdown.
    ///
    /// Calling this function asynchronously requests daemon shutdown. This will eventually
    /// cause the daemon process to exit.
    pub async fn shutdown(&self) {
        let _ = self.0.send(DaemonControlMessage::Shutdown).await;
    }
}

/// A message sent from a daemon handler to the daemon main loop.
enum DaemonControlMessage {
    /// Request daemon shutdown.
    ///
    /// Start rejecting further connections and wait until all clients disconnect.
    Shutdown,
}

/// Run the daemon indefinitely.
#[tokio::main]
async fn run_daemon<S, SE, I, IFut, H, HFut>(
    socket_filename: &str,
    init: I,
    parent: UnixStream,
    handler: H,
) -> Result<(), ()>
where
    I: FnOnce(DaemonControl) -> IFut + Send + 'static,
    IFut: Future<Output = Result<S, SE>> + Send,
    S: Send + Sync + 'static,
    SE: Send + std::fmt::Debug + Display + 'static,
    H: Fn(Arc<S>, TokioUnixStream) -> HFut + Send + 'static,
    HFut: Future<Output = ()> + Send + 'static,
{
    let parent = TokioUnixStream::from_std(parent)
        .map_err(|e| error!("error tokioing UnixStream to fork: {e}"))?;

    let (ready_tx, ready) = oneshot::channel();
    let ready_notifier = tokio::spawn(async move {
        let token = match ready.await {
            Ok(Ok(())) => DaemonReadyToken::Forked,
            Ok(Err(e)) => DaemonReadyToken::FailedState(e),
            Err(_) => DaemonReadyToken::Failed,
        };
        let _ = token
            .write_to(parent)
            .await
            .inspect_err(|e| warn!("error writing to fork parent: {e}"));
    });

    let socket_filename = socket_filename.to_owned();
    if fs::try_exists(&socket_filename)
        .await
        .map_err(|e| error!("could not use file {socket_filename} as socket: {e}"))?
    {
        debug!("attempting to remove old socket file");
        fs::remove_file(&socket_filename)
            .await
            .map_err(|e| error!("could not remove old socket file: {e}"))?;
    }
    let listener =
        UnixListener::bind(socket_filename).map_err(|e| error!("error creating socket: {e}"))?;

    let (sender, mut control_receiver) = mpsc::channel(1);
    let ctrl = DaemonControl(sender);
    let init_res = init(ctrl).await;
    let stringified = init_res.as_ref().map(|_| ()).map_err(|e| e.to_string());
    ready_tx
        .send(stringified)
        .expect("receiver should not be dropped");
    debug!("notified socket ready");

    let state = init_res.map_err(|e| error!("could not produce initial state: {e}"))?;
    let state = Arc::new(state);

    debug!("started main loop");
    let done = wait_token::Waiter::new();
    loop {
        match select! { biased;
            Some(DaemonControlMessage::Shutdown) = control_receiver.recv() => break,
            a = listener.accept() => a,
        } {
            Ok((socket, addr)) => {
                info!("accepted from {addr:?}, spawning handler");
                let state = Arc::clone(&state);
                let token = done.token();
                let h = handler(state, socket);
                tokio::spawn(async move {
                    h.await;
                    drop(token);
                });
            }
            Err(e) => warn!("accept() failed: {:?}", e),
        }
    }
    done.wait().await;
    info!("handler-requested shutdown");
    ready_notifier.await.expect("notifier should not panic");
    Ok(())
}

/// Run the client for as long as needed
#[tokio::main]
async fn run_client<R, C, CFut>(
    socket_filename: &str,
    client: C,
    parent: UnixStream,
) -> Result<R, Error>
where
    C: FnOnce(TokioUnixStream) -> CFut,
    CFut: Future<Output = R>,
{
    let parent = TokioUnixStream::from_std(parent)
        .inspect_err(|e| error!("error tokioing UnixStream to fork: {e}"))
        .map_err(|_| Error::DaemonStatusUnknown)?;
    let ready = DaemonReadyToken::read_from(parent)
        .await
        .inspect_err(|e| error!("error reading from fork parent: {e}"))
        .map_err(|_| Error::DaemonStatusUnknown)?;
    let stream = connect_to_daemon(ready, socket_filename).await?;
    info!("parent ready, starting client");
    Ok(client(stream).await)
}

async fn connect_to_daemon(
    mut ready: DaemonReadyToken,
    socket_filename: &str,
) -> Result<TokioUnixStream, Error> {
    if ready == DaemonReadyToken::Failed {
        Err(Error::DaemonFailed)?
    }
    if let DaemonReadyToken::FailedState(ref mut e) = ready {
        Err(Error::StateFailed(std::mem::take(e)))?
    }
    match TokioUnixStream::connect(socket_filename).await {
        Ok(stream) => Ok(stream),
        Err(e) => match ready {
            DaemonReadyToken::Forked => Err(Error::ConnectionError(e)),
            DaemonReadyToken::Running => {
                info!("daemon running, but not ready: {e}, retrying");
                tokio::time::sleep(DAEMON_CONNECTION_RETRY_DELAY).await;
                TokioUnixStream::connect(socket_filename)
                    .await
                    .map_err(Error::ConnectionError)
            }
            _ => panic!("daemon cannot have failed at this point"),
        },
    }
}

/// A type to send through a socket between fork()'s parent and child to inform the parent about
/// the status of the daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
enum DaemonReadyToken {
    /// Daemon has just been forked by the child and is now ready to accept connections.
    Forked,
    /// Daemon has already been running and it is not known if it is ready to accept connections
    /// (but very likely it is).
    Running,
    /// Daemon could not be run because of an internal error.
    Failed,
    /// Daemon could not be run because of a failure to produce a state.
    ///
    /// The `init` argument of [`with_daemon`] failed to produce a valid state.
    FailedState(String),
}

impl DaemonReadyToken {
    /// Write the token to writer and shut it down.
    async fn write_to(self, mut writer: impl AsyncWriteExt + Unpin) -> tokio::io::Result<()> {
        let (hdr, data) = match self {
            DaemonReadyToken::Forked => (0x4ea11e55, vec![]),
            DaemonReadyToken::Running => (0x4ea1ab1e, vec![]),
            DaemonReadyToken::Failed => (0x5000dead, vec![]),
            DaemonReadyToken::FailedState(e) => (0x00051a1e, e.into_bytes()),
        };
        writer.write_u32(hdr).await?;
        writer.write_all_buf(&mut Cursor::new(data)).await
    }

    /// Read a token from reader until EOF.
    async fn read_from(mut reader: impl AsyncReadExt + Unpin) -> tokio::io::Result<Self> {
        let hdr = reader.read_u32().await?;
        Ok(match hdr {
            0x4ea11e55 => DaemonReadyToken::Forked,
            0x4ea1ab1e => DaemonReadyToken::Running,
            0x5000dead => DaemonReadyToken::Failed,
            0x00051a1e => DaemonReadyToken::FailedState({
                let mut buf = vec![];
                reader.read_to_end(&mut buf).await?;
                String::from_utf8(buf).expect("should decode as utf8")
            }),
            _ => panic!("expected one of the valid headers"),
        })
    }
}

const DAEMON_CONNECTION_RETRY_DELAY: Duration = Duration::from_millis(10);
