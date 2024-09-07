use std::{future::Future, io::Write, os::unix::net::UnixStream, sync::Arc, time::Duration};

use daemonize::Daemonize;
use fork::Fork;
use futures::{
    future::{self, Either},
    never::Never,
};
use log::{debug, error, info};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::{UnixListener, UnixStream as TokioUnixStream},
    sync::oneshot,
};

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
/// The common state is produced by awaiting the `init` future in the daemon process and remains in
/// existence throughout the life of the daemon process.
///
/// The daemon process is spawned when necessary (i.e. when it is not detected as running) and
/// is kept running forever for further clients to connect to it and take advantage of its state.
///
/// # Arguments
///
/// * `pid_filename` - the name of the PID file
/// * `socket_filename` - the name of the UNIX socket file for communication with the daemon
/// * `init` - a future which resolves to the state shared between client handlers on the daemon
///   side
/// * `handler` - a function spawned for each client that connects to the daemon; it can use the
///   shared state and communicate with its client using a stream
/// * `client` - a function that implements the daemon's client instance by communicating with the
///   daemon
pub fn with_daemon<S, IFut, H, HFut, R, C, CFut>(
    pid_filename: &str,
    socket_filename: &str,
    init: IFut,
    handler: H,
    client: C,
) -> Result<Option<R>, String>
where
    IFut: Future<Output = S> + Send + 'static,
    S: Send + Sync + 'static,
    H: Fn(Arc<S>, TokioUnixStream) -> HFut + Send + 'static,
    HFut: Future<Output = ()> + Send + 'static,
    C: FnOnce(TokioUnixStream) -> CFut,
    CFut: Future<Output = R>,
{
    let (mut stream_child, stream_parent) =
        UnixStream::pair().map_err(|e| format!("could not create UnixStream pair: {e}"))?;
    match fork::fork() {
        Ok(Fork::Child) => {
            info!("child process");
            drop(stream_parent);
            let daemonize = Daemonize::new().pid_file(pid_filename);
            match daemonize.start() {
                Ok(_) => {
                    info!("daemonized");
                    stream_child
                        .set_nonblocking(true)
                        .map_err(|e| format!("could not set UnixStream nonblocking: {e}"))?;
                    run_daemon(socket_filename, init, stream_child, handler)?;
                }
                Err(e) => {
                    stream_child
                        .write_all(&(ReadyToken::DaemonRunning as u32).to_be_bytes())
                        .map_err(|e| format!("error writing to parent: {e}"))?;
                    debug!("error daemonizing: {}, assuming daemon running", e);
                }
            }
            Ok(None)
        }
        Ok(Fork::Parent(_)) => {
            drop(stream_child);
            stream_parent
                .set_nonblocking(true)
                .map_err(|e| format!("could not set UnixStream nonblocking: {e}"))?;
            run_client(socket_filename, client, stream_parent).map(Some)
        }
        Err(_) => {
            error!("couldn't fork");
            Err("error fork()ing".to_owned())
        }
    }
}

/// Run the daemon indefinitely.
#[tokio::main]
async fn run_daemon<S, IFut, H, HFut>(
    socket_filename: &str,
    init: IFut,
    child: UnixStream,
    handler: H,
) -> Result<Never, String>
where
    IFut: Future<Output = S> + Send + 'static,
    S: Send + Sync + 'static,
    H: Fn(Arc<S>, TokioUnixStream) -> HFut + Send + 'static,
    HFut: Future<Output = ()> + Send + 'static,
{
    let mut child = TokioUnixStream::from_std(child)
        .map_err(|e| format!("error tokioing UnixStream to fork: {e}"))?;

    let (ready_tx, ready) = oneshot::channel();
    let ready_notifier = tokio::spawn(async move {
        let token = if let Ok(()) = ready.await {
            ReadyToken::DaemonForked
        } else {
            ReadyToken::DeamonFailed
        };
        child
            .write_u32(token as u32)
            .await
            .map_err(|e| format!("error writing to fork parent: {e}"))
    });

    let socket_filename = socket_filename.to_owned();
    let daemon = tokio::spawn(async move {
        tokio::fs::remove_file(&socket_filename)
            .await
            .map_err(|e| format!("could not remove old socket file: {e}"))?;
        let listener = UnixListener::bind(socket_filename)
            .map_err(|e| format!("error creating socket: {e}"))?;
        ready_tx.send(()).expect("receiver should not be dropped");

        let state = Arc::new(init.await);
        loop {
            match listener.accept().await {
                Ok((socket, addr)) => {
                    info!("accepted from {addr:?}");
                    let state = Arc::clone(&state);
                    tokio::spawn(handler(state, socket));
                }
                Err(e) => error!("accept function failed: {:?}", e),
            }
        }
    });

    match future::select(ready_notifier, daemon).await {
        Either::Left((notifier, daemon)) => {
            notifier.expect("notifier task should not panic")?;
            daemon.await.expect("daemon task should not panic")
        }
        Either::Right((daemon, _)) => daemon.expect("daemon task should not panic"),
    }
}

#[tokio::main]
async fn run_client<R, C, CFut>(
    socket_filename: &str,
    client: C,
    parent: UnixStream,
) -> Result<R, String>
where
    C: FnOnce(TokioUnixStream) -> CFut,
    CFut: Future<Output = R>,
{
    let mut parent = TokioUnixStream::from_std(parent)
        .map_err(|e| format!("error tokioing UnixStream to fork: {e}"))?;
    let ready = parent
        .read_u32()
        .await
        .map_err(|e| format!("error reading from fork parent: {e}"))?;
    let ready: ReadyToken =
        num::FromPrimitive::from_u32(ready).expect("ready token should have known value");
    let stream = connect_to_daemon(ready, socket_filename).await?;
    info!("parent ready, {:?}, starting client", ready);
    Ok(client(stream).await)
}

async fn connect_to_daemon(
    ready: ReadyToken,
    socket_filename: &str,
) -> Result<TokioUnixStream, String> {
    if ready == ReadyToken::DeamonFailed {
        Err("daemon failed to start".to_string())?
    }
    match TokioUnixStream::connect(socket_filename).await {
        Ok(stream) => Ok(stream),
        Err(e) => match ready {
            ReadyToken::DaemonForked => Err(format!(
                "could not communicate with just spawned daemon: {e}"
            )),
            ReadyToken::DaemonRunning => {
                info!("daemon running, but not ready, retrying");
                tokio::time::sleep(DAEMON_CONNECTION_RETRY_DELAY).await;
                TokioUnixStream::connect(socket_filename)
                    .await
                    .map_err(|e| format!("could not communicate with daemon after retry: {e}"))
            }
            ReadyToken::DeamonFailed => panic!("deamon cannot have failed at this point"),
        },
    }
}

/// A type to send through a socket between fork()'s parent and child to inform the parent about
/// the status of the daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, num_derive::FromPrimitive)]
enum ReadyToken {
    /// Daemon has just been forked by the child and is now ready to accept connections.
    DaemonForked = 0x4ea11e55,
    /// Daemon has already been running and it is not known if it is ready to accept connections
    /// (but very likely it is).
    DaemonRunning = 0x4ea1ab1e,
    /// Deamon could not be run.
    DeamonFailed = 0x5000dead,
}

const DAEMON_CONNECTION_RETRY_DELAY: Duration = Duration::from_millis(10);
