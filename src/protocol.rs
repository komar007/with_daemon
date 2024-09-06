/// A type to send through a socket between fork()'s parent and child to inform the parent about
/// the status of the daemon.
#[derive(Debug, num_derive::FromPrimitive)]
pub enum ReadyToken {
    /// Daemon has just been forked by the child and is now ready to accept connections.
    DaemonForked = 0x4ea11e55,
    /// Daemon has already been running and it is not known if it is ready to accept connections
    /// (but very likely it is).
    DaemonRunning = 0x4ea1ab1e,
}
