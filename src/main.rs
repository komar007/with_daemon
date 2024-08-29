use std::{
    collections::HashMap, env::args, hash::Hash, io::Write, ops::Add, os::unix::net::UnixStream,
    sync::Arc, time::Duration,
};

use daemonize::Daemonize;
use fork::Fork;
use log::{debug, error, info};
use procfs::{CurrentSI, KernelStats};
use tokio::{
    fs,
    io::{AsyncReadExt, AsyncWriteExt, BufStream},
    net::{UnixListener, UnixStream as TokioUnixStream},
    sync::RwLock,
};

#[macro_use]
extern crate num_derive;

const UPDATE_INTERVAL_MS: u64 = 100;
const DAEMON_RETRY_DELAY: u64 = 10;
const SOCKET_FILENAME: &str = "/tmp/fire.sock";
const PID_FILENAME: &str = "/tmp/fire.pid";

fn main() -> std::io::Result<()> {
    env_logger::init();
    let (mut stream_child, stream_parent) = UnixStream::pair()?;
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
                        .inspect_err(|e| error!("error writing to parent: {e}"))?;
                    debug!("error deamonizing: {}, assuming daemon running", e);
                }
            };
        }
        Ok(Fork::Parent(_)) => {
            drop(stream_child);
            run_client(stream_parent)?;
        }
        Err(_) => {
            error!("couldn't fork");
        }
    }
    Ok(())
}

#[tokio::main]
async fn run_client(other_fork: UnixStream) -> std::io::Result<()> {
    other_fork.set_nonblocking(true)?;
    let mut other_fork = TokioUnixStream::from_std(other_fork)
        .inspect_err(|e| error!("error tokioing UnixStream to fork: {e}"))?;
    let ready = other_fork
        .read_u32()
        .await
        .inspect_err(|e| error!("error reading from fork parent: {e}"))?;
    let ready: ReadyToken =
        num::FromPrimitive::from_u32(ready).expect("expected known value of ready token");
    info!("parent ready, {:?}, starting client", ready);
    let mut stream = connect_to_daemon(ready).await;
    let pid: i32 = args().nth(1).unwrap().parse().unwrap();
    stream
        .write_i32(pid)
        .await
        .inspect_err(|e| error!("error writing to server: {e}"))?;
    stream
        .flush()
        .await
        .inspect_err(|e| error!("error flushing stream: {e}"))?;
    while let Ok(resp) = stream.read_f32().await {
        println!("{resp}");
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

#[derive(Debug, FromPrimitive)]
enum ReadyToken {
    DaemonForked = 0x4ea11e55,
    DaemonRunning = 0x4ea1ab1e,
}

#[tokio::main]
async fn run_daemon(other_fork: UnixStream) -> std::io::Result<()> {
    other_fork.set_nonblocking(true)?;
    let mut other_fork = TokioUnixStream::from_std(other_fork)
        .inspect_err(|e| error!("error tokioing UnixStream to fork: {e}"))?;
    let _ = fs::remove_file(SOCKET_FILENAME).await;
    let listener = UnixListener::bind(SOCKET_FILENAME).expect("should create socket");

    other_fork
        .write_u32(ReadyToken::DaemonForked as u32)
        .await
        .inspect_err(|e| error!("error reading from fork parent: {e}"))?;

    let loads: HashMap<i32, f32> = HashMap::new();
    let loads = Arc::new(RwLock::new(loads));
    tokio::spawn(scrape_pid_loads(loads.clone()));
    loop {
        match listener.accept().await {
            Ok((socket, addr)) => {
                info!("accepted from {addr:?}");
                let loads = loads.clone();
                tokio::spawn(async move {
                    let mut stream = BufStream::new(socket);
                    let Ok(pid) = stream.read_i32().await else {
                        error!("error reading request");
                        return;
                    };
                    loop {
                        let response = loads.read().await.get(&pid).copied().unwrap_or(-1.0);
                        if let Err(e) = stream.write_f32(response).await {
                            error!("error writing response: {e}");
                            break;
                        }
                        if let Err(e) = stream.flush().await {
                            error!("error flushing stream: {e}");
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(UPDATE_INTERVAL_MS)).await;
                    }
                    if let Err(e) = stream.shutdown().await {
                        error!("error shutting down: {e}");
                    }
                });
            }
            Err(e) => error!("accept function failed: {:?}", e),
        }
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
