# `with_daemon` - An async client-daemon abstraction framework

This crate abstracts away the spawning of and connecting to a daemon required to optimize tasks
performed by multiple client instances that are separate processes.

The daemon runs in a separate detached process from the first time the client is used and provides
functionality to multiple instances of the client, taking advantage of the ability to have a common
state shared between client handlers.

## Usage

An example is worth more than a hundred words:

```rust

//! `with_daemon` example: a simple global counter
//!
//! This example demonstrates how to use `with_daemon` in the most basic way.
//!
//! The daemon keeps track of a counter and each call to the client returns the current counter
//! value and increments the counter.
//!
//! When the compiled binary is executed, it spawns the daemon if it's not running yet, and
//! receives the current value from the daemon. This way each execution of the program provides a
//! different integer, until the daemon is manually killed.

use std::{
    error::Error,
    sync::atomic::{AtomicU32, Ordering},
};

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use with_daemon::with_daemon;

fn main() -> Result<(), Box<dyn Error>> {
    let result = with_daemon(
        "/tmp/with_daemon__example_counter.pid",
        "/tmp/with_daemon__example_counter.sock",
        // In this example the state is just an integer starting from 0.
        //
        // `with_daemon` expects a future that resolves to the initial value of the state here.
        // It awaits that future in the daemon process and passes it to each handler.
        async { AtomicU32::new(0) },
        // The handler is given an `Arc` holding the state above and a stream connected
        // bi-directionally to the client it is handling.
        //
        // Here, it increments the state atomically and writes the value back to the client.
        |state, mut stream| async move {
            let previous = state.fetch_add(1, Ordering::SeqCst);
            let _ = stream.write_u32(previous).await;
        },
        // The client is given a stream connected bi-directionally to an instance of handler.
        //
        // Here, it only needs to read the result sent to it by the handler.
        |mut stream| async move { stream.read_u32().await },
    )?; // An error above signifies an internal error in `with_daemon`, for example inability to fork,
        // so in the example we just fail when that happens.

    // `result` here is just what our client closure returns, so an I/O error reading from the
    // stream od the value read.
    println!("result: {}", result?);
    Ok(())
}
```

You can see another, more complicated example in the `examples/` directory.
