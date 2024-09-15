//! `with_daemon` example: memory-cached URL retriever.
//!
//! This example allows to download each requested URL just once, caching the results in memory.
//! It is very simple and crude, it only serves the purpose of illustrating a relatively real-life
//! application of `with_daemon`.
//!
//! You can execute the compiled binary multiple times and asynchronously and as long as the daemon
//! is running it should allow only 1 (successful) download of each requested URL and reuse already
//! downloaded and cached results. Parallel downloads of different URLs are possible and will not
//! wait for each other.

use std::{collections::HashMap, env::args, error::Error, io::Cursor, sync::Arc};

use log::{error, warn};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
    sync::Mutex,
};

use with_daemon::with_daemon;

type State = Mutex<HashMap<String, Arc<Entry>>>;
type Entry = Mutex<Option<String>>;

fn main() -> Result<(), Box<dyn Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("off")).init();
    let result = with_daemon(
        "/tmp/with_daemon__example_cached_url_retriever.pid",
        "/tmp/with_daemon__example_cached_url_retriever.sock",
        // shared state factory, awaited in the daemon process
        |_| async { Result::<_, String>::Ok(State::default()) },
        // handler running in the daemon process, wrapped with a closure for ease of using `?`
        |state, stream| async move {
            if let Err(e) = handler(state, stream).await {
                error!("handler error: {e}");
            }
        },
        // client code running in the client process
        client,
    )?;
    // `result` here is a `Result` because this is what our client returns.
    // In general, it could be any type.
    println!("result: {}", result?);
    Ok(())
}

/// Handle one client by retrieving a URL or using cached result.
///
/// This function reads the URL to retrieve, then looks it up in memory cache, making sure to
/// release the global cache lock as soon as either entry is found or created if it didn't exist
/// and lock the entry before releasing the global cache.
///
/// Then if the entry is vacant (`Option<String>` is None), it performs the retrieval and sets it
/// to `Some` (or leaves it `None` on error for other handler instance to try again).
///
/// If the entry is populated, then no retrieval needs to take place, the cached result is returned
/// to client.
///
/// The data is sent back to the client using the same stream as the one used to read the URL to
/// retrieve.
async fn handler(state: Arc<State>, mut stream: UnixStream) -> Result<(), Box<dyn Error>> {
    let mut input = String::new();
    stream.read_to_string(&mut input).await?;

    let mut entry = {
        let mut cache = state.lock().await;
        let entry = Arc::clone(cache.entry(input.clone()).or_default());
        entry.lock_owned().await
    };
    let cached_result = match *entry {
        // already computed by other handler
        Some(ref result) => result.clone(),
        // not computed yet
        None => {
            // perform very complicated computations...
            match get(&input).await {
                Ok(result) => {
                    *entry = Some(result.clone());
                    result
                }
                Err(e) => {
                    warn!("error retrieving: {e}");
                    format!("error: {e}")
                }
            }
        }
    };
    // no need to keep the entry lock while sending the result back
    drop(entry);
    let mut cursor = Cursor::new(cached_result);
    stream.write_all_buf(&mut cursor).await?;
    stream.shutdown().await?;
    Ok(())
}

/// Retrieve a URL and return read data.
async fn get(url: &str) -> Result<String, Box<dyn Error>> {
    // uncomment below to simulate this is a very complicated operation...
    //tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    let data = reqwest::get(url).await?.text().await?;
    Ok(data)
}

/// Client implementation.
///
/// This function writes the URL to request to the stream, then reads and returns the result.
async fn client(mut stream: UnixStream) -> Result<String, Box<dyn Error>> {
    let input: String = args().nth(1).ok_or("input argument missing")?.parse()?;
    let mut cursor = Cursor::new(input);
    stream.write_all_buf(&mut cursor).await?;
    stream.shutdown().await?;
    let mut result = String::new();
    stream.read_to_string(&mut result).await?;
    Result::<_, Box<dyn Error>>::Ok(result)
}
