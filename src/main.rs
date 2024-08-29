use std::{
    collections::HashMap, env::args, hash::Hash, io::Write, ops::Add, os::unix::net::UnixStream,
    sync::Arc, time::Duration,
};

use daemonize::Daemonize;
use fork::Fork;
use futures::{stream::unfold, StreamExt};
use itertools::Itertools;
use log::{debug, error, info};
use procfs::{CurrentSI, KernelStats};
use tokio::{
    fs,
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, BufStream, BufWriter},
    net::{UnixListener, UnixStream as TokioUnixStream},
    pin, stream,
    sync::RwLock,
};

#[macro_use]
extern crate num_derive;

const UPDATE_INTERVAL_MS: u64 = 100;
const DAEMON_RETRY_DELAY: u64 = 10;
const SOCKET_FILENAME: &str = "/tmp/fire.sock";
const PID_FILENAME: &str = "/tmp/fire.pid";

fn main() -> Result<(), ()> {
    env_logger::init();
    let (mut stream_child, stream_parent) =
        UnixStream::pair().map_err(|e| error!("could not create UnixStream pair: {e}"))?;
    match fork::fork() {
        Ok(Fork::Child) => {
            info!("child process");
            drop(stream_parent);
            let daemonize = Daemonize::new().pid_file(PID_FILENAME);
            match daemonize.start() {
                Ok(_) => {
                    info!("daemonized");
                    run_daemon(stream_child).map_err(|e| error!("{}", e))
                }
                Err(e) => {
                    stream_child
                        .write_all(&(ReadyToken::DaemonRunning as u32).to_be_bytes())
                        .map_err(|e| error!("error writing to parent: {e}"))?;
                    debug!("error deamonizing: {}, assuming daemon running", e);
                    Ok(())
                }
            }
        }
        Ok(Fork::Parent(_)) => {
            drop(stream_child);
            run_client(stream_parent).map_err(|e| {
                e.inspect(|e| error!("{}", e));
            })
        }
        Err(_) => {
            error!("couldn't fork");
            Err(())
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
        num::FromPrimitive::from_u32(ready).expect("expected known value of ready token");
    info!("parent ready, {:?}, starting client", ready);
    let mut stream = connect_to_daemon(ready).await;
    let pids: Result<Vec<i32>, _> = args().skip(1).map(|a| a.parse()).collect();
    let pids = pids.map_err(|_| {
        print_usage();
        None
    })?;
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
    while let Some(loads) = loads_stream.next().await {
        let sum: f32 = loads
            .iter()
            .map(|l| if l.is_nan() { 0.0 } else { *l })
            .sum();
        println!("{} {}", sum, loads.iter().format(" "))
    }
    Ok(())
}

async fn connect_to_daemon(ready: ReadyToken) -> TokioUnixStream {
    let Ok(stream) = TokioUnixStream::connect(SOCKET_FILENAME).await else {
        match ready {
            ReadyToken::DaemonForked => panic!("forked daemon misbehaving"),
            ReadyToken::DaemonRunning => {
                info!("daemon running, but not ready, retrying");
                tokio::time::sleep(Duration::from_millis(DAEMON_RETRY_DELAY)).await;
                let Ok(stream) = TokioUnixStream::connect(SOCKET_FILENAME).await else {
                    panic!("retried connecting to daemon and failed, blood everywhere");
                };
                return stream;
            }
        }
    };
    stream
}

fn print_usage() {
    let name = args().next().expect("args should have program name");
    println!("usage:\n\t{name} pid [..pids]")
}

#[derive(Debug, FromPrimitive)]
enum ReadyToken {
    DaemonForked = 0x4ea11e55,
    DaemonRunning = 0x4ea1ab1e,
}

#[tokio::main]
async fn run_daemon(other_fork: UnixStream) -> Result<(), String> {
    other_fork
        .set_nonblocking(true)
        .map_err(|e| format!("could not set UnixStream nonblocking: {e}"))?;
    let mut other_fork = TokioUnixStream::from_std(other_fork)
        .map_err(|e| format!("error tokioing UnixStream to fork: {e}"))?;
    let _ = fs::remove_file(SOCKET_FILENAME).await;
    let listener = UnixListener::bind(SOCKET_FILENAME).expect("should create socket");

    other_fork
        .write_u32(ReadyToken::DaemonForked as u32)
        .await
        .map_err(|e| format!("error reading from fork parent: {e}"))?;

    let loads: HashMap<i32, f32> = HashMap::new();
    let loads = Arc::new(RwLock::new(loads));
    tokio::spawn(scrape_pid_loads(loads.clone()));
    loop {
        match listener.accept().await {
            Ok((socket, addr)) => {
                info!("accepted from {addr:?}");
                tokio::spawn(serve_client(socket, loads.clone()));
            }
            Err(e) => error!("accept function failed: {:?}", e),
        }
    }
}

async fn serve_client(mut socket: TokioUnixStream, loads: Arc<RwLock<HashMap<i32, f32>>>) {
    let (reader, writer) = socket.split();
    let reader = BufReader::new(reader);
    let mut writer = BufWriter::new(writer);
    let pids: Vec<_> = unfold(reader, |mut reader| async {
        reader.read_i32().await.ok().map(|pid| (pid, reader))
    })
    .collect()
    .await;
    'infinite: loop {
        let pid_loads: Vec<_> = {
            let loads = loads.read().await;
            pids.iter()
                .map(|pid| *loads.get(pid).unwrap_or(&f32::NAN))
                .collect()
        };
        for pid in pid_loads {
            if let Err(e) = writer.write_f32(pid).await {
                error!("error writing response: {e}");
                break 'infinite;
            }
        }
        if let Err(e) = writer.flush().await {
            error!("error flushing stream: {e}");
            break;
        }
        tokio::time::sleep(Duration::from_millis(UPDATE_INTERVAL_MS)).await;
    }
    if let Err(e) = socket.shutdown().await {
        error!("error shutting down: {e}");
    }
}

async fn scrape_pid_loads(out_loads: Arc<RwLock<HashMap<i32, f32>>>) {
    let mut ticks = HashMap::new();
    loop {
        let mut pid_children: HashMap<_, Vec<_>> = Default::default();
        let mut loads = HashMap::new();
        for prc in procfs::process::all_processes().expect("can't read /proc") {
            let Ok(prc) = prc else { continue };
            if let Ok(stat) = prc.stat() {
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
        }
        *out_loads.write().await = get_cumulated(&pid_children, &loads);
        tokio::time::sleep(Duration::from_millis(UPDATE_INTERVAL_MS)).await;
    }
}

fn get_cur_ticks() -> u64 {
    let time = KernelStats::current().expect("can't read /proc");
    time.total.user + time.total.system + time.total.idle
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
