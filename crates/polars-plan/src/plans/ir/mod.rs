mod dot;
mod format;
pub mod inputs;
mod schema;
pub(crate) mod tree_format;
#[cfg(feature = "ir_visualization")]
pub mod visualization;

use std::borrow::Cow;
use std::fmt;

pub use dot::{EscapeLabel, IRDotDisplay, PathsDisplay, ScanSourcesDisplay};
pub use format::{ExprIRDisplay, IRDisplay, write_group_by, write_ir_non_recursive};
use polars_core::prelude::*;
use polars_utils::idx_vec::UnitVec;
use polars_utils::unique_id::UniqueId;
#[cfg(feature = "ir_serde")]
use serde::{Deserialize, Serialize};
use strum_macros::IntoStaticStr;

use self::hive::HivePartitionsDf;
use crate::prelude::*;

#[cfg_attr(feature = "ir_serde", derive(serde::Serialize, serde::Deserialize))]
pub struct IRPlan {
    pub lp_top: Node,
    pub lp_arena: Arena<IR>,
    pub expr_arena: Arena<AExpr>,
}

#[derive(Clone, Copy)]
pub struct IRPlanRef<'a> {
    pub lp_top: Node,
    pub lp_arena: &'a Arena<IR>,
    pub expr_arena: &'a Arena<AExpr>,
}

/// [`IR`] is a representation of [`DslPlan`] with [`Node`]s which are allocated in an [`Arena`]
/// In this IR the logical plan has access to the full dataset.
#[derive(Clone, Debug, Default, IntoStaticStr)]
#[cfg_attr(feature = "ir_serde", derive(Serialize, Deserialize))]
#[strum(serialize_all = "SCREAMING_SNAKE_CASE")]
pub enum IR {
    #[cfg(feature = "python")]
    PythonScan {
        options: PythonOptions,
    },
    Slice {
        input: Node,
        offset: i64,
        len: IdxSize,
    },
    Filter {
        input: Node,
        predicate: ExprIR,
    },
    Scan {
        sources: ScanSources,
        file_info: FileInfo,
        hive_parts: Option<HivePartitionsDf>,
        predicate: Option<ExprIR>,
        /// * None: No skipping
        /// * Some(v): Files were skipped (filtered out), where:
        ///   * v @ true: Filter was fully applied (e.g. refers only to hive parts), so does not need to be applied at execution.
        ///   * v @ false: Filter still needs to be applied on remaining data.
        predicate_file_skip_applied: Option<bool>,
        /// schema of the projected file
        output_schema: Option<SchemaRef>,
        scan_type: Box<FileScanIR>,
        /// generic options that can be used for all file types.
        unified_scan_args: Box<UnifiedScanArgs>,
    },
    DataFrameScan {
        df: Arc<DataFrame>,
        schema: SchemaRef,
        // Schema of the projected file
        // If `None`, no projection is applied
        output_schema: Option<SchemaRef>,
    },
    /// Placeholder for data source when serializing templates.
    /// Used for serializing transformation logic without actual data.
    PlaceholderScan {
        schema: SchemaRef,
        output_schema: Option<SchemaRef>,
    },
    // Only selects columns (semantically only has row access).
    // This is a more restricted operation than `Select`.
    SimpleProjection {
        input: Node,
        columns: SchemaRef,
    },
    // Polars' `select` operation. This may access full materialized data.
    Select {
        input: Node,
        expr: Vec<ExprIR>,
        schema: SchemaRef,
        options: ProjectionOptions,
    },
    Sort {
        input: Node,
        by_column: Vec<ExprIR>,
        slice: Option<(i64, usize)>,
        sort_options: SortMultipleOptions,
    },
    Cache {
        input: Node,
        /// This holds the `Arc<DslPlan>` to guarantee uniqueness.
        id: UniqueId,
    },
    GroupBy {
        input: Node,
        keys: Vec<ExprIR>,
        aggs: Vec<ExprIR>,
        schema: SchemaRef,
        maintain_order: bool,
        options: Arc<GroupbyOptions>,
        apply: Option<PlanCallback<DataFrame, DataFrame>>,
    },
    Join {
        input_left: Node,
        input_right: Node,
        schema: SchemaRef,
        left_on: Vec<ExprIR>,
        right_on: Vec<ExprIR>,
        options: Arc<JoinOptionsIR>,
    },
    HStack {
        input: Node,
        exprs: Vec<ExprIR>,
        schema: SchemaRef,
        options: ProjectionOptions,
    },
    Distinct {
        input: Node,
        options: DistinctOptionsIR,
    },
    MapFunction {
        input: Node,
        function: FunctionIR,
    },
    Union {
        inputs: Vec<Node>,
        options: UnionOptions,
    },
    /// Horizontal concatenation
    /// - Invariant: the names will be unique
    HConcat {
        inputs: Vec<Node>,
        schema: SchemaRef,
        options: HConcatOptions,
    },
    ExtContext {
        input: Node,
        contexts: Vec<Node>,
        schema: SchemaRef,
    },
    Sink {
        input: Node,
        payload: SinkTypeIR,
    },
    /// Node that allows for multiple plans to be executed in parallel with common subplan
    /// elimination and everything.
    SinkMultiple {
        inputs: Vec<Node>,
    },
    #[cfg(feature = "merge_sorted")]
    MergeSorted {
        input_left: Node,
        input_right: Node,
        key: PlSmallStr,
    },
    #[default]
    Invalid,
}

impl IRPlan {
    pub fn new(top: Node, ir_arena: Arena<IR>, expr_arena: Arena<AExpr>) -> Self {
        Self {
            lp_top: top,
            lp_arena: ir_arena,
            expr_arena,
        }
    }

    pub fn root(&self) -> &IR {
        self.lp_arena.get(self.lp_top)
    }

    pub fn as_ref(&self) -> IRPlanRef<'_> {
        IRPlanRef {
            lp_top: self.lp_top,
            lp_arena: &self.lp_arena,
            expr_arena: &self.expr_arena,
        }
    }

    pub fn describe(&self) -> String {
        self.as_ref().describe()
    }

    pub fn describe_tree_format(&self) -> String {
        self.as_ref().describe_tree_format()
    }

    pub fn display(&self) -> format::IRDisplay<'_> {
        self.as_ref().display()
    }

    pub fn display_dot(&self) -> dot::IRDotDisplay<'_> {
        self.as_ref().display_dot()
    }

    /// Convert to a template by replacing DataFrameScan nodes with PlaceholderScan
    pub fn to_template(&self) -> Self {
        let mut new_arena = Arena::with_capacity(self.lp_arena.len());
        let new_top = Self::convert_to_placeholder(self.lp_top, &self.lp_arena, &mut new_arena);
        Self {
            lp_top: new_top,
            lp_arena: new_arena,
            expr_arena: self.expr_arena.clone(),
        }
    }

    #[recursive::recursive]
    fn convert_to_placeholder(node: Node, old_arena: &Arena<IR>, new_arena: &mut Arena<IR>) -> Node {
        let ir = old_arena.get(node);
        let new_ir = match ir {
            IR::DataFrameScan { schema, output_schema, .. } => {
                // Replace with placeholder (no data)
                IR::PlaceholderScan {
                    schema: schema.clone(),
                    output_schema: output_schema.clone(),
                }
            }
            // For nodes with inputs, recursively process
            IR::Select { input, expr, schema, options } => {
                let new_input = Self::convert_to_placeholder(*input, old_arena, new_arena);
                IR::Select {
                    input: new_input,
                    expr: expr.clone(),
                    schema: schema.clone(),
                    options: *options,
                }
            }
            IR::Filter { input, predicate } => {
                let new_input = Self::convert_to_placeholder(*input, old_arena, new_arena);
                IR::Filter {
                    input: new_input,
                    predicate: predicate.clone(),
                }
            }
            IR::Slice { input, offset, len } => {
                let new_input = Self::convert_to_placeholder(*input, old_arena, new_arena);
                IR::Slice {
                    input: new_input,
                    offset: *offset,
                    len: *len,
                }
            }
            IR::GroupBy { input, keys, aggs, schema, maintain_order, options, apply } => {
                let new_input = Self::convert_to_placeholder(*input, old_arena, new_arena);
                IR::GroupBy {
                    input: new_input,
                    keys: keys.clone(),
                    aggs: aggs.clone(),
                    schema: schema.clone(),
                    maintain_order: *maintain_order,
                    options: options.clone(),
                    apply: apply.clone(),
                }
            }
            IR::Join { input_left, input_right, schema, left_on, right_on, options } => {
                let new_left = Self::convert_to_placeholder(*input_left, old_arena, new_arena);
                let new_right = Self::convert_to_placeholder(*input_right, old_arena, new_arena);
                IR::Join {
                    input_left: new_left,
                    input_right: new_right,
                    schema: schema.clone(),
                    left_on: left_on.clone(),
                    right_on: right_on.clone(),
                    options: options.clone(),
                }
            }
            IR::HStack { input, exprs, schema, options } => {
                let new_input = Self::convert_to_placeholder(*input, old_arena, new_arena);
                IR::HStack {
                    input: new_input,
                    exprs: exprs.clone(),
                    schema: schema.clone(),
                    options: *options,
                }
            }
            IR::SimpleProjection { input, columns } => {
                let new_input = Self::convert_to_placeholder(*input, old_arena, new_arena);
                IR::SimpleProjection {
                    input: new_input,
                    columns: columns.clone(),
                }
            }
            IR::Sort { input, by_column, slice, sort_options } => {
                let new_input = Self::convert_to_placeholder(*input, old_arena, new_arena);
                IR::Sort {
                    input: new_input,
                    by_column: by_column.clone(),
                    slice: *slice,
                    sort_options: sort_options.clone(),
                }
            }
            IR::Distinct { input, options } => {
                let new_input = Self::convert_to_placeholder(*input, old_arena, new_arena);
                IR::Distinct {
                    input: new_input,
                    options: options.clone(),
                }
            }
            IR::MapFunction { input, function } => {
                let new_input = Self::convert_to_placeholder(*input, old_arena, new_arena);
                IR::MapFunction {
                    input: new_input,
                    function: function.clone(),
                }
            }
            IR::Cache { input, id } => {
                let new_input = Self::convert_to_placeholder(*input, old_arena, new_arena);
                IR::Cache {
                    input: new_input,
                    id: *id,
                }
            }
            IR::Union { inputs, options } => {
                let new_inputs: Vec<_> = inputs
                    .iter()
                    .map(|&input| Self::convert_to_placeholder(input, old_arena, new_arena))
                    .collect();
                IR::Union {
                    inputs: new_inputs,
                    options: options.clone(),
                }
            }
            IR::HConcat { inputs, schema, options } => {
                let new_inputs: Vec<_> = inputs
                    .iter()
                    .map(|&input| Self::convert_to_placeholder(input, old_arena, new_arena))
                    .collect();
                IR::HConcat {
                    inputs: new_inputs,
                    schema: schema.clone(),
                    options: options.clone(),
                }
            }
            IR::ExtContext { input, contexts, schema } => {
                let new_input = Self::convert_to_placeholder(*input, old_arena, new_arena);
                let new_contexts: Vec<_> = contexts
                    .iter()
                    .map(|&ctx| Self::convert_to_placeholder(ctx, old_arena, new_arena))
                    .collect();
                IR::ExtContext {
                    input: new_input,
                    contexts: new_contexts,
                    schema: schema.clone(),
                }
            }
            IR::Sink { input, payload } => {
                let new_input = Self::convert_to_placeholder(*input, old_arena, new_arena);
                IR::Sink {
                    input: new_input,
                    payload: payload.clone(),
                }
            }
            IR::SinkMultiple { inputs } => {
                let new_inputs: Vec<_> = inputs
                    .iter()
                    .map(|&input| Self::convert_to_placeholder(input, old_arena, new_arena))
                    .collect();
                IR::SinkMultiple {
                    inputs: new_inputs,
                }
            }
            #[cfg(feature = "merge_sorted")]
            IR::MergeSorted { input_left, input_right, key } => {
                let new_left = Self::convert_to_placeholder(*input_left, old_arena, new_arena);
                let new_right = Self::convert_to_placeholder(*input_right, old_arena, new_arena);
                IR::MergeSorted {
                    input_left: new_left,
                    input_right: new_right,
                    key: key.clone(),
                }
            }
            // For nodes without inputs or already placeholders, clone as-is
            _ => ir.clone(),
        };
        new_arena.add(new_ir)
    }

    /// Bind a template IR plan to actual data
    ///
    /// Replaces all PlaceholderScan nodes with the provided data scan node
    pub fn bind_data(&self, data_node: Node, data_arena: &Arena<IR>) -> PolarsResult<Self> {
        let mut new_arena = Arena::with_capacity(self.lp_arena.len());
        let new_top = Self::replace_placeholder(self.lp_top, data_node, data_arena, &self.lp_arena, &mut new_arena)?;
        Ok(Self {
            lp_top: new_top,
            lp_arena: new_arena,
            expr_arena: self.expr_arena.clone(),
        })
    }

    /// Bind a template IR plan to a DataFrame
    ///
    /// Convenience method that converts the DataFrame to IR and binds it
    pub fn bind_to_df(&self, df: Arc<DataFrame>) -> PolarsResult<Self> {
        let schema = df.schema();
        let mut data_arena = Arena::with_capacity(1);
        let data_node = data_arena.add(IR::DataFrameScan {
            df,
            schema,
            output_schema: None,
        });
        self.bind_data(data_node, &data_arena)
    }

    #[recursive::recursive]
    fn replace_placeholder(
        node: Node,
        data_node: Node,
        data_arena: &Arena<IR>,
        template_arena: &Arena<IR>,
        new_arena: &mut Arena<IR>,
    ) -> PolarsResult<Node> {
        let ir = template_arena.get(node);
        let new_ir = match ir {
            IR::PlaceholderScan { schema, .. } => {
                // Validate data schema matches placeholder schema
                let data_ir = data_arena.get(data_node);
                let data_schema = match data_ir {
                    IR::DataFrameScan { schema: data_schema, .. } => data_schema,
                    _ => polars_bail!(ComputeError: "bind_data requires data to be a DataFrameScan"),
                };

                // Schema validation
                if schema.len() != data_schema.len() {
                    polars_bail!(SchemaMismatch:
                        "Schema mismatch: template expects {} columns, data has {}",
                        schema.len(),
                        data_schema.len()
                    );
                }

                // Clone the data IR node
                return Ok(new_arena.add(data_ir.clone()));
            }
            // Recursively replace in nodes with inputs
            IR::Select { input, expr, schema, options } => {
                let new_input = Self::replace_placeholder(*input, data_node, data_arena, template_arena, new_arena)?;
                IR::Select {
                    input: new_input,
                    expr: expr.clone(),
                    schema: schema.clone(),
                    options: *options,
                }
            }
            IR::Filter { input, predicate } => {
                let new_input = Self::replace_placeholder(*input, data_node, data_arena, template_arena, new_arena)?;
                IR::Filter {
                    input: new_input,
                    predicate: predicate.clone(),
                }
            }
            IR::Slice { input, offset, len } => {
                let new_input = Self::replace_placeholder(*input, data_node, data_arena, template_arena, new_arena)?;
                IR::Slice {
                    input: new_input,
                    offset: *offset,
                    len: *len,
                }
            }
            IR::GroupBy { input, keys, aggs, schema, maintain_order, options, apply } => {
                let new_input = Self::replace_placeholder(*input, data_node, data_arena, template_arena, new_arena)?;
                IR::GroupBy {
                    input: new_input,
                    keys: keys.clone(),
                    aggs: aggs.clone(),
                    schema: schema.clone(),
                    maintain_order: *maintain_order,
                    options: options.clone(),
                    apply: apply.clone(),
                }
            }
            IR::Join { input_left, input_right, schema, left_on, right_on, options } => {
                let new_left = Self::replace_placeholder(*input_left, data_node, data_arena, template_arena, new_arena)?;
                let new_right = Self::replace_placeholder(*input_right, data_node, data_arena, template_arena, new_arena)?;
                IR::Join {
                    input_left: new_left,
                    input_right: new_right,
                    schema: schema.clone(),
                    left_on: left_on.clone(),
                    right_on: right_on.clone(),
                    options: options.clone(),
                }
            }
            IR::HStack { input, exprs, schema, options } => {
                let new_input = Self::replace_placeholder(*input, data_node, data_arena, template_arena, new_arena)?;
                IR::HStack {
                    input: new_input,
                    exprs: exprs.clone(),
                    schema: schema.clone(),
                    options: *options,
                }
            }
            IR::SimpleProjection { input, columns } => {
                let new_input = Self::replace_placeholder(*input, data_node, data_arena, template_arena, new_arena)?;
                IR::SimpleProjection {
                    input: new_input,
                    columns: columns.clone(),
                }
            }
            IR::Sort { input, by_column, slice, sort_options } => {
                let new_input = Self::replace_placeholder(*input, data_node, data_arena, template_arena, new_arena)?;
                IR::Sort {
                    input: new_input,
                    by_column: by_column.clone(),
                    slice: *slice,
                    sort_options: sort_options.clone(),
                }
            }
            IR::Distinct { input, options } => {
                let new_input = Self::replace_placeholder(*input, data_node, data_arena, template_arena, new_arena)?;
                IR::Distinct {
                    input: new_input,
                    options: options.clone(),
                }
            }
            IR::MapFunction { input, function } => {
                let new_input = Self::replace_placeholder(*input, data_node, data_arena, template_arena, new_arena)?;
                IR::MapFunction {
                    input: new_input,
                    function: function.clone(),
                }
            }
            IR::Cache { input, id } => {
                let new_input = Self::replace_placeholder(*input, data_node, data_arena, template_arena, new_arena)?;
                IR::Cache {
                    input: new_input,
                    id: *id,
                }
            }
            IR::Union { inputs, options } => {
                let new_inputs: Vec<_> = inputs
                    .iter()
                    .map(|&input| Self::replace_placeholder(input, data_node, data_arena, template_arena, new_arena))
                    .collect::<PolarsResult<_>>()?;
                IR::Union {
                    inputs: new_inputs,
                    options: options.clone(),
                }
            }
            IR::HConcat { inputs, schema, options } => {
                let new_inputs: Vec<_> = inputs
                    .iter()
                    .map(|&input| Self::replace_placeholder(input, data_node, data_arena, template_arena, new_arena))
                    .collect::<PolarsResult<_>>()?;
                IR::HConcat {
                    inputs: new_inputs,
                    schema: schema.clone(),
                    options: options.clone(),
                }
            }
            IR::ExtContext { input, contexts, schema } => {
                let new_input = Self::replace_placeholder(*input, data_node, data_arena, template_arena, new_arena)?;
                let new_contexts: Vec<_> = contexts
                    .iter()
                    .map(|&ctx| Self::replace_placeholder(ctx, data_node, data_arena, template_arena, new_arena))
                    .collect::<PolarsResult<_>>()?;
                IR::ExtContext {
                    input: new_input,
                    contexts: new_contexts,
                    schema: schema.clone(),
                }
            }
            IR::Sink { input, payload } => {
                let new_input = Self::replace_placeholder(*input, data_node, data_arena, template_arena, new_arena)?;
                IR::Sink {
                    input: new_input,
                    payload: payload.clone(),
                }
            }
            IR::SinkMultiple { inputs } => {
                let new_inputs: Vec<_> = inputs
                    .iter()
                    .map(|&input| Self::replace_placeholder(input, data_node, data_arena, template_arena, new_arena))
                    .collect::<PolarsResult<_>>()?;
                IR::SinkMultiple {
                    inputs: new_inputs,
                }
            }
            #[cfg(feature = "merge_sorted")]
            IR::MergeSorted { input_left, input_right, key } => {
                let new_left = Self::replace_placeholder(*input_left, data_node, data_arena, template_arena, new_arena)?;
                let new_right = Self::replace_placeholder(*input_right, data_node, data_arena, template_arena, new_arena)?;
                IR::MergeSorted {
                    input_left: new_left,
                    input_right: new_right,
                    key: key.clone(),
                }
            }
            // For nodes without inputs, just clone
            _ => ir.clone(),
        };
        Ok(new_arena.add(new_ir))
    }
}

impl<'a> IRPlanRef<'a> {
    pub fn root(self) -> &'a IR {
        self.lp_arena.get(self.lp_top)
    }

    pub fn with_root(self, root: Node) -> Self {
        Self {
            lp_top: root,
            lp_arena: self.lp_arena,
            expr_arena: self.expr_arena,
        }
    }

    pub fn display(self) -> format::IRDisplay<'a> {
        format::IRDisplay::new(self)
    }

    pub fn display_dot(self) -> dot::IRDotDisplay<'a> {
        dot::IRDotDisplay::new(self)
    }

    pub fn describe(self) -> String {
        self.display().to_string()
    }

    pub fn describe_tree_format(self) -> String {
        let mut visitor = tree_format::TreeFmtVisitor::default();
        tree_format::TreeFmtNode::root_logical_plan(self).traverse(&mut visitor);
        format!("{visitor:#?}")
    }
}

impl fmt::Debug for IRPlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        <format::IRDisplay as fmt::Display>::fmt(&self.display(), f)
    }
}

impl fmt::Debug for IRPlanRef<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        <format::IRDisplay as fmt::Display>::fmt(&self.display(), f)
    }
}

#[cfg(test)]
mod test {
    use super::*;

    // skipped for now
    #[ignore]
    #[test]
    fn test_alp_size() {
        assert!(size_of::<IR>() <= 152);
    }
}
