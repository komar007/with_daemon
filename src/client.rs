use std::{fmt::Display, time::Duration};

use futures::{stream::unfold, StreamExt as _};
use itertools::Itertools as _;
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::UnixStream as TokioUnixStream,
    pin,
    time::Instant,
};

use crate::config::Field;

/// Run the client for as long as configured.
pub async fn run(
    mut stream: TokioUnixStream,
    pids: Vec<i32>,
    timeout: Option<Duration>,
    fields: Vec<Field>,
) -> Result<(), String> {
    for pid in &pids {
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
    .chunks(pids.len());
    pin!(loads_stream);
    let deadline = timeout.map(|tmout| Instant::now() + tmout);
    while let Some(loads) = loads_stream.next().await {
        println!("{}", OutputLine(&fields, loads));
        if deadline.is_some_and(|d| Instant::now() > d) {
            break;
        }
    }
    Ok(())
}

struct OutputLine<'f>(&'f Vec<Field>, Vec<f32>);

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
                Field::Sum => write!(f, "{sum}")?,
                Field::AllLoads => write!(f, "{}", loads.iter().join(" "))?,
                Field::IfGreater {
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
