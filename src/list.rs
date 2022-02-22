//! Observable append-only list.

use futures::{
    future::{self, BoxFuture},
    Future, FutureExt,
};
use remoc::prelude::*;
use std::{
    fmt,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::sync::{mpsc, oneshot};

struct SubscribeReq<T, Codec> {
    tx: rch::mpsc::Sender<T, Codec>,
    err_tx: oneshot::Sender<rch::mpsc::SendError<()>>,
}

struct Subscription<T, Codec> {
    pos: usize,
    tx: rch::mpsc::Sender<T, Codec>,
    err_tx: oneshot::Sender<rch::mpsc::SendError<()>>,
}

/// A buffer that stores and replays values sent to it.
///
/// Values sent to the replay channel buffer are stored in an internal buffer.
/// Multiple remote MPSC channels can be subscribed to the replay channel buffer and each
/// channel will receive all values sent to the replay channel buffer, even the
/// values that were received before it was subscribed.
///
/// When dropped all channels to the subscribers are closed immediately.
/// Use [keep](Self::keep) if you want to avoid this.
pub struct ReplayBuffer<T, Codec = remoc::codec::Default> {
    sub_tx: mpsc::UnboundedSender<SubscribeReq<T, Codec>>,
    keep_tx: Option<oneshot::Sender<()>>,
}

impl<T, Codec> fmt::Debug for ReplayBuffer<T, Codec> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("ReplayBuffer").finish()
    }
}

impl<T, Codec> Default for ReplayBuffer<T, Codec>
where
    T: RemoteSend + Clone,
    Codec: remoc::codec::Codec,
{
    fn default() -> Self {
        let (_tx, rx) = mpsc::channel(1);
        Self::new(rx)
    }
}

impl<T, Codec> ReplayBuffer<T, Codec>
where
    T: RemoteSend + Clone,
    Codec: remoc::codec::Codec,
{
    /// Creates a new replay channel buffer.
    ///
    /// The replay channel buffer is fed from the receiver `rx`.
    pub fn new(rx: mpsc::Receiver<T>) -> Self {
        let (sub_tx, sub_rx) = mpsc::unbounded_channel();
        let (keep_tx, keep_rx) = oneshot::channel();

        tokio::spawn(Self::buffer_task(rx, sub_rx, keep_rx));

        Self { sub_tx, keep_tx: Some(keep_tx) }
    }

    /// Creates a new replay channel buffer.
    ///
    /// The replay channel buffer is fed from the unbounded receiver `rx`.
    pub fn new_unbounded(mut rx: mpsc::UnboundedReceiver<T>) -> Self {
        let (b_tx, b_rx) = mpsc::channel(16);
        tokio::spawn(async move {
            while let Some(value) = rx.recv().await {
                if b_tx.send(value).await.is_err() {
                    break;
                }
            }
        });

        Self::new(b_rx)
    }

    /// When this replay buffer is dropped, ensures that all outstanding values
    /// are sent to the subscribers.
    pub fn keep(&mut self) {
        if let Some(keep_tx) = self.keep_tx.take() {
            let _ = keep_tx.send(());
        }
    }

    /// Subscribes a remote MPSC channel to the replay channel buffer.
    ///
    /// The channel will receive all values ever sent and future values that will be sent
    /// to the replay channel buffer.
    ///
    /// The returned [SubscriptionHandle] can be used to query for errors that occur
    /// during sending to the channel.
    pub fn subscribe<C, B>(&self, tx: rch::mpsc::Sender<T, C, B>) -> SubscriptionHandle
    where
        C: remoc::codec::Codec,
        B: rch::buffer::Size,
    {
        let tx = tx.set_codec().set_buffer();
        let (err_tx, err_rx) = oneshot::channel();
        if self.sub_tx.send(SubscribeReq { tx, err_tx }).is_err() {
            panic!("replay buffer task was shut down");
        }
        SubscriptionHandle(
            async move {
                match err_rx.await {
                    Ok(err) => Err(err),
                    Err(_) => Ok(()),
                }
            }
            .boxed(),
        )
    }

    async fn buffer_task(
        rx: mpsc::Receiver<T>, sub_rx: mpsc::UnboundedReceiver<SubscribeReq<T, Codec>>,
        keep_rx: oneshot::Receiver<()>,
    ) {
        let mut rx_opt = Some(rx);
        let mut sub_rx_opt = Some(sub_rx);
        let mut buffer: Vec<T> = Vec::new();
        let mut subs: Vec<Subscription<T, Codec>> = Vec::new();
        let mut keep_rx = keep_rx.fuse();

        loop {
            if rx_opt.is_none() {
                subs.retain(|sub| sub.pos < buffer.len());
            }

            let mut permit_tasks = Vec::new();
            for (i, sub) in subs.iter().enumerate() {
                if sub.pos < buffer.len() {
                    permit_tasks.push(async move { (i, sub.tx.reserve().await) }.boxed());
                }
            }

            if sub_rx_opt.is_none() && rx_opt.is_none() && permit_tasks.is_empty() {
                break;
            }

            tokio::select! {
                biased;
                sub_opt = async {
                    match &mut sub_rx_opt {
                        Some(sub_rx) => sub_rx.recv().await,
                        None => future::pending().await,
                    }
                } => {
                    match sub_opt {
                        Some(SubscribeReq { tx, err_tx }) => {
                            subs.push(Subscription { pos: 0, tx, err_tx });
                        }
                        None => sub_rx_opt = None,
                    }
                },
                value_opt = async {
                    match &mut rx_opt {
                        Some(rx) => rx.recv().await,
                        None => future::pending().await,
                    }
                } => {
                    match value_opt {
                        Some(value) => buffer.push(value),
                        None => rx_opt = None,
                    }
                },
                res = &mut keep_rx => {
                    if res.is_err() {
                        break;
                    }
                },
                (i, res) = async move {
                    if permit_tasks.is_empty() {
                        future::pending().await
                    } else {
                        future::select_all(permit_tasks).await.0
                    }
                } => {
                    match res {
                        Ok(permit) => {
                            let pos = &mut subs[i].pos;
                            permit.send(buffer[*pos].clone());
                            *pos += 1;
                        }
                        Err(err) => {
                            let sub = subs.swap_remove(i);
                            let _ = sub.err_tx.send(err);
                        }
                    }
                }
            }
        }
    }
}

impl<T, Codec> Drop for ReplayBuffer<T, Codec> {
    fn drop(&mut self) {
        // empty
    }
}

/// A handle to a subscription to a [ReplayBuffer].
///
/// This can be `await`ed to obtain the error that occurred when sending
/// to this subscription.
/// `Ok(())` is returned if the subscription ends because the [ReplayBuffer] was dropped.
///
/// Dropping this handle will not unsubscribe the channel.
pub struct SubscriptionHandle(BoxFuture<'static, Result<(), rch::mpsc::SendError<()>>>);

impl Future for SubscriptionHandle {
    type Output = Result<(), rch::mpsc::SendError<()>>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        self.0.poll_unpin(cx)
    }
}