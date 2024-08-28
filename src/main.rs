use std::{collections::HashMap, env::args, hash::Hash, ops::Add, sync::Arc, time::Duration};

use daemonize::Daemonize;
use fork::Fork;
use log::{debug, error, info};
use procfs::{CurrentSI, KernelStats};
use tokio::{
    fs,
    io::{AsyncReadExt, AsyncWriteExt, BufStream},
    net::{UnixListener, UnixStream},
    sync::RwLock,
};

fn main() {
    env_logger::init();
    match fork::fork() {
        Ok(Fork::Child) => {
            info!("child process");
            let daemonize = Daemonize::new().pid_file("/tmp/fire.pid");
            match daemonize.start() {
                Ok(_) => {
                    info!("daemonized");
                    run_daemon();
                }
                Err(e) => {
                    debug!("error deamonizing: {}, assuming daemon running", e);
                }
            };
        }
        Ok(Fork::Parent(_)) => {
            run_client();
        }
        Err(_) => {
            error!("couldn't fork");
        }
    }
}

#[tokio::main]
async fn run_client() {
    let mut stream = UnixStream::connect("/tmp/fire.sock").await.unwrap();
    let pid: i32 = args().nth(1).unwrap().parse().unwrap();
    if let Err(e) = stream.write_i32(pid).await {
        error!("error writing to server: {e}");
        return;
    }
    if let Err(e) = stream.flush().await {
        error!("error flushing stream: {e}");
        return;
    }
    while let Ok(resp) = stream.read_f32().await {
        println!("{resp}");
    }
}

#[tokio::main]
async fn run_daemon() {
    let _ = fs::remove_file("/tmp/fire.sock").await;
    let listener = UnixListener::bind("/tmp/fire.sock").expect("should create socket");

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
                        tokio::time::sleep(Duration::from_millis(1000)).await;
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
        tokio::time::sleep(Duration::from_millis(1000)).await;
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
