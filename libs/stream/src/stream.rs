use std::collections::HashMap;
use std::fmt::{self, Debug, Formatter};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use anyhow::{Context as _, Result};
use futures_util::stream::BoxStream;
use tokio::sync::broadcast;
use tokio::time::Interval;
use tokio_stream::Stream;
use yql_dataset::DataSet;
use yql_planner::logical_plan::LogicalPlan;
use yql_planner::physical_plan::PhysicalPlan;

use crate::checkpoint::CheckPointBarrier;
use crate::StreamConfigRef;

pub enum Event {
    DataSet {
        current_watermark: Option<i64>,
        dataset: DataSet,
    },
    CreateCheckPoint(Arc<CheckPointBarrier>),
}

impl Debug for Event {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Event::DataSet {
                current_watermark,
                dataset,
            } => f
                .debug_struct("DataSet")
                .field("current_watermark", current_watermark)
                .field("dataset", dataset)
                .finish(),
            Event::CreateCheckPoint(_) => f.debug_struct("CreateCheckPoint").finish(),
        }
    }
}

pub type EventStream = BoxStream<'static, Result<Event>>;

pub struct CreateStreamContext {
    pub config: StreamConfigRef,
    pub tx_barrier: broadcast::Sender<Arc<CheckPointBarrier>>,
    pub prev_state: HashMap<usize, Vec<u8>>,
}

pub struct DataStream {
    config: StreamConfigRef,
    event_stream: EventStream,
    node_count: usize,
    source_count: usize,
    tx_barrier: broadcast::Sender<Arc<CheckPointBarrier>>,
    checkpoint_interval: Interval,
}

impl DataStream {
    pub fn try_new(config: StreamConfigRef, plan: LogicalPlan) -> Result<Self> {
        Self::try_new_with_graceful_shutdown(
            config,
            plan,
            Option::<futures_util::future::Pending<()>>::None,
        )
    }

    pub fn try_new_with_graceful_shutdown(
        config: StreamConfigRef,
        plan: LogicalPlan,
        signal: Option<impl Future<Output = ()> + Send + 'static>,
    ) -> Result<Self> {
        let plan = PhysicalPlan::try_new(plan)?;
        let node_count = plan.node_count;
        let source_count = plan.source_count;
        let (tx_barrier, _) = broadcast::channel(8);

        let prev_state: HashMap<usize, Vec<u8>> = match &config.load_state_fn {
            Some(load_state_fn) => {
                let data = load_state_fn()?;
                bincode::deserialize(&data).context("failed to deserialize stream state.")?
            }
            None => Default::default(),
        };
        let mut ctx = CreateStreamContext {
            config: config.clone(),
            tx_barrier: tx_barrier.clone(),
            prev_state,
        };

        let event_stream = crate::streams::create_stream(&mut ctx, plan.root)?;
        let checkpoint_interval = tokio::time::interval(config.checkpoint_interval);

        if let Some(signal) = signal {
            tokio::spawn({
                let tx_barrier = tx_barrier.clone();
                async move {
                    signal.await;
                    let barrier = Arc::new(CheckPointBarrier::new(node_count, source_count, true));
                    let _ = tx_barrier.send(barrier);
                }
            });
        }

        Ok(Self {
            config,
            event_stream,
            node_count,
            source_count,
            tx_barrier,
            checkpoint_interval,
        })
    }
}

impl Stream for DataStream {
    type Item = Result<DataSet>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            if self.checkpoint_interval.poll_tick(cx).is_ready()
                && self.config.save_state_fn.is_some()
            {
                let barrier = Arc::new(CheckPointBarrier::new(
                    self.node_count,
                    self.source_count,
                    false,
                ));
                let _ = self.tx_barrier.send(barrier.clone());
                let config = self.config.clone();
                tokio::spawn(save_state(config, barrier));
            }

            return match Pin::new(&mut self.event_stream).poll_next(cx) {
                Poll::Ready(Some(Ok(Event::DataSet { dataset, .. }))) => {
                    Poll::Ready(Some(Ok(dataset)))
                }
                Poll::Ready(Some(Ok(_))) => continue,
                Poll::Ready(Some(Err(err))) => Poll::Ready(Some(Err(err))),
                Poll::Ready(None) => Poll::Ready(None),
                Poll::Pending => Poll::Pending,
            };
        }
    }
}

async fn save_state(config: StreamConfigRef, barrier: Arc<CheckPointBarrier>) {
    tracing::info!(name = %config.name, "create checkpoint");
    barrier.wait().await;

    let data = match bincode::serialize(&barrier.take_state()) {
        Ok(data) => data,
        Err(err) => {
            tracing::error!(
                name = %config.name,
                error = %err,
                "failed to serialize stream state"
            );
            return;
        }
    };

    if let Some(save_state_fn) = &config.save_state_fn {
        match save_state_fn(data) {
            Ok(()) => tracing::info!(name = %config.name, "checkpoint created"),
            Err(err) => {
                tracing::info!(name = %config.name, error = %err, "failed to save checkpoint")
            }
        }
    }
}
