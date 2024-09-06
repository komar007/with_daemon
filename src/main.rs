use std::{io::Write, os::unix::net::UnixStream, process::ExitCode, time::Duration};

use clap::Parser as _;
use config::Config;
use daemonize::Daemonize;
use fork::Fork;
use futures::{
    future::{self, Either},
    never::Never,
};
use log::{debug, error, info};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::UnixStream as TokioUnixStream,
    sync::oneshot,
};

use crate::protocol::ReadyToken;

mod client;
mod config;
mod daemon;
mod protocol;

const UPDATE_INTERVAL: Duration = Duration::from_millis(1000);
const SOCKET_FILENAME: &str = "/tmp/pidtree_mon.sock";
const PID_FILENAME: &str = "/tmp/pidtree_mon.pid";

fn main() -> ExitCode {
    match entrypoint() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            if let Some(e) = e {
                error!("{e}");
                println!("{e}");
            }
            ExitCode::FAILURE
        }
    }
}

fn entrypoint() -> Result<(), Option<String>> {
    env_logger::init();
    let config = Config::parse();
    let (mut stream_child, stream_parent) =
        UnixStream::pair().map_err(|e| format!("could not create UnixStream pair: {e}"))?;
    match fork::fork() {
        Ok(Fork::Child) => {
            info!("child process");
            drop(stream_parent);
            let daemonize = Daemonize::new().pid_file(PID_FILENAME);
            match daemonize.start() {
                Ok(_) => {
                    info!("daemonized");
                    run_daemon(stream_child)?;
                }
                Err(e) => {
                    stream_child
                        .write_all(&(ReadyToken::DaemonRunning as u32).to_be_bytes())
                        .map_err(|e| format!("error writing to parent: {e}"))?;
                    debug!("error deamonizing: {}, assuming daemon running", e);
                }
            }
            Ok(())
        }
        Ok(Fork::Parent(_)) => {
            drop(stream_child);
            let cconfig = client::Config {
                app: config,
                socket_filename: SOCKET_FILENAME.into(),
            };
            run_client(cconfig, stream_parent)
        }
        Err(_) => {
            error!("couldn't fork");
            Err(None)
        }
    }
}

#[tokio::main]
async fn run_daemon(child_stream: UnixStream) -> Result<Never, Option<String>> {
    child_stream
        .set_nonblocking(true)
        .map_err(|e| format!("could not set UnixStream nonblocking: {e}"))?;
    let mut child_stream = TokioUnixStream::from_std(child_stream)
        .map_err(|e| format!("error tokioing UnixStream to fork: {e}"))?;

    let (ready_tx, ready) = oneshot::channel();
    let ready_notifier = tokio::spawn(async move {
        let token = if let Ok(()) = ready.await {
            ReadyToken::DaemonForked
        } else {
            ReadyToken::DeamonFailed
        };
        child_stream
            .write_u32(token as u32)
            .await
            .map_err(|e| Some(format!("error writing to fork parent: {e}")))
    });

    let daemon = tokio::spawn(daemon::run(ready_tx, SOCKET_FILENAME, UPDATE_INTERVAL));

    match future::select(ready_notifier, daemon).await {
        Either::Left((notifier, daemon)) => {
            notifier.expect("notifier task should not panic")?;
            daemon.await.expect("daemon task should not panic")
        }
        Either::Right((daemon, _)) => daemon.expect("daemon task should not panic"),
    }
}

#[tokio::main]
async fn run_client(
    config: client::Config,
    parent_stream: UnixStream,
) -> Result<(), Option<String>> {
    parent_stream
        .set_nonblocking(true)
        .map_err(|e| format!("could not set UnixStream nonblocking: {e}"))?;
    let mut parent_stream = TokioUnixStream::from_std(parent_stream)
        .map_err(|e| format!("error tokioing UnixStream to fork: {e}"))?;
    let ready = parent_stream
        .read_u32()
        .await
        .map_err(|e| format!("error reading from fork parent: {e}"))?;
    let ready: ReadyToken =
        num::FromPrimitive::from_u32(ready).expect("ready token should have known value");
    info!("parent ready, {:?}, starting client", ready);
    client::run(config, ready).await
}
