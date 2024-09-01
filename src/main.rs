use std::{
    collections::HashMap, hash::Hash, io::Write, ops::Add, os::unix::net::UnixStream,
    process::ExitCode, sync::Arc, time::Duration,
};

use clap::Parser;

use daemonize::Daemonize;
use fork::Fork;
use futures::{stream::unfold, StreamExt};
use itertools::Itertools;
use log::{debug, error, info};
use procfs::{CurrentSI, KernelStats};
use tokio::{
    fs,
    io::{AsyncReadExt, AsyncWriteExt, BufReader, BufWriter},
    net::{UnixListener, UnixStream as TokioUnixStream},
    pin,
    sync::broadcast::{self, Sender},
    time::{sleep, sleep_until, Instant},
};

#[macro_use]
extern crate num_derive;

const UPDATE_INTERVAL: Duration = Duration::from_millis(1000);
const DAEMON_RETRY_DELAY: Duration = Duration::from_millis(10);
const MEASURE_PERIOD: Duration = Duration::from_millis(100);
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
                    run_daemon(stream_child)
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
            run_client(stream_parent)
        }
        Err(_) => {
            error!("couldn't fork");
            Err(None)
        }
    }
}

#[tokio::main]
async fn run_client(other_fork: UnixStream) -> Result<(), Option<String>> {
    other_fork
        .set_nonblocking(true)
        .map_err(|e| format!("could not set UnixStream nonblocking: {e}"))?;
    let mut other_fork = TokioUnixStream::from_std(other_fork)
        .map_err(|e| format!("error tokioing UnixStream to fork: {e}"))?;
    let ready = other_fork
        .read_u32()
        .await
        .map_err(|e| format!("error reading from fork parent: {e}"))?;
    let ready: ReadyToken =
        num::FromPrimitive::from_u32(ready).expect("ready token should have known value");
    info!("parent ready, {:?}, starting client", ready);
    let mut stream = connect_to_daemon(ready).await?;
    let config = match Config::try_parse() {
        Ok(config) => config,
        Err(e) => {
            e.print().map_err(|e| format!("error printing help: {e}"))?;
            return Err(None);
        }
    };
    for pid in &config.pids {
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
    .chunks(config.pids.len());
    pin!(loads_stream);
    let deadline = config.timeout.map(|tmout| Instant::now() + tmout);
    while let Some(loads) = loads_stream.next().await {
        let sum: f32 = loads
            .iter()
            .map(|l| if l.is_nan() { 0.0 } else { *l })
            .sum();
        println!("{} {}", sum, loads.iter().format(" "));
        if deadline.is_some_and(|d| Instant::now() > d) {
            break;
        }
    }
    Ok(())
}

#[derive(clap::Parser, Debug)]
#[command(version, about, long_about = None)]
struct Config {
    #[arg(name = "pid", required = true, num_args = 1..)]
    pids: Vec<i32>,
    #[arg(short, long, value_parser = parse_timeout_duration)]
    timeout: Option<Duration>,
}

fn parse_timeout_duration(arg: &str) -> Result<std::time::Duration, std::num::ParseIntError> {
    let seconds = arg.parse()?;
    Ok(std::time::Duration::from_secs(seconds))
}

async fn connect_to_daemon(ready: ReadyToken) -> Result<TokioUnixStream, String> {
    match TokioUnixStream::connect(SOCKET_FILENAME).await {
        Ok(stream) => Ok(stream),
        Err(e) => match ready {
            ReadyToken::DaemonForked => Err(format!("could not communicate with daemon: {e}")),
            ReadyToken::DaemonRunning => {
                info!("daemon running, but not ready, retrying");
                sleep(DAEMON_RETRY_DELAY).await;
                TokioUnixStream::connect(SOCKET_FILENAME)
                    .await
                    .map_err(|e| format!("could not communicate with daemon after retry: {e}"))
            }
        },
    }
}

#[derive(Debug, FromPrimitive)]
enum ReadyToken {
    DaemonForked = 0x4ea11e55,
    DaemonRunning = 0x4ea1ab1e,
}

#[tokio::main]
async fn run_daemon(other_fork: UnixStream) -> Result<(), Option<String>> {
    other_fork
        .set_nonblocking(true)
        .map_err(|e| format!("could not set UnixStream nonblocking: {e}"))?;
    let mut other_fork = TokioUnixStream::from_std(other_fork)
        .map_err(|e| format!("error tokioing UnixStream to fork: {e}"))?;
    let _ = fs::remove_file(SOCKET_FILENAME).await;
    let listener =
        UnixListener::bind(SOCKET_FILENAME).map_err(|e| format!("error creating socket: {e}"))?;

    other_fork
        .write_u32(ReadyToken::DaemonForked as u32)
        .await
        .map_err(|e| format!("error reading from fork parent: {e}"))?;

    let (loads, _) = broadcast::channel(1);
    let sender = loads.clone();
    tokio::spawn(async move {
        loop {
            let next = Instant::now() + UPDATE_INTERVAL;
            measure_pid_loads(&sender).await;
            sleep_until(next).await;
        }
    });
    loop {
        match listener.accept().await {
            Ok((socket, addr)) => {
                info!("accepted from {addr:?}");
                tokio::spawn(serve_client(socket, loads.subscribe()));
            }
            Err(e) => error!("accept function failed: {:?}", e),
        }
    }
}

async fn serve_client(
    mut socket: TokioUnixStream,
    mut loads: broadcast::Receiver<Arc<HashMap<i32, f32>>>,
) {
    let (reader, writer) = socket.split();
    let reader = BufReader::new(reader);
    let mut writer = BufWriter::new(writer);
    let pids: Vec<_> = unfold(reader, |mut reader| async {
        reader.read_i32().await.ok().map(|pid| (pid, reader))
    })
    .collect()
    .await;
    'serving: loop {
        let pid_loads: Vec<_> = {
            let Ok(loads) = loads.recv().await else {
                continue 'serving;
            };
            pids.iter()
                .map(|pid| *loads.get(pid).unwrap_or(&f32::NAN))
                .collect()
        };
        for pid in pid_loads {
            if let Err(e) = writer.write_f32(pid).await {
                error!("error writing response: {e}");
                break 'serving;
            }
        }
        if let Err(e) = writer.flush().await {
            error!("error flushing stream: {e}");
            break 'serving;
        }
    }
    if let Err(e) = socket.shutdown().await {
        error!("error shutting down: {e}");
    }
}

async fn measure_pid_loads(out_loads: &Sender<Arc<HashMap<i32, f32>>>) {
    let mut ticks = HashMap::new();
    for _ in 0..2 {
        let mut pid_children: HashMap<_, Vec<_>> = Default::default();
        let mut loads = HashMap::new();
        for prc in procfs::process::all_processes().expect("can't read /proc") {
            let Ok(prc) = prc else { continue };
            let Ok(stat) = prc.stat() else {
                continue;
            };
            let cur_tics = get_cur_ticks();
            let cur_pid_ticks = stat.utime + stat.stime;
            let Some((last_ticks, last_pid_ticks)) =
                ticks.insert(stat.pid, (cur_tics, cur_pid_ticks))
            else {
                continue;
            };
            pid_children.entry(stat.ppid).or_default().push(stat.pid);
            let load = (cur_pid_ticks - last_pid_ticks) as f32 / (cur_tics - last_ticks) as f32;
            loads.insert(stat.pid, load);
        }
        if !loads.is_empty() {
            let _ = out_loads.send(get_cumulated(&pid_children, &loads).into());
        }
        sleep(MEASURE_PERIOD).await;
    }
}

fn get_cur_ticks() -> u64 {
    let time = KernelStats::current().expect("can't read /proc");
    (time.total.user + time.total.system + time.total.idle) / time.cpu_time.len() as u64
}

fn get_cumulated<Id, V>(children: &HashMap<Id, Vec<Id>>, values: &HashMap<Id, V>) -> HashMap<Id, V>
where
    Id: Copy + Eq + Hash,
    V: Copy + Add<V, Output = V> + std::iter::Sum,
{
    let mut cumulated_loads = HashMap::new();
    for node in values.keys() {
        cumulate(*node, children, values, &mut cumulated_loads);
    }
    cumulated_loads
}

fn cumulate<Id, V>(
    root: Id,
    children: &HashMap<Id, Vec<Id>>,
    values: &HashMap<Id, V>,
    cumulated: &mut HashMap<Id, V>,
) where
    Id: Copy + Eq + Hash,
    V: Copy + Add<V, Output = V> + std::iter::Sum,
{
    if cumulated.contains_key(&root) {
        return;
    }
    let total = values[&root]
        + children
            .get(&root)
            .unwrap_or(&vec![])
            .iter()
            .map(|c| {
                cumulate(*c, children, values, cumulated);
                cumulated[c]
            })
            .sum();
    cumulated.insert(root, total);
}
