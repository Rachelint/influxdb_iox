use std::sync::Arc;

use datafusion::{
    config::ConfigOptions, physical_optimizer::PhysicalOptimizerRule, physical_plan::ExecutionPlan,
};
use serde::Serialize;

use crate::test::format_execution_plan;

#[derive(Debug, Serialize)]
pub struct OptimizationTest {
    input: Vec<String>,
    output: Result<Vec<String>, String>,
}

impl OptimizationTest {
    pub fn new<O>(input_plan: Arc<dyn ExecutionPlan>, opt: O) -> Self
    where
        O: PhysicalOptimizerRule,
    {
        Self::new_with_config(input_plan, opt, &ConfigOptions::default())
    }

    pub fn new_with_config<O>(
        input_plan: Arc<dyn ExecutionPlan>,
        opt: O,
        config: &ConfigOptions,
    ) -> Self
    where
        O: PhysicalOptimizerRule,
    {
        Self {
            input: format_execution_plan(&input_plan),
            output: opt
                .optimize(input_plan, config)
                .map(|plan| format_execution_plan(&plan))
                .map_err(|e| e.to_string()),
        }
    }
}
