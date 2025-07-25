use std::cmp::Ordering;
use std::fmt::Display;
use std::fmt::Formatter;
use std::ops::Deref;
use std::sync::Arc;

use apollo_compiler::ast::Value;
use apollo_compiler::collections::IndexSet;
use itertools::Itertools;
use petgraph::graph::EdgeIndex;
use tracing::debug;
use tracing::debug_span;

use crate::display_helpers::DisplayOption;
use crate::display_helpers::DisplaySlice;
use crate::display_helpers::State as IndentedFormatter;
use crate::display_helpers::write_indented_lines;
use crate::error::FederationError;
use crate::error::SingleFederationError;
use crate::is_leaf_type;
use crate::link::federation_spec_definition::get_federation_spec_definition_from_subgraph;
use crate::link::graphql_definition::BooleanOrVariable;
use crate::link::graphql_definition::DeferDirectiveArguments;
use crate::link::graphql_definition::OperationConditional;
use crate::link::graphql_definition::OperationConditionalKind;
use crate::operation::DirectiveList;
use crate::operation::Field;
use crate::operation::HasSelectionKey;
use crate::operation::InlineFragment;
use crate::operation::Selection;
use crate::operation::SelectionId;
use crate::operation::SelectionKey;
use crate::operation::SelectionSet;
use crate::operation::SiblingTypename;
use crate::query_graph::OverrideConditions;
use crate::query_graph::QueryGraphEdgeTransition;
use crate::query_graph::QueryGraphNodeType;
use crate::query_graph::condition_resolver::ConditionResolution;
use crate::query_graph::condition_resolver::ConditionResolver;
use crate::query_graph::graph_path::ExcludedConditions;
use crate::query_graph::graph_path::ExcludedDestinations;
use crate::query_graph::graph_path::GraphPath;
use crate::query_graph::graph_path::GraphPathTriggerVariant;
use crate::query_graph::graph_path::IndirectPaths;
use crate::query_graph::graph_path::OverrideId;
use crate::query_graph::path_tree::Preference;
use crate::query_plan::FetchDataPathElement;
use crate::schema::ValidFederationSchema;
use crate::schema::position::AbstractTypeDefinitionPosition;
use crate::schema::position::CompositeTypeDefinitionPosition;
use crate::schema::position::InterfaceFieldDefinitionPosition;
use crate::schema::position::ObjectOrInterfaceTypeDefinitionPosition;
use crate::schema::position::OutputTypeDefinitionPosition;
use crate::schema::position::TypeDefinitionPosition;
use crate::utils::logging::format_open_branch;

/// A `GraphPath` whose triggers are operation elements (essentially meaning that the path has been
/// guided by a GraphQL operation).
// PORT_NOTE: As noted in the docs for `GraphPath`, we omit a type parameter for the root node,
// whose constraint is instead checked at runtime. This means the `OpRootPath` type in the JS
// codebase is replaced with this one.
pub(crate) type OpGraphPath = GraphPath<OpGraphPathTrigger, Option<EdgeIndex>>;

#[derive(Debug, Clone, PartialEq, Eq, Hash, derive_more::From, serde::Serialize)]
pub(crate) enum OpGraphPathTrigger {
    OpPathElement(OpPathElement),
    Context(OpGraphPathContext),
}

impl Display for OpGraphPathTrigger {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            OpGraphPathTrigger::OpPathElement(ele) => ele.fmt(f),
            OpGraphPathTrigger::Context(ctx) => ctx.fmt(f),
        }
    }
}

impl GraphPathTriggerVariant for OpGraphPathTrigger {
    fn get_field_parent_type(&self) -> Option<CompositeTypeDefinitionPosition> {
        match self {
            OpGraphPathTrigger::OpPathElement(OpPathElement::Field(field)) => {
                Some(field.field_position.parent())
            }
            _ => None,
        }
    }

    fn get_field_mut(&mut self) -> Option<&mut Field> {
        match self {
            OpGraphPathTrigger::OpPathElement(OpPathElement::Field(field)) => Some(field),
            _ => None,
        }
    }

    fn get_op_path_element(&self) -> Option<&OpPathElement> {
        match self {
            OpGraphPathTrigger::OpPathElement(ele) => Some(ele),
            OpGraphPathTrigger::Context(_) => None,
        }
    }
}

/// A path of operation elements within a GraphQL operation.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, serde::Serialize)]
pub(crate) struct OpPath(pub(crate) Vec<Arc<OpPathElement>>);

impl Deref for OpPath {
    type Target = [Arc<OpPathElement>];
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Display for OpPath {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        for (i, element) in self.0.iter().enumerate() {
            if i > 0 {
                write!(f, "::")?;
            }
            match element.deref() {
                OpPathElement::Field(field) => write!(f, "{field}")?,
                OpPathElement::InlineFragment(fragment) => write!(f, "{fragment}")?,
            }
        }
        Ok(())
    }
}

impl Preference for OpPathElement {
    fn preferred_over(&self, other: &Self) -> Option<bool> {
        match (self, other) {
            (OpPathElement::Field(x), OpPathElement::Field(y)) => {
                // We prefer the one with a sibling typename (= Less).
                // Otherwise, not comparable.
                match (&x.sibling_typename, &y.sibling_typename) {
                    (Some(_), None) => Some(true),
                    (None, Some(_)) => Some(false),
                    _ => None,
                }
            }
            _ => None,
        }
    }
}

impl Preference for OpGraphPathTrigger {
    fn preferred_over(&self, other: &Self) -> Option<bool> {
        match (self, other) {
            (OpGraphPathTrigger::OpPathElement(x), OpGraphPathTrigger::OpPathElement(y)) => {
                x.preferred_over(y)
            }
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, derive_more::From, serde::Serialize)]
pub(crate) enum OpPathElement {
    Field(Field),
    InlineFragment(InlineFragment),
}

impl HasSelectionKey for OpPathElement {
    fn key(&self) -> SelectionKey {
        match self {
            OpPathElement::Field(field) => field.key(),
            OpPathElement::InlineFragment(fragment) => fragment.key(),
        }
    }
}

impl OpPathElement {
    pub(crate) fn directives(&self) -> &DirectiveList {
        match self {
            OpPathElement::Field(field) => &field.directives,
            OpPathElement::InlineFragment(inline_fragment) => &inline_fragment.directives,
        }
    }

    pub(crate) fn schema(&self) -> &ValidFederationSchema {
        match self {
            OpPathElement::Field(field) => field.schema(),
            OpPathElement::InlineFragment(fragment) => fragment.schema(),
        }
    }

    pub(crate) fn is_terminal(&self) -> Result<bool, FederationError> {
        match self {
            OpPathElement::Field(field) => field.is_leaf(),
            OpPathElement::InlineFragment(_) => Ok(false),
        }
    }

    pub(crate) fn sibling_typename(&self) -> Option<&SiblingTypename> {
        match self {
            OpPathElement::Field(field) => field.sibling_typename(),
            OpPathElement::InlineFragment(_) => None,
        }
    }

    pub(crate) fn parent_type_position(&self) -> CompositeTypeDefinitionPosition {
        match self {
            OpPathElement::Field(field) => field.field_position.parent(),
            OpPathElement::InlineFragment(inline) => inline.parent_type_position.clone(),
        }
    }

    pub(crate) fn sub_selection_type_position(
        &self,
    ) -> Result<Option<CompositeTypeDefinitionPosition>, FederationError> {
        match self {
            OpPathElement::Field(field) => Ok(field.output_base_type()?.try_into().ok()),
            OpPathElement::InlineFragment(inline) => Ok(Some(inline.casted_type())),
        }
    }

    pub(crate) fn extract_operation_conditionals(
        &self,
    ) -> Result<Vec<OperationConditional>, FederationError> {
        let mut conditionals = vec![];
        // PORT_NOTE: We explicitly use the order `Skip` and `Include` here, to align with the order
        // used by the JS codebase.
        for kind in [
            OperationConditionalKind::Skip,
            OperationConditionalKind::Include,
        ] {
            let directive_name: &'static str = (&kind).into();
            if let Some(application) = self.directives().get(directive_name) {
                let Some(arg) = application.specified_argument_by_name("if") else {
                    return Err(FederationError::internal(format!(
                        "@{} missing required argument \"if\"",
                        directive_name
                    )));
                };
                let value = match arg.deref() {
                    Value::Variable(variable_name) => {
                        BooleanOrVariable::Variable(variable_name.clone())
                    }
                    Value::Boolean(boolean) => BooleanOrVariable::Boolean(*boolean),
                    _ => {
                        return Err(FederationError::internal(format!(
                            "@{} has invalid value {} for argument \"if\"",
                            directive_name,
                            arg.serialize().no_indent()
                        )));
                    }
                };
                conditionals.push(OperationConditional { kind, value })
            }
        }
        Ok(conditionals)
    }

    pub(crate) fn with_updated_directives(&self, directives: DirectiveList) -> OpPathElement {
        match self {
            OpPathElement::Field(field) => {
                OpPathElement::Field(field.with_updated_directives(directives))
            }
            OpPathElement::InlineFragment(inline_fragment) => {
                OpPathElement::InlineFragment(inline_fragment.with_updated_directives(directives))
            }
        }
    }

    pub(crate) fn as_path_element(&self) -> Option<FetchDataPathElement> {
        match self {
            OpPathElement::Field(field) => Some(field.as_path_element()),
            OpPathElement::InlineFragment(inline_fragment) => inline_fragment.as_path_element(),
        }
    }

    pub(crate) fn defer_directive_args(&self) -> Option<DeferDirectiveArguments> {
        match self {
            OpPathElement::Field(_) => None, // @defer cannot be on field at the moment
            OpPathElement::InlineFragment(inline_fragment) => {
                inline_fragment.defer_directive_arguments().ok().flatten()
            }
        }
    }

    pub(crate) fn has_defer(&self) -> bool {
        match self {
            OpPathElement::Field(_) => false,
            OpPathElement::InlineFragment(inline_fragment) => {
                inline_fragment.directives.has("defer")
            }
        }
    }

    /// Returns this fragment element but with any @defer directive on it removed.
    ///
    /// This method will return `None` if, upon removing @defer, the fragment has no conditions nor
    /// any remaining applied directives (meaning that it carries no information whatsoever and can be
    /// ignored).
    pub(crate) fn without_defer(&self) -> Option<Self> {
        match self {
            Self::Field(_) => Some(self.clone()),
            Self::InlineFragment(inline_fragment) => {
                let updated_directives: DirectiveList = inline_fragment
                    .directives
                    .iter()
                    .filter(|directive| directive.name != "defer")
                    .cloned()
                    .collect();
                if inline_fragment.type_condition_position.is_none()
                    && updated_directives.is_empty()
                {
                    return None;
                }
                if inline_fragment.directives.len() == updated_directives.len() {
                    Some(self.clone())
                } else {
                    // PORT_NOTE: We won't need to port `this.copyAttachementsTo(updated);` line here
                    // since `with_updated_directives` clones the whole `self` and thus sibling
                    // type names should be copied as well.
                    Some(self.with_updated_directives(updated_directives))
                }
            }
        }
    }

    pub(crate) fn rebase_on(
        &self,
        parent_type: &CompositeTypeDefinitionPosition,
        schema: &ValidFederationSchema,
    ) -> Result<OpPathElement, FederationError> {
        match self {
            OpPathElement::Field(field) => Ok(field.rebase_on(parent_type, schema)?.into()),
            OpPathElement::InlineFragment(inline) => {
                Ok(inline.rebase_on(parent_type, schema)?.into())
            }
        }
    }
}

impl Display for OpPathElement {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            OpPathElement::Field(field) => field.fmt(f),
            OpPathElement::InlineFragment(inline_fragment) => inline_fragment.fmt(f),
        }
    }
}

impl From<Field> for OpGraphPathTrigger {
    fn from(value: Field) -> Self {
        OpPathElement::from(value).into()
    }
}

impl From<InlineFragment> for OpGraphPathTrigger {
    fn from(value: InlineFragment) -> Self {
        OpPathElement::from(value).into()
    }
}

/// Records, as we walk a path within a GraphQL operation, important directives encountered
/// (currently `@include` and `@skip` with their conditions).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, serde::Serialize)]
pub(crate) struct OpGraphPathContext {
    /// A list of conditionals (e.g. `[{ kind: Include, value: true}, { kind: Skip, value: $foo }]`)
    /// in the reverse order in which they were applied (so the first element is the inner-most
    /// applied include/skip).
    conditionals: Arc<Vec<OperationConditional>>,
}

impl OpGraphPathContext {
    pub(crate) fn with_context_of(
        &self,
        operation_element: &OpPathElement,
    ) -> Result<OpGraphPathContext, FederationError> {
        if operation_element.directives().is_empty() {
            return Ok(self.clone());
        }

        let mut new_conditionals = operation_element.extract_operation_conditionals()?;
        if new_conditionals.is_empty() {
            return Ok(self.clone());
        }
        new_conditionals.extend(self.iter().cloned());
        Ok(OpGraphPathContext {
            conditionals: Arc::new(new_conditionals),
        })
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.conditionals.is_empty()
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &OperationConditional> {
        self.conditionals.iter()
    }
}

impl Display for OpGraphPathContext {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "[")?;
        let mut iter = self.conditionals.iter();
        if let Some(cond) = iter.next() {
            write!(f, "@{}(if: {})", cond.kind, cond.value)?;
            iter.try_for_each(|cond| write!(f, ", @{}(if: {})", cond.kind, cond.value))?;
        }
        write!(f, "]")
    }
}

/// A vector of graph paths that are being considered simultaneously by the query planner as an
/// option for a path within a GraphQL operation. These arise since the edge to take in a query
/// graph may depend on outcomes that are only known at query plan execution time, and we account
/// for this by splitting a path into multiple paths (one for each possible outcome). The common
/// example is abstract types, where we may end up taking a different edge depending on the runtime
/// type (e.g. during type explosion).
#[derive(Clone, serde::Serialize)]
pub(crate) struct SimultaneousPaths(pub(crate) Vec<Arc<OpGraphPath>>);

impl SimultaneousPaths {
    pub(crate) fn fmt_indented(&self, f: &mut IndentedFormatter) -> std::fmt::Result {
        match self.0.as_slice() {
            [] => f.write("<no path>"),

            [first] => f.write_fmt(format_args!("{first}")),

            _ => {
                f.write("{")?;
                write_indented_lines(f, &self.0, |f, elem| f.write(elem))?;
                f.write("}")
            }
        }
    }
}

impl std::fmt::Debug for SimultaneousPaths {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_list()
            .entries(self.0.iter().map(ToString::to_string))
            .finish()
    }
}

impl Display for SimultaneousPaths {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        self.fmt_indented(&mut IndentedFormatter::new(f))
    }
}

/// One of the options for an `OpenBranch` (see the documentation of that struct for details). This
/// includes the simultaneous paths we are traversing for the option, along with metadata about the
/// traversal.
// PORT_NOTE: The JS codebase stored a `ConditionResolver` callback here, but it was the same for
// a given traversal (and cached resolution across the traversal), so we accordingly store it in
// `QueryPlanTraversal` and pass it down when needed instead.
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct SimultaneousPathsWithLazyIndirectPaths {
    pub(crate) paths: SimultaneousPaths,
    pub(crate) context: OpGraphPathContext,
    pub(crate) excluded_destinations: ExcludedDestinations,
    pub(crate) excluded_conditions: ExcludedConditions,
    pub(crate) lazily_computed_indirect_paths: Vec<Option<OpIndirectPaths>>,
}

impl Display for SimultaneousPathsWithLazyIndirectPaths {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.paths)
    }
}

type OpIndirectPaths = IndirectPaths<OpGraphPathTrigger, Option<EdgeIndex>, ()>;

impl Clone for OpIndirectPaths {
    fn clone(&self) -> Self {
        Self {
            paths: self.paths.clone(),
            dead_ends: (),
        }
    }
}

impl std::fmt::Debug for OpIndirectPaths {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpIndirectPaths")
            .field(
                "paths",
                &self
                    .paths
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>(),
            )
            .field("dead_ends", &self.dead_ends)
            .finish()
    }
}

impl OpIndirectPaths {
    /// When `self` is just-computed indirect paths and given a field that we're trying to advance
    /// after those paths, this method filters any paths that should not be considered.
    ///
    /// Currently, this handles the case where the key used at the end of the indirect path contains
    /// (at top level) the field being queried. Or to make this more concrete, if we're trying to
    /// collect field `id`, and the path's last edge was using key `id`, then we can ignore that
    /// path because this implies that there is a way to fetch `id` "some other way".
    pub(crate) fn filter_non_collecting_paths_for_field(
        &self,
        field: &Field,
    ) -> Result<OpIndirectPaths, FederationError> {
        // We only handle leaves; Things are more complex for non-leaves.
        if !field.is_leaf()? {
            return Ok(self.clone());
        }

        let mut filtered = vec![];
        for path in self.paths.iter() {
            if let Some(Some(last_edge)) = path.edges.last() {
                let last_edge_weight = path.graph.edge_weight(*last_edge)?;
                if matches!(
                    last_edge_weight.transition,
                    QueryGraphEdgeTransition::KeyResolution
                ) {
                    if let Some(conditions) = &last_edge_weight.conditions {
                        if conditions.contains_top_level_field(field)? {
                            continue;
                        }
                    }
                }
            }
            filtered.push(path.clone())
        }
        Ok(if filtered.len() == self.paths.len() {
            self.clone()
        } else {
            OpIndirectPaths {
                paths: Arc::new(filtered),
                dead_ends: (),
            }
        })
    }
}

/// One of the options for a `ClosedBranch` (see the documentation of that struct for details). Note
/// there is an optimization here, in that if some ending section of the path within the GraphQL
/// operation can be satisfied by a query to a single subgraph, then we just record that selection
/// set, and the `SimultaneousPaths` ends at the node at which that query is made instead of a node
/// for the leaf field. The selection set gets copied "as-is" into the `FetchNode`, and also avoids
/// extra `GraphPath` creation and work during `PathTree` merging.
#[derive(Debug, serde::Serialize)]
pub(crate) struct ClosedPath {
    pub(crate) paths: SimultaneousPaths,
    pub(crate) selection_set: Option<Arc<SelectionSet>>,
}

impl ClosedPath {
    pub(crate) fn flatten(
        &self,
    ) -> impl Iterator<Item = (&OpGraphPath, Option<&Arc<SelectionSet>>)> {
        self.paths
            .0
            .iter()
            .map(|path| (path.as_ref(), self.selection_set.as_ref()))
    }
}

impl Display for ClosedPath {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        if let Some(ref selection_set) = self.selection_set {
            write!(f, "{} -> {}", self.paths, selection_set)
        } else {
            write!(f, "{}", self.paths)
        }
    }
}

/// A list of the options generated during query planning for a specific "closed branch", which is a
/// full/closed path in a GraphQL operation (i.e. one that ends in a leaf field).
#[derive(Debug, serde::Serialize)]
pub(crate) struct ClosedBranch(pub(crate) Vec<Arc<ClosedPath>>);

/// A list of the options generated during query planning for a specific "open branch", which is a
/// partial/open path in a GraphQL operation (i.e. one that does not end in a leaf field).
#[derive(Debug, serde::Serialize)]
pub(crate) struct OpenBranch(pub(crate) Vec<SimultaneousPathsWithLazyIndirectPaths>);

#[derive(Debug, serde::Serialize)]
pub(crate) struct OpenBranchAndSelections {
    /// The options for this open branch.
    pub(crate) open_branch: OpenBranch,
    /// A stack of the remaining selections to plan from the node this open branch ends on.
    pub(crate) selections: Vec<Selection>,
}

impl Display for OpenBranchAndSelections {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let Some((current_selection, remaining_selections)) = self.selections.split_last() else {
            return Ok(());
        };
        format_open_branch(f, &(current_selection, &self.open_branch.0))?;
        write!(f, " * Remaining selections:")?;
        if remaining_selections.is_empty() {
            writeln!(f, " (none)")?;
        } else {
            // Print in reverse order since remaining selections are processed in that order.
            writeln!(f)?; // newline
            for selection in remaining_selections.iter().rev() {
                writeln!(f, "   - {selection}")?;
            }
        }
        Ok(())
    }
}

impl OpGraphPath {
    fn next_edge_for_field(
        &self,
        field: &Field,
        override_conditions: &OverrideConditions,
    ) -> Option<EdgeIndex> {
        self.graph
            .edge_for_field(self.tail, field, override_conditions)
    }

    fn next_edge_for_inline_fragment(&self, inline_fragment: &InlineFragment) -> Option<EdgeIndex> {
        self.graph
            .edge_for_inline_fragment(self.tail, inline_fragment)
    }

    fn add_field_edge(
        &self,
        operation_field: Field,
        edge: EdgeIndex,
        condition_resolver: &mut impl ConditionResolver,
        context: &OpGraphPathContext,
    ) -> Result<Option<OpGraphPath>, FederationError> {
        let condition_resolution = self.can_satisfy_conditions(
            edge,
            condition_resolver,
            context,
            &Default::default(),
            &Default::default(),
        )?;
        if matches!(condition_resolution, ConditionResolution::Satisfied { .. }) {
            self.add(
                operation_field.into(),
                edge.into(),
                condition_resolution,
                None,
            )
            .map(Some)
        } else {
            Ok(None)
        }
    }

    pub(crate) fn mark_overriding(
        &self,
        others: &[SimultaneousPaths],
    ) -> (OpGraphPath, Vec<SimultaneousPaths>) {
        let new_id = OverrideId::new();
        let mut new_own_path_ids = self.overriding_path_ids.as_ref().clone();
        new_own_path_ids.insert(new_id);
        let new_self = OpGraphPath {
            own_path_ids: Arc::new(new_own_path_ids),
            ..self.clone()
        };
        let new_others = others
            .iter()
            .map(|option| {
                SimultaneousPaths(
                    option
                        .0
                        .iter()
                        .map(|path| {
                            let mut new_overriding_path_ids =
                                path.overriding_path_ids.as_ref().clone();
                            new_overriding_path_ids.insert(new_id);
                            Arc::new(OpGraphPath {
                                overriding_path_ids: Arc::new(new_overriding_path_ids),
                                ..path.as_ref().clone()
                            })
                        })
                        .collect(),
                )
            })
            .collect();
        (new_self, new_others)
    }

    pub(crate) fn subgraph_jumps(&self) -> Result<u32, FederationError> {
        self.subgraph_jumps_at_idx(0)
    }

    fn subgraph_jumps_at_idx(&self, start_index: usize) -> Result<u32, FederationError> {
        self.edges[start_index..]
            .iter()
            .flatten()
            .try_fold(0, |sum, &edge_index| {
                let (start, end) = self.graph.edge_endpoints(edge_index)?;
                let start = self.graph.node_weight(start)?;
                let end = self.graph.node_weight(end)?;
                let changes_subgraph = start.source != end.source;
                Ok(sum + if changes_subgraph { 1 } else { 0 })
            })
    }

    fn find_longest_common_prefix_length(
        &self,
        other: &OpGraphPath,
    ) -> Result<usize, FederationError> {
        if self.head != other.head {
            return Err(FederationError::internal(
                "Paths unexpectedly did not start at the same node.",
            ));
        }

        Ok(self
            .edges
            .iter()
            .zip(&other.edges)
            .position(|(self_edge, other_edge)| self_edge != other_edge)
            .unwrap_or_else(|| self.edges.len().min(other.edges.len())))
    }

    /// Looks for the longest common prefix for `self` and `other` (assuming that both paths are
    /// built as options for the same "query path"), and then compares whether each path has
    /// subgraph jumps after said prefix.
    ///
    /// Note this method always return something, but the longest common prefix considered may very
    /// well be empty. Also note that this method assumes that the 2 paths have the same root, and
    /// will fail if that's not the case.
    ///
    /// Returns the comparison of whether `self` and `other` have subgraph jumps after said prefix
    /// (e.g. `Ordering::Less` means `self` has zero subgraph jumps after said prefix while `other`
    /// has at least one). If they both have subgraph jumps or neither has subgraph jumps, then we
    /// return `Ordering::Equal`.
    fn compare_subgraph_jumps_after_last_common_node(
        &self,
        other: &OpGraphPath,
    ) -> Result<Ordering, FederationError> {
        let longest_common_prefix_len = self.find_longest_common_prefix_length(other)?;
        let self_jumps = self.subgraph_jumps_at_idx(longest_common_prefix_len)? > 0;
        let other_jumps = other.subgraph_jumps_at_idx(longest_common_prefix_len)? > 0;
        Ok(self_jumps.cmp(&other_jumps))
    }

    pub(crate) fn terminate_with_non_requested_typename_field(
        &self,
        override_conditions: &OverrideConditions,
    ) -> Result<OpGraphPath, FederationError> {
        // If the last step of the path was a fragment/type-condition, we want to remove it before
        // we get __typename. The reason is that this avoid cases where this method would make us
        // build plans like:
        // {
        //   foo {
        //     __typename
        //     ... on A {
        //       __typename
        //     }
        //     ... on B {
        //       __typename
        //     }
        // }
        // Instead, we just generate:
        // {
        //   foo {
        //     __typename
        //   }
        // }
        // Note it's ok to do this because the __typename we add is _not_ requested, it is just
        // added in cases where we need to ensure a selection is not empty, and so this
        // transformation is fine to do.
        let path = self.truncate_trailing_downcasts()?;
        let tail_weight = self.graph.node_weight(path.tail)?;
        let QueryGraphNodeType::SchemaType(tail_type_pos) = &tail_weight.type_ else {
            return Err(FederationError::internal(
                "Unexpectedly found federated root node as tail",
            ));
        };
        let Ok(tail_type_pos) = CompositeTypeDefinitionPosition::try_from(tail_type_pos.clone())
        else {
            return Ok(path);
        };
        let typename_field = Field::new_introspection_typename(
            self.graph.schema_by_source(&tail_weight.source)?,
            &tail_type_pos,
            None,
        );
        let Some(edge) = self
            .graph
            .edge_for_field(path.tail, &typename_field, override_conditions)
        else {
            return Err(FederationError::internal(
                "Unexpectedly missing edge for __typename field",
            ));
        };
        path.add(
            typename_field.into(),
            Some(edge),
            ConditionResolution::no_conditions(),
            None,
        )
    }

    /// Remove all trailing downcast edges and `None` edges.
    fn truncate_trailing_downcasts(&self) -> Result<OpGraphPath, FederationError> {
        let mut runtime_types = Arc::new(self.head_possible_runtime_types()?);
        let mut last_edge_index = None;
        let mut last_runtime_types = runtime_types.clone();
        for (edge_index, edge) in self.edges.iter().enumerate() {
            runtime_types = Arc::new(
                self.graph
                    .advance_possible_runtime_types(&runtime_types, *edge)?,
            );
            let Some(edge) = edge else {
                continue;
            };
            let edge_weight = self.graph.edge_weight(*edge)?;
            if !matches!(
                edge_weight.transition,
                QueryGraphEdgeTransition::Downcast { .. }
            ) {
                last_edge_index = Some(edge_index);
                last_runtime_types = runtime_types.clone();
            }
        }
        let Some(last_edge_index) = last_edge_index else {
            // PORT_NOTE: The JS codebase just returns the same path if all edges are downcast or
            // `None` edges. This is likely a bug, so we instead return the empty path here.
            return OpGraphPath::new(self.graph.clone(), self.head);
        };
        let prefix_length = last_edge_index + 1;
        if prefix_length == self.edges.len() {
            return Ok(self.clone());
        }
        let Some(last_edge) = self.edges[last_edge_index] else {
            return Err(FederationError::internal(
                "Unexpectedly found None for last non-downcast, non-None edge",
            ));
        };
        let (_, last_edge_tail) = self.graph.edge_endpoints(last_edge)?;
        Ok(OpGraphPath {
            graph: self.graph.clone(),
            head: self.head,
            tail: last_edge_tail,
            edges: self.edges[0..prefix_length].to_vec(),
            edge_triggers: self.edge_triggers[0..prefix_length].to_vec(),
            edge_conditions: self.edge_conditions[0..prefix_length].to_vec(),
            last_subgraph_entering_edge_info: self.last_subgraph_entering_edge_info.clone(),
            own_path_ids: self.own_path_ids.clone(),
            overriding_path_ids: self.overriding_path_ids.clone(),
            runtime_types_of_tail: last_runtime_types,
            runtime_types_before_tail_if_last_is_cast: None,
            // TODO: The JS codebase copied this from the current path, which seems like a bug.
            defer_on_tail: self.defer_on_tail.clone(),
            // PORT_NOTE: The JS codebase doesn't properly truncate these fields, this is a bug
            // which we fix here.
            matching_context_ids: self.matching_context_ids[0..prefix_length].to_vec(),
            arguments_to_context_usages: self.arguments_to_context_usages[0..prefix_length]
                .to_vec(),
        })
    }

    pub(crate) fn is_equivalent_save_for_type_explosion_to(
        &self,
        other: &OpGraphPath,
    ) -> Result<bool, FederationError> {
        // We're looking at the specific case where both paths are basically equivalent except for a
        // single step of type-explosion, so if either of the paths don't start and end on the
        // same node, or if `other` is not exactly 1 more step than `self`, we're done.
        if !(self.head == other.head
            && self.tail == other.tail
            && self.edges.len() == other.edges.len() - 1)
        {
            return Ok(false);
        }

        // If the above is true, then we find the first difference in the paths.
        let Some(diff_pos) = self
            .edges
            .iter()
            .zip(&other.edges)
            .position(|(self_edge, other_edge)| self_edge != other_edge)
        else {
            // All edges are the same, but the `other` path has an extra edge. This can't be a type
            // explosion + key resolution, so we consider them not equivalent here.
            //
            // PORT_NOTE: The JS codebase returns `true` here, claiming the paths are the same. This
            // isn't true though as we're skipping the last element of `other` in the JS codebase
            // (and while that edge can't change the `tail`, it doesn't mean that `self` subsumes
            // `other`). We fix this bug here by returning `false` instead of `true`.
            return Ok(false);
        };

        // If the first difference is not a "type-explosion", i.e. if `other` is a cast from an
        // interface to one of the implementation, then we're not in the case we're looking for.
        let Some(self_edge) = self.edges[diff_pos] else {
            return Ok(false);
        };
        let Some(other_edge) = other.edges[diff_pos] else {
            return Ok(false);
        };
        let other_edge_weight = other.graph.edge_weight(other_edge)?;
        let QueryGraphEdgeTransition::Downcast {
            from_type_position, ..
        } = &other_edge_weight.transition
        else {
            return Ok(false);
        };
        if !matches!(
            from_type_position,
            CompositeTypeDefinitionPosition::Interface(_)
        ) {
            return Ok(false);
        }

        // At this point, we want both paths to take the "same" key, but because one is starting
        // from the interface while the other one from an implementation, they won't be technically
        // the "same" edge index. So we check that both are key-resolution edges, to the same
        // subgraph and type, and with the same condition.
        let Some(other_next_edge) = other.edges[diff_pos + 1] else {
            return Ok(false);
        };
        let (_, self_edge_tail) = other.graph.edge_endpoints(self_edge)?;
        let self_edge_weight = other.graph.edge_weight(self_edge)?;
        let (_, other_next_edge_tail) = other.graph.edge_endpoints(other_next_edge)?;
        let other_next_edge_weight = other.graph.edge_weight(other_next_edge)?;
        if !(matches!(
            self_edge_weight.transition,
            QueryGraphEdgeTransition::KeyResolution
        ) && matches!(
            other_next_edge_weight.transition,
            QueryGraphEdgeTransition::KeyResolution
        ) && self_edge_tail == other_next_edge_tail
            && self_edge_weight.conditions == other_next_edge_weight.conditions)
        {
            return Ok(false);
        }

        // So far, so good. Check that the rest of the paths are equal. Note that starts with
        // `diff_pos + 1` for `self`, but `diff_pos + 2` for `other` since we looked at two edges
        // there instead of one.
        Ok(self.edges[(diff_pos + 1)..]
            .iter()
            .zip(other.edges[(diff_pos + 2)..].iter())
            .all(|(self_edge, other_edge)| self_edge == other_edge))
    }

    /// This method is used to detect when using an interface field "directly" could fail (i.e. lead
    /// to a dead end later for the query path) while type-exploding may succeed.
    ///
    /// In general, taking a field from an interface directly or through it's implementation by
    /// type-exploding leads to the same option, and so taking one or the other is more of a matter
    /// of "which is more efficient". But there is a special case where this may not be true, and
    /// this is when all of the following hold:
    /// 1. The interface is implemented by an entity type.
    /// 2. The field being looked at is @shareable.
    /// 3. The field type has a different set of fields (and less fields) in the "current" subgraph
    ///    than in another one.
    ///
    /// For instance, consider if some Subgraph A has this schema:
    /// """
    /// type Query {
    ///   i: I
    /// }
    ///
    /// interface I {
    ///   s: S
    /// }
    ///
    /// type T implements I @key(fields: "id") {
    ///   id: ID!
    ///   s: S @shareable
    /// }
    ///
    /// type S @shareable {
    ///   x: Int
    /// }
    /// """
    /// and if some Subgraph B has this schema:
    /// """
    /// type T @key(fields: "id") {
    ///   id: ID!
    ///   s: S @shareable
    /// }
    ///
    /// type S @shareable {
    ///   x: Int
    ///   y: Int
    /// }
    /// """
    /// and suppose that `{ i { s { y } } }` is queried. If we follow `I.s` in subgraph A then the
    /// `y` field cannot be found, because `S` not being an entity means we cannot "jump" to
    /// subgraph B (even if it was, there may not be a usable key to jump between the 2 subgraphs).
    /// However, if we "type-explode" into implementation `T`, then we can jump to subgraph B from
    /// that, at which point we can reach `y`.
    ///
    /// So the goal of this method is to detect when we might be in such a case: when we are, we
    /// will have to consider type explosion on top of the direct route in case that direct route
    /// ends up "not panning out" (note that by the time this method is called, we're only looking
    /// at the options for type `I.s`; we do not know yet if `y` is queried next and so cannot tell
    /// if type explosion will be necessary or not).
    // PORT_NOTE: In the JS code, this method was a free-standing function called "anImplementationIsEntityWithFieldShareable".
    fn has_an_entity_implementation_with_shareable_field(
        &self,
        source: &Arc<str>,
        interface_field_pos: InterfaceFieldDefinitionPosition,
    ) -> Result<bool, FederationError> {
        let fed_schema = self.graph.schema_by_source(source)?;
        let schema = fed_schema.schema();
        let fed_spec = get_federation_spec_definition_from_subgraph(fed_schema)?;
        let key_directive = fed_spec.key_directive_definition(fed_schema)?;
        let shareable_directive = fed_spec.shareable_directive_definition(fed_schema)?;
        for implementation_type_pos in
            fed_schema.possible_runtime_types(interface_field_pos.parent().into())?
        {
            let implementing_type = implementation_type_pos.get(schema)?;
            if !implementing_type.directives.has(&key_directive.name) {
                continue;
            }
            let implementing_field = implementation_type_pos
                .field(interface_field_pos.field_name.clone())
                .get(schema)?;
            if !implementing_field.directives.has(&shareable_directive.name) {
                continue;
            }

            // Returning `true` for this method has a cost: it will make us consider type-explosion for `itf`, and this can
            // sometime lead to a large number of additional paths to explore, which can have a substantial cost. So we want
            // to limit it if we can avoid it. As it happens, we should return `true` if it is possible that "something"
            // (some field) in the type of `field` is reachable in _another_ subgraph but no in the one of the current path.
            // And while it's not trivial to check this in general, there are some easy cases we can eliminate. For instance,
            // if the type in the current subgraph has only leaf fields, we can check that all other subgraphs reachable
            // from the implementation have the same set of leaf fields.
            let implementing_field_base_type_name = implementing_field.ty.inner_named_type();
            if is_leaf_type(schema, implementing_field_base_type_name) {
                continue;
            }
            let Some(implementing_field_base_type) =
                schema.get_object(implementing_field_base_type_name)
            else {
                // We officially "don't know", so we return "true" so type-explosion is tested.
                return Ok(true);
            };
            if implementing_field_base_type
                .fields
                .values()
                .any(|f| !is_leaf_type(schema, f.ty.inner_named_type()))
            {
                // Similar to above, we declare we "don't know" and test type-explosion.
                return Ok(true);
            }
            for node in self.graph.nodes_for_type(&implementing_type.name)? {
                let node = self.graph.node_weight(*node)?;
                let tail = self.graph.node_weight(self.tail)?;
                if node.source == tail.source {
                    continue;
                }
                let node_fed_schema = self.graph.schema_by_source(&node.source)?;
                let node_schema = node_fed_schema.schema();
                let node_fed_spec = get_federation_spec_definition_from_subgraph(node_fed_schema)?;
                let node_shareable_directive =
                    node_fed_spec.shareable_directive_definition(node_fed_schema)?;
                let build_err = || {
                    Err(FederationError::internal(format!(
                        "{implementation_type_pos} is an object in {} but a {} in {}",
                        tail.source, node.type_, node.source
                    )))
                };
                let QueryGraphNodeType::SchemaType(node_type_pos) = &node.type_ else {
                    return build_err();
                };
                let node_type_pos: ObjectOrInterfaceTypeDefinitionPosition =
                    node_type_pos.clone().try_into()?;
                let node_field_pos = node_type_pos.field(interface_field_pos.field_name.clone());
                let Some(node_field) = node_field_pos.try_get(node_schema) else {
                    continue;
                };
                if !node_field.directives.has(&node_shareable_directive.name) {
                    continue;
                }
                let node_field_base_type_name = node_field.ty.inner_named_type();
                if implementing_field_base_type_name != node_field_base_type_name {
                    // We have a genuine difference here, so we should explore type explosion.
                    return Ok(true);
                }
                let node_field_base_type_pos =
                    node_fed_schema.get_type(node_field_base_type_name.clone())?;
                let Some(node_field_base_type_pos): Option<
                    ObjectOrInterfaceTypeDefinitionPosition,
                > = node_field_base_type_pos.try_into().ok() else {
                    // Similar to above, we have a genuine difference.
                    return Ok(true);
                };

                if !node_field_base_type_pos.fields(node_schema)?.all(|f| {
                    implementing_field_base_type
                        .fields
                        .contains_key(f.field_name())
                }) {
                    // Similar to above, we have a genuine difference.
                    return Ok(true);
                }
                // Note that if the type is the same and the fields are a subset too, then we know
                // the return types of those fields must be leaf types, or merging would have
                // complained.
            }
            return Ok(false);
        }
        Ok(false)
    }

    /// For the first element of the pair, the data has the same meaning as in
    /// `SimultaneousPathsWithLazyIndirectPaths.advance_with_operation_element()`. We also actually
    /// need to return a `Vec` of options of simultaneous paths (because when we type explode, we
    /// create simultaneous paths, but as a field might be resolved by multiple subgraphs, we may
    /// have also created multiple options).
    ///
    /// For the second element, it is true if the result only has type-exploded results.
    #[cfg_attr(feature = "snapshot_tracing", tracing::instrument(
        skip_all,
        level = "trace",
        name = "GraphPath::advance_with_operation_element"
        fields(label = operation_element.to_string())
    ))]
    #[allow(clippy::too_many_arguments)]
    fn advance_with_operation_element(
        &self,
        supergraph_schema: ValidFederationSchema,
        operation_element: &OpPathElement,
        context: &OpGraphPathContext,
        condition_resolver: &mut impl ConditionResolver,
        override_conditions: &OverrideConditions,
        check_cancellation: &dyn Fn() -> Result<(), SingleFederationError>,
        disabled_subgraphs: &IndexSet<Arc<str>>,
    ) -> Result<(Option<Vec<SimultaneousPaths>>, Option<bool>), FederationError> {
        let span = debug_span!(
            "Trying to advance directly",
            from = %self,
            operation_element = %operation_element,
        );
        let _guard = span.enter();
        let tail_weight = self.graph.node_weight(self.tail)?;
        let QueryGraphNodeType::SchemaType(tail_type_pos) = &tail_weight.type_ else {
            // We cannot advance any operation from here. We need to take the initial non-collecting
            // edges first.
            debug!("Cannot advance federated graph root with direct operations");
            return Ok((None, None));
        };
        match operation_element {
            OpPathElement::Field(operation_field) => {
                match tail_type_pos {
                    OutputTypeDefinitionPosition::Object(tail_type_pos) => {
                        // Just take the edge corresponding to the field, if it exists and can be
                        // used.
                        let Some(edge) =
                            self.next_edge_for_field(operation_field, override_conditions)
                        else {
                            debug!(
                                "No edge for field {operation_field} on object type {tail_weight}"
                            );
                            return Ok((None, None));
                        };

                        // If the tail type is an `@interfaceObject`, it's possible that the
                        // requested field is a field of an implementation of the interface. Because
                        // we found an edge, we know that the interface object has the field and we
                        // can use the edge. However, we can't add the operation field as-is to this
                        // path, since it's referring to a parent type that is not in the current
                        // subgraph. We must instead use the tail's type, so we change the field
                        // accordingly.
                        //
                        // TODO: It would be good to understand what parts of query planning rely
                        // on triggers being valid within a subgraph.
                        let mut operation_field = operation_field.clone();
                        if self.tail_is_interface_object()?
                            && *operation_field.field_position.type_name()
                                != tail_type_pos.type_name
                        {
                            let field_on_tail_type = tail_type_pos
                                .field(operation_field.field_position.field_name().clone());
                            if field_on_tail_type
                                .try_get(self.graph.schema_by_source(&tail_weight.source)?.schema())
                                .is_none()
                            {
                                let edge_weight = self.graph.edge_weight(edge)?;
                                return Err(FederationError::internal(format!(
                                    "Unexpectedly missing {} for {} from path {}",
                                    operation_field, edge_weight, self,
                                )));
                            }
                            operation_field = Field {
                                schema: self.graph.schema_by_source(&tail_weight.source)?.clone(),
                                field_position: field_on_tail_type.into(),
                                alias: operation_field.alias.clone(),
                                arguments: operation_field.arguments.clone(),
                                directives: operation_field.directives.clone(),
                                sibling_typename: operation_field.sibling_typename.clone(),
                            }
                        }

                        let field_path = self.add_field_edge(
                            operation_field,
                            edge,
                            condition_resolver,
                            context,
                        )?;
                        match &field_path {
                            Some(_) => debug!("Collected field on object type {tail_weight}"),
                            None => debug!(
                                "Cannot satisfy @requires on field for object type {tail_weight}"
                            ),
                        }
                        Ok((field_path.map(|p| vec![p.into()]), None))
                    }
                    OutputTypeDefinitionPosition::Interface(tail_type_pos) => {
                        // Due to `@interfaceObject`, we could be in a case where the field asked is
                        // not on the interface but rather on one of it's implementations. This can
                        // happen if we just entered the subgraph on an interface `@key` and are
                        // and coming from an `@interfaceObject`. In that case, we'll skip checking
                        // for a direct interface edge and simply cast into that implementation
                        // below.
                        let field_is_of_an_implementation =
                            *operation_field.field_position.type_name() != tail_type_pos.type_name;

                        // First, we check if there is a direct edge from the interface (which only
                        // happens if we're in a subgraph that knows all of the implementations of
                        // that interface globally and all of them resolve the field). If there is
                        // one, then we have 2 options:
                        //  - We take that edge.
                        //  - We type-explode (like when we don't have a direct interface edge).
                        // We want to avoid looking at both options if we can because it multiplies
                        // planning work quickly if we always check both options. And in general,
                        // taking the interface edge is better than type explosion "if it works",
                        // so we distinguish a number of cases where we know that either:
                        // - Type-exploding cannot work unless taking the interface edge also does
                        //   (the `has_an_entity_implementation_with_shareable_field()` call).
                        // - Type-exploding cannot be more efficient than the direct path (when no
                        //   `@provides` are involved; if a `@provides` is involved in one of the
                        //    implementations, then type-exploding may lead to a shorter overall
                        //    plan thanks to that `@provides`).
                        let interface_edge = if field_is_of_an_implementation {
                            None
                        } else {
                            self.next_edge_for_field(operation_field, override_conditions)
                        };
                        let interface_path = if let Some(interface_edge) = &interface_edge {
                            let field_path = self.add_field_edge(
                                operation_field.clone(),
                                *interface_edge,
                                condition_resolver,
                                context,
                            )?;
                            if field_path.is_none() {
                                let interface_edge_weight =
                                    self.graph.edge_weight(*interface_edge)?;
                                return Err(FederationError::internal(format!(
                                    "Interface edge {} unexpectedly had conditions",
                                    interface_edge_weight
                                )));
                            }
                            field_path
                        } else {
                            None
                        };
                        let direct_path_overrides_type_explosion =
                            if let Some(interface_edge) = &interface_edge {
                                // There are 2 separate cases where we going to do both "direct" and
                                // "type-exploding" options:
                                // 1. There is an `@provides`: in that case the "type-exploding
                                //    case can legitimately be more efficient and we want to =
                                //    consider it "all the way"
                                // 2. In the sub-case of
                                //    `!has_an_entity_implementation_with_shareable_field(...)`,
                                //    where we want to have the type-exploding option only for the
                                //    case where the "direct" one fails later. But in that case,
                                //    we'll remember that if the direct option pans out, then we can
                                //    ignore the type-exploding one.
                                // `direct_path_overrides_type_explosion` indicates that we're in
                                // the 2nd case above, not the 1st one.
                                operation_field
                                    .field_position
                                    .is_introspection_typename_field()
                                    || (!self.graph.is_provides_edge(*interface_edge)?
                                        && !self.graph.has_an_implementation_with_provides(
                                            &tail_weight.source,
                                            tail_type_pos.field(
                                                operation_field.field_position.field_name().clone(),
                                            ),
                                        )?)
                            } else {
                                false
                            };
                        if direct_path_overrides_type_explosion {
                            // We can special-case terminal (leaf) fields: as long they have no
                            // `@provides`, then the path ends there and there is no need to check
                            // type explosion "in case the direct path doesn't pan out".
                            // Additionally, if we're not in the case where an implementation
                            // is an entity with a shareable field, then there is no case where the
                            // direct case wouldn't "pan out" but the type explosion would, so we
                            // can ignore type-exploding there too.
                            //
                            // TODO: We should re-assess this when we support `@requires` on
                            // interface fields (typically, should we even try to type-explode
                            // if the direct edge cannot be satisfied? Probably depends on the exact
                            // semantics of `@requires` on interface fields).
                            let operation_field_type_name = operation_field
                                .field_position
                                .get(operation_field.schema.schema())?
                                .ty
                                .inner_named_type();
                            let is_operation_field_type_leaf = matches!(
                                operation_field
                                    .schema
                                    .get_type(operation_field_type_name.clone())?,
                                TypeDefinitionPosition::Scalar(_) | TypeDefinitionPosition::Enum(_)
                            );
                            if is_operation_field_type_leaf
                                || !self.has_an_entity_implementation_with_shareable_field(
                                    &tail_weight.source,
                                    tail_type_pos
                                        .field(operation_field.field_position.field_name().clone()),
                                )?
                            {
                                let Some(interface_path) = interface_path else {
                                    return Err(FederationError::internal(
                                        "Unexpectedly missing interface path",
                                    ));
                                };
                                debug!(
                                    "Collecting (leaf) field on interface {tail_weight} without type-exploding"
                                );
                                return Ok((Some(vec![interface_path.into()]), None));
                            }
                            debug!("Collecting field on interface {tail_weight} as 1st option");
                        }

                        // There are 2 main cases to handle here:
                        // - The most common is that it's a field of the interface that is queried,
                        //   and so we should type-explode because either we didn't had a direct
                        //   edge, or `@provides` makes it potentially worthwhile to check with type
                        //   explosion.
                        // - But, as mentioned earlier, we could be in the case where the field
                        //   queried is actually of one of the implementation of the interface. In
                        //   that case, we only want to consider that one implementation.
                        let implementations = if field_is_of_an_implementation {
                            let CompositeTypeDefinitionPosition::Object(field_parent_pos) =
                                &operation_field.field_position.parent()
                            else {
                                return Err(FederationError::internal(format!(
                                    "{} requested on {}, but field's parent {} is not an object type",
                                    operation_field.field_position,
                                    tail_type_pos,
                                    operation_field.field_position.type_name()
                                )));
                            };
                            if !self.runtime_types_of_tail.contains(field_parent_pos) {
                                return Err(FederationError::internal(format!(
                                    "{} requested on {}, but field's parent {} is not an implementation type",
                                    operation_field.field_position,
                                    tail_type_pos,
                                    operation_field.field_position.type_name()
                                )));
                            }
                            debug!("Casting into requested type {field_parent_pos}");
                            Arc::new(IndexSet::from_iter([field_parent_pos.clone()]))
                        } else {
                            match &interface_path {
                                Some(_) => debug!(
                                    "No direct edge: type exploding interface {tail_weight} into possible runtime types {:?}",
                                    self.runtime_types_of_tail
                                ),
                                None => debug!(
                                    "Type exploding interface {tail_weight} into possible runtime types {:?} as 2nd option",
                                    self.runtime_types_of_tail
                                ),
                            }
                            self.runtime_types_of_tail.clone()
                        };

                        // We type-explode. For all implementations, we need to call
                        // `advance_with_operation_element()` on a made-up inline fragment. If
                        // any gives us empty options, we bail.
                        let mut options_for_each_implementation = vec![];
                        for implementation_type_pos in implementations.as_ref() {
                            debug!("Handling implementation {implementation_type_pos}");
                            let span = debug_span!(" |");
                            let guard = span.enter();
                            let implementation_inline_fragment = InlineFragment {
                                schema: self.graph.schema_by_source(&tail_weight.source)?.clone(),
                                parent_type_position: tail_type_pos.clone().into(),
                                type_condition_position: Some(
                                    implementation_type_pos.clone().into(),
                                ),
                                directives: Default::default(),
                                selection_id: SelectionId::new(),
                            };
                            let implementation_options =
                                SimultaneousPathsWithLazyIndirectPaths::new(
                                    self.clone().into(),
                                    context.clone(),
                                    Default::default(),
                                    Default::default(),
                                )
                                .advance_with_operation_element(
                                    supergraph_schema.clone(),
                                    &implementation_inline_fragment.into(),
                                    condition_resolver,
                                    override_conditions,
                                    check_cancellation,
                                    disabled_subgraphs,
                                )?;
                            // If we find no options for that implementation, we bail (as we need to
                            // simultaneously advance all implementations).
                            let Some(mut implementation_options) = implementation_options else {
                                drop(guard);
                                debug!(
                                    "Cannot collect field from {implementation_type_pos}: stopping with options [{interface_path:?}]"
                                );
                                return Ok((interface_path.map(|p| vec![p.into()]), None));
                            };
                            // If the new inline fragment makes it so that we're on an unsatisfiable
                            // branch, we just ignore that implementation.
                            if implementation_options.is_empty() {
                                debug!(
                                    "Cannot ever get {implementation_type_pos} from this branch, ignoring it"
                                );
                                continue;
                            }
                            // For each option, we call `advance_with_operation_element()` again on
                            // our own operation element (the field), which gives us some options
                            // (or not and we bail).
                            let mut field_options = vec![];
                            debug!(
                                "Trying to collect field from options {implementation_options:?}"
                            );
                            for implementation_option in &mut implementation_options {
                                let span = debug_span!(
                                    "implementation option",
                                    implementation_option = %implementation_option
                                );
                                let _guard = span.enter();
                                let field_options_for_implementation = implementation_option
                                    .advance_with_operation_element(
                                        supergraph_schema.clone(),
                                        operation_element,
                                        condition_resolver,
                                        override_conditions,
                                        check_cancellation,
                                        disabled_subgraphs,
                                    )?;
                                let Some(field_options_for_implementation) =
                                    field_options_for_implementation
                                else {
                                    debug!("Cannot collect field");
                                    continue;
                                };
                                // Advancing a field should never get us into an unsatisfiable
                                // condition (only fragments can).
                                if field_options_for_implementation.is_empty() {
                                    return Err(FederationError::internal(format!(
                                        "Unexpected unsatisfiable path after {}",
                                        operation_field
                                    )));
                                }
                                debug!(
                                    "Collected field: adding {field_options_for_implementation:?}"
                                );
                                field_options.extend(
                                    field_options_for_implementation
                                        .into_iter()
                                        .map(|s| s.paths),
                                );
                            }
                            // If we find no options to advance that implementation, we bail (as we
                            // need to simultaneously advance all implementations).
                            if field_options.is_empty() {
                                drop(guard);
                                debug!(
                                    "Cannot collect field from {implementation_type_pos}: stopping with options [{}]",
                                    DisplayOption::new(&interface_path)
                                );
                                return Ok((interface_path.map(|p| vec![p.into()]), None));
                            };
                            debug!("Collected field from {implementation_type_pos}");
                            options_for_each_implementation.push(field_options);
                        }
                        let all_options = SimultaneousPaths::flat_cartesian_product(
                            options_for_each_implementation,
                            check_cancellation,
                        )?;
                        if let Some(interface_path) = interface_path {
                            let (interface_path, all_options) =
                                if direct_path_overrides_type_explosion {
                                    interface_path.mark_overriding(&all_options)
                                } else {
                                    (interface_path, all_options)
                                };
                            let options = vec![interface_path.into()]
                                .into_iter()
                                .chain(all_options)
                                .collect::<Vec<_>>();
                            debug!("With type-exploded options: {}", DisplaySlice(&options));
                            Ok((Some(options), None))
                        } else {
                            debug!("With type-exploded options: {}", DisplaySlice(&all_options));
                            // TODO: This appears to be the only place returning non-None for the
                            // 2nd argument, so this could be Option<(Vec<SimultaneousPaths>, bool)>
                            // instead.
                            Ok((Some(all_options), Some(true)))
                        }
                    }
                    OutputTypeDefinitionPosition::Union(_) => {
                        let Some(typename_edge) =
                            self.next_edge_for_field(operation_field, override_conditions)
                        else {
                            return Err(FederationError::internal(
                                "Should always have an edge for __typename edge on an union",
                            ));
                        };
                        let field_path = self.add_field_edge(
                            operation_field.clone(),
                            typename_edge,
                            condition_resolver,
                            context,
                        )?;
                        debug!("Trivial collection of __typename for union");
                        Ok((field_path.map(|p| vec![p.into()]), None))
                    }
                    _ => {
                        // Only object, interfaces, and unions (only for __typename) have fields, so
                        // the query should have been flagged invalid if a field was selected on
                        // something else.
                        Err(FederationError::internal(format!(
                            "Unexpectedly found field {} on non-composite type {}",
                            operation_field, tail_type_pos,
                        )))
                    }
                }
            }
            OpPathElement::InlineFragment(operation_inline_fragment) => {
                let type_condition_name = operation_inline_fragment
                    .type_condition_position
                    .as_ref()
                    .map(|pos| pos.type_name())
                    .unwrap_or_else(|| tail_type_pos.type_name())
                    .clone();
                if type_condition_name == *tail_type_pos.type_name() {
                    // If there is no type condition (or the condition is the type we're already
                    // on), it means we're essentially just applying some directives (could be a
                    // `@skip`/`@include` for instance). This doesn't make us take any edge, but if
                    // the operation element does has directives, we record it.
                    debug!(
                        "No edge to take for condition {operation_inline_fragment} from current type"
                    );
                    let fragment_path = if operation_inline_fragment.directives.is_empty() {
                        self.clone()
                    } else {
                        self.add(
                            operation_inline_fragment.clone().into(),
                            None,
                            ConditionResolution::no_conditions(),
                            operation_inline_fragment.defer_directive_arguments()?,
                        )?
                    };
                    return Ok((Some(vec![fragment_path.into()]), None));
                }
                match tail_type_pos {
                    OutputTypeDefinitionPosition::Interface(_)
                    | OutputTypeDefinitionPosition::Union(_) => {
                        let tail_type_pos: AbstractTypeDefinitionPosition =
                            tail_type_pos.clone().try_into()?;

                        // If we have an edge for the typecast, take that.
                        if let Some(edge) =
                            self.next_edge_for_inline_fragment(operation_inline_fragment)
                        {
                            let edge_weight = self.graph.edge_weight(edge)?;
                            if edge_weight.conditions.is_some() {
                                return Err(FederationError::internal(
                                    "Unexpectedly found condition on inline fragment collecting edge",
                                ));
                            }
                            let fragment_path = self.add(
                                operation_inline_fragment.clone().into(),
                                Some(edge),
                                ConditionResolution::no_conditions(),
                                operation_inline_fragment.defer_directive_arguments()?,
                            )?;
                            debug!(
                                "Using type-casting edge for {type_condition_name} from current type"
                            );
                            return Ok((Some(vec![fragment_path.into()]), None));
                        }

                        // Otherwise, check what the intersection is between the possible runtime
                        // types of the tail type and the ones of the typecast. We need to be able
                        // to go into all those types simultaneously (a.k.a. type explosion).
                        let from_types = self.runtime_types_of_tail.clone();
                        let to_types = supergraph_schema.possible_runtime_types(
                            supergraph_schema
                                .get_type(type_condition_name.clone())?
                                .try_into()?,
                        )?;
                        let intersection = from_types.intersection(&to_types);
                        debug!(
                            "Trying to type-explode into intersection between current type and {type_condition_name} = [{}]",
                            intersection.clone().format(",")
                        );
                        let mut options_for_each_implementation = vec![];
                        for implementation_type_pos in intersection {
                            let span = debug_span!(
                                "attempt type explosion",
                                implementation_type = %implementation_type_pos
                            );
                            let guard = span.enter();
                            let implementation_inline_fragment = InlineFragment {
                                schema: self.graph.schema_by_source(&tail_weight.source)?.clone(),
                                parent_type_position: tail_type_pos.clone().into(),
                                type_condition_position: Some(
                                    implementation_type_pos.clone().into(),
                                ),
                                directives: operation_inline_fragment.directives.clone(),
                                selection_id: SelectionId::new(),
                            };
                            let implementation_options =
                                SimultaneousPathsWithLazyIndirectPaths::new(
                                    self.clone().into(),
                                    context.clone(),
                                    Default::default(),
                                    Default::default(),
                                )
                                .advance_with_operation_element(
                                    supergraph_schema.clone(),
                                    &implementation_inline_fragment.into(),
                                    condition_resolver,
                                    override_conditions,
                                    check_cancellation,
                                    disabled_subgraphs,
                                )?;
                            let Some(implementation_options) = implementation_options else {
                                drop(guard);
                                debug!(
                                    "Cannot advance into {implementation_type_pos} from current type: no options for operation."
                                );
                                return Ok((None, None));
                            };
                            // If the new inline fragment makes it so that we're on an unsatisfiable
                            // branch, we just ignore that implementation.
                            if implementation_options.is_empty() {
                                debug!("Cannot ever get type name from this branch, ignoring it");
                                continue;
                            }
                            options_for_each_implementation.push(
                                implementation_options
                                    .into_iter()
                                    .map(|s| s.paths)
                                    .collect(),
                            );
                            debug!(
                                "Advanced into type from current type: {options_for_each_implementation:?}"
                            );
                        }
                        let all_options = SimultaneousPaths::flat_cartesian_product(
                            options_for_each_implementation,
                            check_cancellation,
                        )?;
                        debug!("Type-exploded options: {}", DisplaySlice(&all_options));
                        Ok((Some(all_options), None))
                    }
                    OutputTypeDefinitionPosition::Object(tail_type_pos) => {
                        // We've already handled the case of a fragment whose type condition is the
                        // same as the tail type. But the fragment might be for either:
                        // - A super-type of the tail type. In which case, we're pretty much in the
                        //   same case than if there were no particular type condition.
                        // - If the tail type is an `@interfaceObject`, then this can be an
                        //   implementation type of the interface in the supergraph. In that case,
                        //   the type condition is not a known type of the subgraph, but the
                        //   subgraph might still be able to handle some of fields, so in that case,
                        //   we essentially "ignore" the fragment for now. We will re-add it back
                        //   later for fields that are not in the current subgraph after we've taken
                        //   an `@key` for the interface.
                        // - An incompatible type. This can happen for a type that intersects a
                        //   super-type of the tail type (since GraphQL allows a fragment as long as
                        //   there is an intersection). In that case, the whole operation element
                        //   simply cannot ever return anything.
                        let type_condition_pos = supergraph_schema.get_type(type_condition_name)?;
                        let abstract_type_condition_pos: Option<AbstractTypeDefinitionPosition> =
                            type_condition_pos.clone().try_into().ok();
                        if let Some(type_condition_pos) = abstract_type_condition_pos {
                            if supergraph_schema
                                .possible_runtime_types(type_condition_pos.into())?
                                .contains(tail_type_pos)
                            {
                                debug!("Type is a super-type of the current type. No edge to take");
                                // Type condition is applicable on the tail type, so the types are
                                // already exploded but the condition can reference types from the
                                // supergraph that are not present in the local subgraph.
                                //
                                // If the operation element has applied directives we need to
                                // convert it to an inline fragment without type condition,
                                // otherwise we ignore the fragment altogether.
                                if operation_inline_fragment.directives.is_empty() {
                                    return Ok((Some(vec![self.clone().into()]), None));
                                }
                                let operation_inline_fragment = InlineFragment {
                                    schema: self
                                        .graph
                                        .schema_by_source(&tail_weight.source)?
                                        .clone(),
                                    parent_type_position: tail_type_pos.clone().into(),
                                    type_condition_position: None,
                                    directives: operation_inline_fragment.directives.clone(),
                                    selection_id: SelectionId::new(),
                                };
                                let defer_directive_arguments =
                                    operation_inline_fragment.defer_directive_arguments()?;
                                let fragment_path = self.add(
                                    operation_inline_fragment.into(),
                                    None,
                                    ConditionResolution::no_conditions(),
                                    defer_directive_arguments,
                                )?;
                                return Ok((Some(vec![fragment_path.into()]), None));
                            }
                        }

                        if self.tail_is_interface_object()? {
                            let mut fake_downcast_edge = None;
                            for edge in self.next_edges()? {
                                let edge_weight = self.graph.edge_weight(edge)?;
                                let QueryGraphEdgeTransition::InterfaceObjectFakeDownCast {
                                    to_type_name,
                                    ..
                                } = &edge_weight.transition
                                else {
                                    continue;
                                };
                                if type_condition_pos.type_name() == to_type_name {
                                    fake_downcast_edge = Some(edge);
                                    break;
                                };
                            }
                            if let Some(fake_downcast_edge) = fake_downcast_edge {
                                let condition_resolution = self.can_satisfy_conditions(
                                    fake_downcast_edge,
                                    condition_resolver,
                                    context,
                                    &Default::default(),
                                    &Default::default(),
                                )?;
                                if matches!(
                                    condition_resolution,
                                    ConditionResolution::Unsatisfied { .. }
                                ) {
                                    return Ok((None, None));
                                }
                                let fragment_path = self.add(
                                    operation_inline_fragment.clone().into(),
                                    Some(fake_downcast_edge),
                                    condition_resolution,
                                    operation_inline_fragment.defer_directive_arguments()?,
                                )?;
                                return Ok((Some(vec![fragment_path.into()]), None));
                            }
                        }

                        debug!("Cannot ever get type from current type: returning empty branch");
                        // The operation element we're dealing with can never return results (the
                        // type conditions applied have no intersection). This means we can fulfill
                        // this operation element (by doing nothing and returning an empty result),
                        // which we indicate by return ingan empty list of options.
                        Ok((Some(vec![]), None))
                    }
                    _ => {
                        // We shouldn't have a fragment on a non-composite type.
                        Err(FederationError::internal(format!(
                            "Unexpectedly found inline fragment {} on non-composite type {}",
                            operation_inline_fragment, tail_type_pos,
                        )))
                    }
                }
            }
        }
    }

    /// Given an `OpGraphPath` and a `SimultaneousPaths` that represent 2 different options to reach
    /// the same query leaf field, checks if one can be shown to be always "better" (more
    /// efficient/optimal) than the other one, regardless of any surrounding context (i.e.
    /// regardless of what the rest of the query plan would be for any other query leaf field).
    ///
    /// Returns the comparison of the complexity of `self` and `other` (e.g. `Ordering::Less` means
    /// `self` is better/has less complexity than `other`). If we can't guarantee anything (at least
    /// "out of context"), then we return `Ordering::Equal`.
    fn compare_single_vs_multi_path_options_complexity_out_of_context(
        &self,
        other: &SimultaneousPaths,
    ) -> Result<Ordering, FederationError> {
        // This handles the same case as the single-path-only case, but compares the single path
        // against each path of the `SimultaneousPaths`, and only "ignores" the `SimultaneousPaths`
        // if all its paths can be ignored.
        //
        // Note that this happens less often than the single-path-only case, but with `@provides` on
        // an interface, you can have cases where on one hand you can get something completely on
        // the current subgraph, but the type-exploded case has to still be generated due to the
        // leaf field not being the one just after the "provided" interface.
        for other_path in other.0.iter() {
            // Note: Not sure if it is possible for a path of the `SimultaneousPaths` option to
            // subsume the single-path one in practice, but if it does, we ignore it because it's
            // not obvious that this is enough to get rid of `self` (maybe if `self` is provably a
            // bit costlier than one of the paths of `other`, but `other` may have many paths and
            // could still be collectively worst than `self`).
            if self.compare_single_path_options_complexity_out_of_context(other_path)?
                != Ordering::Less
            {
                return Ok(Ordering::Equal);
            }
        }
        Ok(Ordering::Less)
    }

    /// Given 2 `OpGraphPath`s that represent 2 different paths to reach the same query leaf field,
    /// checks if one can be shown to be always "better" (more efficient/optimal) than the other
    /// one, regardless of any surrounding context (i.e. regardless of what the rest of the query
    /// plan would be for any other query leaf field).
    ///
    /// Returns the comparison of the complexity of `self` and `other` (e.g. `Ordering::Less` means
    /// `self` is better/has less complexity than `other`). If we can't guarantee anything (at least
    /// "out of context"), then we return `Ordering::Equal`.
    fn compare_single_path_options_complexity_out_of_context(
        &self,
        other: &OpGraphPath,
    ) -> Result<Ordering, FederationError> {
        // Currently, this method only handles the case where we have something like:
        //  -  `self`: <some prefix> -[t]-> T(A)               -[u]-> U(A) -[x] -> Int(A)
        //  - `other`: <some prefix> -[t]-> T(A) -[key]-> T(B) -[u]-> U(B) -[x] -> Int(B)
        // That is, where we have 2 choices that are identical up to the "end", when one stays in
        // the subgraph (`self`, which stays in A) while the other uses a key to get to another
        // subgraph (`other`, going to B).
        //
        // In such a case, whatever else the query plan might be doing, it can never be "worse"
        // to use `self` than to use `other` because both will force the same "fetch dependency
        // graph node" up to the end, but `other` may force one more fetch that `self` does not.
        // Do note that we say "may" above, because the rest of the query plan may very well have a
        // forced choice like:
        //  - `option`: <some prefix> -[t]-> T(A) -[key]-> T(B) -[u]-> U(B) -[y] -> Int(B)
        // in which case the query plan will have the jump from A to B after `t` regardless of
        // whether we use `self` or `other`, but while in that particular case `self` and `other`
        // are about comparable in terms of performance, `self` is still not worse than `other` (and
        // in other situations, `self` may be genuinely be better).
        //
        // Note that this is in many ways just a generalization of a heuristic we use earlier for
        // leaf fields. That is, we will never get as input to this method something like:
        //  -  `self`: <some prefix> -[t]-> T(A)               -[x] -> Int(A)
        //  - `other`: <some prefix> -[t]-> T(A) -[key]-> T(B) -[x] -> Int(B)
        // because when the code is asked for the options for `x` after `<some prefix> -[t]-> T(A)`,
        // it notices that `x` is a leaf and is in `A`, so it doesn't ever look for alternative
        // paths. But this only works for direct leaves of an entity. In the example at the start,
        // field `u` makes this not work, because when we compute choices for `u`, we don't yet know
        // what comes after that, and so we have to take the option of going to subgraph `B` into
        // account (it may very well be that whatever comes after `u` is not in `A`, for instance).
        let self_tail_weight = self.graph.node_weight(self.tail)?;
        let other_tail_weight = self.graph.node_weight(other.tail)?;
        if self_tail_weight.source != other_tail_weight.source {
            // As described above, we want to know if one of the paths has no jumps at all (after
            // the common prefix) while the other has some.
            self.compare_subgraph_jumps_after_last_common_node(other)
        } else {
            Ok(Ordering::Equal)
        }
    }
}

impl SimultaneousPaths {
    /// Given options generated for the advancement of each path of a `SimultaneousPaths`, generate
    /// the options for the `SimultaneousPaths` as a whole.
    fn flat_cartesian_product(
        options_for_each_path: Vec<Vec<SimultaneousPaths>>,
        check_cancellation: &dyn Fn() -> Result<(), SingleFederationError>,
    ) -> Result<Vec<SimultaneousPaths>, FederationError> {
        // This can be written more tersely with a bunch of `reduce()`/`flat_map()`s and friends,
        // but when interfaces type-explode into many implementations, this can end up with fairly
        // large `Vec`s and be a bottleneck, and a more iterative version that pre-allocates `Vec`s
        // is quite a bit faster.
        if options_for_each_path.is_empty() {
            return Ok(vec![]);
        }

        // Track, for each path, which option index we're at.
        let mut option_indexes = vec![0; options_for_each_path.len()];

        // Pre-allocate `Vec` for the result.
        let num_options = options_for_each_path
            .iter()
            .fold(1_usize, |product, options| {
                product.saturating_mul(options.len())
            });
        if num_options > 1_000_000 {
            return Err(SingleFederationError::QueryPlanComplexityExceeded {
                message: format!(
                    "Excessive number of combinations for a given path: {num_options}"
                ),
            }
            .into());
        }
        let mut product = Vec::with_capacity(num_options);

        // Compute the cartesian product.
        for _ in 0..num_options {
            check_cancellation()?;
            let num_simultaneous_paths = options_for_each_path
                .iter()
                .zip(&option_indexes)
                .map(|(options, option_index)| options[*option_index].0.len())
                .sum();
            let mut simultaneous_paths = Vec::with_capacity(num_simultaneous_paths);

            for (options, option_index) in options_for_each_path.iter().zip(&option_indexes) {
                simultaneous_paths.extend(options[*option_index].0.iter().cloned());
            }
            product.push(SimultaneousPaths(simultaneous_paths));

            for (options, option_index) in options_for_each_path.iter().zip(&mut option_indexes) {
                if *option_index == options.len() - 1 {
                    *option_index = 0
                } else {
                    *option_index += 1;
                    break;
                }
            }
        }

        Ok(product)
    }

    /// Given 2 `SimultaneousPaths` that represent 2 different options to reach the same query leaf
    /// field, checks if one can be shown to be always "better" (more efficient/optimal) than the
    /// other one, regardless of any surrounding context (i.e. regardless of what the rest of the
    /// query plan would be for any other query leaf field).
    ///
    /// Note that this method is used on the final options of a given "query path", so all the
    /// heuristics done within `GraphPath` to avoid unnecessary options have already been applied
    /// (e.g. avoiding the consideration of paths that do 2 successive key jumps when there is a
    /// 1-jump equivalent), so this focus on what can be done is based on the fact that the path
    /// considered is "finished".
    ///
    /// Returns the comparison of the complexity of `self` and `other` (e.g. `Ordering::Less` means
    /// `self` is better/has less complexity than `other`). If we can't guarantee anything (at least
    /// "out of context"), then we return `Ordering::Equal`.
    fn compare_options_complexity_out_of_context(
        &self,
        other: &SimultaneousPaths,
    ) -> Result<Ordering, FederationError> {
        match (self.0.as_slice(), other.0.as_slice()) {
            ([a], [b]) => a.compare_single_path_options_complexity_out_of_context(b),
            ([a], _) => a.compare_single_vs_multi_path_options_complexity_out_of_context(other),
            (_, [b]) => b
                .compare_single_vs_multi_path_options_complexity_out_of_context(self)
                .map(Ordering::reverse),
            _ => Ok(Ordering::Equal),
        }
    }
}

impl From<Arc<OpGraphPath>> for SimultaneousPaths {
    fn from(value: Arc<OpGraphPath>) -> Self {
        Self(vec![value])
    }
}

impl From<OpGraphPath> for SimultaneousPaths {
    fn from(value: OpGraphPath) -> Self {
        Self::from(Arc::new(value))
    }
}

impl SimultaneousPathsWithLazyIndirectPaths {
    pub(crate) fn new(
        paths: SimultaneousPaths,
        context: OpGraphPathContext,
        excluded_destinations: ExcludedDestinations,
        excluded_conditions: ExcludedConditions,
    ) -> SimultaneousPathsWithLazyIndirectPaths {
        SimultaneousPathsWithLazyIndirectPaths {
            lazily_computed_indirect_paths: std::iter::repeat_with(|| None)
                .take(paths.0.len())
                .collect(),
            paths,
            context,
            excluded_destinations,
            excluded_conditions,
        }
    }

    /// For a given "input" path (identified by an idx in `paths`), each of its indirect options.
    fn indirect_options(
        &mut self,
        path_index: usize,
        condition_resolver: &mut impl ConditionResolver,
        override_conditions: &OverrideConditions,
        disabled_subgraphs: &IndexSet<Arc<str>>,
    ) -> Result<OpIndirectPaths, FederationError> {
        if let Some(indirect_paths) = &self.lazily_computed_indirect_paths[path_index] {
            Ok(indirect_paths.clone())
        } else {
            let new_indirect_paths = self.compute_indirect_paths(
                path_index,
                condition_resolver,
                override_conditions,
                disabled_subgraphs,
            )?;
            self.lazily_computed_indirect_paths[path_index] = Some(new_indirect_paths.clone());
            Ok(new_indirect_paths)
        }
    }

    fn compute_indirect_paths(
        &self,
        path_index: usize,
        condition_resolver: &mut impl ConditionResolver,
        override_conditions: &OverrideConditions,
        disabled_subgraphs: &IndexSet<Arc<str>>,
    ) -> Result<OpIndirectPaths, FederationError> {
        self.paths.0[path_index].advance_with_non_collecting_and_type_preserving_transitions(
            &self.context,
            condition_resolver,
            &self.excluded_destinations,
            &self.excluded_conditions,
            override_conditions,
            // The transitions taken by this method are non-collecting transitions, in which case
            // the trigger is the context (which is really a hack to provide context information for
            // keys during fetch dependency graph updating).
            |_, context| OpGraphPathTrigger::Context(context.clone()),
            |graph, node, trigger, override_conditions| {
                Ok(graph.edge_for_op_graph_path_trigger(node, trigger, override_conditions))
            },
            disabled_subgraphs,
        )
    }

    fn create_lazy_options(
        &self,
        options: Vec<SimultaneousPaths>,
        context: OpGraphPathContext,
    ) -> Vec<SimultaneousPathsWithLazyIndirectPaths> {
        options
            .into_iter()
            .map(|paths| {
                SimultaneousPathsWithLazyIndirectPaths::new(
                    paths,
                    context.clone(),
                    self.excluded_destinations.clone(),
                    self.excluded_conditions.clone(),
                )
            })
            .collect()
    }

    /// Returns `None` if the operation cannot be dealt with/advanced. Otherwise, it returns a `Vec`
    /// of options we can be in after advancing the operation, each option being a set of
    /// simultaneous paths in the subgraphs (a single path in the simple case, but type exploding
    /// may make us explore multiple paths simultaneously).
    ///
    /// The lists of options can be empty, which has the special meaning that the operation is
    /// guaranteed to have no results (it corresponds to unsatisfiable conditions), meaning that as
    /// far as query planning goes, we can just ignore the operation but otherwise continue.
    // PORT_NOTE: In the JS codebase, this was named `advanceSimultaneousPathsWithOperation`.
    pub(crate) fn advance_with_operation_element(
        &mut self,
        supergraph_schema: ValidFederationSchema,
        operation_element: &OpPathElement,
        condition_resolver: &mut impl ConditionResolver,
        override_conditions: &OverrideConditions,
        check_cancellation: &dyn Fn() -> Result<(), SingleFederationError>,
        disabled_subgraphs: &IndexSet<Arc<str>>,
    ) -> Result<Option<Vec<SimultaneousPathsWithLazyIndirectPaths>>, FederationError> {
        debug!(
            "Trying to advance paths for operation: path = {}, operation = {operation_element}",
            self.paths
        );
        let span = debug_span!(" |");
        let _guard = span.enter();
        let updated_context = self.context.with_context_of(operation_element)?;
        let mut options_for_each_path = vec![];

        // To call the mutating method `indirect_options()`, we need to not hold any immutable
        // references to `self`, which means cloning these paths when iterating.
        let paths = self.paths.0.clone();
        for (path_index, path) in paths.iter().enumerate() {
            check_cancellation()?;
            debug!("Computing options for {path}");
            let span = debug_span!(" |");
            let gaurd = span.enter();
            let mut options = None;
            let should_reenter_subgraph = path.defer_on_tail.is_some()
                && matches!(operation_element, OpPathElement::Field(_));
            if !should_reenter_subgraph {
                debug!("Direct options");
                let span = debug_span!(" |");
                let gaurd = span.enter();
                let (advance_options, has_only_type_exploded_results) = path
                    .advance_with_operation_element(
                        supergraph_schema.clone(),
                        operation_element,
                        &updated_context,
                        condition_resolver,
                        override_conditions,
                        check_cancellation,
                        disabled_subgraphs,
                    )?;
                debug!("{advance_options:?}");
                drop(gaurd);
                // If we've got some options, there are a number of cases where there is no point
                // looking for indirect paths:
                // - If the operation element is terminal: this means we just found a direct edge
                //   that is terminal, so no indirect options could be better (this is not true for
                //   non-terminal operation element, where the direct route may end up being a dead
                //   end later). One exception however is when `advanceWithOperationElement()`
                //   type-exploded (meaning that we're on an interface), because in that case, the
                //   type-exploded options have already taken indirect edges into account, so it's
                //   possible that an indirect edge _from the interface_ could be better, but only
                //   if there wasn't a "true" direct edge on the interface, which is what
                //   `has_only_type_exploded_results` tells us.
                // - If we get options, but an empty set of them, which signifies the operation
                //   element corresponds to unsatisfiable conditions and we can essentially ignore
                //   it.
                // - If the operation element is a fragment in general: if we were able to find a
                //   direct option, that means the type is known in the "current" subgraph, and so
                //   we'll still be able to take any indirect edges that we could take now later,
                //   for the follow-up operation element. And pushing the decision will give us more
                //   context and may avoid a bunch of state explosion in practice.
                if let Some(advance_options) = advance_options {
                    if advance_options.is_empty()
                        || (operation_element.is_terminal()?
                            && !has_only_type_exploded_results.unwrap_or(false))
                        || matches!(operation_element, OpPathElement::InlineFragment(_))
                    {
                        debug!("Final options for {path}: {advance_options:?}");
                        // Note that if options is empty, that means this particular "branch" is
                        // unsatisfiable, so we should just ignore it.
                        if !advance_options.is_empty() {
                            options_for_each_path.push(advance_options);
                        }
                        continue;
                    } else {
                        options = Some(advance_options);
                    }
                }
            }

            // If there was not a valid direct path (or we didn't check those because we entered a
            // defer), that's ok, we'll just try with non-collecting edges.
            let mut options = options.unwrap_or_else(Vec::new);
            if let OpPathElement::Field(operation_field) = operation_element {
                // Add whatever options can be obtained by taking some non-collecting edges first.
                let paths_with_non_collecting_edges = self
                    .indirect_options(
                        path_index,
                        condition_resolver,
                        override_conditions,
                        disabled_subgraphs,
                    )?
                    .filter_non_collecting_paths_for_field(operation_field)?;
                if !paths_with_non_collecting_edges.paths.is_empty() {
                    debug!(
                        "{} indirect paths",
                        paths_with_non_collecting_edges.paths.len()
                    );
                    for path_with_non_collecting_edges in
                        paths_with_non_collecting_edges.paths.iter()
                    {
                        debug!("For indirect path {path_with_non_collecting_edges}:");
                        let span = debug_span!(" |");
                        let _gaurd = span.enter();
                        let (advance_options, _) = path_with_non_collecting_edges
                            .advance_with_operation_element(
                                supergraph_schema.clone(),
                                operation_element,
                                &updated_context,
                                condition_resolver,
                                override_conditions,
                                check_cancellation,
                                disabled_subgraphs,
                            )?;
                        // If we can't advance the operation element after that path, ignore it,
                        // it's just not an option.
                        let Some(advance_options) = advance_options else {
                            debug!("Ignoring: cannot be advanced with {operation_element}");
                            continue;
                        };
                        debug!("Adding valid option: {advance_options:?}");
                        // `advance_with_operation_element()` can return an empty `Vec` only if the
                        // operation element is a fragment with a type condition that, on top of the
                        // "current" type is unsatisfiable. But as we've only taken type-preserving
                        // transitions, we cannot get an empty result at this point if we didn't get
                        // one when testing direct transitions above (in which case we would have
                        // exited the method early).
                        if advance_options.is_empty() {
                            return Err(FederationError::internal(format!(
                                "Unexpected empty options after non-collecting path {} for {}",
                                path_with_non_collecting_edges, operation_element,
                            )));
                        }
                        // There is a special case we can deal with now. Namely, suppose we have a
                        // case where a query is reaching an interface I in a subgraph S1, we query
                        // some field of that interface x, and say that x is provided in subgraph S2
                        // but by an @interfaceObject for I.
                        //
                        // As we look for direct options for I.x in S1 initially, we won't find `x`,
                        // so we will try to type-explode I (let's say into implementations A and
                        // B). And in some cases doing so is necessary, but it may also lead to the
                        // type-exploding option to look like:
                        //  [
                        //    I(S1) -[... on A]-> A(S1) -[key]-> I(S2) -[x] -> Int(S2),
                        //    I(S1) -[... on B]-> B(S1) -[key]-> I(S2) -[x] -> Int(S2),
                        //  ]
                        // But as we look at indirect options now (still from I in S1), we will note
                        // that we can also do:
                        //    I(S1) -[key]-> I(S2) -[x] -> Int(S2),
                        // And while both options are technically valid, the new one really subsumes
                        // the first one: there is no point in type-exploding to take a key to the
                        // same exact subgraph if using the key on the interface directly works.
                        //
                        // So here, we look for that case and remove any type-exploding option that
                        // the new path renders unnecessary. Do note that we only make that check
                        // when the new option is a single-path option, because this gets kind of
                        // complicated otherwise.
                        if path_with_non_collecting_edges.tail_is_interface_object()? {
                            for indirect_option in &advance_options {
                                if indirect_option.0.len() == 1 {
                                    let mut new_options = vec![];
                                    for option in options {
                                        let mut is_equivalent = true;
                                        for path in &option.0 {
                                            is_equivalent = is_equivalent
                                                && indirect_option.0[0]
                                                    .is_equivalent_save_for_type_explosion_to(
                                                        path,
                                                    )?;
                                        }
                                        if !is_equivalent {
                                            new_options.push(option)
                                        }
                                    }
                                    options = new_options;
                                }
                            }
                        }
                        options.extend(advance_options);
                    }
                } else {
                    debug!("no indirect paths");
                }
            }

            // If we were entering a @defer, we've skipped the potential "direct" options because we
            // need an "indirect" one (a key/root query) to be able to actually defer. But in rare
            // cases, it's possible we actually couldn't resolve the key fields needed to take a key
            // but could still find a direct path. If so, it means it's a corner case where we
            // cannot do query-planner-based-@defer and have to fall back on not deferring.
            if options.is_empty() && should_reenter_subgraph {
                let span = debug_span!(
                    "Cannot defer (no indirect options); falling back to direct options"
                );
                let _guard = span.enter();
                let (advance_options, _) = path.advance_with_operation_element(
                    supergraph_schema.clone(),
                    operation_element,
                    &updated_context,
                    condition_resolver,
                    override_conditions,
                    check_cancellation,
                    disabled_subgraphs,
                )?;
                options = advance_options.unwrap_or_else(Vec::new);
                debug!("{options:?}");
            }

            // At this point, if options is empty, it means we found no ways to advance the
            // operation element for this path, so we should return `None`.
            if options.is_empty() {
                drop(gaurd);
                debug!("No valid options for {operation_element}, aborting.");
                return Ok(None);
            } else {
                options_for_each_path.push(options);
            }
        }

        let all_options =
            SimultaneousPaths::flat_cartesian_product(options_for_each_path, check_cancellation)?;
        debug!("{all_options:?}");
        Ok(Some(self.create_lazy_options(all_options, updated_context)))
    }
}

// PORT_NOTE: JS passes a ConditionResolver here, we do not: see port note for
// `SimultaneousPathsWithLazyIndirectPaths`
#[allow(clippy::too_many_arguments)]
pub(crate) fn create_initial_options(
    initial_path: GraphPath<OpGraphPathTrigger, Option<EdgeIndex>>,
    initial_type: &QueryGraphNodeType,
    initial_context: OpGraphPathContext,
    condition_resolver: &mut impl ConditionResolver,
    excluded_edges: ExcludedDestinations,
    excluded_conditions: ExcludedConditions,
    override_conditions: &OverrideConditions,
    disabled_subgraphs: &IndexSet<Arc<str>>,
) -> Result<Vec<SimultaneousPathsWithLazyIndirectPaths>, FederationError> {
    let initial_paths = SimultaneousPaths::from(initial_path);
    let mut lazy_initial_path = SimultaneousPathsWithLazyIndirectPaths::new(
        initial_paths,
        initial_context.clone(),
        excluded_edges,
        excluded_conditions,
    );

    if initial_type.is_federated_root_type() {
        let initial_options = lazy_initial_path.indirect_options(
            0,
            condition_resolver,
            override_conditions,
            disabled_subgraphs,
        )?;
        let options = initial_options
            .paths
            .iter()
            .cloned()
            .map(SimultaneousPaths::from)
            .collect();
        Ok(lazy_initial_path.create_lazy_options(options, initial_context))
    } else {
        Ok(vec![lazy_initial_path])
    }
}

impl ClosedBranch {
    /// This method is called on a closed branch (i.e. on all the possible options found to get a
    /// particular leaf of the query being planned), and when there is more than one option, it
    /// tries a last effort at checking an option can be shown to be less efficient than another one
    /// _whatever the rest of the query plan is_ (that is, whatever the options for any other leaf
    /// of the query are).
    ///
    /// In practice, this compares all pairs of options and calls the heuristics of
    /// `compare_options_complexity_out_of_context()` on them to see if one strictly subsumes the
    /// other (and if that's the case, the subsumed one is ignored).
    pub(crate) fn maybe_eliminate_strictly_more_costly_paths(
        self,
    ) -> Result<ClosedBranch, FederationError> {
        if self.0.len() <= 1 {
            return Ok(self);
        }

        // Keep track of which options should be kept.
        let mut keep_options = vec![true; self.0.len()];
        for option_index in 0..(self.0.len()) {
            if !keep_options[option_index] {
                continue;
            }
            // We compare the current option to every other remaining option.
            //
            // PORT_NOTE: We don't technically need to iterate in reverse order here, but the JS
            // codebase does, and we do the same to ensure the result is the same. (The JS codebase
            // requires this because it removes from the array it's iterating through.)
            let option = &self.0[option_index];
            let mut keep_option = true;
            for (other_option, keep_other_option) in self.0[(option_index + 1)..]
                .iter()
                .zip(&mut keep_options[(option_index + 1)..])
                .rev()
            {
                if !*keep_other_option {
                    continue;
                }
                match option
                    .paths
                    .compare_options_complexity_out_of_context(&other_option.paths)?
                {
                    Ordering::Less => {
                        *keep_other_option = false;
                    }
                    Ordering::Equal => {}
                    Ordering::Greater => {
                        keep_option = false;
                        break;
                    }
                }
            }
            if !keep_option {
                keep_options[option_index] = false;
            }
        }

        Ok(ClosedBranch(
            self.0
                .into_iter()
                .zip(&keep_options)
                .filter(|&(_, &keep_option)| keep_option)
                .map(|(option, _)| option)
                .collect(),
        ))
    }
}

impl OpPath {
    pub(crate) fn len(&self) -> usize {
        self.0.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub(crate) fn strip_prefix(&self, maybe_prefix: &Self) -> Option<Self> {
        self.0
            .strip_prefix(&*maybe_prefix.0)
            .map(|slice| Self(slice.to_vec()))
    }

    pub(crate) fn with_pushed(&self, element: Arc<OpPathElement>) -> Self {
        let mut new = self.0.clone();
        new.push(element);
        Self(new)
    }

    pub(crate) fn conditional_directives(&self) -> DirectiveList {
        self.0
            .iter()
            .flat_map(|path_element| {
                path_element
                    .directives()
                    .iter()
                    .filter(|d| d.name == "include" || d.name == "skip")
            })
            .cloned()
            .collect()
    }

    /// Filter any fragment element in the provided path whose type condition does not exist in the provided schema.
    /// Not that if the fragment element should be filtered but it has applied directives, then we preserve those applications by
    /// replacing with a fragment with no condition (but if there are no directive, we simply remove the fragment from the path).
    // JS PORT NOTE: this method was called filterOperationPath in JS codebase
    pub(crate) fn filter_on_schema(&self, schema: &ValidFederationSchema) -> OpPath {
        let mut filtered: Vec<Arc<OpPathElement>> = vec![];
        for element in &self.0 {
            match element.as_ref() {
                OpPathElement::InlineFragment(fragment) => {
                    if let Some(type_condition) = &fragment.type_condition_position {
                        if schema.get_type(type_condition.type_name().clone()).is_err() {
                            if element.directives().is_empty() {
                                continue; // skip this element
                            } else {
                                // Replace this element with an unconditioned inline fragment
                                let updated_fragment = fragment.with_updated_type_condition(None);
                                filtered.push(Arc::new(OpPathElement::InlineFragment(
                                    updated_fragment,
                                )));
                            }
                        } else {
                            filtered.push(element.clone());
                        }
                    } else {
                        filtered.push(element.clone());
                    }
                }
                _ => {
                    filtered.push(element.clone());
                }
            }
        }
        OpPath(filtered)
    }

    pub(crate) fn has_only_fragments(&self) -> bool {
        // JS PORT NOTE: this was checking for FragmentElement which was used for both inline fragments and spreads
        self.0
            .iter()
            .all(|p| matches!(p.as_ref(), OpPathElement::InlineFragment(_)))
    }
}

pub(crate) fn concat_paths_in_parents(
    first: &Option<Arc<OpPath>>,
    second: &Option<Arc<OpPath>>,
) -> Option<Arc<OpPath>> {
    if let (Some(first), Some(second)) = (first, second) {
        Some(Arc::new(concat_op_paths(first.deref(), second.deref())))
    } else {
        None
    }
}

pub(crate) fn concat_op_paths(head: &OpPath, tail: &OpPath) -> OpPath {
    // While this is mainly a simple array concatenation, we optimize slightly by recognizing if the
    // tail path starts by a fragment selection that is useless given the end of the head path
    let Some(last_of_head) = head.last() else {
        return tail.clone();
    };
    let mut result = head.clone();
    if tail.is_empty() {
        return result;
    }
    let conditionals = head.conditional_directives();
    let tail_path = tail.0.clone();

    // Note that in practice, we may be able to eliminate a few elements at the beginning of the path
    // due do conditionals ('@skip' and '@include'). Indeed, a (tail) path crossing multiple conditions
    // may start with: [ ... on X @include(if: $c1), ... on X @skip(if: $c2), (...)], but if `head`
    // already ends on type `X` _and_ both the conditions on `$c1` and `$c2` are already found on `head`,
    // then we can remove both fragments in `tail`.
    let mut tail_iter = tail_path.iter();
    for tail_node in &mut tail_iter {
        if !is_useless_followup_element(last_of_head, tail_node, &conditionals)
            .is_ok_and(|is_useless| is_useless)
        {
            result.0.push(tail_node.clone());
            break;
        }
    }
    result.0.extend(tail_iter.cloned());
    result
}

fn is_useless_followup_element(
    first: &OpPathElement,
    followup: &OpPathElement,
    conditionals: &DirectiveList,
) -> Result<bool, FederationError> {
    let type_of_first: Option<CompositeTypeDefinitionPosition> = match first {
        OpPathElement::Field(field) => Some(field.output_base_type()?.try_into()?),
        OpPathElement::InlineFragment(fragment) => fragment.type_condition_position.clone(),
    };

    let Some(type_of_first) = type_of_first else {
        return Ok(false);
    };

    // The followup is useless if it's a fragment (with no directives we would want to preserve) whose type
    // is already that of the first element (or a supertype).
    match followup {
        OpPathElement::Field(_) => Ok(false),
        OpPathElement::InlineFragment(fragment) => {
            let Some(type_of_second) = fragment.type_condition_position.clone() else {
                return Ok(false);
            };

            let are_useless_directives = fragment.directives.is_empty()
                || fragment.directives.iter().all(|d| conditionals.contains(d));
            let is_same_type = type_of_first.type_name() == type_of_second.type_name();
            let is_subtype = first
                .schema()
                .schema()
                .is_subtype(type_of_second.type_name(), type_of_first.type_name());
            Ok(are_useless_directives && (is_same_type || is_subtype))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use apollo_compiler::Name;
    use apollo_compiler::Schema;
    use petgraph::stable_graph::EdgeIndex;
    use petgraph::stable_graph::NodeIndex;

    use crate::operation::Field;
    use crate::query_graph::build_query_graph::build_query_graph;
    use crate::query_graph::condition_resolver::ConditionResolution;
    use crate::query_graph::graph_path::operation::OpGraphPath;
    use crate::query_graph::graph_path::operation::OpGraphPathTrigger;
    use crate::query_graph::graph_path::operation::OpPathElement;
    use crate::schema::ValidFederationSchema;
    use crate::schema::position::ObjectFieldDefinitionPosition;

    #[test]
    fn path_display() {
        let src = r#"
       type Query
       {
          t: T
       }

       type T
       {
          otherId: ID!
          id: ID!
       }
        "#;
        let schema = Schema::parse_and_validate(src, "./").unwrap();
        let schema = ValidFederationSchema::new(schema).unwrap();
        let name = "S1".into();
        let graph = build_query_graph(name, schema.clone()).unwrap();
        let path = OpGraphPath::new(Arc::new(graph), NodeIndex::new(0)).unwrap();
        // NOTE: in general GraphPath would be used against a federated supergraph which would have
        // a root node [query](_)* followed by a Query(S1) node
        // This test is run against subgraph schema meaning it will start from Query(S1) node instead
        assert_eq!(path.to_string(), "Query(S1) (types: [Query])");
        let pos = ObjectFieldDefinitionPosition {
            type_name: Name::new("T").unwrap(),
            field_name: Name::new("t").unwrap(),
        };
        let field = Field::from_position(&schema, pos.into());
        let trigger = OpGraphPathTrigger::OpPathElement(OpPathElement::Field(field));
        let path = path
            .add(
                trigger,
                Some(EdgeIndex::new(3)),
                ConditionResolution::Satisfied {
                    cost: 0.0,
                    path_tree: None,
                    context_map: None,
                },
                None,
            )
            .unwrap();
        assert_eq!(path.to_string(), "Query(S1) --[t]--> T(S1) (types: [T])");
        let pos = ObjectFieldDefinitionPosition {
            type_name: Name::new("ID").unwrap(),
            field_name: Name::new("id").unwrap(),
        };
        let field = Field::from_position(&schema, pos.into());
        let trigger = OpGraphPathTrigger::OpPathElement(OpPathElement::Field(field));
        let path = path
            .add(
                trigger,
                Some(EdgeIndex::new(1)),
                ConditionResolution::Satisfied {
                    cost: 0.0,
                    path_tree: None,
                    context_map: None,
                },
                None,
            )
            .unwrap();
        assert_eq!(
            path.to_string(),
            "Query(S1) --[t]--> T(S1) --[id]--> ID(S1)"
        );
    }
}
