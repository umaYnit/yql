use std::collections::BTreeMap;
use std::sync::Arc;

use ahash::AHashMap;
use anyhow::Result;
use itertools::Itertools;
use serde::{Deserialize, Serialize};
use tokio_stream::StreamExt;

use crate::array::{
    ArrayExt, ArrayRef, BooleanType, DataType, Float32Type, Float64Type, Int16Type, Int32Type,
    Int64Type, Int8Type, NullArray, PrimitiveBuilder, Scalar, StringBuilder, TimestampArray,
    TimestampType,
};
use crate::dataset::{DataSet, SchemaRef};
use crate::execution::dataset::{DataSetExt, GroupedKey};
use crate::execution::stream::{CreateStreamContext, Event, EventStream};
use crate::execution::streams::create_stream;
use crate::expr::physical_expr::PhysicalExpr;
use crate::expr::ExprState;
use crate::planner::physical_plan::PhysicalAggregateNode;
use crate::planner::window::Window;

macro_rules! append_primitive_value {
    ($columns:expr, $aggregate_states:expr, $index:expr, $ty:ty, $scalar_ty:ident) => {{
        let mut builder = PrimitiveBuilder::<$ty>::with_capacity($aggregate_states.len());
        for state in $aggregate_states.values() {
            builder.append_opt(if let Scalar::$scalar_ty(value) = &state.values[$index] {
                Some(*value)
            } else {
                None
            });
        }
        $columns.push(Arc::new(builder.finish()));
    }};
}

type SavedWindow = (i64, i64, Vec<(GroupedKey, Vec<ExprState>, Vec<Scalar>)>);

#[derive(Serialize, Deserialize)]
struct SavedState {
    group_exprs: Vec<ExprState>,
    windows: Vec<SavedWindow>,
}

struct AggregateState {
    aggr_exprs: Vec<PhysicalExpr>,
    values: Vec<Scalar>,
}

#[derive(Default)]
struct WindowState {
    start_time: i64,
    end_time: i64,
    children: AHashMap<GroupedKey, AggregateState>,
}

pub struct AggregateManager {
    schema: SchemaRef,
    group_exprs: Vec<PhysicalExpr>,
    aggr_exprs: Vec<PhysicalExpr>,
    window: Window,
    time_idx: usize,
    windows: BTreeMap<i64, WindowState>,
}

impl AggregateManager {
    fn load_state(&mut self, data: Vec<u8>) -> Result<()> {
        let saved_state: SavedState = bincode::deserialize(&data)?;

        for (expr, data) in self.group_exprs.iter_mut().zip(saved_state.group_exprs) {
            expr.load_state(data)?;
        }

        for (start, end, groups) in saved_state.windows {
            let mut window_state = WindowState {
                start_time: start,
                end_time: end,
                children: Default::default(),
            };
            for (key, expr_state, scalars) in groups {
                let mut aggregate_state = AggregateState {
                    aggr_exprs: self.aggr_exprs.clone(),
                    values: scalars,
                };
                for (expr, data) in aggregate_state.aggr_exprs.iter_mut().zip(expr_state) {
                    expr.load_state(data)?;
                }
                window_state.children.insert(key, aggregate_state);
            }
            self.windows.insert(start, window_state);
        }
        Ok(())
    }

    fn save_state(&self) -> Result<Vec<u8>> {
        let group_exprs = self
            .group_exprs
            .iter()
            .map(|expr| expr.save_state())
            .try_collect()?;

        let mut windows = Vec::new();
        for (start, window) in &self.windows {
            let mut groups = Vec::new();
            for (grouped_key, aggregate_state) in &window.children {
                groups.push((
                    grouped_key.clone(),
                    aggregate_state
                        .aggr_exprs
                        .iter()
                        .map(|expr| expr.save_state())
                        .try_collect()?,
                    aggregate_state.values.clone(),
                ));
            }
            windows.push((*start, window.end_time, groups));
        }

        let saved_state = SavedState {
            group_exprs,
            windows,
        };
        Ok(bincode::serialize(&saved_state)?)
    }

    fn process_dataset(
        &mut self,
        start: i64,
        end: i64,
        grouped_key: GroupedKey,
        dataset: &DataSet,
    ) -> Result<()> {
        let window_state = self.windows.entry(start).or_insert_with(|| WindowState {
            start_time: start,
            end_time: end,
            children: Default::default(),
        });

        let aggregate_state = match window_state.children.get_mut(&grouped_key) {
            Some(aggregate_state) => aggregate_state,
            None => window_state
                .children
                .entry(grouped_key)
                .or_insert(AggregateState {
                    aggr_exprs: self.aggr_exprs.clone(),
                    values: vec![Scalar::Null; self.aggr_exprs.len()],
                }),
        };
        for (expr, scalar) in aggregate_state
            .aggr_exprs
            .iter_mut()
            .zip(aggregate_state.values.iter_mut())
        {
            let array = expr.eval(dataset)?;
            *scalar = array.scalar_value(array.len() - 1);
        }

        Ok(())
    }

    fn aggregate(
        &mut self,
        dataset: &DataSet,
        current_watermark: Option<i64>,
    ) -> Result<Vec<DataSet>> {
        let mut datasets = Vec::new();

        for item in dataset.group_by_window(self.time_idx, &self.window)? {
            let (start, end, dataset) = item?;

            for item in dataset.group_by_exprs(&mut self.group_exprs)? {
                let (grouped_key, dataset) = item?;
                self.process_dataset(start, end, grouped_key, &dataset)?;
            }
        }

        let mut completed_windows = Vec::new();
        if let Some(current_watermark) = current_watermark {
            while let Some((start, window)) = self.windows.iter().next() {
                if current_watermark > window.end_time {
                    let start = *start;
                    if let Some(window) = self.windows.remove(&start) {
                        completed_windows.push(window);
                    }
                } else {
                    break;
                }
            }
        }

        for window in completed_windows {
            let mut columns = Vec::with_capacity(self.aggr_exprs.len());

            for index in 0..self.aggr_exprs.len() {
                let field = &self.schema.fields()[index];

                match field.data_type {
                    DataType::Null => {
                        columns.push(Arc::new(NullArray::new(window.children.len())) as ArrayRef)
                    }
                    DataType::Int8 => {
                        append_primitive_value!(columns, window.children, index, Int8Type, Int8)
                    }
                    DataType::Int16 => {
                        append_primitive_value!(columns, window.children, index, Int16Type, Int16)
                    }
                    DataType::Int32 => {
                        append_primitive_value!(columns, window.children, index, Int32Type, Int32)
                    }
                    DataType::Int64 => {
                        append_primitive_value!(columns, window.children, index, Int64Type, Int64)
                    }
                    DataType::Float32 => {
                        append_primitive_value!(
                            columns,
                            window.children,
                            index,
                            Float32Type,
                            Float32
                        )
                    }
                    DataType::Float64 => {
                        append_primitive_value!(
                            columns,
                            window.children,
                            index,
                            Float64Type,
                            Float64
                        )
                    }
                    DataType::Boolean => {
                        append_primitive_value!(
                            columns,
                            window.children,
                            index,
                            BooleanType,
                            Boolean
                        )
                    }
                    DataType::Timestamp(_) => append_primitive_value!(
                        columns,
                        window.children,
                        index,
                        TimestampType,
                        Timestamp
                    ),
                    DataType::String => {
                        let mut builder = StringBuilder::with_capacity(window.children.len());
                        for state in window.children.values() {
                            builder.append_opt(
                                if let Scalar::String(value) = &state.values[index] {
                                    Some(value)
                                } else {
                                    None
                                },
                            );
                        }
                        columns.push(Arc::new(builder.finish()));
                    }
                }
            }

            columns.push(Arc::new(TimestampArray::new_scalar(
                window.children.len(),
                Some(window.start_time),
            )));
            datasets.push(DataSet::try_new(self.schema.clone(), columns)?);
        }

        Ok(datasets)
    }
}

pub fn create_aggregate_stream(
    ctx: &mut CreateStreamContext,
    node: PhysicalAggregateNode,
) -> Result<EventStream> {
    let PhysicalAggregateNode {
        id,
        schema,
        group_exprs,
        aggr_exprs,
        window,
        time_idx,
        input,
    } = node;
    let mut manager = AggregateManager {
        schema,
        group_exprs,
        aggr_exprs,
        window,
        time_idx,
        windows: Default::default(),
    };
    if let Some(prev_state) = ctx.prev_state.remove(&id) {
        manager.load_state(prev_state)?;
    }

    let mut input = create_stream(ctx, *input)?;

    Ok(Box::pin(async_stream::try_stream! {
        while let Some(event) = input.next().await.transpose()? {
            match event {
                Event::DataSet{ current_watermark, dataset } => {
                    for dataset in manager.aggregate(&dataset, current_watermark)? {
                        yield Event::DataSet{ current_watermark, dataset };
                    }
                }
                Event::CreateCheckPoint(barrier) => {
                    if !barrier.is_saved(id) {
                        barrier.set_state(id, Some(manager.save_state()?));
                    }
                    yield Event::CreateCheckPoint(barrier.clone());
                    if barrier.is_exit() {
                        break;
                    }
                }
            }
        }
    }))
}
