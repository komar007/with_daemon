use std::{process::ExitCode, time::Duration};

use clap::Parser as _;
use framework::with_daemon;
use log::error;

use config::Config;
use worker::Worker;

mod client;
mod config;
mod framework;
mod worker;

const UPDATE_INTERVAL: Duration = Duration::from_millis(1000);
const SOCKET_FILENAME: &str = "/tmp/pidtree_mon.sock";
const PID_FILENAME: &str = "/tmp/pidtree_mon.pid";

fn main() -> ExitCode {
    match entrypoint() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!("{e}");
            println!("{e}");
            ExitCode::FAILURE
        }
    }
}

fn entrypoint() -> Result<(), String> {
    env_logger::init();
    let config = Config::parse();
    let framework_res = with_daemon(
        PID_FILENAME,
        SOCKET_FILENAME,
        Worker::new(UPDATE_INTERVAL),
        Worker::handle_client,
        |stream| client::run(stream, config.pids, config.timeout, config.fields),
    );
    let client_res = framework_res.map_err(|e| format!("framework: {e}"))?;
    if let Some(res) = client_res {
        res.map_err(|e| format!("client: {e}"))
    } else {
        Ok(())
    }
}
