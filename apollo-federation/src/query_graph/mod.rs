use std::fmt::Display;
use std::fmt::Formatter;
use std::hash::Hash;
use std::ops::Deref;
use std::ops::DerefMut;
use std::sync::Arc;

use apollo_compiler::Name;
use apollo_compiler::Node;
use apollo_compiler::collections::IndexMap;
use apollo_compiler::collections::IndexSet;
use apollo_compiler::schema::NamedType;
use apollo_compiler::schema::Type;
use petgraph::Direction;
use petgraph::graph::DiGraph;
use petgraph::graph::EdgeIndex;
use petgraph::graph::EdgeReference;
use petgraph::graph::NodeIndex;
use petgraph::visit::EdgeRef;

use crate::bail;
use crate::error::FederationError;
use crate::error::SingleFederationError;
use crate::internal_error;
use crate::operation::Field;
use crate::operation::InlineFragment;
use crate::operation::SelectionSet;
use crate::schema::ValidFederationSchema;
use crate::schema::field_set::parse_field_set;
use crate::schema::position::CompositeTypeDefinitionPosition;
use crate::schema::position::FieldDefinitionPosition;
use crate::schema::position::InterfaceFieldDefinitionPosition;
use crate::schema::position::ObjectFieldArgumentDefinitionPosition;
use crate::schema::position::ObjectTypeDefinitionPosition;
use crate::schema::position::OutputTypeDefinitionPosition;
use crate::schema::position::SchemaRootDefinitionKind;
use crate::utils::FallibleIterator;

pub mod build_query_graph;
pub(crate) mod condition_resolver;
pub(crate) mod graph_path;
pub mod output;
pub(crate) mod path_tree;

pub use build_query_graph::build_federated_query_graph;
use graph_path::operation::OpGraphPathContext;
use graph_path::operation::OpGraphPathTrigger;
use graph_path::operation::OpPathElement;

use crate::query_graph::condition_resolver::ConditionResolution;
use crate::query_graph::condition_resolver::ConditionResolver;
use crate::query_graph::graph_path::ExcludedConditions;
use crate::query_graph::graph_path::ExcludedDestinations;
use crate::query_plan::QueryPlanCost;
use crate::query_plan::query_planning_traversal::non_local_selections_estimation;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct QueryGraphNode {
    /// The GraphQL type this node points to.
    pub(crate) type_: QueryGraphNodeType,
    /// An identifier of the underlying schema containing the `type_` this node points to. This is
    /// mainly used in federated query graphs, where the `source` is a subgraph name.
    pub(crate) source: Arc<str>,
    /// True if there is a cross-subgraph edge that is reachable from this node.
    pub(crate) has_reachable_cross_subgraph_edges: bool,
    /// @provides works by creating duplicates of the node/type involved in the provides and adding
    /// the provided edges only to those copies. This means that with @provides, you can have more
    /// than one node per-type-and-subgraph in a query graph. Which is fine, but this `provide_id`
    /// allows distinguishing if a node was created as part of this @provides duplication or not.
    /// The value of this field has no other meaning than to be unique per-@provide, and so all the
    /// nodes copied for a given @provides application will have the same `provide_id`. Overall,
    /// this mostly exists for debugging visualization.
    pub(crate) provide_id: Option<u32>,
    // If present, this node represents a root node of the corresponding kind.
    pub(crate) root_kind: Option<SchemaRootDefinitionKind>,
}

impl QueryGraphNode {
    pub(crate) fn is_root_node(&self) -> bool {
        matches!(self.type_, QueryGraphNodeType::FederatedRootType(_))
    }
}

impl Display for QueryGraphNode {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}({})", self.type_, self.source)?;
        if let Some(provide_id) = self.provide_id {
            write!(f, "-{}", provide_id)?;
        }
        if self.is_root_node() {
            write!(f, "*")?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, derive_more::From, derive_more::IsVariant)]
pub(crate) enum QueryGraphNodeType {
    SchemaType(OutputTypeDefinitionPosition),
    FederatedRootType(SchemaRootDefinitionKind),
}

impl Display for QueryGraphNodeType {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            QueryGraphNodeType::SchemaType(pos) => pos.fmt(f),
            QueryGraphNodeType::FederatedRootType(root_kind) => {
                write!(f, "[{root_kind}]")
            }
        }
    }
}

impl TryFrom<QueryGraphNodeType> for CompositeTypeDefinitionPosition {
    type Error = FederationError;

    fn try_from(value: QueryGraphNodeType) -> Result<Self, Self::Error> {
        match value {
            QueryGraphNodeType::SchemaType(ty) => Ok(ty.try_into()?),
            QueryGraphNodeType::FederatedRootType(_) => Err(FederationError::internal(format!(
                r#"Type "{value}" was unexpectedly not a composite type"#
            ))),
        }
    }
}

impl TryFrom<QueryGraphNodeType> for ObjectTypeDefinitionPosition {
    type Error = FederationError;

    fn try_from(value: QueryGraphNodeType) -> Result<Self, Self::Error> {
        match value {
            QueryGraphNodeType::SchemaType(ty) => Ok(ty.try_into()?),
            QueryGraphNodeType::FederatedRootType(_) => Err(FederationError::internal(format!(
                r#"Type "{value}" was unexpectedly not an object type"#
            ))),
        }
    }
}

/// Contains all of the data necessary to connect the object field argument (`argument_coordinate`)
/// with the `@fromContext` to its (grand)parent types contain a matching selection.
#[derive(Debug, PartialEq, Clone)]
pub struct ContextCondition {
    context: String,
    subgraph_name: Arc<str>,
    // This is purposely left unparsed in query graphs, due to @fromContext selection sets being
    // duck-typed.
    selection: String,
    types_with_context_set: IndexSet<CompositeTypeDefinitionPosition>,
    // PORT_NOTE: This field was renamed because the JS name (`namedParameter`) left confusion to
    // how it was different from the argument name.
    argument_name: Name,
    // PORT_NOTE: This field was renamed because the JS name (`coordinate`) was too vague.
    argument_coordinate: ObjectFieldArgumentDefinitionPosition,
    // PORT_NOTE: This field was renamed from the JS name (`argType`) for consistency with the rest
    // of the naming in this struct.
    argument_type: Node<Type>,
}

#[derive(Debug, PartialEq, Clone)]
pub(crate) struct QueryGraphEdge {
    /// Indicates what kind of edge this is and what the edge does/represents. For instance, if the
    /// edge represents a field, the `transition` will be a `FieldCollection` transition and will
    /// link to the definition of the field it represents.
    pub(crate) transition: QueryGraphEdgeTransition,
    /// Optional conditions on an edge.
    ///
    /// Conditions are a select of selections (in the GraphQL sense) that the traversal of a query
    /// graph needs to "collect" (traverse edges with transitions corresponding to those selections)
    /// in order to be able to collect that edge.
    ///
    /// Conditions are primarily used for edges corresponding to @key, in which case they correspond
    /// to the fields composing the @key. In other words, for an @key edge, conditions basically
    /// represent the fact that you need the key to be able to use an @key edge.
    ///
    /// Outside of keys, @requires edges also rely on conditions.
    pub(crate) conditions: Option<Arc<SelectionSet>>,
    /// Edges can require that an override condition (provided during query
    /// planning) be met in order to be taken. This is used for progressive
    /// @override, where (at least) 2 subgraphs can resolve the same field, but
    /// one of them has an @override with a label. If the override condition
    /// matches the query plan parameters, this edge can be taken.
    pub(crate) override_condition: Option<OverrideCondition>,
    /// All arguments with `@fromContext` that need to be matched to an upstream graph path field
    /// whose parent type has the corresponding `@context`.
    pub(crate) required_contexts: Vec<ContextCondition>,
}

impl QueryGraphEdge {
    pub(crate) fn new(
        transition: QueryGraphEdgeTransition,
        conditions: Option<Arc<SelectionSet>>,
    ) -> Self {
        Self {
            transition,
            conditions,
            override_condition: None,
            required_contexts: Vec::new(),
        }
    }

    fn satisfies_override_conditions(&self, conditions_to_check: &OverrideConditions) -> bool {
        if let Some(override_condition) = &self.override_condition {
            override_condition.check(conditions_to_check)
        } else {
            true
        }
    }
}

impl Display for QueryGraphEdge {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        if matches!(
            self.transition,
            QueryGraphEdgeTransition::SubgraphEnteringTransition
        ) && self.conditions.is_none()
        {
            return Ok(());
        }

        match (&self.override_condition, &self.conditions) {
            (Some(override_condition), Some(conditions)) => write!(
                f,
                "{}, {} ⊢ {}",
                conditions, override_condition, self.transition
            ),
            (Some(override_condition), None) => {
                write!(f, "{} ⊢ {}", override_condition, self.transition)
            }
            (None, Some(conditions)) => write!(f, "{} ⊢ {}", conditions, self.transition),
            _ => self.transition.fmt(f),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct OverrideCondition {
    pub(crate) label: Arc<str>,
    pub(crate) condition: bool,
}

impl OverrideCondition {
    pub(crate) fn check(&self, override_conditions: &OverrideConditions) -> bool {
        override_conditions.get(&self.label) == Some(&self.condition)
    }
}

/// For query planning, this is a map of all override condition labels to whether that label is set.
/// For composition satisfiability, this is the same thing, but it's only some of the override
/// conditions. Specifically, for top-level queries in satisfiability, this will only contain those
/// override conditions encountered in the path. For conditions queries in satisfiability, this will
/// be an empty map.
#[derive(Debug, Clone, Default)]
pub(crate) struct OverrideConditions(IndexMap<Arc<str>, bool>);

impl Deref for OverrideConditions {
    type Target = IndexMap<Arc<str>, bool>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for OverrideConditions {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl OverrideConditions {
    pub(crate) fn new(graph: &QueryGraph, enabled_conditions: &IndexSet<String>) -> Self {
        Self(
            graph
                .override_condition_labels
                .iter()
                .map(|label| (label.clone(), enabled_conditions.contains(label.as_ref())))
                .collect(),
        )
    }
}

impl Display for OverrideCondition {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} = {}", self.label, self.condition)
    }
}

/// The type of query graph edge "transition".
///
/// An edge transition encodes what the edge corresponds to, in the underlying GraphQL schema.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum QueryGraphEdgeTransition {
    /// A field edge, going from (a node for) the field parent type to the field's (base) type.
    FieldCollection {
        /// The name of the schema containing the field.
        source: Arc<str>,
        /// The object/interface field being collected.
        field_definition_position: FieldDefinitionPosition,
        /// Whether this field is part of an @provides.
        is_part_of_provides: bool,
    },
    /// A downcast edge, going from a composite type (object, interface, or union) to another
    /// composite type that intersects that type (i.e. has at least one possible runtime object type
    /// in common with it).
    Downcast {
        /// The name of the schema containing the from/to types.
        source: Arc<str>,
        /// The parent type of the type condition, i.e. the type of the selection set containing
        /// the type condition.
        from_type_position: CompositeTypeDefinitionPosition,
        /// The type of the type condition, i.e. the type coming after "... on".
        to_type_position: CompositeTypeDefinitionPosition,
    },
    /// A key edge (only found in federated query graphs) going from an entity type in a particular
    /// subgraph to the same entity type but in another subgraph. Key transition edges _must_ have
    /// `conditions` corresponding to the key fields.
    KeyResolution,
    /// A root type edge (only found in federated query graphs) going from a root type (query,
    /// mutation or subscription) of a subgraph to the (same) root type of another subgraph. It
    /// encodes the fact that if a subgraph field returns a root type, any subgraph can be queried
    /// from there.
    RootTypeResolution {
        /// The kind of schema root resolved.
        root_kind: SchemaRootDefinitionKind,
    },
    /// A subgraph-entering edge, which is a special case only used for edges coming out of the root
    /// nodes of "federated" query graphs. It does not correspond to any physical GraphQL elements
    /// but can be understood as the fact that the router is always free to start querying any of
    /// the subgraph services as needed.
    SubgraphEnteringTransition,
    /// A "fake" downcast edge (only found in federated query graphs) going from an @interfaceObject
    /// type to an implementation. This encodes the fact that an @interfaceObject type "stands-in"
    /// for any possible implementations (in the supergraph) of the corresponding interface. It is
    /// "fake" because the corresponding edge stays on the @interfaceObject type (this is also why
    /// the "to type" is only a name: that to/casted type does not actually exist in the subgraph
    /// in which the corresponding edge will be found).
    InterfaceObjectFakeDownCast {
        /// The name of the schema containing the from type.
        source: Arc<str>,
        /// The parent type of the type condition, i.e. the type of the selection set containing
        /// the type condition.
        from_type_position: CompositeTypeDefinitionPosition,
        /// The type of the type condition, i.e. the type coming after "... on".
        to_type_name: Name,
    },
}

impl QueryGraphEdgeTransition {
    pub(crate) fn collect_operation_elements(&self) -> bool {
        match self {
            QueryGraphEdgeTransition::FieldCollection { .. } => true,
            QueryGraphEdgeTransition::Downcast { .. } => true,
            QueryGraphEdgeTransition::KeyResolution => false,
            QueryGraphEdgeTransition::RootTypeResolution { .. } => false,
            QueryGraphEdgeTransition::SubgraphEnteringTransition => false,
            QueryGraphEdgeTransition::InterfaceObjectFakeDownCast { .. } => true,
        }
    }

    pub(crate) fn matches_supergraph_transition(
        &self,
        other: &Self,
    ) -> Result<bool, FederationError> {
        Ok(match self {
            QueryGraphEdgeTransition::FieldCollection {
                field_definition_position,
                ..
            } => {
                let QueryGraphEdgeTransition::FieldCollection {
                    field_definition_position: other_field_definition_position,
                    ..
                } = other
                else {
                    return Ok(false);
                };
                field_definition_position.field_name()
                    == other_field_definition_position.field_name()
            }
            QueryGraphEdgeTransition::Downcast {
                to_type_position, ..
            } => {
                let QueryGraphEdgeTransition::Downcast {
                    to_type_position: other_to_type_position,
                    ..
                } = other
                else {
                    return Ok(false);
                };
                to_type_position.type_name() == other_to_type_position.type_name()
            }
            QueryGraphEdgeTransition::InterfaceObjectFakeDownCast { to_type_name, .. } => {
                let QueryGraphEdgeTransition::InterfaceObjectFakeDownCast {
                    to_type_name: other_to_type_name,
                    ..
                } = other
                else {
                    return Ok(false);
                };
                to_type_name == other_to_type_name
            }
            _ => {
                bail!(
                    "Supergraphs shouldn't have a transition that doesn't collect elements; got {}",
                    other,
                );
            }
        })
    }
}

impl Display for QueryGraphEdgeTransition {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            QueryGraphEdgeTransition::FieldCollection {
                field_definition_position,
                ..
            } => {
                write!(f, "{}", field_definition_position.field_name())
            }
            QueryGraphEdgeTransition::Downcast {
                to_type_position, ..
            } => {
                write!(f, "... on {}", to_type_position.type_name())
            }
            QueryGraphEdgeTransition::KeyResolution => {
                write!(f, "key()")
            }
            QueryGraphEdgeTransition::RootTypeResolution { root_kind } => {
                write!(f, "{}()", root_kind)
            }
            QueryGraphEdgeTransition::SubgraphEnteringTransition => {
                write!(f, "∅")
            }
            QueryGraphEdgeTransition::InterfaceObjectFakeDownCast { to_type_name, .. } => {
                write!(f, "... on {}", to_type_name)
            }
        }
    }
}

#[derive(Debug)]
pub struct QueryGraph {
    /// The "current" source of the query graph. For query graphs representing a single source
    /// graph, this will only ever be one value, but it will change for "federated" query graphs
    /// while they're being built (and after construction, will become FEDERATED_GRAPH_ROOT_SOURCE,
    /// which is a reserved placeholder value).
    current_source: Arc<str>,
    /// The nodes/edges of the query graph. Note that nodes/edges should never be removed, so
    /// indexes are immutable when a node/edge is created.
    graph: DiGraph<QueryGraphNode, QueryGraphEdge>,
    /// The sources on which the query graph was built, which is a set (potentially of size 1) of
    /// GraphQL schema keyed by the name identifying them. Note that the `source` strings in the
    /// nodes/edges of a query graph are guaranteed to be valid key in this map.
    sources: IndexMap<Arc<str>, ValidFederationSchema>,
    /// For federated query graphs, this is a map from subgraph names to their schemas. This is the
    /// same as `sources`, but is missing the dummy source FEDERATED_GRAPH_ROOT_SOURCE which isn't
    /// really a subgraph.
    subgraphs_by_name: IndexMap<Arc<str>, ValidFederationSchema>,
    /// For federated query graphs, this is the supergraph schema; otherwise, this is `None`.
    supergraph_schema: Option<ValidFederationSchema>,
    /// A map (keyed by source) that associates type names of the underlying schema on which this
    /// query graph was built to each of the nodes that points to a type of that name. Note that for
    /// a "federated" query graph source, each type name will only map to a single node.
    types_to_nodes_by_source: IndexMap<Arc<str>, IndexMap<NamedType, IndexSet<NodeIndex>>>,
    /// A map (keyed by source) that associates schema root kinds to root nodes.
    root_kinds_to_nodes_by_source:
        IndexMap<Arc<str>, IndexMap<SchemaRootDefinitionKind, NodeIndex>>,
    /// Maps an edge to the possible edges that can follow it "productively", that is without
    /// creating a trivially inefficient path.
    ///
    /// More precisely, this map is equivalent to looking at the out edges of a given edge's tail
    /// node and filtering those edges that "never make sense" after the given edge, which mainly
    /// amounts to avoiding chaining @key edges when we know there is guaranteed to be a better
    /// option. As an example, suppose we have 3 subgraphs A, B and C which all defined an
    /// `@key(fields: "id")` on some entity type `T`. Then it is never interesting to take that @key
    /// edge from B -> C after A -> B because if we're in A and want to get to C, we can always do
    /// A -> C (of course, this is only true because it's the "same" key).
    ///
    /// See `precompute_non_trivial_followup_edges` for more details on which exact edges are
    /// filtered.
    ///
    /// Lastly, note that the main reason for having this field is that its result is pre-computed.
    /// Which in turn is done for performance reasons: having the same key defined in multiple
    /// subgraphs is _the_ most common pattern, and while our later algorithms (composition
    /// validation and query planning) would know to not select those trivially inefficient
    /// "detours", they might have to redo those checks many times and pre-computing once it is
    /// significantly faster (and pretty easy). FWIW, when originally introduced, this optimization
    /// lowered composition validation on a big composition (100+ subgraphs) from ~4 minutes to
    /// ~10 seconds.
    non_trivial_followup_edges: IndexMap<EdgeIndex, Vec<EdgeIndex>>,
    // PORT_NOTE: This field was renamed from the JS name (`subgraphToArgIndices`) to better
    // align with downstream code.
    /// Maps subgraph names to another map, for any subgraph with usages of `@fromContext`. This
    /// other map then maps subgraph argument positions/coordinates (for `@fromContext` arguments)
    /// to a unique identifier string (specifically, unique across pairs of subgraph names and
    /// argument coordinates). This identifier is called the "context ID".
    arguments_to_context_ids_by_source:
        IndexMap<Arc<str>, IndexMap<ObjectFieldArgumentDefinitionPosition, Name>>,
    override_condition_labels: IndexSet<Arc<str>>,
    /// To speed up the estimation of counting non-local selections, we precompute specific metadata
    /// about the query graph and store that here.
    non_local_selection_metadata: non_local_selections_estimation::QueryGraphMetadata,
}

impl QueryGraph {
    pub(crate) fn name(&self) -> &Arc<str> {
        &self.current_source
    }

    pub(crate) fn graph(&self) -> &DiGraph<QueryGraphNode, QueryGraphEdge> {
        &self.graph
    }

    pub(crate) fn supergraph_schema(&self) -> Result<ValidFederationSchema, FederationError> {
        self.supergraph_schema
            .clone()
            .ok_or_else(|| internal_error!("Supergraph schema unexpectedly missing"))
    }

    pub(crate) fn node_weight(&self, node: NodeIndex) -> Result<&QueryGraphNode, FederationError> {
        self.graph
            .node_weight(node)
            .ok_or_else(|| internal_error!("Node unexpectedly missing"))
    }

    fn node_weight_mut(&mut self, node: NodeIndex) -> Result<&mut QueryGraphNode, FederationError> {
        self.graph
            .node_weight_mut(node)
            .ok_or_else(|| internal_error!("Node unexpectedly missing"))
    }

    pub(crate) fn edge_weight(&self, edge: EdgeIndex) -> Result<&QueryGraphEdge, FederationError> {
        self.graph
            .edge_weight(edge)
            .ok_or_else(|| internal_error!("Edge unexpectedly missing"))
    }

    fn edge_weight_mut(&mut self, edge: EdgeIndex) -> Result<&mut QueryGraphEdge, FederationError> {
        self.graph
            .edge_weight_mut(edge)
            .ok_or_else(|| internal_error!("Edge unexpectedly missing"))
    }

    pub(crate) fn edge_head_weight(
        &self,
        edge: EdgeIndex,
    ) -> Result<&QueryGraphNode, FederationError> {
        let (head_id, _) = self.edge_endpoints(edge)?;
        self.node_weight(head_id)
    }

    pub(crate) fn edge_endpoints(
        &self,
        edge: EdgeIndex,
    ) -> Result<(NodeIndex, NodeIndex), FederationError> {
        self.graph
            .edge_endpoints(edge)
            .ok_or_else(|| internal_error!("Edge unexpectedly missing"))
    }

    pub(crate) fn schema(&self) -> Result<&ValidFederationSchema, FederationError> {
        self.schema_by_source(&self.current_source)
    }

    pub(crate) fn schema_by_source(
        &self,
        source: &str,
    ) -> Result<&ValidFederationSchema, FederationError> {
        self.sources
            .get(source)
            .ok_or_else(|| internal_error!(r#"Schema for "{source}" unexpectedly missing"#))
    }

    pub(crate) fn subgraph_schemas(&self) -> &IndexMap<Arc<str>, ValidFederationSchema> {
        &self.subgraphs_by_name
    }

    pub(crate) fn subgraphs(&self) -> impl Iterator<Item = (&Arc<str>, &ValidFederationSchema)> {
        self.subgraphs_by_name.iter()
    }

    /// Returns the node indices whose name matches the given type name.
    pub(crate) fn nodes_for_type(
        &self,
        name: &Name,
    ) -> Result<&IndexSet<NodeIndex>, FederationError> {
        self.types_to_nodes()?
            .get(name)
            .ok_or_else(|| internal_error!("No nodes unexpectedly found for type"))
    }

    pub(crate) fn types_to_nodes(
        &self,
    ) -> Result<&IndexMap<NamedType, IndexSet<NodeIndex>>, FederationError> {
        self.types_to_nodes_by_source(&self.current_source)
    }

    pub(super) fn types_to_nodes_by_source(
        &self,
        source: &str,
    ) -> Result<&IndexMap<NamedType, IndexSet<NodeIndex>>, FederationError> {
        self.types_to_nodes_by_source.get(source).ok_or_else(|| {
            SingleFederationError::Internal {
                message: "Types-to-nodes map unexpectedly missing".to_owned(),
            }
            .into()
        })
    }

    fn types_to_nodes_mut(
        &mut self,
    ) -> Result<&mut IndexMap<NamedType, IndexSet<NodeIndex>>, FederationError> {
        self.types_to_nodes_by_source
            .get_mut(&self.current_source)
            .ok_or_else(|| {
                SingleFederationError::Internal {
                    message: "Types-to-nodes map unexpectedly missing".to_owned(),
                }
                .into()
            })
    }

    pub(crate) fn root_kinds_to_nodes(
        &self,
    ) -> Result<&IndexMap<SchemaRootDefinitionKind, NodeIndex>, FederationError> {
        self.root_kinds_to_nodes_by_source
            .get(&self.current_source)
            .ok_or_else(|| {
                SingleFederationError::Internal {
                    message: "Root-kinds-to-nodes map unexpectedly missing".to_owned(),
                }
                .into()
            })
    }

    pub(crate) fn root_kinds_to_nodes_by_source(
        &self,
        source: &str,
    ) -> Result<&IndexMap<SchemaRootDefinitionKind, NodeIndex>, FederationError> {
        self.root_kinds_to_nodes_by_source
            .get(source)
            .ok_or_else(|| {
                SingleFederationError::Internal {
                    message: "Root-kinds-to-nodes map unexpectedly missing".to_owned(),
                }
                .into()
            })
    }

    fn root_kinds_to_nodes_mut(
        &mut self,
    ) -> Result<&mut IndexMap<SchemaRootDefinitionKind, NodeIndex>, FederationError> {
        self.root_kinds_to_nodes_by_source
            .get_mut(&self.current_source)
            .ok_or_else(|| {
                SingleFederationError::Internal {
                    message: "Root-kinds-to-nodes map unexpectedly missing".to_owned(),
                }
                .into()
            })
    }

    pub(crate) fn context_id_by_source_and_argument(
        &self,
        source: &str,
        argument: &ObjectFieldArgumentDefinitionPosition,
    ) -> Result<&Name, FederationError> {
        self.arguments_to_context_ids_by_source
            .get(source)
            .and_then(|r| r.get(argument))
            .ok_or_else(|| {
                internal_error!("context ID unexpectedly missing for @fromContext argument")
            })
    }

    pub(crate) fn is_context_used(&self) -> bool {
        !self.arguments_to_context_ids_by_source.is_empty()
    }

    pub(crate) fn non_local_selection_metadata(
        &self,
    ) -> &non_local_selections_estimation::QueryGraphMetadata {
        &self.non_local_selection_metadata
    }

    /// All outward edges from the given node (including self-key and self-root-type-resolution
    /// edges). Primarily used by `@defer`, when needing to re-enter a subgraph for a deferred
    /// section.
    pub(crate) fn out_edges_with_federation_self_edges(
        &self,
        node: NodeIndex,
    ) -> Vec<EdgeReference<QueryGraphEdge>> {
        Self::sorted_edges(self.graph.edges_directed(node, Direction::Outgoing))
    }

    /// The outward edges from the given node, minus self-key and self-root-type-resolution edges,
    /// as they're rarely useful (currently only used by `@defer`).
    pub(crate) fn out_edges(&self, node: NodeIndex) -> Vec<EdgeReference<QueryGraphEdge>> {
        Self::sorted_edges(self.graph.edges_directed(node, Direction::Outgoing).filter(
            |edge_ref| {
                !(edge_ref.source() == edge_ref.target()
                    && matches!(
                        edge_ref.weight().transition,
                        QueryGraphEdgeTransition::KeyResolution
                            | QueryGraphEdgeTransition::RootTypeResolution { .. }
                    ))
            },
        ))
    }

    /// Edge iteration order is unspecified in petgraph, but appears to be
    /// *reverse* insertion order in practice.
    /// This can affect generated query plans, such as when two options have the same cost.
    /// To match the JS code base, we want to iterate in insertion order.
    ///
    /// Sorting by edge indices relies on documented behavior:
    /// <https://docs.rs/petgraph/latest/petgraph/graph/struct.Graph.html#graph-indices>
    ///
    /// As of this writing, edges of the query graph are removed
    /// in `FederatedQueryGraphBuilder::update_edge_tail` which specifically preserves indices
    /// by pairing with an insertion.
    fn sorted_edges<'graph>(
        edges: impl Iterator<Item = EdgeReference<'graph, QueryGraphEdge>>,
    ) -> Vec<EdgeReference<'graph, QueryGraphEdge>> {
        let mut edges: Vec<_> = edges.collect();
        edges.sort_by_key(|e| -> EdgeIndex { e.id() });
        edges
    }

    pub(crate) fn is_self_key_or_root_edge(
        &self,
        edge: EdgeIndex,
    ) -> Result<bool, FederationError> {
        let edge_weight = self.edge_weight(edge)?;
        let (head, tail) = self.edge_endpoints(edge)?;
        let head_weight = self.node_weight(head)?;
        let tail_weight = self.node_weight(tail)?;
        Ok(head_weight.source == tail_weight.source
            && matches!(
                edge_weight.transition,
                QueryGraphEdgeTransition::KeyResolution
                    | QueryGraphEdgeTransition::RootTypeResolution { .. }
            ))
    }

    // PORT_NOTE: In the JS codebase, this was named `hasValidDirectKeyEdge`.
    pub(crate) fn has_satisfiable_direct_key_edge(
        &self,
        from_node: NodeIndex,
        to_subgraph: &str,
        condition_resolver: &mut impl ConditionResolver,
        max_cost: QueryPlanCost,
    ) -> Result<bool, FederationError> {
        for edge_ref in self.out_edges(from_node) {
            let edge_weight = edge_ref.weight();
            if !matches!(
                edge_weight.transition,
                QueryGraphEdgeTransition::KeyResolution
            ) {
                continue;
            }

            let tail = edge_ref.target();
            let tail_weight = self.node_weight(tail)?;
            if tail_weight.source.as_ref() != to_subgraph {
                continue;
            }

            let condition_resolution = condition_resolver.resolve(
                edge_ref.id(),
                &OpGraphPathContext::default(),
                &ExcludedDestinations::default(),
                &ExcludedConditions::default(),
                None,
            )?;
            let ConditionResolution::Satisfied { cost, .. } = condition_resolution else {
                continue;
            };

            // During composition validation, we consider all conditions to have cost 1.
            if cost <= max_cost {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub(crate) fn locally_satisfiable_key(
        &self,
        edge_index: EdgeIndex,
    ) -> Result<Option<SelectionSet>, FederationError> {
        let edge_head = self.edge_head_weight(edge_index)?;
        let QueryGraphNodeType::SchemaType(type_position) = &edge_head.type_ else {
            return Err(FederationError::internal(
                "Unable to compute locally_satisfiable_key. Edge head was unexpectedly pointing to a federated root type",
            ));
        };
        let Some(subgraph_schema) = self.sources.get(&edge_head.source) else {
            return Err(FederationError::internal(format!(
                "Could not find subgraph source {}",
                edge_head.source
            )));
        };
        let Some(metadata) = subgraph_schema.subgraph_metadata() else {
            return Err(FederationError::internal(format!(
                "Could not find federation metadata for source {}",
                edge_head.source
            )));
        };
        let key_directive_definition = metadata
            .federation_spec_definition()
            .key_directive_definition(subgraph_schema)?;
        let external_metadata = metadata.external_metadata();
        let composite_type_position: CompositeTypeDefinitionPosition =
            type_position.clone().try_into()?;
        let type_ = composite_type_position.get(subgraph_schema.schema())?;
        type_
            .directives()
            .get_all(&key_directive_definition.name)
            .map(|key| {
                metadata
                    .federation_spec_definition()
                    .key_directive_arguments(key)
            })
            .and_then(|key_value| {
                parse_field_set(
                    subgraph_schema,
                    composite_type_position.type_name().clone(),
                    key_value.fields,
                    true,
                )
            })
            .find_ok(|selection| !external_metadata.selects_any_external_field(selection))
    }

    pub(crate) fn edge_for_field(
        &self,
        node: NodeIndex,
        field: &Field,
        override_conditions: &OverrideConditions,
    ) -> Option<EdgeIndex> {
        let mut candidates = self.out_edges(node).into_iter().filter_map(|edge_ref| {
            let edge_weight = edge_ref.weight();
            let QueryGraphEdgeTransition::FieldCollection {
                field_definition_position,
                ..
            } = &edge_weight.transition
            else {
                return None;
            };

            if !edge_weight.satisfies_override_conditions(override_conditions) {
                return None;
            }

            // We explicitly avoid comparing parent type's here, to allow interface object
            // fields to match operation fields with the same name but differing types.
            if field.field_position.field_name() == field_definition_position.field_name() {
                Some(edge_ref.id())
            } else {
                None
            }
        });
        if let Some(candidate) = candidates.next() {
            // PORT_NOTE: The JS codebase used an assertion rather than a debug assertion here. We
            // consider it unlikely for there to be more than one candidate given all the code paths
            // that create edges, so we've downgraded this to a debug assertion.
            debug_assert!(
                candidates.next().is_none(),
                "Unexpectedly found multiple candidates",
            );
            Some(candidate)
        } else {
            None
        }
    }

    pub(crate) fn edge_for_inline_fragment(
        &self,
        node: NodeIndex,
        inline_fragment: &InlineFragment,
    ) -> Option<EdgeIndex> {
        let Some(type_condition_pos) = &inline_fragment.type_condition_position else {
            // No type condition means the type hasn't changed, meaning there is no edge to take.
            return None;
        };
        let mut candidates = self.out_edges(node).into_iter().filter_map(|edge_ref| {
            let edge_weight = edge_ref.weight();
            let QueryGraphEdgeTransition::Downcast {
                to_type_position, ..
            } = &edge_weight.transition
            else {
                return None;
            };
            // We explicitly avoid comparing type kinds, to allow interface object types to
            // match operation inline fragments (where the supergraph type kind is interface,
            // but the subgraph type kind is object).
            if type_condition_pos.type_name() == to_type_position.type_name() {
                Some(edge_ref.id())
            } else {
                None
            }
        });
        if let Some(candidate) = candidates.next() {
            // PORT_NOTE: The JS codebase used an assertion rather than a debug assertion here. We
            // consider it unlikely for there to be more than one candidate given all the code paths
            // that create edges, so we've downgraded this to a debug assertion.
            debug_assert!(
                candidates.next().is_none(),
                "Unexpectedly found multiple candidates",
            );
            Some(candidate)
        } else {
            None
        }
    }

    pub(crate) fn edge_for_op_graph_path_trigger(
        &self,
        node: NodeIndex,
        op_graph_path_trigger: &OpGraphPathTrigger,
        override_conditions: &OverrideConditions,
    ) -> Option<Option<EdgeIndex>> {
        let OpGraphPathTrigger::OpPathElement(op_path_element) = op_graph_path_trigger else {
            return None;
        };
        match op_path_element {
            OpPathElement::Field(field) => self
                .edge_for_field(node, field, override_conditions)
                .map(Some),
            OpPathElement::InlineFragment(inline_fragment) => {
                if inline_fragment.type_condition_position.is_some() {
                    self.edge_for_inline_fragment(node, inline_fragment)
                        .map(Some)
                } else {
                    Some(None)
                }
            }
        }
    }

    pub(crate) fn edge_for_transition_graph_path_trigger(
        &self,
        node: NodeIndex,
        transition_graph_path_trigger: &QueryGraphEdgeTransition,
        override_conditions: &OverrideConditions,
    ) -> Result<Option<EdgeIndex>, FederationError> {
        for edge_ref in self.out_edges(node) {
            let edge_weight = edge_ref.weight();
            if edge_weight
                .transition
                .matches_supergraph_transition(transition_graph_path_trigger)?
                && edge_weight.satisfies_override_conditions(override_conditions)
            {
                return Ok(Some(edge_ref.id()));
            }
        }
        Ok(None)
    }

    /// Given the possible runtime types at the head of the given edge, returns the possible runtime
    /// types after traversing the edge.
    // PORT_NOTE: Named `updateRuntimeTypes` in the JS codebase.
    pub(crate) fn advance_possible_runtime_types(
        &self,
        possible_runtime_types: &IndexSet<ObjectTypeDefinitionPosition>,
        edge: Option<EdgeIndex>,
    ) -> Result<IndexSet<ObjectTypeDefinitionPosition>, FederationError> {
        let Some(edge) = edge else {
            return Ok(possible_runtime_types.clone());
        };

        let edge_weight = self.edge_weight(edge)?;
        let (_, tail) = self.edge_endpoints(edge)?;
        let tail_weight = self.node_weight(tail)?;
        let QueryGraphNodeType::SchemaType(tail_type_pos) = &tail_weight.type_ else {
            return Err(FederationError::internal(
                "Unexpectedly encountered federation root node as tail node.",
            ));
        };
        match &edge_weight.transition {
            QueryGraphEdgeTransition::FieldCollection {
                source,
                field_definition_position,
                ..
            } => {
                if CompositeTypeDefinitionPosition::try_from(tail_type_pos.clone()).is_err() {
                    return Ok(IndexSet::default());
                }
                let schema = self.schema_by_source(source)?;
                let mut new_possible_runtime_types = IndexSet::default();
                for possible_runtime_type in possible_runtime_types {
                    let field_pos =
                        possible_runtime_type.field(field_definition_position.field_name().clone());
                    let Some(field) = field_pos.try_get(schema.schema()) else {
                        continue;
                    };
                    let field_type_pos: CompositeTypeDefinitionPosition = schema
                        .get_type(field.ty.inner_named_type().clone())?
                        .try_into()?;
                    new_possible_runtime_types
                        .extend(schema.possible_runtime_types(field_type_pos)?);
                }
                Ok(new_possible_runtime_types)
            }
            QueryGraphEdgeTransition::Downcast {
                source,
                to_type_position,
                ..
            } => Ok(self
                .schema_by_source(source)?
                .possible_runtime_types(to_type_position.clone())?
                .intersection(possible_runtime_types)
                .cloned()
                .collect()),
            QueryGraphEdgeTransition::KeyResolution => {
                let tail_type_pos: CompositeTypeDefinitionPosition =
                    tail_type_pos.clone().try_into()?;
                Ok(self
                    .schema_by_source(&tail_weight.source)?
                    .possible_runtime_types(tail_type_pos)?)
            }
            QueryGraphEdgeTransition::RootTypeResolution { .. } => {
                let OutputTypeDefinitionPosition::Object(tail_type_pos) = tail_type_pos.clone()
                else {
                    return Err(FederationError::internal(
                        "Unexpectedly encountered non-object root operation type.",
                    ));
                };
                Ok(IndexSet::from_iter([tail_type_pos]))
            }
            QueryGraphEdgeTransition::SubgraphEnteringTransition => {
                let OutputTypeDefinitionPosition::Object(tail_type_pos) = tail_type_pos.clone()
                else {
                    return Err(FederationError::internal(
                        "Unexpectedly encountered non-object root operation type.",
                    ));
                };
                Ok(IndexSet::from_iter([tail_type_pos]))
            }
            QueryGraphEdgeTransition::InterfaceObjectFakeDownCast { .. } => {
                Ok(possible_runtime_types.clone())
            }
        }
    }

    /// Returns a selection set that can be used as a key for the given type, and that can be
    /// entirely resolved in the same subgraph. Returns None if such a key does not exist for the
    /// given type.
    pub(crate) fn get_locally_satisfiable_key(
        &self,
        node_index: NodeIndex,
    ) -> Result<Option<SelectionSet>, FederationError> {
        let node = self.node_weight(node_index)?;
        let type_name = match &node.type_ {
            QueryGraphNodeType::SchemaType(ty) => {
                CompositeTypeDefinitionPosition::try_from(ty.clone())?
            }
            QueryGraphNodeType::FederatedRootType(_) => {
                return Err(FederationError::internal(format!(
                    "get_locally_satisfiable_key must be called on a composite type, got {}",
                    node.type_
                )));
            }
        };
        let schema = self.schema_by_source(&node.source)?;
        let Some(metadata) = schema.subgraph_metadata() else {
            return Err(FederationError::internal(format!(
                "Could not find subgraph metadata for source {}",
                node.source
            )));
        };
        let key_directive_definition = metadata
            .federation_spec_definition()
            .key_directive_definition(schema)?;

        let ty = type_name.get(schema.schema())?;

        ty.directives()
            .get_all(&key_directive_definition.name)
            .filter_map(|key| {
                key.specified_argument_by_name("fields")
                    .and_then(|arg| arg.as_str())
            })
            .map(|value| parse_field_set(schema, ty.name().clone(), value, true))
            .find_ok(|selection| {
                !metadata
                    .external_metadata()
                    .selects_any_external_field(selection)
            })
    }

    pub(crate) fn is_cross_subgraph_edge(&self, edge: EdgeIndex) -> Result<bool, FederationError> {
        let (head, tail) = self.edge_endpoints(edge)?;
        let head_weight = self.node_weight(head)?;
        let tail_weight = self.node_weight(tail)?;
        Ok(head_weight.source != tail_weight.source)
    }

    pub(crate) fn is_provides_edge(&self, edge: EdgeIndex) -> Result<bool, FederationError> {
        let edge_weight = self.edge_weight(edge)?;
        let QueryGraphEdgeTransition::FieldCollection {
            is_part_of_provides,
            ..
        } = &edge_weight.transition
        else {
            return Ok(false);
        };
        Ok(*is_part_of_provides)
    }

    pub(crate) fn has_an_implementation_with_provides(
        &self,
        source: &Arc<str>,
        interface_field_definition_position: InterfaceFieldDefinitionPosition,
    ) -> Result<bool, FederationError> {
        let schema = self.schema_by_source(source)?;
        let Some(metadata) = schema.subgraph_metadata() else {
            return Err(FederationError::internal(format!(
                "Interface should have come from a federation subgraph {}",
                source
            )));
        };

        let provides_directive_definition = metadata
            .federation_spec_definition()
            .provides_directive_definition(schema)?;

        Ok(schema
            .possible_runtime_types(interface_field_definition_position.parent().into())?
            .into_iter()
            .map(|object_type_definition_position| {
                let field_pos = object_type_definition_position
                    .field(interface_field_definition_position.field_name.clone());
                field_pos.get(schema.schema())
            })
            .ok_and_any(|field| field.directives.has(&provides_directive_definition.name))?)
    }
}
