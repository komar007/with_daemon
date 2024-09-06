use std::{fmt::Display, time::Duration};

use futures::{stream::unfold, StreamExt as _};
use itertools::Itertools as _;
use log::info;
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::UnixStream as TokioUnixStream,
    pin,
    time::Instant,
};

use crate::{config, protocol::ReadyToken};

/// Client'a configuration
pub struct Config {
    /// Base application config
    pub app: config::Config,
    /// Name of the UNIX socket for daemon-client communication
    pub socket_filename: String,
}

/// Run the client for as long as configured.
pub async fn run(config: Config, ready: ReadyToken) -> Result<(), Option<String>> {
    let mut stream = connect_to_daemon(ready, &config.socket_filename).await?;
    for pid in &config.app.pids {
        stream
            .write_i32(*pid)
            .await
            .map_err(|e| format!("error writing to server: {e}"))?;
    }
    stream
        .flush()
        .await
        .map_err(|e| format!("error flushing stream: {e}"))?;
    stream
        .shutdown()
        .await
        .map_err(|e| format!("error shutting down stream: {e}"))?;
    let loads_stream = unfold(stream, |mut stream| async {
        stream.read_f32().await.ok().map(|load| (load, stream))
    })
    .chunks(config.app.pids.len());
    pin!(loads_stream);
    let deadline = config.app.timeout.map(|tmout| Instant::now() + tmout);
    while let Some(loads) = loads_stream.next().await {
        println!("{}", OutputLine(&config.app.fields, loads));
        if deadline.is_some_and(|d| Instant::now() > d) {
            break;
        }
    }
    Ok(())
}

struct OutputLine<'f>(&'f Vec<config::Field>, Vec<f32>);

impl<'f> Display for OutputLine<'f> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let OutputLine(spec, loads) = self;
        let sum: f32 = loads
            .iter()
            .map(|l| if l.is_nan() { 0.0 } else { *l })
            .sum();
        let mut any_written = false;
        for field in spec.iter() {
            if any_written {
                write!(f, " ")?;
            }
            match field {
                config::Field::Sum => write!(f, "{sum}")?,
                config::Field::AllLoads => write!(f, "{}", loads.iter().join(" "))?,
                config::Field::IfGreater {
                    value,
                    then,
                    otherwise,
                } => {
                    if sum > *value {
                        write!(f, "{}", then)?
                    } else if let Some(o) = otherwise {
                        write!(f, "{}", o)?
                    }
                }
            }
            any_written = true;
        }
        Ok(())
    }
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
                tokio::time::sleep(DAEMON_RETRY_DELAY).await;
                TokioUnixStream::connect(socket_filename)
                    .await
                    .map_err(|e| format!("could not communicate with daemon after retry: {e}"))
            }
            ReadyToken::DeamonFailed => panic!("deamon cannot have failed at this point"),
        },
    }
}

const DAEMON_RETRY_DELAY: Duration = Duration::from_millis(10);
