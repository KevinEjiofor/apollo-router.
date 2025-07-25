use std::sync::Arc;

use apollo_compiler::Name;
use apollo_compiler::Node;
use apollo_compiler::ast::Type;
use apollo_compiler::collections::IndexMap;
use petgraph::graph::EdgeIndex;

use crate::error::FederationError;
use crate::operation::SelectionSet;
use crate::query_graph::QueryGraph;
use crate::query_graph::graph_path::ExcludedConditions;
use crate::query_graph::graph_path::ExcludedDestinations;
use crate::query_graph::graph_path::operation::OpGraphPathContext;
use crate::query_graph::path_tree::OpPathTree;
use crate::query_plan::QueryPlanCost;

#[derive(Debug, Clone)]
pub(crate) struct ContextMapEntry {
    pub(crate) levels_in_data_path: usize,
    pub(crate) levels_in_query_path: usize,
    pub(crate) path_tree: Option<Arc<OpPathTree>>,
    pub(crate) selection_set: SelectionSet,
    // PORT_NOTE: This field was renamed from the JS name (`paramName`) to better align with naming
    // in ContextCondition.
    pub(crate) argument_name: Name,
    // PORT_NOTE: This field was renamed from the JS name (`argType`) to better align with naming in
    // ContextCondition.
    pub(crate) argument_type: Node<Type>,
    pub(crate) context_id: Name,
}

/// Note that `ConditionResolver`s are guaranteed to be only called for edge with conditions.
pub(crate) trait ConditionResolver {
    fn resolve(
        &mut self,
        edge: EdgeIndex,
        context: &OpGraphPathContext,
        excluded_destinations: &ExcludedDestinations,
        excluded_conditions: &ExcludedConditions,
        extra_conditions: Option<&SelectionSet>,
    ) -> Result<ConditionResolution, FederationError>;
}

#[derive(Debug, Clone)]
pub(crate) enum ConditionResolution {
    Satisfied {
        cost: QueryPlanCost,
        path_tree: Option<Arc<OpPathTree>>,
        context_map: Option<IndexMap<Name, ContextMapEntry>>,
    },
    Unsatisfied {
        reason: Option<UnsatisfiedConditionReason>,
    },
}

impl std::fmt::Display for ConditionResolution {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConditionResolution::Satisfied {
                cost,
                path_tree,
                context_map,
            } => {
                writeln!(f, "Satisfied: cost={cost}")?;
                if let Some(path_tree) = path_tree {
                    writeln!(f, "path_tree:\n{path_tree}")?;
                }
                if let Some(context_map) = context_map {
                    writeln!(f, ", context_map:\n{context_map:?}")?;
                }
                Ok(())
            }
            ConditionResolution::Unsatisfied { reason } => {
                writeln!(f, "Unsatisfied: reason={:?}", reason)
            }
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) enum UnsatisfiedConditionReason {
    NoPostRequireKey,
    NoSetContext,
}

impl ConditionResolution {
    pub(crate) fn no_conditions() -> Self {
        Self::Satisfied {
            cost: 0.0,
            path_tree: None,
            context_map: None,
        }
    }

    pub(crate) fn unsatisfied_conditions() -> Self {
        Self::Unsatisfied { reason: None }
    }
}

#[derive(Debug, derive_more::IsVariant)]
pub(crate) enum ConditionResolutionCacheResult {
    /// Cache hit.
    Hit(ConditionResolution),
    /// Cache miss; can be inserted into cache.
    Miss,
    /// The value can't be cached; Or, an incompatible value is already in cache.
    NotApplicable,
}

pub(crate) struct ConditionResolverCache {
    // For every edge having a condition, we cache the resolution its conditions when possible.
    // We save resolution with the set of excluded edges that were used to compute it: the reason we do this is
    // that excluded edges impact the resolution, so we should only used a cached value if we know the excluded
    // edges are the same as when caching, and while we could decide to cache only when we have no excluded edges
    // at all, this would sub-optimal for types that have multiple keys, as the algorithm will always at least
    // include the previous key edges to the excluded edges of other keys. In other words, if we only cached
    // when we have no excluded edges, we'd only ever use the cache for the first key of every type. However,
    // as the algorithm always try keys in the same order (the order of the edges in the query graph), including
    // the excluded edges we see on the first ever call is actually the proper thing to do.
    edge_states: IndexMap<EdgeIndex, (ConditionResolution, ExcludedDestinations)>,
}

impl ConditionResolverCache {
    pub(crate) fn new() -> Self {
        Self {
            edge_states: Default::default(),
        }
    }

    pub(crate) fn contains(
        &mut self,
        edge: EdgeIndex,
        context: &OpGraphPathContext,
        excluded_destinations: &ExcludedDestinations,
        excluded_conditions: &ExcludedConditions,
        extra_conditions: Option<&SelectionSet>,
    ) -> ConditionResolutionCacheResult {
        if extra_conditions.is_some() {
            return ConditionResolutionCacheResult::NotApplicable;
        }
        // We don't cache if there is a context or excluded conditions because those would impact the resolution and
        // we don't want to cache a value per-context and per-excluded-conditions (we also don't cache per-excluded-edges though
        // instead we cache a value only for the first-see excluded edges; see above why that work in practice).
        // TODO: we could actually have a better handling of the context: it doesn't really change how we'd resolve the condition, it's only
        // that the context, if not empty, would have to be added to the trigger of key edges in the resolution path tree when appropriate
        // and we currently don't handle that. But we could cache with an empty context, and then apply the proper transformation on the
        // cached value `pathTree` when the context is not empty. That said, the context is about active @include/@skip and it's not use
        // that commonly, so this is probably not an urgent improvement.
        if !context.is_empty() || !excluded_conditions.is_empty() {
            return ConditionResolutionCacheResult::NotApplicable;
        }

        if let Some((cached_resolution, cached_excluded_destinations)) = self.edge_states.get(&edge)
        {
            // Cache hit.
            // Ensure we have the same excluded destinations as when we cached the value.
            if cached_excluded_destinations == excluded_destinations {
                return ConditionResolutionCacheResult::Hit(cached_resolution.clone());
            }
            // Otherwise, fall back to non-cached computation
            ConditionResolutionCacheResult::NotApplicable
        } else {
            // Cache miss
            ConditionResolutionCacheResult::Miss
        }
    }

    pub(crate) fn insert(
        &mut self,
        edge: EdgeIndex,
        resolution: ConditionResolution,
        excluded_destinations: ExcludedDestinations,
    ) {
        self.edge_states
            .insert(edge, (resolution, excluded_destinations));
    }
}

/// A query plan resolver for edge conditions that caches the outcome per edge.
// PORT_NOTE: This ports the `cachingConditionResolver` function from JS. In JS version, the
//            function creates a closure capturing the QueryPlanningTraversal/ValidationTraversal
//            instance itself The same would be infeasible to implement in Rust due to the cyclic
//            references. Instead, in Rust, it is implemented as `CachingConditionResolver` and
//            `ConditionResolver` traits that will be implemented by `QueryPlanningTraversal` and
//            `ValidationTraversal` structs.
pub(crate) trait CachingConditionResolver {
    fn query_graph(&self) -> &QueryGraph;

    fn resolve_without_cache(
        &self,
        edge: EdgeIndex,
        context: &OpGraphPathContext,
        excluded_destinations: &ExcludedDestinations,
        excluded_conditions: &ExcludedConditions,
        extra_conditions: Option<&SelectionSet>,
    ) -> Result<ConditionResolution, FederationError>;

    fn resolver_cache(&mut self) -> &mut ConditionResolverCache;

    fn resolve_with_cache(
        &mut self,
        edge: EdgeIndex,
        context: &OpGraphPathContext,
        excluded_destinations: &ExcludedDestinations,
        excluded_conditions: &ExcludedConditions,
        extra_conditions: Option<&SelectionSet>,
    ) -> Result<ConditionResolution, FederationError> {
        let cache_result = self.resolver_cache().contains(
            edge,
            context,
            excluded_destinations,
            excluded_conditions,
            extra_conditions,
        );

        if let ConditionResolutionCacheResult::Hit(cached_resolution) = cache_result {
            return Ok(cached_resolution);
        }

        let resolution = self.resolve_without_cache(
            edge,
            context,
            excluded_destinations,
            excluded_conditions,
            extra_conditions,
        )?;
        // See if this resolution is eligible to be inserted into the cache.
        if cache_result.is_miss() {
            self.resolver_cache()
                .insert(edge, resolution.clone(), excluded_destinations.clone());
        }
        Ok(resolution)
    }
}

/// Blanket implementation of `ConditionResolver` for any type that implements
/// `CachingConditionResolver`.
impl<T: CachingConditionResolver> ConditionResolver for T {
    fn resolve(
        &mut self,
        edge: EdgeIndex,
        context: &OpGraphPathContext,
        excluded_destinations: &ExcludedDestinations,
        excluded_conditions: &ExcludedConditions,
        extra_conditions: Option<&SelectionSet>,
    ) -> Result<ConditionResolution, FederationError> {
        // Invariant check: The edge must have conditions.
        let graph = &self.query_graph();
        let edge_data = graph.edge_weight(edge)?;
        assert!(
            edge_data.conditions.is_some() || extra_conditions.is_some(),
            "Should not have been called for edge without conditions"
        );

        self.resolve_with_cache(
            edge,
            context,
            excluded_destinations,
            excluded_conditions,
            extra_conditions,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query_graph::graph_path::operation::OpGraphPathContext;
    //use crate::link::graphql_definition::{OperationConditional, OperationConditionalKind, BooleanOrVariable};

    #[test]
    fn test_condition_resolver_cache() {
        let mut cache = ConditionResolverCache::new();

        let edge1 = EdgeIndex::new(1);
        let empty_context = OpGraphPathContext::default();
        let empty_destinations = ExcludedDestinations::default();
        let empty_conditions = ExcludedConditions::default();

        assert!(
            cache
                .contains(
                    edge1,
                    &empty_context,
                    &empty_destinations,
                    &empty_conditions,
                    None
                )
                .is_miss()
        );

        cache.insert(
            edge1,
            ConditionResolution::unsatisfied_conditions(),
            empty_destinations.clone(),
        );

        assert!(
            cache
                .contains(
                    edge1,
                    &empty_context,
                    &empty_destinations,
                    &empty_conditions,
                    None
                )
                .is_hit(),
        );

        let edge2 = EdgeIndex::new(2);

        assert!(
            cache
                .contains(
                    edge2,
                    &empty_context,
                    &empty_destinations,
                    &empty_conditions,
                    None
                )
                .is_miss()
        );
    }
}
