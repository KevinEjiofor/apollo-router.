use std::cell::Cell;
use std::num::NonZeroU32;
use std::ops::ControlFlow;
use std::sync::Arc;

use apollo_compiler::ExecutableDocument;
use apollo_compiler::Name;
use apollo_compiler::collections::IndexMap;
use apollo_compiler::collections::IndexSet;
use apollo_compiler::validation::Valid;
use itertools::Itertools;
use serde::Deserialize;
use serde::Serialize;
use tracing::trace;

use super::ConditionNode;
use super::QueryPlanCost;
use super::fetch_dependency_graph::FetchIdGenerator;
use crate::ApiSchemaOptions;
use crate::Supergraph;
use crate::bail;
use crate::error::FederationError;
use crate::error::SingleFederationError;
use crate::internal_error;
use crate::operation::NormalizedDefer;
use crate::operation::Operation;
use crate::operation::SelectionSet;
use crate::operation::normalize_operation;
use crate::query_graph::OverrideConditions;
use crate::query_graph::QueryGraph;
use crate::query_graph::QueryGraphNodeType;
use crate::query_graph::build_federated_query_graph;
use crate::query_graph::path_tree::OpPathTree;
use crate::query_plan::PlanNode;
use crate::query_plan::QueryPlan;
use crate::query_plan::SequenceNode;
use crate::query_plan::TopLevelPlanNode;
use crate::query_plan::fetch_dependency_graph::FetchDependencyGraph;
use crate::query_plan::fetch_dependency_graph::FetchDependencyGraphNodePath;
use crate::query_plan::fetch_dependency_graph::compute_nodes_for_tree;
use crate::query_plan::fetch_dependency_graph_processor::FetchDependencyGraphProcessor;
use crate::query_plan::fetch_dependency_graph_processor::FetchDependencyGraphToCostProcessor;
use crate::query_plan::fetch_dependency_graph_processor::FetchDependencyGraphToQueryPlanProcessor;
use crate::query_plan::query_planning_traversal::BestQueryPlanInfo;
use crate::query_plan::query_planning_traversal::QueryPlanningParameters;
use crate::query_plan::query_planning_traversal::QueryPlanningTraversal;
use crate::query_plan::query_planning_traversal::convert_type_from_subgraph;
use crate::query_plan::query_planning_traversal::non_local_selections_estimation;
use crate::schema::ValidFederationSchema;
use crate::schema::position::AbstractTypeDefinitionPosition;
use crate::schema::position::CompositeTypeDefinitionPosition;
use crate::schema::position::InterfaceTypeDefinitionPosition;
use crate::schema::position::ObjectTypeDefinitionPosition;
use crate::schema::position::OutputTypeDefinitionPosition;
use crate::schema::position::SchemaRootDefinitionKind;
use crate::schema::position::TypeDefinitionPosition;
use crate::utils::logging::snapshot;

#[derive(Debug, Clone, Hash, Serialize)]
pub struct QueryPlannerConfig {
    /// If enabled, the query planner will attempt to extract common subselections into named
    /// fragments. This can significantly reduce the size of the query sent to subgraphs.
    ///
    /// Defaults to false.
    pub generate_query_fragments: bool,

    /// **TODO:** This option is not implemented, and the behaviour is *always enabled*.
    /// <https://github.com/apollographql/router/pull/5871>
    ///
    /// Whether to run GraphQL validation against the extracted subgraph schemas. Recommended in
    /// non-production settings or when debugging.
    ///
    /// Defaults to false.
    pub subgraph_graphql_validation: bool,

    // Side-note: implemented as an object instead of single boolean because we expect to add more
    // to this soon enough. In particular, once defer-passthrough to subgraphs is implemented, the
    // idea would be to add a new `passthrough_subgraphs` option that is the list of subgraphs to
    // which we can pass through some @defer (and it would be empty by default). Similarly, once we
    // support @stream, grouping the options here will make sense too.
    pub incremental_delivery: QueryPlanIncrementalDeliveryConfig,

    /// A sub-set of configurations that are meant for debugging or testing. All the configurations
    /// in this sub-set are provided without guarantees of stability (they may be dangerous) or
    /// continued support (they may be removed without warning).
    pub debug: QueryPlannerDebugConfig,

    /// Enables type conditioned fetching.
    /// This flag is a workaround, which may yield significant
    /// performance degradation when computing query plans,
    /// and increase query plan size.
    ///
    /// If you aren't aware of this flag, you probably don't need it.
    pub type_conditioned_fetching: bool,
}

#[allow(clippy::derivable_impls)] // it's derivable right now, but we might change the defaults
impl Default for QueryPlannerConfig {
    fn default() -> Self {
        Self {
            generate_query_fragments: false,
            subgraph_graphql_validation: false,
            incremental_delivery: Default::default(),
            debug: Default::default(),
            type_conditioned_fetching: false,
        }
    }
}

#[derive(Debug, Clone, Default, Hash, Serialize)]
pub struct QueryPlanIncrementalDeliveryConfig {
    /// Enables `@defer` support in the query planner, breaking up the query plan with [DeferNode]s
    /// as appropriate.
    ///
    /// If false, operations with `@defer` are still accepted, but are planned as if they did not
    /// contain `@defer` directives.
    ///
    /// Defaults to false.
    ///
    /// [DeferNode]: crate::query_plan::DeferNode
    #[serde(default)]
    pub enable_defer: bool,
}

#[derive(Debug, Clone, Hash, Serialize)]
pub struct QueryPlannerDebugConfig {
    /// Query planning is an exploratory process. Depending on the specificities and feature used by
    /// subgraphs, there could exist may different theoretical valid (if not always efficient) plans
    /// for a given query, and at a high level, the query planner generates those possible choices,
    /// evaluates them, and return the best one. In some complex cases however, the number of
    /// theoretically possible plans can be very large, and to keep query planning time acceptable,
    /// the query planner caps the maximum number of plans it evaluates. This config allows to
    /// configure that cap. Note if planning a query hits that cap, then the planner will still
    /// always return a "correct" plan, but it may not return _the_ optimal one, so this config can
    /// be considered a trade-off between the worst-time for query planning computation processing,
    /// and the risk of having non-optimal query plans (impacting query runtimes).
    ///
    /// This value currently defaults to 10000, but this default is considered an implementation
    /// detail and is subject to change. We do not recommend setting this value unless it is to
    /// debug a specific issue (with unexpectedly slow query planning for instance). Remember that
    /// setting this value too low can negatively affect query runtime (due to the use of
    /// sub-optimal query plans).
    // TODO: should there additionally be a max_evaluated_cost?
    pub max_evaluated_plans: NonZeroU32,

    /// Before creating query plans, for each path of fields in the query we compute all the
    /// possible options to traverse that path via the subgraphs. Multiple options can arise because
    /// fields in the path can be provided by multiple subgraphs, and abstract types (i.e. unions
    /// and interfaces) returned by fields sometimes require the query planner to traverse through
    /// each constituent object type. The number of options generated in this computation can grow
    /// large if the schema or query are sufficiently complex, and that will increase the time spent
    /// planning.
    ///
    /// This config allows specifying a per-path limit to the number of options considered. If any
    /// path's options exceeds this limit, query planning will abort and the operation will fail.
    ///
    /// The default value is None, which specifies no limit.
    pub paths_limit: Option<u32>,
}

impl Default for QueryPlannerDebugConfig {
    fn default() -> Self {
        Self {
            max_evaluated_plans: NonZeroU32::new(10_000).unwrap(),
            paths_limit: None,
        }
    }
}

// PORT_NOTE: renamed from PlanningStatistics in the JS codebase.
#[derive(Debug, PartialEq, Default, Serialize, Deserialize)]
pub struct QueryPlanningStatistics {
    pub evaluated_plan_count: Cell<usize>,
    pub evaluated_plan_paths: Cell<usize>,
    /// `best_plan_cost` can be NaN, if the cost is not computed or irrelevant.
    pub best_plan_cost: f64,
}

#[derive(Clone)]
pub struct QueryPlanOptions<'a> {
    /// A set of labels which will be used _during query planning_ to
    /// enable/disable edges with a matching label in their override condition.
    /// Edges with override conditions require their label to be present or absent
    /// from this set in order to be traversable. These labels enable the
    /// progressive @override feature.
    // PORT_NOTE: In JS implementation this was a Map
    pub override_conditions: Vec<String>,
    /// An optional function that will be called to check if the query plan should be cancelled.
    ///
    /// Cooperative cancellation occurs when the original client has abandoned the query.
    /// When this happens, the query plan should be cancelled to free up resources.
    ///
    /// This function should return `ControlFlow::Break` if the query plan should be cancelled.
    ///
    /// Defaults to `None`.
    pub check_for_cooperative_cancellation: Option<&'a dyn Fn() -> ControlFlow<()>>,
    /// Impose a limit on the number of non-local selections, which can be a
    /// performance hazard. On by default.
    pub non_local_selections_limit_enabled: bool,
    /// Names of subgraphs that are disabled and should be avoided during
    /// planning. If this is non-empty, query planner may error if it cannot
    /// find a plan that doesn't use the disabled subgraphs, specifically with
    /// `SingleFederationError::NoPlanFoundWithDisabledSubgraphs`.
    pub disabled_subgraph_names: IndexSet<String>,
}

impl Default for QueryPlanOptions<'_> {
    fn default() -> Self {
        Self {
            override_conditions: Vec::new(),
            check_for_cooperative_cancellation: None,
            non_local_selections_limit_enabled: true,
            disabled_subgraph_names: Default::default(),
        }
    }
}

impl std::fmt::Debug for QueryPlanOptions<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QueryPlanOptions")
            .field("override_conditions", &self.override_conditions)
            .field(
                "check_for_cooperative_cancellation",
                if self.check_for_cooperative_cancellation.is_some() {
                    &"Some(...)"
                } else {
                    &"None"
                },
            )
            .field(
                "non_local_selections_limit_enabled",
                &self.non_local_selections_limit_enabled,
            )
            .finish()
    }
}

pub struct QueryPlanner {
    config: QueryPlannerConfig,
    federated_query_graph: Arc<QueryGraph>,
    supergraph_schema: ValidFederationSchema,
    api_schema: ValidFederationSchema,
    /// A set of the names of interface types for which at least one subgraph use an
    /// @interfaceObject to abstract that interface.
    interface_types_with_interface_objects: IndexSet<InterfaceTypeDefinitionPosition>,
    /// A set of the names of interface or union types that have inconsistent "runtime types" across
    /// subgraphs.
    // PORT_NOTE: Named `inconsistentAbstractTypesRuntimes` in the JS codebase, which was slightly
    // confusing.
    abstract_types_with_inconsistent_runtime_types: IndexSet<Name>,
}

impl QueryPlanner {
    #[cfg_attr(
        feature = "snapshot_tracing",
        tracing::instrument(level = "trace", skip_all, name = "QueryPlanner::new")
    )]
    pub fn new(
        supergraph: &Supergraph,
        config: QueryPlannerConfig,
    ) -> Result<Self, FederationError> {
        let supergraph_schema = supergraph.schema.clone();
        let api_schema = supergraph.to_api_schema(ApiSchemaOptions {
            include_defer: config.incremental_delivery.enable_defer,
            ..Default::default()
        })?;
        let query_graph = build_federated_query_graph(
            supergraph_schema.clone(),
            api_schema.clone(),
            Some(true),
            Some(true),
        )?;

        let interface_types_with_interface_objects = supergraph
            .schema
            .get_types()
            .filter_map(|position| match position {
                TypeDefinitionPosition::Interface(interface_position) => Some(interface_position),
                _ => None,
            })
            .map(|position| {
                let is_interface_object = query_graph
                    .subgraphs()
                    .map(|(_name, schema)| {
                        let Some(position) = schema.try_get_type(position.type_name.clone()) else {
                            return Ok(false);
                        };
                        schema.is_interface_object_type(position)
                    })
                    .process_results(|mut iter| iter.any(|b| b))?;
                Ok::<_, FederationError>((position, is_interface_object))
            })
            .process_results(|iter| {
                iter.flat_map(|(position, is_interface_object)| {
                    if is_interface_object {
                        Some(position)
                    } else {
                        None
                    }
                })
                .collect::<IndexSet<_>>()
            })?;

        let is_inconsistent = |position: AbstractTypeDefinitionPosition| {
            let mut sources = query_graph.subgraphs().filter_map(|(_name, subgraph)| {
                match subgraph.try_get_type(position.type_name().clone())? {
                    // This is only called for type names that are abstract in the supergraph, so it
                    // can only be an object in a subgraph if it is an `@interfaceObject`. And as `@interfaceObject`s
                    // "stand-in" for all possible runtime types, they don't create inconsistencies by themselves
                    // and we can ignore them.
                    TypeDefinitionPosition::Object(_) => None,
                    TypeDefinitionPosition::Interface(interface) => Some(
                        subgraph
                            .referencers()
                            .get_interface_type(&interface.type_name)
                            .ok()?
                            .object_types
                            .clone(),
                    ),
                    TypeDefinitionPosition::Union(union_) => Some(
                        union_
                            .try_get(subgraph.schema())?
                            .members
                            .iter()
                            .map(|member| ObjectTypeDefinitionPosition::new(member.name.clone()))
                            .collect(),
                    ),
                    _ => None,
                }
            });

            let Some(expected_runtimes) = sources.next() else {
                return false;
            };
            !sources.all(|runtimes| runtimes == expected_runtimes)
        };

        let abstract_types_with_inconsistent_runtime_types = supergraph
            .schema
            .get_types()
            .filter_map(|position| AbstractTypeDefinitionPosition::try_from(position).ok())
            .filter(|position| is_inconsistent(position.clone()))
            .map(|position| position.type_name().clone())
            .collect::<IndexSet<_>>();

        Ok(Self {
            config,
            federated_query_graph: Arc::new(query_graph),
            supergraph_schema,
            api_schema,
            interface_types_with_interface_objects,
            abstract_types_with_inconsistent_runtime_types,
        })
    }

    pub fn subgraph_schemas(&self) -> &IndexMap<Arc<str>, ValidFederationSchema> {
        self.federated_query_graph.subgraph_schemas()
    }

    // PORT_NOTE: this receives an `Operation` object in JS which is a concept that doesn't exist in apollo-rs.
    #[cfg_attr(
        feature = "snapshot_tracing",
        tracing::instrument(level = "trace", skip_all, name = "QueryPlanner::build_query_plan")
    )]
    pub fn build_query_plan(
        &self,
        document: &Valid<ExecutableDocument>,
        operation_name: Option<Name>,
        options: QueryPlanOptions,
    ) -> Result<QueryPlan, FederationError> {
        let operation = document
            .operations
            .get(operation_name.as_ref().map(|name| name.as_str()))
            .map_err(|_| {
                if operation_name.is_some() {
                    SingleFederationError::UnknownOperation
                } else {
                    SingleFederationError::OperationNameNotProvided
                }
            })?;
        if operation.selection_set.is_empty() {
            // This should never happen because `operation` comes from a known-valid document.
            crate::bail!("Invalid operation: empty selection set")
        }

        let is_subscription = operation.is_subscription();

        let statistics = QueryPlanningStatistics::default();

        let normalized_operation = normalize_operation(
            operation,
            &document.fragments,
            &self.api_schema,
            &self.interface_types_with_interface_objects,
            &|| {
                QueryPlanningParameters::check_cancellation_with(
                    &options.check_for_cooperative_cancellation,
                )
            },
        )?;

        let NormalizedDefer {
            operation: normalized_operation,
            assigned_defer_labels,
            defer_conditions,
            has_defers,
        } = normalized_operation.with_normalized_defer()?;
        if has_defers && is_subscription {
            return Err(SingleFederationError::DeferredSubscriptionUnsupported.into());
        }

        if normalized_operation.selection_set.is_empty() {
            return Ok(QueryPlan::default());
        }

        snapshot!(
            "NormalizedOperation",
            serde_json_bytes::json!({
                "original": &operation.serialize().to_string(),
                "normalized": &normalized_operation.to_string()
            })
            .to_string(),
            "normalized operation"
        );

        let Some(root) = self
            .federated_query_graph
            .root_kinds_to_nodes()?
            .get(&normalized_operation.root_kind)
        else {
            bail!(
                "Shouldn't have a {0} operation if the subgraphs don't have a {0} root",
                normalized_operation.root_kind
            )
        };

        let operation_compression = if self.config.generate_query_fragments {
            SubgraphOperationCompression::GenerateFragments
        } else {
            SubgraphOperationCompression::Disabled
        };
        let mut processor = FetchDependencyGraphToQueryPlanProcessor::new(
            normalized_operation.variables.clone(),
            normalized_operation.directives.clone(),
            operation_compression,
            operation.name.clone(),
            assigned_defer_labels,
        );
        let mut parameters = QueryPlanningParameters {
            supergraph_schema: self.supergraph_schema.clone(),
            federated_query_graph: self.federated_query_graph.clone(),
            operation: Arc::new(normalized_operation),
            head: *root,
            // PORT_NOTE(@goto-bus-stop): In JS, `root` is a `RootVertex`, which is dynamically
            // checked at various points in query planning. This is our Rust equivalent of that.
            head_must_be_root: true,
            statistics: &statistics,
            abstract_types_with_inconsistent_runtime_types: self
                .abstract_types_with_inconsistent_runtime_types
                .clone()
                .into(),
            config: self.config.clone(),
            override_conditions: OverrideConditions::new(
                &self.federated_query_graph,
                &IndexSet::from_iter(options.override_conditions),
            ),
            check_for_cooperative_cancellation: options.check_for_cooperative_cancellation,
            fetch_id_generator: Arc::new(FetchIdGenerator::new()),
            disabled_subgraphs: self
                .federated_query_graph
                .subgraphs()
                .filter_map(|(subgraph, _)| {
                    if options.disabled_subgraph_names.contains(subgraph.as_ref()) {
                        Some(subgraph.clone())
                    } else {
                        None
                    }
                })
                .collect(),
        };

        let mut non_local_selection_state = options
            .non_local_selections_limit_enabled
            .then(non_local_selections_estimation::State::default);
        let (root_node, cost) = if !defer_conditions.is_empty() {
            compute_plan_for_defer_conditionals(
                &mut parameters,
                &mut processor,
                defer_conditions,
                &mut non_local_selection_state,
            )
        } else {
            compute_plan_internal(
                &mut parameters,
                &mut processor,
                has_defers,
                &mut non_local_selection_state,
            )
        }?;

        let root_node = match root_node {
            // If this is a subscription, we want to make sure that we return a SubscriptionNode rather than a PlanNode
            // We potentially will need to separate "primary" from "rest"
            // Note that if it is a subscription, we are guaranteed that nothing is deferred.
            Some(PlanNode::Fetch(root_node)) if is_subscription => Some(
                TopLevelPlanNode::Subscription(crate::query_plan::SubscriptionNode {
                    primary: root_node,
                    rest: None,
                }),
            ),
            Some(PlanNode::Sequence(root_node)) if is_subscription => {
                let Some((primary, rest)) = root_node.nodes.split_first() else {
                    // TODO(@goto-bus-stop): We could probably guarantee this in the type system
                    bail!("Invalid query plan: Sequence must have at least one node");
                };
                let PlanNode::Fetch(primary) = primary.clone() else {
                    bail!("Invalid query plan: Primary node of a subscription is not a Fetch");
                };
                let rest = PlanNode::Sequence(SequenceNode {
                    nodes: rest.to_vec(),
                });
                Some(TopLevelPlanNode::Subscription(
                    crate::query_plan::SubscriptionNode {
                        primary,
                        rest: Some(Box::new(rest)),
                    },
                ))
            }
            Some(node) if is_subscription => {
                bail!(
                    "Invalid query plan for subscription: unexpected {} at root",
                    node.node_kind()
                );
            }
            Some(PlanNode::Fetch(inner)) => Some(TopLevelPlanNode::Fetch(inner)),
            Some(PlanNode::Sequence(inner)) => Some(TopLevelPlanNode::Sequence(inner)),
            Some(PlanNode::Parallel(inner)) => Some(TopLevelPlanNode::Parallel(inner)),
            Some(PlanNode::Flatten(inner)) => Some(TopLevelPlanNode::Flatten(inner)),
            Some(PlanNode::Defer(inner)) => Some(TopLevelPlanNode::Defer(inner)),
            Some(PlanNode::Condition(inner)) => Some(TopLevelPlanNode::Condition(inner)),
            None => None,
        };

        let plan = QueryPlan {
            node: root_node,
            statistics: QueryPlanningStatistics {
                best_plan_cost: cost,
                ..statistics
            },
        };

        snapshot!(
            "QueryPlan",
            plan.to_string(),
            "QueryPlan from build_query_plan"
        );
        snapshot!(
            plan.statistics,
            "QueryPlanningStatistics from build_query_plan"
        );

        Ok(plan)
    }

    /// Get Query Planner's API Schema.
    pub fn api_schema(&self) -> &ValidFederationSchema {
        &self.api_schema
    }

    pub fn supergraph_schema(&self) -> &ValidFederationSchema {
        &self.supergraph_schema
    }
}

fn compute_root_serial_dependency_graph(
    parameters: &QueryPlanningParameters,
    has_defers: bool,
    non_local_selection_state: &mut Option<non_local_selections_estimation::State>,
) -> Result<Vec<FetchDependencyGraph>, FederationError> {
    let QueryPlanningParameters {
        supergraph_schema,
        federated_query_graph,
        operation,
        ..
    } = parameters;
    let root_type: Option<CompositeTypeDefinitionPosition> = if has_defers {
        supergraph_schema
            .schema()
            .root_operation(operation.root_kind.into())
            .and_then(|name| supergraph_schema.get_type(name.clone()).ok())
            .and_then(|ty| ty.try_into().ok())
    } else {
        None
    };
    // We have to serially compute a plan for each top-level selection.
    let mut split_roots = operation.selection_set.clone().split_top_level_fields();
    let mut digest = Vec::new();
    let selection_set = split_roots
        .next()
        .ok_or_else(|| FederationError::internal("Empty top level fields"))?;
    let BestQueryPlanInfo {
        mut fetch_dependency_graph,
        path_tree: mut prev_path,
        ..
    } = compute_root_parallel_best_plan(
        parameters,
        selection_set,
        has_defers,
        non_local_selection_state,
    )?;
    let mut prev_subgraph = only_root_subgraph(&fetch_dependency_graph)?;
    for selection_set in split_roots {
        let BestQueryPlanInfo {
            fetch_dependency_graph: new_dep_graph,
            path_tree: new_path,
            ..
        } = compute_root_parallel_best_plan(
            parameters,
            selection_set,
            has_defers,
            non_local_selection_state,
        )?;
        let new_subgraph = only_root_subgraph(&new_dep_graph)?;
        if new_subgraph == prev_subgraph {
            // The new operation (think 'mutation' operation) is on the same subgraph than the previous one, so we can concat them in a single fetch
            // and rely on the subgraph to enforce seriability. Do note that we need to `concat()` and not `merge()` because if we have
            // mutation Mut {
            //    mut1 {...}
            //    mut2 {...}
            //    mut1 {...}
            // }
            // then we should _not_ merge the 2 `mut1` fields (contrarily to what happens on queried fields).

            Arc::make_mut(&mut prev_path).extend(&new_path);
            fetch_dependency_graph = FetchDependencyGraph::new(
                supergraph_schema.clone(),
                federated_query_graph.clone(),
                root_type.clone(),
                fetch_dependency_graph.fetch_id_generation.clone(),
            );
            compute_root_fetch_groups(
                operation.root_kind,
                federated_query_graph,
                &mut fetch_dependency_graph,
                &prev_path,
                parameters.config.type_conditioned_fetching,
                &|| parameters.check_cancellation(),
            )?;
        } else {
            // PORT_NOTE: It is unclear if they correct thing to do here is get the next ID, use
            // the current ID that is inside the fetch dep graph's ID generator, or to use the
            // starting ID. Because this method ensure uniqueness between IDs, this approach was
            // taken; however, it could be the case that this causes unforeseen issues.
            digest.push(std::mem::replace(
                &mut fetch_dependency_graph,
                new_dep_graph,
            ));
            prev_path = new_path;
            prev_subgraph = new_subgraph;
        }
    }
    digest.push(fetch_dependency_graph);
    Ok(digest)
}

fn only_root_subgraph(graph: &FetchDependencyGraph) -> Result<Arc<str>, FederationError> {
    let mut iter = graph.root_node_by_subgraph_iter();
    let (Some((name, _)), None) = (iter.next(), iter.next()) else {
        return Err(FederationError::internal(format!(
            "{graph} should have only one root."
        )));
    };
    Ok(name.clone())
}

#[cfg_attr(
    feature = "snapshot_tracing",
    tracing::instrument(level = "trace", skip_all, name = "compute_root_fetch_groups")
)]
pub(crate) fn compute_root_fetch_groups(
    root_kind: SchemaRootDefinitionKind,
    federated_query_graph: &QueryGraph,
    dependency_graph: &mut FetchDependencyGraph,
    path: &OpPathTree,
    type_conditioned_fetching_enabled: bool,
    check_cancellation: &dyn Fn() -> Result<(), SingleFederationError>,
) -> Result<(), FederationError> {
    // The root of the pathTree is one of the "fake" root of the subgraphs graph,
    // which belongs to no subgraph but points to each ones.
    // So we "unpack" the first level of the tree to find out our top level groups
    // (and initialize our stack).
    // Note that we can safely ignore the triggers of that first level
    // as it will all be free transition, and we know we cannot have conditions.
    for child in &path.childs {
        let edge = child.edge.expect("The root edge should not be None");
        let (_source_node, target_node) = path.graph.edge_endpoints(edge)?;
        let target_node = path.graph.node_weight(target_node)?;
        let subgraph_name = &target_node.source;
        let root_type: CompositeTypeDefinitionPosition = match &target_node.type_ {
            QueryGraphNodeType::SchemaType(OutputTypeDefinitionPosition::Object(object)) => {
                object.clone().into()
            }
            ty => {
                return Err(FederationError::internal(format!(
                    "expected an object type for the root of a subgraph, found {ty}"
                )));
            }
        };
        let fetch_dependency_node = dependency_graph.get_or_create_root_node(
            subgraph_name,
            root_kind,
            root_type.clone(),
        )?;
        snapshot!(
            "FetchDependencyGraph",
            dependency_graph.to_dot(),
            "tree_with_root_node"
        );
        let subgraph_schema = federated_query_graph.schema_by_source(subgraph_name)?;
        let supergraph_root_type = convert_type_from_subgraph(
            root_type,
            subgraph_schema,
            &dependency_graph.supergraph_schema,
        )?;
        compute_nodes_for_tree(
            dependency_graph,
            &child.tree,
            fetch_dependency_node,
            FetchDependencyGraphNodePath::new(
                dependency_graph.supergraph_schema.clone(),
                type_conditioned_fetching_enabled,
                supergraph_root_type,
            )?,
            Default::default(),
            &Default::default(),
            check_cancellation,
        )?;
    }
    Ok(())
}

fn compute_root_parallel_dependency_graph(
    parameters: &QueryPlanningParameters,
    has_defers: bool,
    non_local_selection_state: &mut Option<non_local_selections_estimation::State>,
) -> Result<(FetchDependencyGraph, QueryPlanCost), FederationError> {
    trace!("Starting process to construct a parallel fetch dependency graph");
    let selection_set = parameters.operation.selection_set.clone();
    let best_plan = compute_root_parallel_best_plan(
        parameters,
        selection_set,
        has_defers,
        non_local_selection_state,
    )?;
    snapshot!(
        "FetchDependencyGraph",
        best_plan.fetch_dependency_graph.to_dot(),
        "Fetch dependency graph returned from compute_root_parallel_best_plan"
    );
    Ok((best_plan.fetch_dependency_graph, best_plan.cost))
}

fn compute_root_parallel_best_plan(
    parameters: &QueryPlanningParameters,
    selection: SelectionSet,
    has_defers: bool,
    non_local_selection_state: &mut Option<non_local_selections_estimation::State>,
) -> Result<BestQueryPlanInfo, FederationError> {
    let planning_traversal = QueryPlanningTraversal::new(
        parameters,
        selection,
        has_defers,
        parameters.operation.root_kind,
        FetchDependencyGraphToCostProcessor,
        non_local_selection_state.as_mut(),
    )?;

    // Getting no plan means the query is essentially unsatisfiable (it's a valid query, but we can prove it will never return a result),
    // so we just return an empty plan.
    Ok(planning_traversal
        .find_best_plan()?
        .unwrap_or_else(|| BestQueryPlanInfo::empty(parameters)))
}

fn compute_plan_internal(
    parameters: &mut QueryPlanningParameters,
    processor: &mut FetchDependencyGraphToQueryPlanProcessor,
    has_defers: bool,
    non_local_selection_state: &mut Option<non_local_selections_estimation::State>,
) -> Result<(Option<PlanNode>, QueryPlanCost), FederationError> {
    let root_kind = parameters.operation.root_kind;

    let (main, deferred, primary_selection, cost) = if root_kind
        == SchemaRootDefinitionKind::Mutation
    {
        let dependency_graphs = compute_root_serial_dependency_graph(
            parameters,
            has_defers,
            non_local_selection_state,
        )?;
        let mut main = None;
        let mut deferred = vec![];
        let mut primary_selection = None::<SelectionSet>;
        for mut dependency_graph in dependency_graphs {
            let (local_main, local_deferred) =
                dependency_graph.process(&mut *processor, root_kind)?;
            main = match main {
                Some(unlocal_main) => processor.reduce_sequence([Some(unlocal_main), local_main]),
                None => local_main,
            };
            deferred.extend(local_deferred);
            let new_selection = dependency_graph.defer_tracking.primary_selection;
            match primary_selection.as_mut() {
                Some(selection) => {
                    if let Some(new_selection) = new_selection {
                        selection.add_local_selection_set(&new_selection)?
                    }
                }
                None => primary_selection = new_selection,
            }
        }
        // No cost computation necessary. Return NaN for cost.
        (main, deferred, primary_selection, f64::NAN)
    } else {
        let (mut dependency_graph, cost) = compute_root_parallel_dependency_graph(
            parameters,
            has_defers,
            non_local_selection_state,
        )?;

        let (main, deferred) = dependency_graph.process(&mut *processor, root_kind)?;
        snapshot!(
            "FetchDependencyGraph",
            dependency_graph.to_dot(),
            "Plan after calling FetchDependencyGraph::process"
        );
        // XXX(@goto-bus-stop) Maybe `.defer_tracking` should be on the return value of `process()`..?
        let primary_selection = dependency_graph.defer_tracking.primary_selection;

        (main, deferred, primary_selection, cost)
    };

    if deferred.is_empty() {
        Ok((main, cost))
    } else {
        let Some(primary_selection) = primary_selection else {
            unreachable!("Should have had a primary selection created");
        };
        let reduced_main = processor.reduce_defer(main, &primary_selection, deferred)?;
        Ok((reduced_main, cost))
    }
}

fn compute_plan_for_defer_conditionals(
    parameters: &mut QueryPlanningParameters,
    processor: &mut FetchDependencyGraphToQueryPlanProcessor,
    defer_conditions: IndexMap<Name, IndexSet<String>>,
    non_local_selection_state: &mut Option<non_local_selections_estimation::State>,
) -> Result<(Option<PlanNode>, QueryPlanCost), FederationError> {
    generate_condition_nodes(
        parameters.operation.clone(),
        defer_conditions.iter(),
        &mut |op| {
            parameters.operation = op;
            compute_plan_internal(parameters, processor, true, non_local_selection_state)
        },
    )
}

fn generate_condition_nodes<'a>(
    op: Arc<Operation>,
    mut conditions: impl Clone + Iterator<Item = (&'a Name, &'a IndexSet<String>)>,
    on_final_operation: &mut impl FnMut(
        Arc<Operation>,
    ) -> Result<(Option<PlanNode>, f64), FederationError>,
) -> Result<(Option<PlanNode>, f64), FederationError> {
    match conditions.next() {
        None => on_final_operation(op),
        Some((cond, labels)) => {
            let else_op = Arc::unwrap_or_clone(op.clone()).reduce_defer(labels)?;
            let if_op = op;
            let (if_node, if_cost) =
                generate_condition_nodes(if_op, conditions.clone(), on_final_operation)?;
            let (else_node, else_cost) = generate_condition_nodes(
                Arc::new(else_op),
                conditions.clone(),
                on_final_operation,
            )?;
            let node = ConditionNode {
                condition_variable: cond.clone(),
                if_clause: if_node.map(Box::new),
                else_clause: else_node.map(Box::new),
            };
            Ok((
                Some(PlanNode::Condition(Box::new(node))),
                if_cost.max(else_cost),
            ))
        }
    }
}

pub(crate) enum SubgraphOperationCompression {
    GenerateFragments,
    Disabled,
}

impl SubgraphOperationCompression {
    /// Compress a subgraph operation.
    pub(crate) fn compress(
        &mut self,
        operation: Operation,
    ) -> Result<Valid<ExecutableDocument>, FederationError> {
        match self {
            Self::GenerateFragments => Ok(operation.generate_fragments()?),
            Self::Disabled => {
                let operation_document = operation.try_into().map_err(|err: FederationError| {
                    if err.has_invalid_graphql_error() {
                        internal_error!(
                            "Query planning produced an invalid subgraph operation.\n{err}"
                        )
                    } else {
                        err
                    }
                })?;
                Ok(operation_document)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_SUPERGRAPH: &str = r#"
schema
  @link(url: "https://specs.apollo.dev/link/v1.0")
  @link(url: "https://specs.apollo.dev/join/v0.2", for: EXECUTION)
{
  query: Query
}

directive @join__field(graph: join__Graph!, requires: join__FieldSet, provides: join__FieldSet, type: String, external: Boolean, override: String, usedOverridden: Boolean) repeatable on FIELD_DEFINITION | INPUT_FIELD_DEFINITION

directive @join__graph(name: String!, url: String!) on ENUM_VALUE

directive @join__implements(graph: join__Graph!, interface: String!) repeatable on OBJECT | INTERFACE

directive @join__type(graph: join__Graph!, key: join__FieldSet, extension: Boolean! = false, resolvable: Boolean! = true) repeatable on OBJECT | INTERFACE | UNION | ENUM | INPUT_OBJECT | SCALAR

directive @link(url: String, as: String, for: link__Purpose, import: [link__Import]) repeatable on SCHEMA

type Book implements Product
  @join__implements(graph: PRODUCTS, interface: "Product")
  @join__implements(graph: REVIEWS, interface: "Product")
  @join__type(graph: PRODUCTS, key: "id")
  @join__type(graph: REVIEWS, key: "id")
{
  id: ID!
  price: Price @join__field(graph: PRODUCTS)
  title: String @join__field(graph: PRODUCTS)
  vendor: User @join__field(graph: PRODUCTS)
  pages: Int @join__field(graph: PRODUCTS)
  avg_rating: Int @join__field(graph: PRODUCTS, requires: "reviews { rating }")
  reviews: [Review] @join__field(graph: PRODUCTS, external: true) @join__field(graph: REVIEWS)
}

enum Currency
  @join__type(graph: PRODUCTS)
{
  USD
  EUR
}

scalar join__FieldSet

enum join__Graph {
  ACCOUNTS @join__graph(name: "accounts", url: "")
  PRODUCTS @join__graph(name: "products", url: "")
  REVIEWS @join__graph(name: "reviews", url: "")
}

scalar link__Import

enum link__Purpose {
  """
  `SECURITY` features provide metadata necessary to securely resolve fields.
  """
  SECURITY

  """
  `EXECUTION` features provide metadata necessary for operation execution.
  """
  EXECUTION
}

type Movie implements Product
  @join__implements(graph: PRODUCTS, interface: "Product")
  @join__implements(graph: REVIEWS, interface: "Product")
  @join__type(graph: PRODUCTS, key: "id")
  @join__type(graph: REVIEWS, key: "id")
{
  id: ID!
  price: Price @join__field(graph: PRODUCTS)
  title: String @join__field(graph: PRODUCTS)
  vendor: User @join__field(graph: PRODUCTS)
  length_minutes: Int @join__field(graph: PRODUCTS)
  avg_rating: Int @join__field(graph: PRODUCTS, requires: "reviews { rating }")
  reviews: [Review] @join__field(graph: PRODUCTS, external: true) @join__field(graph: REVIEWS)
}

type Price
  @join__type(graph: PRODUCTS)
{
  value: Int
  currency: Currency
}

interface Product
  @join__type(graph: PRODUCTS)
  @join__type(graph: REVIEWS)
{
  id: ID!
  price: Price @join__field(graph: PRODUCTS)
  vendor: User @join__field(graph: PRODUCTS)
  avg_rating: Int @join__field(graph: PRODUCTS)
  reviews: [Review] @join__field(graph: REVIEWS)
}

type Query
  @join__type(graph: ACCOUNTS)
  @join__type(graph: PRODUCTS)
  @join__type(graph: REVIEWS)
{
  userById(id: ID!): User @join__field(graph: ACCOUNTS)
  me: User! @join__field(graph: ACCOUNTS) @join__field(graph: REVIEWS)
  productById(id: ID!): Product @join__field(graph: PRODUCTS)
  search(filter: SearchFilter): [Product] @join__field(graph: PRODUCTS)
  bestRatedProducts(limit: Int): [Product] @join__field(graph: REVIEWS)
}

type Review
  @join__type(graph: PRODUCTS)
  @join__type(graph: REVIEWS)
{
  rating: Int @join__field(graph: PRODUCTS, external: true) @join__field(graph: REVIEWS)
  product: Product @join__field(graph: REVIEWS)
  author: User @join__field(graph: REVIEWS)
  text: String @join__field(graph: REVIEWS)
}

input SearchFilter
  @join__type(graph: PRODUCTS)
{
  pattern: String!
  vendorName: String
}

type User
  @join__type(graph: ACCOUNTS, key: "id")
  @join__type(graph: PRODUCTS, key: "id", resolvable: false)
  @join__type(graph: REVIEWS, key: "id")
{
  id: ID!
  name: String @join__field(graph: ACCOUNTS)
  email: String @join__field(graph: ACCOUNTS)
  password: String @join__field(graph: ACCOUNTS)
  nickname: String @join__field(graph: ACCOUNTS, override: "reviews")
  reviews: [Review] @join__field(graph: REVIEWS)
}
    "#;

    #[test]
    fn plan_simple_query_for_single_subgraph() {
        let supergraph = Supergraph::new(TEST_SUPERGRAPH).unwrap();
        let planner = QueryPlanner::new(&supergraph, Default::default()).unwrap();

        let document = ExecutableDocument::parse_and_validate(
            planner.api_schema().schema(),
            r#"
            {
                userById(id: 1) {
                    name
                    email
                }
            }
            "#,
            "operation.graphql",
        )
        .unwrap();
        let plan = planner
            .build_query_plan(&document, None, Default::default())
            .unwrap();
        insta::assert_snapshot!(plan, @r###"
        QueryPlan {
          Fetch(service: "accounts") {
            {
              userById(id: 1) {
                name
                email
              }
            }
          },
        }
        "###);
    }

    #[test]
    fn plan_simple_query_for_multiple_subgraphs() {
        let supergraph = Supergraph::new(TEST_SUPERGRAPH).unwrap();
        let planner = QueryPlanner::new(&supergraph, Default::default()).unwrap();

        let document = ExecutableDocument::parse_and_validate(
            planner.api_schema().schema(),
            r#"
            {
                bestRatedProducts {
                    vendor { name }
                }
            }
            "#,
            "operation.graphql",
        )
        .unwrap();
        let plan = planner
            .build_query_plan(&document, None, Default::default())
            .unwrap();
        insta::assert_snapshot!(plan, @r###"
        QueryPlan {
          Sequence {
            Fetch(service: "reviews") {
              {
                bestRatedProducts {
                  __typename
                  ... on Book {
                    __typename
                    id
                  }
                  ... on Movie {
                    __typename
                    id
                  }
                }
              }
            },
            Flatten(path: "bestRatedProducts.@") {
              Fetch(service: "products") {
                {
                  ... on Book {
                    __typename
                    id
                  }
                  ... on Movie {
                    __typename
                    id
                  }
                } =>
                {
                  ... on Book {
                    vendor {
                      __typename
                      id
                    }
                  }
                  ... on Movie {
                    vendor {
                      __typename
                      id
                    }
                  }
                }
              },
            },
            Flatten(path: "bestRatedProducts.@.vendor") {
              Fetch(service: "accounts") {
                {
                  ... on User {
                    __typename
                    id
                  }
                } =>
                {
                  ... on User {
                    name
                  }
                }
              },
            },
          },
        }
        "###);
    }

    #[test]
    fn plan_simple_root_field_query_for_multiple_subgraphs() {
        let supergraph = Supergraph::new(TEST_SUPERGRAPH).unwrap();
        let planner = QueryPlanner::new(&supergraph, Default::default()).unwrap();

        let document = ExecutableDocument::parse_and_validate(
            planner.api_schema().schema(),
            r#"
            {
                userById(id: 1) {
                    name
                    email
                }
                bestRatedProducts {
                    id
                    avg_rating
                }
            }
            "#,
            "operation.graphql",
        )
        .unwrap();
        let plan = planner
            .build_query_plan(&document, None, Default::default())
            .unwrap();
        insta::assert_snapshot!(plan, @r###"
              QueryPlan {
                Parallel {
                  Fetch(service: "accounts") {
                    {
                      userById(id: 1) {
                        name
                        email
                      }
                    }
                  },
                  Sequence {
                    Fetch(service: "reviews") {
                      {
                        bestRatedProducts {
                          __typename
                          id
                          ... on Book {
                            __typename
                            id
                            reviews {
                              rating
                            }
                          }
                          ... on Movie {
                            __typename
                            id
                            reviews {
                              rating
                            }
                          }
                        }
                      }
                    },
                    Flatten(path: "bestRatedProducts.@") {
                      Fetch(service: "products") {
                        {
                          ... on Book {
                            __typename
                            id
                            reviews {
                              rating
                            }
                          }
                          ... on Movie {
                            __typename
                            id
                            reviews {
                              rating
                            }
                          }
                        } =>
                        {
                          ... on Book {
                            avg_rating
                          }
                          ... on Movie {
                            avg_rating
                          }
                        }
                      },
                    },
                  },
                },
              }
        "###);
    }

    #[test]
    fn test_optimize_no_fragments_generated() {
        let supergraph = Supergraph::new(TEST_SUPERGRAPH).unwrap();
        let api_schema = supergraph.to_api_schema(Default::default()).unwrap();
        let document = ExecutableDocument::parse_and_validate(
            api_schema.schema(),
            r#"
            {
                userById(id: 1) {
                    id
                    ...userFields
                },
                another_user: userById(id: 2) {
                  name
                  email
              }
            }
            fragment userFields on User {
                name
                email
            }
            "#,
            "operation.graphql",
        )
        .unwrap();

        let config = QueryPlannerConfig {
            generate_query_fragments: true,
            ..Default::default()
        };
        let planner = QueryPlanner::new(&supergraph, config).unwrap();
        let plan = planner
            .build_query_plan(&document, None, Default::default())
            .unwrap();
        insta::assert_snapshot!(plan, @r###"
        QueryPlan {
          Fetch(service: "accounts") {
            {
              userById(id: 1) {
                id
                name
                email
              }
              another_user: userById(id: 2) {
                name
                email
              }
            }
          },
        }
        "###);
    }

    #[test]
    fn drop_operation_root_level_typename() {
        let supergraph = Supergraph::new(TEST_SUPERGRAPH).unwrap();
        let planner = QueryPlanner::new(&supergraph, Default::default()).unwrap();

        let document = ExecutableDocument::parse_and_validate(
            planner.api_schema().schema(),
            r#"
            {
                __typename
                bestRatedProducts {
                    id
                }
            }
            "#,
            "operation.graphql",
        )
        .unwrap();
        let plan = planner
            .build_query_plan(&document, None, Default::default())
            .unwrap();
        // Note: There should be no `__typename` selection at the root level.
        insta::assert_snapshot!(plan, @r###"
        QueryPlan {
          Fetch(service: "reviews") {
            {
              bestRatedProducts {
                __typename
                id
              }
            }
          },
        }
        "###);
    }
}
