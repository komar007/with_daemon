use std::{io::Write, os::unix::net::UnixStream, process::ExitCode, time::Duration};

use clap::Parser as _;
use config::Config;
use daemonize::Daemonize;
use fork::Fork;
use log::{debug, error, info};

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
                    daemon::run(stream_child, SOCKET_FILENAME, UPDATE_INTERVAL)
                }
                Err(e) => {
                    stream_child
                        .write_all(&(ReadyToken::DaemonRunning as u32).to_be_bytes())
                        .map_err(|e| format!("error writing to parent: {e}"))?;
                    debug!("error deamonizing: {}, assuming daemon running", e);
                    Ok(())
                }
            }
        }
        Ok(Fork::Parent(_)) => {
            drop(stream_child);
            let cconfig = client::Config {
                app: config,
                socket_filename: SOCKET_FILENAME.into(),
            };
            client::run(cconfig, stream_parent)
        }
        Err(_) => {
            error!("couldn't fork");
            Err(None)
        }
    }
}
