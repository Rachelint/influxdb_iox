//! An optimizer rule that transforms a plan
//! to fill gaps in time series data.

mod range_predicate;

use crate::exec::gapfill::{FillStrategy, GapFill, GapFillParams};
use datafusion::{
    error::{DataFusionError, Result},
    logical_expr::{
        expr_rewriter::{ExprRewritable, ExprRewriter, RewriteRecursion},
        expr_visitor::{ExprVisitable, ExpressionVisitor, Recursion},
        utils::expr_to_columns,
        Aggregate, BuiltinScalarFunction, Extension, LogicalPlan,
    },
    optimizer::{optimizer::ApplyOrder, OptimizerConfig, OptimizerRule},
    prelude::{col, Expr},
};
use query_functions::gapfill::DATE_BIN_GAPFILL_UDF_NAME;
use std::{
    collections::HashSet,
    ops::{Bound, Range},
    sync::Arc,
};

// This optimizer rule enables gap-filling semantics for SQL queries
/// that contain calls to `DATE_BIN_GAPFILL()`.
///
/// In SQL a typical gap-filling query might look like this:
/// ```sql
/// SELECT
///   location,
///   DATE_BIN_GAPFILL(INTERVAL '1 minute', time, '1970-01-01T00:00:00Z') AS minute,
///   AVG(temp)
/// FROM temps
/// WHERE time > NOW() - INTERVAL '6 hours' AND time < NOW()
/// GROUP BY LOCATION, MINUTE
/// ```
/// The aggregation step of the initial logical plan looks like this:
/// ```text
///   Aggregate: groupBy=[[datebingapfill(IntervalDayTime("60000"), temps.time, TimestampNanosecond(0, None)))]], aggr=[[AVG(temps.temp)]]
/// ```
/// However, `DATE_BIN_GAPFILL()` does not have an actual implementation like other functions.
/// Instead, the plan is transformed to this:
/// ```text
/// GapFill: groupBy=[[datebingapfill(IntervalDayTime("60000"), temps.time, TimestampNanosecond(0, None)))]], aggr=[[AVG(temps.temp)]], start=..., stop=...
///   Aggregate: groupBy=[[datebingapfill(IntervalDayTime("60000"), temps.time, TimestampNanosecond(0, None)))]], aggr=[[AVG(temps.temp)]]
/// ```
/// This optimizer rule makes that transformation.
pub struct HandleGapFill;

impl HandleGapFill {
    pub fn new() -> Self {
        Self {}
    }
}

impl Default for HandleGapFill {
    fn default() -> Self {
        Self::new()
    }
}

impl OptimizerRule for HandleGapFill {
    fn try_optimize(
        &self,
        plan: &LogicalPlan,
        _config: &dyn OptimizerConfig,
    ) -> Result<Option<LogicalPlan>> {
        handle_gap_fill(plan)
    }

    fn name(&self) -> &str {
        "handle_gap_fill"
    }

    fn apply_order(&self) -> Option<ApplyOrder> {
        Some(ApplyOrder::BottomUp)
    }
}

fn handle_gap_fill(plan: &LogicalPlan) -> Result<Option<LogicalPlan>> {
    let res = match plan {
        LogicalPlan::Aggregate(aggr) => handle_aggregate(aggr)?,
        _ => None,
    };

    if res.is_none() {
        // no transformation was applied,
        // so make sure the plan is not using gap filling
        // functions in an unsupported way.
        check_node(plan)?;
    }

    Ok(res)
}

fn handle_aggregate(aggr: &Aggregate) -> Result<Option<LogicalPlan>> {
    let Aggregate {
        input,
        group_expr,
        aggr_expr,
        schema,
        ..
    } = aggr;

    // new_group_expr has DATE_BIN_GAPFILL replaced with DATE_BIN.
    let RewriteInfo {
        new_group_expr,
        date_bin_gapfill_index,
        date_bin_gapfill_args,
    } = if let Some(v) = replace_date_bin_gapfill(group_expr)? {
        v
    } else {
        return Ok(None);
    };

    let new_aggr_plan = {
        // Create the aggregate node with the same output schema as the orignal
        // one. This means that there will be an output column called `date_bin_gapfill(...)`
        // even though the actual expression populating that column will be `date_bin(...)`.
        // This seems acceptable since it avoids having to deal with renaming downstream.
        let new_aggr_plan = Aggregate::try_new_with_schema(
            Arc::clone(input),
            new_group_expr,
            aggr_expr.clone(),
            Arc::clone(schema),
        )?;
        let new_aggr_plan = LogicalPlan::Aggregate(new_aggr_plan);
        check_node(&new_aggr_plan)?;
        new_aggr_plan
    };

    let new_gap_fill_plan =
        build_gapfill_node(new_aggr_plan, date_bin_gapfill_index, date_bin_gapfill_args)?;
    Ok(Some(new_gap_fill_plan))
}

fn build_gapfill_node(
    new_aggr_plan: LogicalPlan,
    date_bin_gapfill_index: usize,
    date_bin_gapfill_args: Vec<Expr>,
) -> Result<LogicalPlan> {
    if date_bin_gapfill_args.len() != 3 {
        return Err(DataFusionError::Plan(format!(
            "DATE_BIN_GAPFILL expects 3 arguments, got {}",
            date_bin_gapfill_args.len()
        )));
    }

    let mut args_iter = date_bin_gapfill_args.into_iter();

    // Ensure that stride argument is a scalar
    let stride = args_iter.next().unwrap();
    validate_scalar_expr("stride argument to DATE_BIN_GAPFILL", &stride)?;

    // Ensure that the source argument is a column
    let time_col = args_iter.next().unwrap().try_into_col().map_err(|_| {
        DataFusionError::Plan(
            "DATE_BIN_GAPFILL requires a column as the source argument".to_string(),
        )
    })?;

    // Ensure that a time range was specified and is valid for gap filling
    let time_range = range_predicate::find_time_range(new_aggr_plan.inputs()[0], &time_col)?;
    validate_time_range(&time_range)?;

    // Ensure that origin argument is a scalar
    let origin = args_iter.next().unwrap();
    validate_scalar_expr("origin argument to DATE_BIN_GAPFILL", &origin)?;

    // Make sure the time output to the gapfill node matches what the
    // aggregate output was.
    let time_column =
        col(new_aggr_plan.schema().fields()[date_bin_gapfill_index].qualified_column());

    let aggr = Aggregate::try_from_plan(&new_aggr_plan)?;
    let mut new_group_expr: Vec<_> = aggr
        .schema
        .fields()
        .iter()
        .map(|f| Expr::Column(f.qualified_column()))
        .collect();
    let aggr_expr = new_group_expr.split_off(aggr.group_expr.len());

    // For now, we can only fill with null values.
    // In the future, this rule will allow a projection to be pushed into the
    // GapFill node, e.g., if it contains an item like `LOCF(<col>)`.
    let fill_behavior = aggr_expr
        .iter()
        .cloned()
        .map(|e| (e, FillStrategy::Null))
        .collect();

    Ok(LogicalPlan::Extension(Extension {
        node: Arc::new(GapFill::try_new(
            Arc::new(new_aggr_plan),
            new_group_expr,
            aggr_expr,
            GapFillParams {
                stride,
                time_column,
                origin,
                time_range,
                fill_strategy: fill_behavior,
            },
        )?),
    }))
}

fn validate_time_range(range: &Range<Bound<Expr>>) -> Result<()> {
    let Range { ref start, ref end } = range;
    let (start, end) = match (start, end) {
        (Bound::Unbounded, Bound::Unbounded) => {
            return Err(DataFusionError::Plan(
                "no time bounds found for gap fill query".to_string(),
            ))
        }
        (Bound::Unbounded, _) => Err(DataFusionError::Plan(
            "no lower time bound found for gap fill query".to_string(),
        )),
        (_, Bound::Unbounded) => Err(DataFusionError::Plan(
            "no upper time bound found for gap fill query".to_string(),
        )),
        (
            Bound::Included(start) | Bound::Excluded(start),
            Bound::Included(end) | Bound::Excluded(end),
        ) => Ok((start, end)),
    }?;
    validate_scalar_expr("lower time bound", start)?;
    validate_scalar_expr("upper time bound", end)
}

fn validate_scalar_expr(what: &str, e: &Expr) -> Result<()> {
    let mut cols = HashSet::new();
    expr_to_columns(e, &mut cols)?;
    if !cols.is_empty() {
        Err(DataFusionError::Plan(format!(
            "{what} for gap fill query must evaluate to a scalar"
        )))
    } else {
        Ok(())
    }
}

struct RewriteInfo {
    // Group expressions with DATE_BIN_GAPFILL rewritten to DATE_BIN.
    new_group_expr: Vec<Expr>,
    // The index of the group expression that contained the call to DATE_BIN_GAPFILL.
    date_bin_gapfill_index: usize,
    // The arguments to the call to DATE_BIN_GAPFILL.
    date_bin_gapfill_args: Vec<Expr>,
}

// Iterate over the group expression list.
// If it finds no occurrences of date_bin_gapfill, it will return None.
// If it finds more than one occurrence it will return an error.
// Otherwise it will return a RewriteInfo for the optimizer rule to use.
fn replace_date_bin_gapfill(group_expr: &[Expr]) -> Result<Option<RewriteInfo>> {
    let mut date_bin_gapfill_count = 0;
    let mut dbg_idx = None;
    group_expr
        .iter()
        .enumerate()
        .try_for_each(|(i, e)| -> Result<()> {
            let fn_cnt = count_date_bin_gapfill(e)?;
            date_bin_gapfill_count += fn_cnt;
            if fn_cnt > 0 {
                dbg_idx = Some(i);
            }
            Ok(())
        })?;
    match date_bin_gapfill_count {
        0 => return Ok(None),
        2.. => {
            return Err(DataFusionError::Plan(
                "DATE_BIN_GAPFILL specified more than once".to_string(),
            ))
        }
        _ => (),
    }
    let date_bin_gapfill_index = dbg_idx.expect("should be found exactly one call");

    let mut rewriter = DateBinGapfillRewriter { args: None };
    let group_expr = group_expr
        .iter()
        .enumerate()
        .map(|(i, e)| {
            if i == date_bin_gapfill_index {
                e.clone().rewrite(&mut rewriter)
            } else {
                Ok(e.clone())
            }
        })
        .collect::<Result<Vec<_>>>()?;
    let date_bin_gapfill_args = rewriter.args.expect("should have found args");

    Ok(Some(RewriteInfo {
        new_group_expr: group_expr,
        date_bin_gapfill_index,
        date_bin_gapfill_args,
    }))
}

struct DateBinGapfillRewriter {
    args: Option<Vec<Expr>>,
}

impl ExprRewriter for DateBinGapfillRewriter {
    fn pre_visit(&mut self, expr: &Expr) -> Result<RewriteRecursion> {
        match expr {
            Expr::ScalarUDF { fun, .. } if fun.name == DATE_BIN_GAPFILL_UDF_NAME => {
                Ok(RewriteRecursion::Mutate)
            }
            _ => Ok(RewriteRecursion::Continue),
        }
    }

    fn mutate(&mut self, expr: Expr) -> Result<Expr> {
        match expr {
            Expr::ScalarUDF { fun, args } if fun.name == DATE_BIN_GAPFILL_UDF_NAME => {
                self.args = Some(args.clone());
                Ok(Expr::ScalarFunction {
                    fun: BuiltinScalarFunction::DateBin,
                    args,
                })
            }
            _ => Ok(expr),
        }
    }
}

fn count_date_bin_gapfill(e: &Expr) -> Result<usize> {
    struct Finder {
        count: usize,
    }
    impl ExpressionVisitor for Finder {
        fn pre_visit(mut self, expr: &Expr) -> Result<Recursion<Self>> {
            match expr {
                Expr::ScalarUDF { fun, .. } if fun.name == DATE_BIN_GAPFILL_UDF_NAME => {
                    self.count += 1;
                }
                _ => (),
            };
            Ok(Recursion::Continue(self))
        }
    }
    let f = Finder { count: 0 };
    let f = e.accept(f)?;
    Ok(f.count)
}

fn check_node(node: &LogicalPlan) -> Result<()> {
    node.expressions().iter().try_for_each(|expr| {
        let count = count_date_bin_gapfill(expr)?;
        if count > 0 {
            Err(DataFusionError::Plan(format!(
                "{DATE_BIN_GAPFILL_UDF_NAME} may only be used as a GROUP BY expression"
            )))
        } else {
            Ok(())
        }
    })
}

#[cfg(test)]
mod test {
    use std::sync::Arc;

    use super::HandleGapFill;

    use arrow::datatypes::{DataType, Field, Schema, TimeUnit};
    use datafusion::error::Result;
    use datafusion::logical_expr::{logical_plan, LogicalPlan, LogicalPlanBuilder};
    use datafusion::optimizer::optimizer::Optimizer;
    use datafusion::optimizer::OptimizerContext;
    use datafusion::prelude::{avg, case, col, lit, lit_timestamp_nano, Expr};
    use datafusion::scalar::ScalarValue;
    use query_functions::gapfill::DATE_BIN_GAPFILL_UDF_NAME;

    fn table_scan() -> Result<LogicalPlan> {
        let schema = Schema::new(vec![
            Field::new(
                "time",
                DataType::Timestamp(TimeUnit::Nanosecond, None),
                false,
            ),
            Field::new(
                "time2",
                DataType::Timestamp(TimeUnit::Nanosecond, None),
                false,
            ),
            Field::new("loc", DataType::Utf8, false),
            Field::new("temp", DataType::Float64, false),
        ]);
        logical_plan::table_scan(Some("temps"), &schema, None)?.build()
    }

    fn date_bin_gapfill(interval: Expr, time: Expr) -> Result<Expr> {
        date_bin_gapfill_with_origin(interval, time, lit_timestamp_nano(0))
    }

    fn date_bin_gapfill_with_origin(interval: Expr, time: Expr, origin: Expr) -> Result<Expr> {
        Ok(Expr::ScalarUDF {
            fun: query_functions::registry().udf(DATE_BIN_GAPFILL_UDF_NAME)?,
            args: vec![interval, time, origin],
        })
    }

    fn optimize(plan: &LogicalPlan) -> Result<Option<LogicalPlan>> {
        let optimizer = Optimizer::with_rules(vec![Arc::new(HandleGapFill::default())]);
        optimizer.optimize_recursively(
            optimizer.rules.get(0).unwrap(),
            plan,
            &OptimizerContext::new(),
        )
    }

    fn assert_optimizer_err(plan: &LogicalPlan, expected: &str) {
        match optimize(plan) {
            Ok(plan) => assert_eq!(format!("{}", plan.unwrap().display_indent()), "an error"),
            Err(ref e) => {
                let actual = e.to_string();
                if expected.is_empty() || !actual.contains(expected) {
                    assert_eq!(actual, expected)
                }
            }
        }
    }

    fn assert_optimization_skipped(plan: &LogicalPlan) -> Result<()> {
        let new_plan = optimize(plan)?;
        if new_plan.is_none() {
            return Ok(());
        }
        assert_eq!(
            format!("{}", plan.display_indent()),
            format!("{}", new_plan.unwrap().display_indent())
        );
        Ok(())
    }

    fn assert_optimized_plan_eq(plan: &LogicalPlan, expected: &str) -> Result<()> {
        let new_plan = optimize(plan)?;
        let new_lines = new_plan
            .expect("plan should have been optimized")
            .display_indent()
            .to_string();
        let new_lines = new_lines.split('\n');
        let expected_lines = expected.split('\n');
        // compare each line rather than the whole blob, to make it easier
        // to read output when tests fail.
        expected_lines
            .zip(new_lines)
            .for_each(|(expected, actual)| assert_eq!(expected, actual));
        Ok(())
    }

    #[test]
    fn misplaced_fns_err() -> Result<()> {
        // date_bin_gapfill used in a filter should produce an error
        let scan = table_scan()?;
        let plan = LogicalPlanBuilder::from(scan)
            .filter(
                date_bin_gapfill(
                    lit(ScalarValue::IntervalDayTime(Some(600_000))),
                    col("temp"),
                )?
                .gt(lit(100.0)),
            )?
            .build()?;
        assert_optimizer_err(
            &plan,
            "Error during planning: date_bin_gapfill may only be used as a GROUP BY expression",
        );
        Ok(())
    }

    #[test]
    fn nonscalar_origin() -> Result<()> {
        let plan = LogicalPlanBuilder::from(table_scan()?)
            .filter(
                col("time")
                    .gt_eq(lit_timestamp_nano(1000))
                    .and(col("time").lt(lit_timestamp_nano(2000))),
            )?
            .aggregate(
                vec![date_bin_gapfill_with_origin(
                    lit(ScalarValue::IntervalDayTime(Some(60_000))),
                    col("time"),
                    col("time2"),
                )?],
                vec![avg(col("temp"))],
            )?
            .build()?;
        assert_optimizer_err(
            &plan,
            "Error during planning: origin argument to DATE_BIN_GAPFILL for gap fill query must evaluate to a scalar",
        );
        Ok(())
    }

    #[test]
    fn nonscalar_stride() -> Result<()> {
        let stride = case(col("loc"))
            .when(
                lit("kitchen"),
                lit(ScalarValue::IntervalDayTime(Some(60_000))),
            )
            .otherwise(lit(ScalarValue::IntervalDayTime(Some(30_000))))
            .unwrap();

        let plan = LogicalPlanBuilder::from(table_scan()?)
            .filter(
                col("time")
                    .gt_eq(lit_timestamp_nano(1000))
                    .and(col("time").lt(lit_timestamp_nano(2000))),
            )?
            .aggregate(
                vec![date_bin_gapfill(stride, col("time"))?],
                vec![avg(col("temp"))],
            )?
            .build()?;
        assert_optimizer_err(
            &plan,
            "Error during planning: stride argument to DATE_BIN_GAPFILL for gap fill query must evaluate to a scalar",
        );
        Ok(())
    }

    #[test]
    fn time_range_errs() -> Result<()> {
        let cases = vec![
            (
                lit(true),
                "Error during planning: no time bounds found for gap fill query",
            ),
            (
                col("time").gt_eq(lit_timestamp_nano(1000)),
                "Error during planning: no upper time bound found for gap fill query",
            ),
            (
                col("time").lt(lit_timestamp_nano(2000)),
                "Error during planning: no lower time bound found for gap fill query",
            ),
            (
                col("time").gt_eq(col("time2")).and(
                    col("time").lt(lit_timestamp_nano(2000))),
                "Error during planning: lower time bound for gap fill query must evaluate to a scalar",
            ),
            (
                col("time").gt_eq(lit_timestamp_nano(2000)).and(
                    col("time").lt(col("time2"))),
                "Error during planning: upper time bound for gap fill query must evaluate to a scalar",
            )
        ];
        for c in cases {
            let plan = LogicalPlanBuilder::from(table_scan()?)
                .filter(c.0)?
                .aggregate(
                    vec![date_bin_gapfill(
                        lit(ScalarValue::IntervalDayTime(Some(60_000))),
                        col("time"),
                    )?],
                    vec![avg(col("temp"))],
                )?
                .build()?;
            assert_optimizer_err(&plan, c.1);
        }
        Ok(())
    }

    #[test]
    fn no_change() -> Result<()> {
        let plan = LogicalPlanBuilder::from(table_scan()?)
            .aggregate(vec![col("loc")], vec![avg(col("temp"))])?
            .build()?;
        assert_optimization_skipped(&plan)?;
        Ok(())
    }

    #[test]
    fn date_bin_gapfill_simple() -> Result<()> {
        let plan = LogicalPlanBuilder::from(table_scan()?)
            .filter(
                col("time")
                    .gt_eq(lit_timestamp_nano(1000))
                    .and(col("time").lt(lit_timestamp_nano(2000))),
            )?
            .aggregate(
                vec![date_bin_gapfill(
                    lit(ScalarValue::IntervalDayTime(Some(60_000))),
                    col("time"),
                )?],
                vec![avg(col("temp"))],
            )?
            .build()?;

        let dbg_args = "IntervalDayTime(\"60000\"),temps.time,TimestampNanosecond(0, None)";
        let expected = format!(
            "GapFill: groupBy=[[date_bin_gapfill({dbg_args})]], aggr=[[AVG(temps.temp)]], time_column=date_bin_gapfill({dbg_args}), stride=IntervalDayTime(\"60000\"), range=Included(TimestampNanosecond(1000, None))..Excluded(TimestampNanosecond(2000, None))\
           \n  Aggregate: groupBy=[[datebin(IntervalDayTime(\"60000\"), temps.time, TimestampNanosecond(0, None))]], aggr=[[AVG(temps.temp)]]\
           \n    Filter: temps.time >= TimestampNanosecond(1000, None) AND temps.time < TimestampNanosecond(2000, None)\
           \n      TableScan: temps");
        assert_optimized_plan_eq(&plan, &expected)?;
        Ok(())
    }

    #[test]
    fn two_group_exprs() -> Result<()> {
        // grouping by date_bin_gapfill(...), loc
        let plan = LogicalPlanBuilder::from(table_scan()?)
            .filter(
                col("time")
                    .gt_eq(lit_timestamp_nano(1000))
                    .and(col("time").lt(lit_timestamp_nano(2000))),
            )?
            .aggregate(
                vec![
                    date_bin_gapfill(lit(ScalarValue::IntervalDayTime(Some(60_000))), col("time"))?,
                    col("loc"),
                ],
                vec![avg(col("temp"))],
            )?
            .build()?;

        let dbg_args = "IntervalDayTime(\"60000\"),temps.time,TimestampNanosecond(0, None)";
        let expected = format!(
            "GapFill: groupBy=[[date_bin_gapfill({dbg_args}), temps.loc]], aggr=[[AVG(temps.temp)]], time_column=date_bin_gapfill({dbg_args}), stride=IntervalDayTime(\"60000\"), range=Included(TimestampNanosecond(1000, None))..Excluded(TimestampNanosecond(2000, None))\
           \n  Aggregate: groupBy=[[datebin(IntervalDayTime(\"60000\"), temps.time, TimestampNanosecond(0, None)), temps.loc]], aggr=[[AVG(temps.temp)]]\
           \n    Filter: temps.time >= TimestampNanosecond(1000, None) AND temps.time < TimestampNanosecond(2000, None)\
           \n      TableScan: temps");
        assert_optimized_plan_eq(&plan, &expected)?;
        Ok(())
    }

    #[test]
    fn double_date_bin_gapfill() -> Result<()> {
        let plan = LogicalPlanBuilder::from(table_scan()?)
            .aggregate(
                vec![
                    date_bin_gapfill(lit(ScalarValue::IntervalDayTime(Some(60_000))), col("time"))?,
                    date_bin_gapfill(lit(ScalarValue::IntervalDayTime(Some(30_000))), col("time"))?,
                ],
                vec![avg(col("temp"))],
            )?
            .build()?;
        assert_optimizer_err(
            &plan,
            "Error during planning: DATE_BIN_GAPFILL specified more than once",
        );
        Ok(())
    }

    #[test]
    fn with_projection() -> Result<()> {
        let dbg_args = "IntervalDayTime(\"60000\"),temps.time,TimestampNanosecond(0, None)";
        let plan = LogicalPlanBuilder::from(table_scan()?)
            .filter(
                col("time")
                    .gt_eq(lit_timestamp_nano(1000))
                    .and(col("time").lt(lit_timestamp_nano(2000))),
            )?
            .aggregate(
                vec![date_bin_gapfill(
                    lit(ScalarValue::IntervalDayTime(Some(60_000))),
                    col("time"),
                )?],
                vec![avg(col("temp"))],
            )?
            .project(vec![
                col(format!("date_bin_gapfill({dbg_args})")),
                col("AVG(temps.temp)"),
            ])?
            .build()?;

        let expected = format!(
            "Projection: date_bin_gapfill({dbg_args}), AVG(temps.temp)\
           \n  GapFill: groupBy=[[date_bin_gapfill({dbg_args})]], aggr=[[AVG(temps.temp)]], time_column=date_bin_gapfill({dbg_args}), stride=IntervalDayTime(\"60000\"), range=Included(TimestampNanosecond(1000, None))..Excluded(TimestampNanosecond(2000, None))\
           \n    Aggregate: groupBy=[[datebin(IntervalDayTime(\"60000\"), temps.time, TimestampNanosecond(0, None))]], aggr=[[AVG(temps.temp)]]\
           \n      Filter: temps.time >= TimestampNanosecond(1000, None) AND temps.time < TimestampNanosecond(2000, None)\
           \n        TableScan: temps");
        assert_optimized_plan_eq(&plan, &expected)?;
        Ok(())
    }
}
