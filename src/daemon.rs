use std::{collections::HashMap, hash::Hash, ops::Add, sync::Arc, time::Duration};

use futures::{never::Never, stream::unfold, StreamExt};
use log::{error, info};
use procfs::{CurrentSI, KernelStats};
use tokio::{
    fs,
    io::{AsyncReadExt, AsyncWriteExt, BufReader, BufWriter},
    net::{UnixListener, UnixStream as TokioUnixStream},
    sync::{
        broadcast::{self},
        oneshot,
    },
    time::{sleep, sleep_until, Instant},
};

/// How long to wait between 2 measurements.
const MEASURE_PERIOD: Duration = Duration::from_millis(100);

/// Run the daemon indefinitely.
///
/// It is assumed that the process that spawns the daemon uses fork() and expects a trigger when
/// the daemon is ready to accept connections. The integer value of [ReadyToken::DaemonForked] will
/// be sent through `other_fork` when that happens.
pub async fn run(
    ready: oneshot::Sender<()>,
    socket_filename: &str,
    update_interval: Duration,
) -> Result<Never, Option<String>> {
    fs::remove_file(socket_filename)
        .await
        .map_err(|e| format!("could not remove old socket file: {e}"))?;
    let listener =
        UnixListener::bind(socket_filename).map_err(|e| format!("error creating socket: {e}"))?;
    ready.send(()).expect("receiver should not be dropped");

    let (loads, _) = broadcast::channel(1);
    let sender = loads.clone();
    tokio::spawn(async move {
        loop {
            let next = Instant::now() + update_interval;
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

async fn measure_pid_loads(out_loads: &broadcast::Sender<Arc<HashMap<i32, f32>>>) {
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
