use tokio::sync::mpsc;

pub struct Waiter {
    receiver: mpsc::Receiver<()>,
    token: Token,
}

#[derive(Clone)]
pub struct Token(#[allow(dead_code)] mpsc::Sender<()>);

impl Waiter {
    pub fn new() -> Self {
        let (sender, receiver) = mpsc::channel::<()>(1);
        Self {
            receiver,
            token: Token(sender),
        }
    }

    pub fn token(&self) -> Token {
        self.token.clone()
    }

    pub async fn wait(mut self) {
        drop(self.token);
        let res = self.receiver.recv().await;
        assert!(res.is_none());
    }
}
