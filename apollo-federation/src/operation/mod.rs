//! GraphQL operation types for apollo-federation.
//!
//! ## Selection types
//! Each "conceptual" type consists of up to three actual types: a data type, an "element"
//! type, and a selection type.
//! - The data type records the data about the type. Things like a field name or fragment type
//!   condition are in the data type. These types can be constructed and modified with plain rust.
//! - The element type contains the data type and maintains a key for the data. These types provide
//!   APIs for modifications that keep the key up-to-date.
//! - The selection type contains the element type and, for composite fields, a subselection.
//!
//! For example, for fields, the data type is [`FieldData`], the element type is
//! [`Field`], and the selection type is [`FieldSelection`].

use std::borrow::Cow;
use std::cmp::Ordering;
use std::fmt::Display;
use std::fmt::Formatter;
use std::hash::Hash;
use std::hash::Hasher;
use std::ops::Deref;
use std::sync::Arc;
use std::sync::atomic;

use apollo_compiler::Name;
use apollo_compiler::Node;
use apollo_compiler::ast;
use apollo_compiler::collections::HashMap;
use apollo_compiler::collections::IndexMap;
use apollo_compiler::collections::IndexSet;
use apollo_compiler::executable;
use apollo_compiler::executable::FieldSet;
use apollo_compiler::executable::Fragment;
use apollo_compiler::name;
use apollo_compiler::schema::Directive;
use apollo_compiler::ty;
use apollo_compiler::validation::Valid;
use itertools::Itertools;

use crate::compat::coerce_executable_values;
use crate::error::FederationError;
use crate::error::SingleFederationError;
use crate::link::graphql_definition::BooleanOrVariable;
use crate::link::graphql_definition::DeferDirectiveArguments;
use crate::query_graph::graph_path::operation::OpPathElement;
use crate::query_plan::FetchDataKeyRenamer;
use crate::query_plan::FetchDataPathElement;
use crate::query_plan::FetchDataRewrite;
use crate::query_plan::conditions::Conditions;
use crate::schema::ValidFederationSchema;
use crate::schema::definitions::types_can_be_merged;
use crate::schema::position::AbstractTypeDefinitionPosition;
use crate::schema::position::CompositeTypeDefinitionPosition;
use crate::schema::position::FieldDefinitionPosition;
use crate::schema::position::InterfaceTypeDefinitionPosition;
use crate::schema::position::SchemaRootDefinitionKind;
use crate::supergraph::GRAPHQL_STRING_TYPE_NAME;
use crate::utils::MultiIndexMap;

mod contains;
mod directive_list;
mod merging;
mod optimize;
mod rebase;
mod simplify;
#[cfg(test)]
mod tests;

pub(crate) use contains::*;
pub(crate) use directive_list::DirectiveList;
pub(crate) use merging::*;
pub(crate) use rebase::*;
#[cfg(test)]
pub(crate) use tests::never_cancel;

pub(crate) const TYPENAME_FIELD: Name = name!("__typename");

// Global storage for the counter used to uniquely identify selections
static NEXT_ID: atomic::AtomicUsize = atomic::AtomicUsize::new(1);

/// Opaque wrapper of the unique selection ID type.
///
/// NOTE: This ID does not ensure that IDs are unique because its internal counter resets on
/// startup. It currently implements `Serialize` for debugging purposes. It should not implement
/// `Deserialize`, and, more specifically, it should not be used for caching until uniqueness is
/// provided (i.e. the inner type is a `Uuid` or the like).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, serde::Serialize)]
pub(crate) struct SelectionId(usize);

impl SelectionId {
    pub(crate) fn new() -> Self {
        // atomically increment global counter
        Self(NEXT_ID.fetch_add(1, atomic::Ordering::AcqRel))
    }
}

/// A list of arguments to a field or directive.
///
/// All arguments and input object values are sorted in a consistent order.
///
/// This type is immutable and cheaply cloneable.
#[derive(Clone, PartialEq, Eq, Default, serde::Serialize)]
pub(crate) struct ArgumentList {
    /// The inner list *must* be sorted with `sort_arguments`.
    #[serde(
        serialize_with = "crate::utils::serde_bridge::serialize_optional_slice_of_exe_argument_nodes"
    )]
    inner: Option<Arc<[Node<executable::Argument>]>>,
}

impl std::fmt::Debug for ArgumentList {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        // Print the slice representation.
        self.deref().fmt(f)
    }
}

/// Sort an input value, which means specifically sorting their object values by keys (assuming no
/// duplicates).
///
/// After sorting, hashing and plain-Rust equality have the expected result for values that are
/// spec-equivalent.
fn sort_value(value: &mut executable::Value) {
    use apollo_compiler::executable::Value;
    match value {
        Value::List(elems) => {
            elems
                .iter_mut()
                .for_each(|value| sort_value(value.make_mut()));
        }
        Value::Object(pairs) => {
            pairs
                .iter_mut()
                .for_each(|(_, value)| sort_value(value.make_mut()));
            pairs.sort_by(|left, right| left.0.cmp(&right.0));
        }
        _ => {}
    }
}

/// Sort arguments, which means specifically sorting arguments by names and object values by keys
/// (assuming no duplicates).
///
/// After sorting, hashing and plain-Rust equality have the expected result for lists that are
/// spec-equivalent.
fn sort_arguments(arguments: &mut [Node<executable::Argument>]) {
    arguments
        .iter_mut()
        .for_each(|arg| sort_value(arg.make_mut().value.make_mut()));
    arguments.sort_by(|left, right| left.name.cmp(&right.name));
}

impl From<Vec<Node<executable::Argument>>> for ArgumentList {
    fn from(mut arguments: Vec<Node<executable::Argument>>) -> Self {
        if arguments.is_empty() {
            return Self::new();
        }

        sort_arguments(&mut arguments);

        Self {
            inner: Some(Arc::from(arguments)),
        }
    }
}

impl FromIterator<Node<executable::Argument>> for ArgumentList {
    fn from_iter<T: IntoIterator<Item = Node<executable::Argument>>>(iter: T) -> Self {
        Self::from(Vec::from_iter(iter))
    }
}

impl Deref for ArgumentList {
    type Target = [Node<executable::Argument>];

    fn deref(&self) -> &Self::Target {
        self.inner.as_deref().unwrap_or_default()
    }
}

impl ArgumentList {
    /// Create an empty argument list.
    pub(crate) const fn new() -> Self {
        Self { inner: None }
    }

    /// Create a argument list with a single argument.
    ///
    /// This sorts any input object values provided to the argument.
    pub(crate) fn one(argument: impl Into<Node<executable::Argument>>) -> Self {
        Self::from(vec![argument.into()])
    }
}

/// An analogue of the apollo-compiler type `Operation` with these changes:
/// - Stores the schema that the operation is queried against.
/// - Swaps `operation_type` with `root_kind` (using the analogous apollo-federation type).
/// - Encloses collection types in `Arc`s to facilitate cheaper cloning.
/// - Expands all named fragments into inline fragments.
/// - Deduplicates all selections within its selection sets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Operation {
    pub(crate) schema: ValidFederationSchema,
    pub(crate) root_kind: SchemaRootDefinitionKind,
    pub(crate) name: Option<Name>,
    pub(crate) variables: Arc<Vec<Node<executable::VariableDefinition>>>,
    pub(crate) directives: DirectiveList,
    pub(crate) selection_set: SelectionSet,
}

impl Operation {
    /// Parse an operation from a source string.
    #[cfg(any(test, doc))]
    pub fn parse(
        schema: ValidFederationSchema,
        source_text: &str,
        source_name: &str,
    ) -> Result<Self, FederationError> {
        let document = apollo_compiler::ExecutableDocument::parse_and_validate(
            schema.schema(),
            source_text,
            source_name,
        )?;
        let operation = document.operations.iter().next().expect("operation exists");
        let normalized_operation = Operation {
            schema: schema.clone(),
            root_kind: operation.operation_type.into(),
            name: operation.name.clone(),
            variables: Arc::new(operation.variables.clone()),
            directives: operation.directives.clone().into(),
            selection_set: SelectionSet::from_selection_set(
                &operation.selection_set,
                &FragmentSpreadCache::init(&document.fragments, &schema, &never_cancel),
                &schema,
                &never_cancel,
            )?,
        };
        Ok(normalized_operation)
    }
}

/// An analogue of the apollo-compiler type `SelectionSet` with these changes:
/// - For the type, stores the schema and the position in that schema instead of just the
///   `NamedType`.
/// - Stores selections in a map so they can be normalized efficiently.
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct SelectionSet {
    #[serde(skip)]
    pub(crate) schema: ValidFederationSchema,
    pub(crate) type_position: CompositeTypeDefinitionPosition,
    pub(crate) selections: Arc<SelectionMap>,
}

impl PartialEq for SelectionSet {
    fn eq(&self, other: &Self) -> bool {
        self.selections == other.selections
    }
}

impl Eq for SelectionSet {}

impl Hash for SelectionSet {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.selections.hash(state);
    }
}

mod selection_map;

pub(crate) use selection_map::FieldSelectionValue;
pub(crate) use selection_map::HasSelectionKey;
pub(crate) use selection_map::InlineFragmentSelectionValue;
pub(crate) use selection_map::SelectionKey;
pub(crate) use selection_map::SelectionMap;
pub(crate) use selection_map::SelectionValue;

/// An analogue of the apollo-compiler type `Selection` that stores our other selection analogues
/// instead of the apollo-compiler types.
#[derive(Debug, Clone, PartialEq, Eq, Hash, derive_more::IsVariant, serde::Serialize)]
pub(crate) enum Selection {
    Field(Arc<FieldSelection>),
    InlineFragment(Arc<InlineFragmentSelection>),
}

impl Selection {
    pub(crate) fn from_field(field: Field, sub_selections: Option<SelectionSet>) -> Self {
        Self::Field(Arc::new(field.with_subselection(sub_selections)))
    }

    /// Build a selection from an OpPathElement and a sub-selection set.
    pub(crate) fn from_element(
        element: OpPathElement,
        sub_selections: Option<SelectionSet>,
    ) -> Result<Selection, FederationError> {
        // PORT_NOTE: This is TODO item is copied from the JS `selectionOfElement` function.
        // TODO: validate that the subSelection is ok for the element
        match element {
            OpPathElement::Field(field) => Ok(Self::from_field(field, sub_selections)),
            OpPathElement::InlineFragment(inline_fragment) => {
                let Some(sub_selections) = sub_selections else {
                    return Err(FederationError::internal(
                        "unexpected inline fragment without sub-selections",
                    ));
                };
                Ok(InlineFragmentSelection::new(inline_fragment, sub_selections).into())
            }
        }
    }

    pub(crate) fn schema(&self) -> &ValidFederationSchema {
        match self {
            Selection::Field(field_selection) => &field_selection.field.schema,
            Selection::InlineFragment(inline_fragment_selection) => {
                &inline_fragment_selection.inline_fragment.schema
            }
        }
    }

    fn directives(&self) -> &DirectiveList {
        match self {
            Selection::Field(field_selection) => &field_selection.field.directives,
            Selection::InlineFragment(inline_fragment_selection) => {
                &inline_fragment_selection.inline_fragment.directives
            }
        }
    }

    pub(crate) fn element(&self) -> OpPathElement {
        match self {
            Selection::Field(field_selection) => {
                OpPathElement::Field(field_selection.field.clone())
            }
            Selection::InlineFragment(inline_fragment_selection) => {
                OpPathElement::InlineFragment(inline_fragment_selection.inline_fragment.clone())
            }
        }
    }

    // Note: Fragment spreads can be present in optimized operations.
    pub(crate) fn selection_set(&self) -> Option<&SelectionSet> {
        match self {
            Selection::Field(field_selection) => field_selection.selection_set.as_ref(),
            Selection::InlineFragment(inline_fragment_selection) => {
                Some(&inline_fragment_selection.selection_set)
            }
        }
    }

    /// Returns true if the selection key is `__typename` *without directives*.
    pub(crate) fn is_typename_field(&self) -> bool {
        if let Selection::Field(field) = self {
            *field.field.response_name() == TYPENAME_FIELD && field.field.directives.is_empty()
        } else {
            false
        }
    }

    /// Returns the conditions for inclusion of this selection.
    ///
    /// # Errors
    /// Returns an error if the selection contains a fragment spread, or if any of the
    /// @skip/@include directives are invalid (per GraphQL validation rules).
    pub(crate) fn conditions(&self) -> Result<Conditions, FederationError> {
        let self_conditions = Conditions::from_directives(self.directives())?;
        if let Conditions::Boolean(false) = self_conditions {
            // Never included, so there is no point recursing.
            Ok(Conditions::Boolean(false))
        } else {
            match self {
                Selection::Field(_) => {
                    // The sub-selections of this field don't affect whether we should query this
                    // field, so we explicitly do not merge them in.
                    //
                    // PORT_NOTE: The JS codebase merges the sub-selections' conditions in with the
                    // field's conditions when field's selections are non-boolean. This is arguably
                    // a bug, so we've fixed it here.
                    Ok(self_conditions)
                }
                Selection::InlineFragment(inline) => {
                    Ok(self_conditions.merge(inline.selection_set.conditions()?))
                }
            }
        }
    }

    pub(crate) fn with_updated_selection_set(
        &self,
        selection_set: Option<SelectionSet>,
    ) -> Result<Self, FederationError> {
        match self {
            Selection::Field(field) => Ok(Selection::from(
                field.with_updated_selection_set(selection_set),
            )),
            Selection::InlineFragment(inline_fragment) => {
                let Some(selection_set) = selection_set else {
                    return Err(FederationError::internal(
                        "updating inline fragment without a sub-selection set",
                    ));
                };
                Ok(inline_fragment
                    .with_updated_selection_set(selection_set)
                    .into())
            }
        }
    }

    pub(crate) fn with_updated_selections<S: Into<Selection>>(
        &self,
        type_position: CompositeTypeDefinitionPosition,
        selections: impl IntoIterator<Item = S>,
    ) -> Result<Self, FederationError> {
        let new_sub_selection =
            SelectionSet::from_raw_selections(self.schema().clone(), type_position, selections);
        self.with_updated_selection_set(Some(new_sub_selection))
    }

    /// Apply the `mapper` to self.selection_set, if it exists, and return a new `Selection`.
    /// - Note: The returned selection may have no subselection set or an empty one if the mapper
    ///   returns so, which may make the returned selection invalid. It's caller's responsibility
    ///   to appropriately handle invalid return values.
    pub(crate) fn map_selection_set(
        &self,
        mapper: impl FnOnce(&SelectionSet) -> Result<Option<SelectionSet>, FederationError>,
    ) -> Result<Self, FederationError> {
        if let Some(selection_set) = self.selection_set() {
            self.with_updated_selection_set(mapper(selection_set)?)
        } else {
            // selection has no (sub-)selection set.
            Ok(self.clone())
        }
    }

    pub(crate) fn any_element(&self, predicate: &mut impl FnMut(OpPathElement) -> bool) -> bool {
        match self {
            Selection::Field(field_selection) => field_selection.any_element(predicate),
            Selection::InlineFragment(inline_fragment_selection) => {
                inline_fragment_selection.any_element(predicate)
            }
        }
    }
}

impl From<FieldSelection> for Selection {
    fn from(value: FieldSelection) -> Self {
        Self::Field(value.into())
    }
}

impl From<InlineFragmentSelection> for Selection {
    fn from(value: InlineFragmentSelection) -> Self {
        Self::InlineFragment(value.into())
    }
}

impl HasSelectionKey for Selection {
    fn key(&self) -> SelectionKey<'_> {
        match self {
            Selection::Field(field_selection) => field_selection.key(),
            Selection::InlineFragment(inline_fragment_selection) => inline_fragment_selection.key(),
        }
    }
}

impl Ord for Selection {
    fn cmp(&self, other: &Self) -> Ordering {
        fn compare_directives(d1: &DirectiveList, d2: &DirectiveList) -> Ordering {
            if d1 == d2 {
                Ordering::Equal
            } else if d1.is_empty() {
                Ordering::Less
            } else if d2.is_empty() {
                Ordering::Greater
            } else {
                d1.to_string().cmp(&d2.to_string())
            }
        }

        match (self, other) {
            (Selection::Field(f1), Selection::Field(f2)) => {
                // cannot have two fields with the same response name so no need to check args or directives
                f1.field.response_name().cmp(f2.field.response_name())
            }
            (Selection::Field(_), _) => Ordering::Less,
            (Selection::InlineFragment(_), Selection::Field(_)) => Ordering::Greater,
            (Selection::InlineFragment(i1), Selection::InlineFragment(i2)) => {
                // compare type conditions and then directives
                let first_type_position = &i1.inline_fragment.type_condition_position;
                let second_type_position = &i2.inline_fragment.type_condition_position;
                match (first_type_position, second_type_position) {
                    (Some(t1), Some(t2)) => {
                        let compare_type_conditions = t1.type_name().cmp(t2.type_name());
                        if compare_type_conditions == Ordering::Equal {
                            // compare directive lists
                            compare_directives(
                                &i1.inline_fragment.directives,
                                &i2.inline_fragment.directives,
                            )
                        } else {
                            compare_type_conditions
                        }
                    }
                    (Some(_), None) => Ordering::Less,
                    (None, Some(_)) => Ordering::Greater,
                    (None, None) => {
                        // compare directive lists
                        compare_directives(
                            &i1.inline_fragment.directives,
                            &i2.inline_fragment.directives,
                        )
                    }
                }
            }
        }
    }
}

impl PartialOrd for Selection {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, derive_more::From)]
pub(crate) enum SelectionOrSet {
    Selection(Selection),
    SelectionSet(SelectionSet),
}

mod field_selection {
    use std::hash::Hash;
    use std::hash::Hasher;

    use apollo_compiler::Name;
    use serde::Serialize;

    use super::TYPENAME_FIELD;
    use crate::error::FederationError;
    use crate::operation::ArgumentList;
    use crate::operation::DirectiveList;
    use crate::operation::HasSelectionKey;
    use crate::operation::SelectionKey;
    use crate::operation::SelectionSet;
    use crate::query_plan::FetchDataPathElement;
    use crate::schema::ValidFederationSchema;
    use crate::schema::position::CompositeTypeDefinitionPosition;
    use crate::schema::position::FieldDefinitionPosition;
    use crate::schema::position::TypeDefinitionPosition;

    /// An analogue of the apollo-compiler type `Field` with these changes:
    /// - Makes the selection set optional. This is because `SelectionSet` requires a type of
    ///   `CompositeTypeDefinitionPosition`, which won't exist for fields returning a non-composite type
    ///   (scalars and enums).
    /// - Stores the field data (other than the selection set) in `Field`, to facilitate
    ///   operation paths and graph paths.
    /// - For the field definition, stores the schema and the position in that schema instead of just
    ///   the `FieldDefinition` (which contains no references to the parent type or schema).
    /// - Encloses collection types in `Arc`s to facilitate cheaper cloning.
    #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
    pub(crate) struct FieldSelection {
        pub(crate) field: Field,
        pub(crate) selection_set: Option<SelectionSet>,
    }

    impl HasSelectionKey for FieldSelection {
        fn key(&self) -> SelectionKey<'_> {
            self.field.key()
        }
    }

    impl FieldSelection {
        pub(crate) fn with_updated_selection_set(
            &self,
            selection_set: Option<SelectionSet>,
        ) -> Self {
            Self {
                field: self.field.clone(),
                selection_set,
            }
        }
    }

    // SiblingTypename indicates how the sibling __typename field should be restored.
    // PORT_NOTE: The JS version used the empty string to indicate unaliased sibling typenames.
    // Here we use an enum to make the distinction explicit.
    #[derive(Debug, Clone, Serialize)]
    pub(crate) enum SiblingTypename {
        Unaliased,
        Aliased(Name), // the sibling __typename has been aliased
    }

    impl SiblingTypename {
        pub(crate) fn alias(&self) -> Option<&Name> {
            match self {
                SiblingTypename::Unaliased => None,
                SiblingTypename::Aliased(alias) => Some(alias),
            }
        }
    }

    /// The non-selection-set data of `FieldSelection`, used with operation paths and graph
    /// paths.
    #[derive(Debug, Clone, Serialize)]
    pub(crate) struct Field {
        #[serde(skip)]
        pub(crate) schema: ValidFederationSchema,
        pub(crate) field_position: FieldDefinitionPosition,
        pub(crate) alias: Option<Name>,
        pub(crate) arguments: ArgumentList,
        pub(crate) directives: DirectiveList,
        pub(crate) sibling_typename: Option<SiblingTypename>,
    }

    impl PartialEq for Field {
        fn eq(&self, other: &Self) -> bool {
            self.field_position.field_name() == other.field_position.field_name()
                && self.key() == other.key()
                && self.arguments == other.arguments
        }
    }

    impl Eq for Field {}

    impl Hash for Field {
        fn hash<H: Hasher>(&self, state: &mut H) {
            self.field_position.field_name().hash(state);
            self.key().hash(state);
            self.arguments.hash(state);
        }
    }

    impl Field {
        /// Create a trivial field selection without any arguments or directives.
        #[cfg(test)]
        pub(crate) fn from_position(
            schema: &ValidFederationSchema,
            field_position: FieldDefinitionPosition,
        ) -> Self {
            Self {
                schema: schema.clone(),
                field_position,
                alias: None,
                arguments: Default::default(),
                directives: Default::default(),
                sibling_typename: None,
            }
        }

        // Note: The `schema` argument must be a subgraph schema, so the __typename field won't
        // need to be rebased, which would fail (since __typename fields are undefined).
        pub(crate) fn new_introspection_typename(
            schema: &ValidFederationSchema,
            parent_type: &CompositeTypeDefinitionPosition,
            alias: Option<Name>,
        ) -> Self {
            Self {
                schema: schema.clone(),
                field_position: parent_type.introspection_typename_field(),
                alias,
                arguments: Default::default(),
                directives: Default::default(),
                sibling_typename: None,
            }
        }

        pub(crate) fn schema(&self) -> &ValidFederationSchema {
            &self.schema
        }

        pub(crate) fn name(&self) -> &Name {
            self.field_position.field_name()
        }

        pub(crate) fn response_name(&self) -> &Name {
            self.alias.as_ref().unwrap_or_else(|| self.name())
        }

        // Is this a plain simple __typename without any directive or alias?
        pub(crate) fn is_plain_typename_field(&self) -> bool {
            *self.field_position.field_name() == TYPENAME_FIELD
                && self.directives.is_empty()
                && self.alias.is_none()
        }

        pub(crate) fn sibling_typename(&self) -> Option<&SiblingTypename> {
            self.sibling_typename.as_ref()
        }

        pub(crate) fn sibling_typename_mut(&mut self) -> &mut Option<SiblingTypename> {
            &mut self.sibling_typename
        }

        pub(crate) fn as_path_element(&self) -> FetchDataPathElement {
            FetchDataPathElement::Key(self.response_name().clone(), Default::default())
        }

        pub(crate) fn output_base_type(&self) -> Result<TypeDefinitionPosition, FederationError> {
            let definition = self.field_position.get(self.schema.schema())?;
            self.schema
                .get_type(definition.ty.inner_named_type().clone())
        }

        pub(crate) fn is_leaf(&self) -> Result<bool, FederationError> {
            let base_type_position = self.output_base_type()?;
            Ok(matches!(
                base_type_position,
                TypeDefinitionPosition::Scalar(_) | TypeDefinitionPosition::Enum(_)
            ))
        }

        pub(crate) fn with_updated_directives(&self, directives: impl Into<DirectiveList>) -> Self {
            Self {
                directives: directives.into(),
                ..self.clone()
            }
        }

        pub(crate) fn with_updated_alias(&self, alias: Name) -> Self {
            Self {
                alias: Some(alias),
                ..self.clone()
            }
        }

        /// Turn this `Field` into a `FieldSelection` with the given sub-selection. If this is
        /// meant to be a leaf selection, use `None`.
        pub(crate) fn with_subselection(
            self,
            selection_set: Option<SelectionSet>,
        ) -> FieldSelection {
            if cfg!(debug_assertions) {
                if let Some(ref selection_set) = selection_set {
                    if let Ok(field_type) = self.output_base_type() {
                        if let Ok(field_type_position) =
                            CompositeTypeDefinitionPosition::try_from(field_type)
                        {
                            debug_assert_eq!(
                                field_type_position, selection_set.type_position,
                                "Field and its selection set should point to the same type position [field position: {}, selection position: {}]",
                                field_type_position, selection_set.type_position,
                            );
                            debug_assert_eq!(
                                self.schema, selection_set.schema,
                                "Field and its selection set should point to the same schema",
                            );
                        } else {
                            debug_assert!(
                                false,
                                "Field with subselection does not reference CompositeTypePosition"
                            );
                        }
                    } else {
                        debug_assert!(
                            false,
                            "Field with subselection does not reference CompositeTypePosition"
                        );
                    }
                }
            }

            FieldSelection {
                field: self,
                selection_set,
            }
        }
    }

    impl HasSelectionKey for Field {
        fn key(&self) -> SelectionKey<'_> {
            SelectionKey::Field {
                response_name: self.response_name(),
                directives: &self.directives,
            }
        }
    }
}

pub(crate) use field_selection::Field;
pub(crate) use field_selection::FieldSelection;
pub(crate) use field_selection::SiblingTypename;

mod inline_fragment_selection {
    use std::hash::Hash;
    use std::hash::Hasher;

    use serde::Serialize;

    use crate::error::FederationError;
    use crate::link::graphql_definition::DeferDirectiveArguments;
    use crate::link::graphql_definition::defer_directive_arguments;
    use crate::operation::DirectiveList;
    use crate::operation::HasSelectionKey;
    use crate::operation::SelectionId;
    use crate::operation::SelectionKey;
    use crate::operation::SelectionSet;
    use crate::operation::is_deferred_selection;
    use crate::query_plan::FetchDataPathElement;
    use crate::schema::ValidFederationSchema;
    use crate::schema::position::CompositeTypeDefinitionPosition;

    /// An analogue of the apollo-compiler type `InlineFragment` with these changes:
    /// - Stores the inline fragment data (other than the selection set) in `InlineFragment`,
    ///   to facilitate operation paths and graph paths.
    /// - For the type condition, stores the schema and the position in that schema instead of just
    ///   the `NamedType`.
    /// - Stores the parent type explicitly, which means storing the position (in apollo-compiler, this
    ///   is in the parent selection set).
    /// - Encloses collection types in `Arc`s to facilitate cheaper cloning.
    #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
    pub(crate) struct InlineFragmentSelection {
        pub(crate) inline_fragment: InlineFragment,
        pub(crate) selection_set: SelectionSet,
    }

    impl InlineFragmentSelection {
        pub(crate) fn with_updated_selection_set(&self, selection_set: SelectionSet) -> Self {
            Self {
                inline_fragment: self.inline_fragment.clone(),
                selection_set,
            }
        }

        pub(crate) fn with_updated_directives(&self, directives: impl Into<DirectiveList>) -> Self {
            Self {
                inline_fragment: self.inline_fragment.with_updated_directives(directives),
                selection_set: self.selection_set.clone(),
            }
        }

        pub(crate) fn with_updated_directives_and_selection_set(
            &self,
            directives: impl Into<DirectiveList>,
            selection_set: SelectionSet,
        ) -> Self {
            Self {
                inline_fragment: self.inline_fragment.with_updated_directives(directives),
                selection_set,
            }
        }
    }

    impl HasSelectionKey for InlineFragmentSelection {
        fn key(&self) -> SelectionKey<'_> {
            self.inline_fragment.key()
        }
    }

    /// The non-selection-set data of `InlineFragmentSelection`, used with operation paths and
    /// graph paths.
    #[derive(Debug, Clone, Serialize)]
    pub(crate) struct InlineFragment {
        #[serde(skip)]
        pub(crate) schema: ValidFederationSchema,
        pub(crate) parent_type_position: CompositeTypeDefinitionPosition,
        pub(crate) type_condition_position: Option<CompositeTypeDefinitionPosition>,
        pub(crate) directives: DirectiveList,
        pub(crate) selection_id: SelectionId,
    }

    impl PartialEq for InlineFragment {
        fn eq(&self, other: &Self) -> bool {
            self.key() == other.key()
        }
    }

    impl Eq for InlineFragment {}

    impl Hash for InlineFragment {
        fn hash<H: Hasher>(&self, state: &mut H) {
            self.key().hash(state);
        }
    }

    impl InlineFragment {
        pub(crate) fn schema(&self) -> &ValidFederationSchema {
            &self.schema
        }

        pub(crate) fn with_updated_type_condition(
            &self,
            type_condition_position: Option<CompositeTypeDefinitionPosition>,
        ) -> Self {
            Self {
                type_condition_position,
                ..self.clone()
            }
        }

        pub(crate) fn with_updated_directives(&self, directives: impl Into<DirectiveList>) -> Self {
            Self {
                directives: directives.into(),
                ..self.clone()
            }
        }

        pub(crate) fn as_path_element(&self) -> Option<FetchDataPathElement> {
            let condition = self.type_condition_position.clone()?;

            Some(FetchDataPathElement::TypenameEquals(
                condition.type_name().clone(),
            ))
        }

        pub(crate) fn defer_directive_arguments(
            &self,
        ) -> Result<Option<DeferDirectiveArguments>, FederationError> {
            if let Some(directive) = self.directives.get("defer") {
                Ok(Some(defer_directive_arguments(directive)?))
            } else {
                Ok(None)
            }
        }

        pub(crate) fn casted_type(&self) -> CompositeTypeDefinitionPosition {
            self.type_condition_position
                .clone()
                .unwrap_or_else(|| self.parent_type_position.clone())
        }
    }

    impl HasSelectionKey for InlineFragment {
        fn key(&self) -> SelectionKey<'_> {
            if is_deferred_selection(&self.directives) {
                SelectionKey::Defer {
                    deferred_id: self.selection_id,
                }
            } else {
                SelectionKey::InlineFragment {
                    type_condition: self
                        .type_condition_position
                        .as_ref()
                        .map(|pos| pos.type_name()),
                    directives: &self.directives,
                }
            }
        }
    }
}

pub(crate) use inline_fragment_selection::InlineFragment;
pub(crate) use inline_fragment_selection::InlineFragmentSelection;

use self::selection_map::OwnedSelectionKey;
use crate::schema::position::INTROSPECTION_TYPENAME_FIELD_NAME;

/// the return type of `lazy_map` function's `mapper` closure argument
#[derive(derive_more::From)]
pub(crate) enum SelectionMapperReturn {
    #[allow(unused)] // may be better to keep unused than to add back when necessary
    None,
    Selection(Selection),
    SelectionList(Vec<Selection>),
}

impl FromIterator<Selection> for SelectionMapperReturn {
    fn from_iter<T>(iter: T) -> Self
    where
        T: IntoIterator<Item = Selection>,
    {
        Self::SelectionList(Vec::from_iter(iter))
    }
}

impl SelectionSet {
    pub(crate) fn empty(
        schema: ValidFederationSchema,
        type_position: CompositeTypeDefinitionPosition,
    ) -> Self {
        Self {
            schema,
            type_position,
            selections: Default::default(),
        }
    }

    pub(crate) fn split_top_level_fields(self) -> impl Iterator<Item = SelectionSet> {
        // NOTE: Ideally, we could just use a generator but, instead, we have to manually implement
        // one :(
        struct TopLevelFieldSplitter {
            parent_type: CompositeTypeDefinitionPosition,
            starting_set: selection_map::IntoValues,
            stack: Vec<(OpPathElement, Self)>,
        }

        impl TopLevelFieldSplitter {
            fn new(selection_set: SelectionSet) -> Self {
                Self {
                    parent_type: selection_set.type_position,
                    starting_set: Arc::unwrap_or_clone(selection_set.selections).into_values(),
                    stack: Vec::new(),
                }
            }
        }

        impl Iterator for TopLevelFieldSplitter {
            type Item = SelectionSet;

            fn next(&mut self) -> Option<Self::Item> {
                loop {
                    match self.stack.last_mut() {
                        None => {
                            let selection = self.starting_set.next()?;
                            if selection.is_field() {
                                return Some(SelectionSet::from_selection(
                                    self.parent_type.clone(),
                                    selection,
                                ));
                            } else if let Some(set) = selection.selection_set().cloned() {
                                self.stack.push((selection.element(), Self::new(set)));
                            }
                        }
                        Some((element, top)) => {
                            match top.find_map(|set| {
                                let parent_type = element.parent_type_position();
                                Selection::from_element(element.clone(), Some(set))
                                    .ok()
                                    .map(|sel| SelectionSet::from_selection(parent_type, sel))
                            }) {
                                Some(set) => return Some(set),
                                None => {
                                    self.stack.pop();
                                }
                            }
                        }
                    }
                }
            }
        }

        TopLevelFieldSplitter::new(self)
    }

    /// PORT_NOTE: JS calls this `newCompositeTypeSelectionSet`
    pub(crate) fn for_composite_type(
        schema: ValidFederationSchema,
        type_position: CompositeTypeDefinitionPosition,
    ) -> Self {
        let typename_field = Field::new_introspection_typename(&schema, &type_position, None)
            .with_subselection(None);
        Self {
            schema,
            type_position,
            selections: Arc::new(std::iter::once(typename_field).collect()),
        }
    }

    /// Build a selection set from a single selection.
    pub(crate) fn from_selection(
        type_position: CompositeTypeDefinitionPosition,
        selection: Selection,
    ) -> Self {
        let schema = selection.schema().clone();
        let mut selection_map = SelectionMap::new();
        selection_map.insert(selection);
        Self {
            schema,
            type_position,
            selections: Arc::new(selection_map),
        }
    }

    /// Build a selection set from the given selections. This does **not** handle merging of
    /// selections with the same keys!
    pub(crate) fn from_raw_selections<S: Into<Selection>>(
        schema: ValidFederationSchema,
        type_position: CompositeTypeDefinitionPosition,
        selections: impl IntoIterator<Item = S>,
    ) -> Self {
        Self {
            schema,
            type_position,
            selections: Arc::new(selections.into_iter().collect()),
        }
    }

    #[cfg(any(doc, test))]
    pub(crate) fn parse(
        schema: ValidFederationSchema,
        type_position: CompositeTypeDefinitionPosition,
        source_text: &str,
    ) -> Result<Self, FederationError> {
        let selection_set = crate::schema::field_set::parse_field_set_without_normalization(
            schema.schema(),
            type_position.type_name().clone(),
            source_text,
            false,
        )?
        .0;
        let fragments = Default::default();
        SelectionSet::from_selection_set(&selection_set, &fragments, &schema, &never_cancel)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.selections.is_empty()
    }

    pub(crate) fn contains_top_level_field(&self, field: &Field) -> Result<bool, FederationError> {
        if let Some(selection) = self.selections.get(field.key()) {
            let Selection::Field(field_selection) = selection else {
                return Err(SingleFederationError::Internal {
                    message: format!(
                        "Field selection key for field \"{}\" references non-field selection",
                        field.field_position,
                    ),
                }
                .into());
            };
            Ok(field_selection.field == *field)
        } else {
            Ok(false)
        }
    }

    /// Normalize this selection set (merging selections with the same keys), with the following
    /// additional transformations:
    /// - Expand fragment spreads into inline fragments.
    /// - Remove `__schema` or `__type` introspection fields, as these shouldn't be handled by query
    ///   planning.
    /// - Hoist fragment spreads/inline fragments into their parents if they have no directives and
    ///   their parent type matches.
    ///
    /// Note this function asserts that the type of the selection set is a composite type (i.e. this
    /// isn't the empty selection set of some leaf field), and will return error if this is not the
    /// case.
    pub(crate) fn from_selection_set(
        selection_set: &executable::SelectionSet,
        fragments_cache: &FragmentSpreadCache,
        schema: &ValidFederationSchema,
        check_cancellation: &dyn Fn() -> Result<(), SingleFederationError>,
    ) -> Result<SelectionSet, FederationError> {
        let type_position: CompositeTypeDefinitionPosition =
            schema.get_type(selection_set.ty.clone())?.try_into()?;
        let mut normalized_selections = vec![];
        SelectionSet::normalize_selections(
            &selection_set.selections,
            &type_position,
            &mut normalized_selections,
            fragments_cache,
            schema,
            check_cancellation,
        )?;
        let mut merged = SelectionSet {
            schema: schema.clone(),
            type_position,
            selections: Arc::new(SelectionMap::new()),
        };
        merged.merge_selections_into(normalized_selections.iter())?;
        Ok(merged)
    }

    /// A helper function for normalizing a list of selections into a destination.
    fn normalize_selections(
        selections: &[executable::Selection],
        parent_type_position: &CompositeTypeDefinitionPosition,
        destination: &mut Vec<Selection>,
        fragments_cache: &FragmentSpreadCache,
        schema: &ValidFederationSchema,
        check_cancellation: &dyn Fn() -> Result<(), SingleFederationError>,
    ) -> Result<(), FederationError> {
        for selection in selections {
            check_cancellation()?;
            match selection {
                executable::Selection::Field(field_selection) => {
                    let Some(normalized_field_selection) = FieldSelection::from_field(
                        field_selection,
                        parent_type_position,
                        fragments_cache,
                        schema,
                        check_cancellation,
                    )?
                    else {
                        continue;
                    };
                    destination.push(Selection::from(normalized_field_selection));
                }
                executable::Selection::FragmentSpread(fragment_spread_selection) => {
                    // convert to inline fragment
                    let inline_fragment_selection = InlineFragmentSelection::from_fragment_spread(
                        parent_type_position, // the parent type of this inline selection
                        fragment_spread_selection,
                        fragments_cache,
                        schema,
                    )?;
                    // We can hoist/collapse named fragments if their type condition is on the
                    // parent type and they don't have any directives.
                    let fragment_type_condition = inline_fragment_selection
                        .inline_fragment
                        .type_condition_position
                        .clone();
                    if fragment_type_condition
                        .is_some_and(|position| &position == parent_type_position)
                        && fragment_spread_selection.directives.is_empty()
                    {
                        destination.extend(inline_fragment_selection.selection_set);
                    } else {
                        destination.push(Selection::InlineFragment(Arc::new(
                            inline_fragment_selection,
                        )));
                    }
                }
                executable::Selection::InlineFragment(inline_fragment_selection) => {
                    let is_on_parent_type =
                        if let Some(type_condition) = &inline_fragment_selection.type_condition {
                            type_condition == parent_type_position.type_name()
                        } else {
                            true
                        };
                    // We can hoist/collapse inline fragments if their type condition is on the
                    // parent type (or they have no type condition) and they don't have any
                    // directives.
                    //
                    // PORT_NOTE: The JS codebase didn't hoist inline fragments, only fragment
                    // spreads (presumably because named fragments would commonly be on the same
                    // type as their fragment spread usages). It should be fine to also hoist inline
                    // fragments though if we notice they're similarly useless (and presumably later
                    // transformations in the JS codebase would take care of this).
                    if is_on_parent_type && inline_fragment_selection.directives.is_empty() {
                        SelectionSet::normalize_selections(
                            &inline_fragment_selection.selection_set.selections,
                            parent_type_position,
                            destination,
                            fragments_cache,
                            schema,
                            check_cancellation,
                        )?;
                    } else {
                        let normalized_inline_fragment_selection =
                            InlineFragmentSelection::from_inline_fragment(
                                inline_fragment_selection,
                                parent_type_position,
                                fragments_cache,
                                schema,
                                check_cancellation,
                            )?;
                        destination.push(Selection::InlineFragment(Arc::new(
                            normalized_inline_fragment_selection,
                        )));
                    }
                }
            }
        }
        Ok(())
    }

    /// Modifies the provided selection set to optimize the handling of __typename selections for query planning.
    ///
    /// __typename information can always be provided by any subgraph declaring that type. While this data can be
    /// theoretically fetched from multiple sources, in practice it doesn't really matter which subgraph we use
    /// for the __typename and we should just get it from the same source as the one that was used to resolve
    /// other fields.
    ///
    /// In most cases, selecting __typename won't be a problem as query planning algorithm ignores "obviously"
    /// inefficient paths. Typically, querying the __typename of an entity is generally ok because when looking at
    /// a path, the query planning algorithm always favor getting a field "locally" if it can (which it always can
    /// for __typename) and ignore alternative that would jump subgraphs.
    ///
    /// When querying a __typename after a @shareable field, query planning algorithm would consider getting the
    /// __typename from EACH version of the @shareable field. This unnecessarily explodes the number of possible
    /// query plans with some useless options and results in degraded performance. Since the number of possible
    /// plans doubles for every field for which there is a choice, eliminating unnecessary choices improves query
    /// planning performance.
    ///
    /// It is unclear how to do this cleanly with the current planning algorithm, so this method is a workaround
    /// so we can efficiently generate query plans. In order to prevent the query planner from spending time
    /// exploring those useless __typename options, we "remove" the unnecessary __typename selections from the
    /// operation. Since we need to ensure that the __typename field will still need to be queried, we "tag"
    /// one of the "sibling" selections (using "attachment") to remember that __typename needs to be added
    /// back eventually. The core query planning algorithm will ignore that tag, and because __typename has been
    /// otherwise removed, we'll save any related work. As we build the final query plan, we'll check back for
    /// those "tags" and add back the __typename selections. As this only happen after the query planning
    /// algorithm has computed all choices, we achieve our goal of not considering useless choices due to
    /// __typename. Do note that if __typename is the "only" selection of some selection set, then we leave it
    /// untouched, and let the query planning algorithm treat it as any other field. We have no other choice in
    /// that case, and that's actually what we want.
    pub(crate) fn optimize_sibling_typenames(
        &mut self,
        interface_types_with_interface_objects: &IndexSet<InterfaceTypeDefinitionPosition>,
    ) -> Result<(), FederationError> {
        let is_interface_object =
            interface_types_with_interface_objects.contains(&InterfaceTypeDefinitionPosition {
                type_name: self.type_position.type_name().clone(),
            });
        let mut typename_field_key: Option<OwnedSelectionKey> = None;
        let mut sibling_field_key: Option<OwnedSelectionKey> = None;

        let mutable_selection_map = Arc::make_mut(&mut self.selections);
        for entry in mutable_selection_map.values_mut() {
            let key = entry.key().to_owned_key();
            match entry {
                SelectionValue::Field(mut field_selection) => {
                    if field_selection.get().field.is_plain_typename_field()
                        && !is_interface_object
                        && typename_field_key.is_none()
                    {
                        typename_field_key = Some(key);
                    } else if sibling_field_key.is_none() {
                        sibling_field_key = Some(key);
                    }

                    if let Some(field_selection_set) = field_selection.get_selection_set_mut() {
                        field_selection_set
                            .optimize_sibling_typenames(interface_types_with_interface_objects)?;
                    }
                }
                SelectionValue::InlineFragment(mut inline_fragment) => {
                    inline_fragment
                        .get_selection_set_mut()
                        .optimize_sibling_typenames(interface_types_with_interface_objects)?;
                }
            }
        }

        if let (Some(typename_key), Some(sibling_field_key)) =
            (typename_field_key, sibling_field_key)
        {
            if let (
                Some((_, Selection::Field(typename_field))),
                Some(SelectionValue::Field(mut sibling_field)),
            ) = (
                mutable_selection_map.remove(typename_key.as_borrowed_key()),
                mutable_selection_map.get_mut(sibling_field_key.as_borrowed_key()),
            ) {
                // Note that as we tag the element, we also record the alias used if any since that
                // needs to be preserved.
                let sibling_typename = match &typename_field.field.alias {
                    None => SiblingTypename::Unaliased,
                    Some(alias) => SiblingTypename::Aliased(alias.clone()),
                };
                *sibling_field.get_sibling_typename_mut() = Some(sibling_typename);
            } else {
                unreachable!("typename and sibling fields must both exist at this point")
            }
        }
        Ok(())
    }

    pub(crate) fn without_empty_branches(&self) -> Option<Cow<'_, Self>> {
        let filtered = self.filter_recursive_depth_first(&mut |sel| match sel {
            Selection::Field(field) => {
                if let Some(set) = &field.selection_set {
                    !set.is_empty()
                } else {
                    true
                }
            }
            Selection::InlineFragment(inline) => !inline.selection_set.is_empty(),
        });
        if filtered.selections.is_empty() {
            None
        } else {
            Some(filtered)
        }
    }

    pub(crate) fn filter_recursive_depth_first(
        &self,
        predicate: &mut dyn FnMut(&Selection) -> bool,
    ) -> Cow<'_, Self> {
        match self.selections.filter_recursive_depth_first(predicate) {
            Cow::Borrowed(_) => Cow::Borrowed(self),
            Cow::Owned(selections) => Cow::Owned(Self {
                schema: self.schema.clone(),
                type_position: self.type_position.clone(),
                selections: Arc::new(selections),
            }),
        }
    }

    /// Returns the conditions for inclusion of this selection set.
    ///
    /// This tries to be smart about including or excluding the whole selection set.
    /// - If all selections have the same condition, returns that condition.
    /// - If selections in the set have different conditions, the selection set must always be
    ///   included, so the individual selections can be evaluated.
    ///
    /// # Errors
    /// Returns an error if the selection set contains a fragment spread, or if any of the
    /// @skip/@include directives are invalid (per GraphQL validation rules).
    pub(crate) fn conditions(&self) -> Result<Conditions, FederationError> {
        // If the conditions of all the selections within the set are the same,
        // then those are conditions of the whole set and we return it.
        // Otherwise, we just return `true`
        // (which essentially translate to "that selection always need to be queried").
        // Note that for the case where the set has only 1 selection,
        // then this just mean we return the condition of that one selection.
        // Also note that in theory we could be a tad more precise,
        // and when all the selections have variable conditions,
        // we could return the intersection of all of them,
        // but we don't bother for now as that has probably extremely rarely an impact in practice.
        let mut selections = self.selections.values();
        let Some(first_selection) = selections.next() else {
            // we shouldn't really get here for well-formed selection, so whether we return true or false doesn't matter
            // too much, but in principle, if there is no selection, we should be cool not including it.
            return Ok(Conditions::Boolean(false));
        };
        let conditions = first_selection.conditions()?;
        for selection in selections {
            if selection.conditions()? != conditions {
                return Ok(Conditions::Boolean(true));
            }
        }
        Ok(conditions)
    }

    /// Build a selection by merging all items in the given selections (slice).
    /// - Assumes all items in the slice have the same selection key.
    fn make_selection<'a>(
        schema: &ValidFederationSchema,
        parent_type: &CompositeTypeDefinitionPosition,
        selections: impl Iterator<Item = &'a Selection>,
    ) -> Result<Selection, FederationError> {
        let mut iter = selections;
        let Some(first) = iter.next() else {
            // PORT_NOTE: The TypeScript version asserts here.
            return Err(FederationError::internal(
                "Should not be called without any updates",
            ));
        };
        let Some(second) = iter.next() else {
            // Optimize for the simple case of a single selection, as we don't have to do anything
            // complex to merge the sub-selections.
            return first.rebase_on(parent_type, schema);
        };

        let element = first.element().rebase_on(parent_type, schema)?;
        let sub_selection_parent_type: Option<CompositeTypeDefinitionPosition> =
            element.sub_selection_type_position()?;

        let Some(ref sub_selection_parent_type) = sub_selection_parent_type else {
            // This is a leaf, so all updates should correspond ot the same field and we just use the first.
            return Selection::from_element(element, /*sub_selection*/ None);
        };

        // This case has a sub-selection. Merge all sub-selection updates.
        let mut sub_selection_updates: MultiIndexMap<SelectionKey, Selection> =
            MultiIndexMap::new();
        for selection in [first, second].into_iter().chain(iter) {
            if let Some(sub_selection_set) = selection.selection_set() {
                sub_selection_updates.extend(
                    sub_selection_set
                        .selections
                        .values()
                        .map(|v| (v.key(), v.clone())),
                );
            }
        }
        let updated_sub_selection = Some(Self::make_selection_set(
            schema,
            sub_selection_parent_type,
            sub_selection_updates.values().map(|v| v.iter()),
        )?);
        Selection::from_element(element, updated_sub_selection)
    }

    /// Build a selection set by aggregating all items from the `selection_key_groups` iterator.
    /// - Assumes each item (slice) from the iterator has the same selection key within the slice.
    /// - Note that if the same selection key repeats in a later group, the previous group will be
    ///   ignored and replaced by the new group.
    pub(crate) fn make_selection_set<'a>(
        schema: &ValidFederationSchema,
        parent_type: &CompositeTypeDefinitionPosition,
        selection_key_groups: impl Iterator<Item = impl Iterator<Item = &'a Selection>>,
    ) -> Result<SelectionSet, FederationError> {
        selection_key_groups
            .map(|group| Self::make_selection(schema, parent_type, group))
            .try_collect()
            .map(|result| SelectionSet {
                schema: schema.clone(),
                type_position: parent_type.clone(),
                selections: Arc::new(result),
            })
    }

    // PORT_NOTE: Some features of the TypeScript `lazyMap` were not ported:
    // - `parentType` (optional) parameter: This is only used in `SelectionSet.normalize` method,
    //   but its Rust version doesn't use `lazy_map`.
    // - `mapper` may return a `SelectionSet`.
    //   For simplicity, this case was not ported. It was used by `normalize` method in the TypeScript.
    //   But, the Rust version doesn't use `lazy_map`.
    // - `mapper` may return `PathBasedUpdate`.
    //   The `PathBasedUpdate` case is only used in `withFieldAliased` function in the TypeScript
    //   version, but its Rust version doesn't use `lazy_map`.
    // PORT_NOTE #2: Taking ownership of `self` in this method was considered. However, calling
    // `Arc::make_mut` on the `Arc` fields of `self` didn't seem better than cloning Arc instances.
    pub(crate) fn lazy_map(
        &self,
        mut mapper: impl FnMut(&Selection) -> Result<SelectionMapperReturn, FederationError>,
    ) -> Result<SelectionSet, FederationError> {
        let mut iter = self.selections.values();

        // Find the first object that is not identical after mapping
        let Some((index, (_, first_changed))) = iter
            .by_ref()
            .map(|sel| (sel, mapper(sel)))
            .enumerate()
            .find(|(_, (sel, updated))|
                !matches!(&updated, Ok(SelectionMapperReturn::Selection(updated)) if updated == *sel))
        else {
            // All selections are identical after mapping, so just clone `self`.
            return Ok(self.clone());
        };

        // The mapped selection could be an error, so we need to not forget about it.
        let first_changed = first_changed?;
        // Copy the first half of the selections until the `index`-th item, since they are not
        // changed.
        let mut updated_selections = MultiIndexMap::new();
        updated_selections.extend(
            self.selections
                .values()
                .take(index)
                .map(|v| (v.key().to_owned_key(), v.clone())),
        );

        let mut update_new_selection = |selection| match selection {
            SelectionMapperReturn::None => {} // Removed; Skip it.
            SelectionMapperReturn::Selection(new_selection) => {
                updated_selections.insert(new_selection.key().to_owned_key(), new_selection)
            }
            SelectionMapperReturn::SelectionList(new_selections) => updated_selections.extend(
                new_selections
                    .into_iter()
                    .map(|s| (s.key().to_owned_key(), s)),
            ),
        };

        // Now update the rest of the selections using the `mapper` function.
        update_new_selection(first_changed);

        for selection in iter {
            update_new_selection(mapper(selection)?)
        }

        Self::make_selection_set(
            &self.schema,
            &self.type_position,
            updated_selections.values().map(|v| v.iter()),
        )
    }

    pub(crate) fn add_back_typename_in_attachments(&self) -> Result<SelectionSet, FederationError> {
        self.lazy_map(|selection| {
            let selection_element = selection.element();
            let updated = selection
                .map_selection_set(|ss| ss.add_back_typename_in_attachments().map(Some))?;
            let Some(sibling_typename) = selection_element.sibling_typename() else {
                // No sibling typename to add back
                return Ok(updated.into());
            };
            // We need to add the query __typename for the current type in the current group.
            let field_element = Field::new_introspection_typename(
                &self.schema,
                &selection.element().parent_type_position(),
                sibling_typename.alias().cloned(),
            );
            let typename_selection =
                Selection::from_element(field_element.into(), /*subselection*/ None)?;
            Ok([typename_selection, updated].into_iter().collect())
        })
    }

    /// Adds __typename field for selection sets on abstract types.
    ///
    /// __typename is added to the sub selection set of a given selection in following conditions
    /// * if a given selection is a field, we add a __typename sub selection if its selection set type
    ///   position is an abstract type
    /// * if a given selection is a fragment, we only add __typename sub selection if fragment specifies
    ///   type condition and that type condition is an abstract type.
    pub(crate) fn add_typename_field_for_abstract_types(
        &self,
        parent_type_if_abstract: Option<AbstractTypeDefinitionPosition>,
    ) -> Result<SelectionSet, FederationError> {
        let mut selection_map = SelectionMap::new();
        if let Some(parent) = parent_type_if_abstract {
            // We don't handle aliased __typename fields. This means we may end up with additional
            // __typename sub selection. This should be fine though as aliased __typenames should
            // be rare occurrence.
            if !self.has_top_level_typename_field() {
                let typename_selection = Selection::from_field(
                    Field::new_introspection_typename(&self.schema, &parent.into(), None),
                    None,
                );
                selection_map.insert(typename_selection);
            }
        }
        for selection in self.selections.values() {
            selection_map.insert(if let Some(selection_set) = selection.selection_set() {
                let abstract_type = match selection {
                    Selection::Field(field_selection) => field_selection
                        .selection_set
                        .as_ref()
                        .map(|s| s.type_position.clone()),
                    Selection::InlineFragment(inline_fragment_selection) => {
                        inline_fragment_selection
                            .inline_fragment
                            .type_condition_position
                            .clone()
                    }
                }
                .and_then(|ty| ty.try_into().ok());
                let updated_selection_set =
                    selection_set.add_typename_field_for_abstract_types(abstract_type)?;

                if updated_selection_set == *selection_set {
                    selection.clone()
                } else {
                    selection.with_updated_selection_set(Some(updated_selection_set))?
                }
            } else {
                selection.clone()
            });
        }

        Ok(SelectionSet {
            schema: self.schema.clone(),
            type_position: self.type_position.clone(),
            selections: Arc::new(selection_map),
        })
    }

    fn has_top_level_typename_field(&self) -> bool {
        const TYPENAME_KEY: SelectionKey = SelectionKey::Field {
            response_name: &TYPENAME_FIELD,
            directives: &DirectiveList::new(),
        };

        self.selections.contains_key(TYPENAME_KEY)
    }

    /// Adds a path, and optional some selections following that path, to this selection map.
    ///
    /// Today, it is possible here to add conflicting paths, such as:
    /// - `add_at_path("field1(arg: 1)")`
    /// - `add_at_path("field1(arg: 2)")`
    ///
    /// Users of this method should guarantee that this doesn't happen. Otherwise, converting this
    /// SelectionSet back to an ExecutableDocument will return a validation error.
    ///
    /// The final selections are optional. If `path` ends on a leaf field, then no followup
    /// selections would make sense.
    /// When final selections are provided, unnecessary fragments will be automatically removed
    /// at the junction between the path and those final selections.
    ///
    /// For instance, suppose that we have:
    ///  - a `path` argument that is `a::b::c`,
    ///    where the type of the last field `c` is some object type `C`.
    ///  - a `selections` argument that is `{ ... on C { d } }`.
    ///
    /// Then the resulting built selection set will be: `{ a { b { c { d } } }`,
    /// and in particular the `... on C` fragment will be eliminated since it is unecesasry
    /// (since again, `c` is of type `C`).
    // Notes on NamedFragments argument: `add_at_path` only deals with expanded operations, so
    // the NamedFragments argument to `rebase_on` is not needed (passing the default value).
    pub(crate) fn add_at_path(
        &mut self,
        path: &[Arc<OpPathElement>],
        selection_set: Option<&Arc<SelectionSet>>,
    ) -> Result<(), FederationError> {
        // PORT_NOTE: This method was ported from the JS class `SelectionSetUpdates`. Unlike the
        // JS code, this mutates the selection set map in-place.
        match path.split_first() {
            // If we have a sub-path, recurse.
            Some((ele, path @ &[_, ..])) => {
                let element = ele.rebase_on(&self.type_position, &self.schema)?;
                let Some(sub_selection_type) = element.sub_selection_type_position()? else {
                    return Err(FederationError::internal("unexpected error: add_at_path encountered a field that is not of a composite type".to_string()));
                };
                let element_key = element.key().to_owned_key();
                let mut selection = Arc::make_mut(&mut self.selections)
                    .entry(element_key.as_borrowed_key())
                    .or_insert(|| {
                        Selection::from_element(
                            element,
                            // We immediately add a selection afterward to make this selection set
                            // valid.
                            Some(SelectionSet::empty(self.schema.clone(), sub_selection_type)),
                        )
                    })?;
                match &mut selection {
                    SelectionValue::Field(field) => match field.get_selection_set_mut() {
                        Some(sub_selection) => sub_selection.add_at_path(path, selection_set)?,
                        None => return Err(FederationError::internal("add_at_path encountered a field without a subselection which should never happen".to_string())),
                    },
                    SelectionValue::InlineFragment(fragment) => fragment
                        .get_selection_set_mut()
                        .add_at_path(path, selection_set)?,
                };
            }
            // If we have no sub-path, we can add the selection.
            Some((ele, &[])) => {
                // PORT_NOTE: The JS code waited until the final selection was being constructed to
                // turn the path and selection set into a selection. Because we are mutating things
                // in-place, we eagerly construct the selection that needs to be rebased on the target
                // schema.
                let element = ele.rebase_on(&self.type_position, &self.schema)?;
                if selection_set.is_none() || selection_set.is_some_and(|s| s.is_empty()) {
                    // This is a somewhat common case when dealing with `@key` "conditions" that we can
                    // end up with trying to add empty sub selection set on a non-leaf node. There is
                    // nothing to do here - we know will have a node at specified path but currently
                    // we don't have any sub selections so there is nothing to merge.
                    // JS code was doing this check in `makeSelectionSet`
                    if !ele.is_terminal()? {
                        return Ok(());
                    } else {
                        // add leaf
                        let selection = Selection::from_element(element, None)?;
                        self.add_local_selection(&selection)?
                    }
                } else {
                    let sub_selection_type_pos = element.sub_selection_type_position()?.ok_or_else(|| {
                        FederationError::internal("unexpected: Element has a selection set with non-composite base type")
                    })?;
                    let selection_set = selection_set
                        .map(|selection_set| {
                            let selections = selection_set.without_unnecessary_fragments(
                                &sub_selection_type_pos,
                                &self.schema,
                            );
                            let mut selection_set = SelectionSet::empty(
                                self.schema.clone(),
                                sub_selection_type_pos.clone(),
                            );
                            for selection in selections.iter() {
                                selection_set.add_local_selection(
                                    &selection.rebase_on(&sub_selection_type_pos, &self.schema)?,
                                )?;
                            }
                            Ok::<_, FederationError>(selection_set)
                        })
                        .transpose()?;
                    let selection = Selection::from_element(element, selection_set)?;
                    self.add_local_selection(&selection)?
                }
            }
            // If we don't have any path, we rebase and merge in the given sub selections at the root.
            None => {
                if let Some(sel) = selection_set {
                    self.add_selection_set(sel)?
                }
            }
        }
        Ok(())
    }

    // - `self` must be fragment-spread-free.
    pub(crate) fn add_aliases_for_non_merging_fields(
        &self,
    ) -> Result<(SelectionSet, Vec<Arc<FetchDataRewrite>>), FederationError> {
        let mut aliases = Vec::new();
        compute_aliases_for_non_merging_fields(
            vec![SelectionSetAtPath {
                path: Vec::new(),
                selections: Some(self.clone()),
            }],
            &mut aliases,
            &self.schema,
        )?;

        let updated = self.with_field_aliased(&aliases)?;
        let output_rewrites = aliases
            .into_iter()
            .map(
                |FieldToAlias {
                     mut path,
                     response_name,
                     alias,
                 }| {
                    path.push(FetchDataPathElement::Key(alias, Default::default()));
                    Arc::new(FetchDataRewrite::KeyRenamer(FetchDataKeyRenamer {
                        path,
                        rename_key_to: response_name,
                    }))
                },
            )
            .collect::<Vec<_>>();

        Ok((updated, output_rewrites))
    }

    pub(crate) fn with_field_aliased(
        &self,
        aliases: &[FieldToAlias],
    ) -> Result<SelectionSet, FederationError> {
        if aliases.is_empty() {
            return Ok(self.clone());
        }

        let mut at_current_level: IndexMap<FetchDataPathElement, &FieldToAlias> =
            IndexMap::default();
        let mut remaining: Vec<&FieldToAlias> = Vec::new();

        for alias in aliases {
            if !alias.path.is_empty() {
                remaining.push(alias);
            } else {
                at_current_level.insert(
                    FetchDataPathElement::Key(alias.response_name.clone(), Default::default()),
                    alias,
                );
            }
        }

        let mut selection_map = SelectionMap::new();
        for selection in self.selections.values() {
            let path_element = selection.element().as_path_element();
            let subselection_aliases = remaining
                .iter()
                .filter_map(|alias| {
                    if alias.path.first() == path_element.as_ref() {
                        Some(FieldToAlias {
                            path: alias.path[1..].to_vec(),
                            response_name: alias.response_name.clone(),
                            alias: alias.alias.clone(),
                        })
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>();
            let selection_set = selection.selection_set();
            let updated_selection_set = selection_set
                .map(|selection_set| selection_set.with_field_aliased(&subselection_aliases))
                .transpose()?;

            match selection {
                Selection::Field(field) => {
                    let alias = path_element.and_then(|elem| at_current_level.get(&elem));
                    if alias.is_none() && selection_set == updated_selection_set.as_ref() {
                        selection_map.insert(selection.clone());
                    } else {
                        let updated_field = match alias {
                            Some(alias) => field.field.with_updated_alias(alias.alias.clone()),
                            None => field.field.clone(),
                        };
                        selection_map
                            .insert(Selection::from_field(updated_field, updated_selection_set));
                    }
                }
                Selection::InlineFragment(_) => {
                    if selection_set == updated_selection_set.as_ref() {
                        selection_map.insert(selection.clone());
                    } else {
                        selection_map
                            .insert(selection.with_updated_selection_set(updated_selection_set)?);
                    }
                }
            }
        }

        Ok(SelectionSet {
            schema: self.schema.clone(),
            type_position: self.type_position.clone(),
            selections: Arc::new(selection_map),
        })
    }

    /// # Preconditions
    /// The selection set must not contain named fragment spreads.
    fn fields_in_set(&self) -> Vec<CollectedFieldInSet> {
        let mut fields = Vec::new();

        for selection in self.selections.values() {
            match selection {
                Selection::Field(field) => fields.push(CollectedFieldInSet {
                    path: Vec::new(),
                    field: field.clone(),
                }),
                Selection::InlineFragment(inline_fragment) => {
                    let condition = inline_fragment
                        .inline_fragment
                        .type_condition_position
                        .as_ref();
                    let header = match condition {
                        Some(cond) => vec![FetchDataPathElement::TypenameEquals(
                            cond.type_name().clone(),
                        )],
                        None => vec![],
                    };
                    for CollectedFieldInSet { path, field } in
                        inline_fragment.selection_set.fields_in_set().into_iter()
                    {
                        let mut new_path = header.clone();
                        new_path.extend(path);
                        fields.push(CollectedFieldInSet {
                            path: new_path,
                            field,
                        })
                    }
                }
            }
        }
        fields
    }

    pub(crate) fn validate(
        &self,
        _variable_definitions: &[Node<executable::VariableDefinition>],
    ) -> Result<(), FederationError> {
        if self.selections.is_empty() {
            Err(FederationError::internal("Invalid empty selection set"))
        } else {
            self.selections
                .values()
                .filter_map(|selection| selection.selection_set())
                .try_for_each(|s| s.validate(_variable_definitions))
        }
    }

    /// Using path-based updates along with selection sets may result in some inefficiencies.
    /// Specifically, we may end up with some unnecessary top-level inline fragment selections, i.e.
    /// fragments without any directives and with the type condition equal to (or a supertype of)
    /// the parent type of the fragment. This method inlines those unnecessary top-level fragments.
    ///
    /// JS PORT NOTE: In Rust implementation we are doing the selection set updates in-place whereas
    /// JS code was pooling the updates and only apply those when building the final selection set.
    /// See `makeSelectionSet` method for details.
    fn without_unnecessary_fragments(
        &self,
        parent_type: &CompositeTypeDefinitionPosition,
        schema: &ValidFederationSchema,
    ) -> Vec<Selection> {
        let mut final_selections = vec![];
        fn process_selection_set(
            selection_set: &SelectionSet,
            final_selections: &mut Vec<Selection>,
            parent_type: &CompositeTypeDefinitionPosition,
            schema: &ValidFederationSchema,
        ) {
            for selection in selection_set.selections.values() {
                match selection {
                    Selection::InlineFragment(inline_fragment) => {
                        if inline_fragment.is_unnecessary(parent_type, schema) {
                            process_selection_set(
                                &inline_fragment.selection_set,
                                final_selections,
                                parent_type,
                                schema,
                            );
                        } else {
                            final_selections.push(selection.clone());
                        }
                    }
                    _ => {
                        final_selections.push(selection.clone());
                    }
                }
            }
        }
        process_selection_set(self, &mut final_selections, parent_type, schema);

        final_selections
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &Selection> {
        self.selections.values()
    }

    /// Returns true if any elements in this selection set or its descendants returns true for the
    /// given predicate. Fragment spread selections are converted to inline fragment elements, and
    /// their fragment selection sets are recursed into. Note this method is short-circuiting.
    // PORT_NOTE: The JS codebase calls this "some()", but that's easy to confuse with "Some" in
    // Rust.
    pub(crate) fn any_element(&self, predicate: &mut impl FnMut(OpPathElement) -> bool) -> bool {
        self.selections
            .values()
            .any(|selection| selection.any_element(predicate))
    }
}

impl IntoIterator for SelectionSet {
    type Item = Selection;
    type IntoIter = selection_map::IntoValues;

    fn into_iter(self) -> Self::IntoIter {
        Arc::unwrap_or_clone(self.selections).into_values()
    }
}

impl<'a> IntoIterator for &'a SelectionSet {
    type Item = &'a Selection;
    type IntoIter = selection_map::Values<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.selections.values()
    }
}

#[derive(Clone, Debug)]
pub(crate) struct SelectionSetAtPath {
    path: Vec<FetchDataPathElement>,
    selections: Option<SelectionSet>,
}

pub(crate) struct FieldToAlias {
    path: Vec<FetchDataPathElement>,
    response_name: Name,
    alias: Name,
}

pub(crate) struct SeenResponseName {
    field_name: Name,
    field_type: executable::Type,
    selections: Option<Vec<SelectionSetAtPath>>,
}

pub(crate) struct CollectedFieldInSet {
    path: Vec<FetchDataPathElement>,
    field: Arc<FieldSelection>,
}

struct FieldInPath {
    path: Vec<FetchDataPathElement>,
    field: Arc<FieldSelection>,
}

// - `selections` must be fragment-spread-free.
fn compute_aliases_for_non_merging_fields(
    selections: Vec<SelectionSetAtPath>,
    alias_collector: &mut Vec<FieldToAlias>,
    schema: &ValidFederationSchema,
) -> Result<(), FederationError> {
    let mut seen_response_names: IndexMap<Name, SeenResponseName> = IndexMap::default();

    // - `s.selections` must be fragment-spread-free.
    fn rebased_fields_in_set(s: &SelectionSetAtPath) -> impl Iterator<Item = FieldInPath> {
        s.selections.iter().flat_map(|s2| {
            s2.fields_in_set()
                .into_iter()
                .map(|CollectedFieldInSet { path, field }| {
                    let mut new_path = s.path.clone();
                    new_path.extend(path);
                    FieldInPath {
                        path: new_path,
                        field,
                    }
                })
        })
    }

    for FieldInPath { mut path, field } in selections.iter().flat_map(rebased_fields_in_set) {
        let field_schema = field.field.schema().schema();
        let field_name = field.field.name();
        let response_name = field.field.response_name();
        let field_type = &field.field.field_position.get(field_schema)?.ty;

        match seen_response_names.get(response_name) {
            Some(previous) => {
                if &previous.field_name == field_name
                    && types_can_be_merged(&previous.field_type, field_type, schema.schema())?
                {
                    let output_type = schema.get_type(field_type.inner_named_type().clone())?;
                    // If the type is non-composite, then we're all set. But if it is composite, we need to record the sub-selection to that response name
                    // as we need to "recurse" on the merged of both the previous and this new field.
                    if output_type.is_composite_type() {
                        match &previous.selections {
                            None => {
                                return Err(SingleFederationError::Internal {
                                    message: format!(
                                        "Should have added selections for `'{:?}\'",
                                        previous.field_type
                                    ),
                                }
                                .into());
                            }
                            Some(s) => {
                                let mut selections = s.clone();
                                let mut p = path.clone();
                                p.push(FetchDataPathElement::Key(
                                    response_name.clone(),
                                    Default::default(),
                                ));
                                selections.push(SelectionSetAtPath {
                                    path: p,
                                    selections: field.selection_set.clone(),
                                });
                                seen_response_names.insert(
                                    response_name.clone(),
                                    SeenResponseName {
                                        field_name: previous.field_name.clone(),
                                        field_type: previous.field_type.clone(),
                                        selections: Some(selections),
                                    },
                                )
                            }
                        };
                    }
                } else {
                    // We need to alias the new occurrence.
                    let alias = gen_alias_name(response_name, &seen_response_names);

                    // Given how we generate aliases, it's is very unlikely that the generated alias will conflict with any of the other response name
                    // at the level, but it's theoretically possible. By adding the alias to the seen names, we ensure that in the remote change that
                    // this ever happen, we'll avoid the conflict by giving another alias to the followup occurrence.
                    let selections = match field.selection_set.as_ref() {
                        Some(s) => {
                            let mut p = path.clone();
                            p.push(FetchDataPathElement::Key(alias.clone(), Default::default()));
                            Some(vec![SelectionSetAtPath {
                                path: p,
                                selections: Some(s.clone()),
                            }])
                        }
                        None => None,
                    };

                    seen_response_names.insert(
                        alias.clone(),
                        SeenResponseName {
                            field_name: field_name.clone(),
                            field_type: field_type.clone(),
                            selections,
                        },
                    );

                    // Lastly, we record that the added alias need to be rewritten back to the proper response name post query.

                    alias_collector.push(FieldToAlias {
                        path,
                        response_name: response_name.clone(),
                        alias,
                    })
                }
            }
            None => {
                let selections: Option<Vec<SelectionSetAtPath>> = match field.selection_set.as_ref()
                {
                    Some(s) => {
                        path.push(FetchDataPathElement::Key(
                            response_name.clone(),
                            Default::default(),
                        ));
                        Some(vec![SelectionSetAtPath {
                            path,
                            selections: Some(s.clone()),
                        }])
                    }
                    None => None,
                };
                seen_response_names.insert(
                    response_name.clone(),
                    SeenResponseName {
                        field_name: field_name.clone(),
                        field_type: field_type.clone(),
                        selections,
                    },
                );
            }
        }
    }

    seen_response_names
        .into_values()
        .filter_map(|selections| selections.selections)
        .try_for_each(|selections| {
            compute_aliases_for_non_merging_fields(selections, alias_collector, schema)
        })
}

fn gen_alias_name(base_name: &Name, unavailable_names: &IndexMap<Name, SeenResponseName>) -> Name {
    let mut counter = 0usize;
    loop {
        if let Ok(name) = Name::try_from(format!("{base_name}__alias_{counter}")) {
            if !unavailable_names.contains_key(&name) {
                return name;
            }
        }
        counter += 1;
    }
}

impl Field {
    fn with_updated_position(
        &self,
        schema: ValidFederationSchema,
        field_position: FieldDefinitionPosition,
    ) -> Self {
        Self {
            schema,
            field_position,
            ..self.clone()
        }
    }
}

impl FieldSelection {
    /// Normalize this field selection (merging selections with the same keys), with the following
    /// additional transformations:
    /// - Expand fragment spreads into inline fragments.
    /// - Remove `__schema` or `__type` introspection fields, as these shouldn't be handled by query
    ///   planning.
    /// - Hoist fragment spreads/inline fragments into their parents if they have no directives and
    ///   their parent type matches.
    pub(crate) fn from_field(
        field: &executable::Field,
        parent_type_position: &CompositeTypeDefinitionPosition,
        fragments_cache: &FragmentSpreadCache,
        schema: &ValidFederationSchema,
        check_cancellation: &dyn Fn() -> Result<(), SingleFederationError>,
    ) -> Result<Option<FieldSelection>, FederationError> {
        // Skip __schema/__type introspection fields as router takes care of those, and they do not
        // need to be query planned.
        if field.name == "__schema" || field.name == "__type" {
            return Ok(None);
        }
        let field_position = parent_type_position.field(field.name.clone())?;
        // We might be able to validate that the returned `FieldDefinition` matches that within
        // the given `field`, but on the off-chance there's a mutation somewhere in between
        // Operation creation and the creation of the ValidFederationSchema, it's safer to just
        // confirm it exists in this schema.
        field_position.get(schema.schema())?;
        let is_composite = CompositeTypeDefinitionPosition::try_from(
            schema.get_type(field.selection_set.ty.clone())?,
        )
        .is_ok();

        Ok(Some(FieldSelection {
            field: Field {
                schema: schema.clone(),
                field_position,
                alias: field.alias.clone(),
                arguments: field.arguments.clone().into(),
                directives: field.directives.clone().into(),
                sibling_typename: None,
            },
            selection_set: if is_composite {
                Some(SelectionSet::from_selection_set(
                    &field.selection_set,
                    fragments_cache,
                    schema,
                    check_cancellation,
                )?)
            } else {
                None
            },
        }))
    }

    fn with_updated_element(&self, field: Field) -> Self {
        Self {
            field,
            selection_set: self.selection_set.clone(),
        }
    }

    pub(crate) fn any_element(&self, predicate: &mut impl FnMut(OpPathElement) -> bool) -> bool {
        if predicate(self.field.clone().into()) {
            return true;
        }
        if let Some(selection_set) = &self.selection_set {
            if selection_set.any_element(predicate) {
                return true;
            }
        }
        false
    }
}

impl Field {
    pub(crate) fn parent_type_position(&self) -> CompositeTypeDefinitionPosition {
        self.field_position.parent()
    }
}

impl InlineFragmentSelection {
    pub(crate) fn new(inline_fragment: InlineFragment, selection_set: SelectionSet) -> Self {
        debug_assert_eq!(
            inline_fragment.casted_type(),
            selection_set.type_position,
            "Inline fragment type condition and its selection set should point to the same type position",
        );
        debug_assert_eq!(
            inline_fragment.schema, selection_set.schema,
            "Inline fragment and its selection set should point to the same schema",
        );
        Self {
            inline_fragment,
            selection_set,
        }
    }

    /// Normalize this inline fragment selection (merging selections with the same keys), with the
    /// following additional transformations:
    /// - Expand fragment spreads into inline fragments.
    /// - Remove `__schema` or `__type` introspection fields, as these shouldn't be handled by query
    ///   planning.
    /// - Hoist fragment spreads/inline fragments into their parents if they have no directives and
    ///   their parent type matches.
    pub(crate) fn from_inline_fragment(
        inline_fragment: &executable::InlineFragment,
        parent_type_position: &CompositeTypeDefinitionPosition,
        fragments_cache: &FragmentSpreadCache,
        schema: &ValidFederationSchema,
        check_cancellation: &dyn Fn() -> Result<(), SingleFederationError>,
    ) -> Result<InlineFragmentSelection, FederationError> {
        let type_condition_position: Option<CompositeTypeDefinitionPosition> =
            if let Some(type_condition) = &inline_fragment.type_condition {
                Some(schema.get_type(type_condition.clone())?.try_into()?)
            } else {
                None
            };
        let new_selection_set = SelectionSet::from_selection_set(
            &inline_fragment.selection_set,
            fragments_cache,
            schema,
            check_cancellation,
        )?;
        let new_inline_fragment = InlineFragment {
            schema: schema.clone(),
            parent_type_position: parent_type_position.clone(),
            type_condition_position,
            directives: inline_fragment.directives.clone().into(),
            selection_id: SelectionId::new(),
        };
        Ok(InlineFragmentSelection::new(
            new_inline_fragment,
            new_selection_set,
        ))
    }

    pub(crate) fn from_fragment_spread(
        parent_type_position: &CompositeTypeDefinitionPosition,
        fragment_spread: &executable::FragmentSpread,
        fragments_cache: &FragmentSpreadCache,
        schema: &ValidFederationSchema,
    ) -> Result<InlineFragmentSelection, FederationError> {
        let valid_schema = schema.schema();
        // verify fragment exists
        let Some(fragment_selection) = fragments_cache.get(&fragment_spread.fragment_name) else {
            return Err(SingleFederationError::Internal {
                message: format!(
                    "Fragment spread referenced non-existent fragment \"{}\"",
                    fragment_spread.fragment_name,
                ),
            }
            .into());
        };

        // verify fragment spread directives can be applied on inline fragments
        for directive in fragment_spread.directives.iter() {
            let Some(definition) = valid_schema.directive_definitions.get(&directive.name) else {
                return Err(FederationError::internal(format!(
                    "Undefined directive {}",
                    directive.name
                )));
            };
            if !definition
                .locations
                .contains(&apollo_compiler::schema::DirectiveLocation::InlineFragment)
            {
                return Err(SingleFederationError::UnsupportedSpreadDirective {
                    name: directive.name.clone(),
                }
                .into());
            }
        }

        // Note: We assume that fragment.type_condition() is the same as fragment.selection_set.ty.
        Ok(InlineFragmentSelection::new(
            InlineFragment {
                schema: schema.clone(),
                parent_type_position: parent_type_position.clone(),
                type_condition_position: Some(fragment_selection.type_position.clone()),
                directives: fragment_spread.directives.clone().into(),
                selection_id: SelectionId::new(),
            },
            fragment_selection.clone(),
        ))
    }

    pub(crate) fn casted_type(&self) -> &CompositeTypeDefinitionPosition {
        self.inline_fragment
            .type_condition_position
            .as_ref()
            .unwrap_or(&self.inline_fragment.parent_type_position)
    }

    /// Returns true if this inline fragment selection is "unnecessary" and should be inlined.
    ///
    /// Fragment is unnecessary if following are true:
    /// * it has no applied directives
    /// * has no type condition OR type condition is equal to (or a supertype of) `parent`
    fn is_unnecessary(
        &self,
        parent: &CompositeTypeDefinitionPosition,
        schema: &ValidFederationSchema,
    ) -> bool {
        if !self.inline_fragment.directives.is_empty() {
            return false;
        }
        let Some(type_condition) = &self.inline_fragment.type_condition_position else {
            return true;
        };
        type_condition.type_name() == parent.type_name()
            || schema
                .schema()
                .is_subtype(type_condition.type_name(), parent.type_name())
    }

    pub(crate) fn any_element(&self, predicate: &mut impl FnMut(OpPathElement) -> bool) -> bool {
        if predicate(self.inline_fragment.clone().into()) {
            return true;
        }
        self.selection_set.any_element(predicate)
    }
}

// @defer handling: removing and normalization

const DEFER_DIRECTIVE_NAME: Name = name!("defer");
const DEFER_LABEL_ARGUMENT_NAME: Name = name!("label");
const DEFER_IF_ARGUMENT_NAME: Name = name!("if");

pub(crate) struct NormalizedDefer {
    /// The operation modified to normalize @defer applications.
    pub(crate) operation: Operation,
    /// True if the operation contains any @defer applications.
    pub(crate) has_defers: bool,
    /// `@defer(label:)` values assigned by normalization.
    pub(crate) assigned_defer_labels: IndexSet<String>,
    /// Map of variable conditions to the @defer labels depending on those conditions.
    pub(crate) defer_conditions: IndexMap<Name, IndexSet<String>>,
}

struct DeferNormalizer {
    used_labels: IndexSet<String>,
    assigned_labels: IndexSet<String>,
    conditions: IndexMap<Name, IndexSet<String>>,
    label_offset: usize,
}

impl DeferNormalizer {
    fn new(selection_set: &SelectionSet) -> Result<Self, FederationError> {
        let mut digest = Self {
            used_labels: IndexSet::default(),
            label_offset: 0,
            assigned_labels: IndexSet::default(),
            conditions: IndexMap::default(),
        };
        let mut stack = selection_set.into_iter().collect::<Vec<_>>();
        while let Some(selection) = stack.pop() {
            if let Selection::InlineFragment(inline) = selection {
                if let Some(args) = inline.inline_fragment.defer_directive_arguments()? {
                    let DeferDirectiveArguments { label, if_: _ } = args;
                    if let Some(label) = label {
                        digest.used_labels.insert(label);
                    }
                }
            }
            stack.extend(selection.selection_set().into_iter().flatten());
        }
        Ok(digest)
    }

    fn get_label(&mut self) -> String {
        loop {
            let digest = format!("qp__{}", self.label_offset);
            self.label_offset += 1;
            if !self.used_labels.contains(&digest) {
                self.assigned_labels.insert(digest.clone());
                return digest;
            }
        }
    }

    fn register_condition(&mut self, label: String, cond: Name) {
        self.conditions.entry(cond).or_default().insert(label);
    }
}

impl FieldSelection {
    /// Returns true if the selection or any of its subselections uses the @defer directive.
    fn has_defer(&self) -> bool {
        // Fields don't have @defer, so we only check the subselection.
        self.selection_set.as_ref().is_some_and(|s| s.has_defer())
    }
}

impl InlineFragment {
    /// Returns true if the fragment has a @defer directive.
    fn has_defer(&self) -> bool {
        self.directives.has(&DEFER_DIRECTIVE_NAME)
    }

    /// Create a new inline fragment without @defer directive applications that have a matching label.
    fn reduce_defer(&self, defer_labels: &IndexSet<String>) -> Result<Self, FederationError> {
        let mut reduce_defer = self.clone();
        reduce_defer.directives.remove_defer(defer_labels);
        Ok(reduce_defer)
    }
}

impl InlineFragmentSelection {
    /// Returns true if the selection or any of its subselections uses the @defer directive.
    fn has_defer(&self) -> bool {
        self.inline_fragment.has_defer()
            || self
                .selection_set
                .selections
                .values()
                .any(|s| s.has_defer())
    }

    fn normalize_defer(self, normalizer: &mut DeferNormalizer) -> Result<Self, FederationError> {
        // This should always be `Some`
        let Some(args) = self.inline_fragment.defer_directive_arguments()? else {
            return Ok(self);
        };

        let mut remove_defer = false;
        #[expect(clippy::redundant_clone)]
        let mut args_copy = args.clone();
        if let Some(BooleanOrVariable::Boolean(b)) = &args.if_ {
            if *b {
                args_copy.if_ = None;
            } else {
                remove_defer = true;
            }
        }

        if args_copy.label.is_none() {
            args_copy.label = Some(normalizer.get_label());
        }

        if remove_defer {
            let directives: DirectiveList = self
                .inline_fragment
                .directives
                .iter()
                .filter(|dir| dir.name != "defer")
                .cloned()
                .collect();
            return Ok(self.with_updated_directives(directives));
        }

        // NOTE: If this is `Some`, it will be a variable.
        if let Some(BooleanOrVariable::Variable(cond)) = args_copy.if_.clone() {
            normalizer.register_condition(args_copy.label.clone().unwrap(), cond);
        }

        if args_copy == args {
            Ok(self)
        } else {
            let directives: DirectiveList = self
                .inline_fragment
                .directives
                .iter()
                .map(|dir| {
                    if dir.name == "defer" {
                        let mut dir: Directive = (**dir).clone();
                        dir.arguments.retain(|arg| {
                            ![DEFER_LABEL_ARGUMENT_NAME, DEFER_IF_ARGUMENT_NAME].contains(&arg.name)
                        });
                        dir.arguments.push(
                            (DEFER_LABEL_ARGUMENT_NAME, args_copy.label.clone().unwrap()).into(),
                        );
                        if let Some(cond) = args_copy.if_.clone() {
                            dir.arguments.push((DEFER_IF_ARGUMENT_NAME, cond).into());
                        }
                        Node::new(dir)
                    } else {
                        dir.clone()
                    }
                })
                .collect();
            Ok(self.with_updated_directives(directives))
        }
    }
}

impl Selection {
    /// Returns true if the selection or any of its subselections uses the @defer directive.
    pub(crate) fn has_defer(&self) -> bool {
        match self {
            Selection::Field(field_selection) => field_selection.has_defer(),
            Selection::InlineFragment(inline_fragment_selection) => {
                inline_fragment_selection.has_defer()
            }
        }
    }

    /// Create a new selection without @defer directive applications that have a matching label.
    fn reduce_defer(&self, defer_labels: &IndexSet<String>) -> Result<Self, FederationError> {
        match self {
            Selection::Field(field) => {
                let Some(selection_set) = field
                    .selection_set
                    .as_ref()
                    .filter(|selection_set| selection_set.has_defer())
                else {
                    return Ok(Selection::Field(Arc::clone(field)));
                };

                Ok(field
                    .with_updated_selection_set(Some(selection_set.reduce_defer(defer_labels)?))
                    .into())
            }
            Selection::InlineFragment(frag) => {
                let inline_fragment = frag.inline_fragment.reduce_defer(defer_labels)?;
                let selection_set = frag.selection_set.reduce_defer(defer_labels)?;
                Ok(InlineFragmentSelection::new(inline_fragment, selection_set).into())
            }
        }
    }

    fn normalize_defer(self, normalizer: &mut DeferNormalizer) -> Result<Self, FederationError> {
        match self {
            Selection::Field(field) => Ok(Self::Field(Arc::new(
                field.with_updated_selection_set(
                    field
                        .selection_set
                        .clone()
                        .map(|set| set.normalize_defer(normalizer))
                        .transpose()?,
                ),
            ))),
            Selection::InlineFragment(inline) => inline
                .with_updated_selection_set(
                    inline.selection_set.clone().normalize_defer(normalizer)?,
                )
                .normalize_defer(normalizer)
                .map(|inline| Self::InlineFragment(Arc::new(inline))),
        }
    }
}

impl SelectionSet {
    /// Create a new selection set without @defer directive applications that have a matching label.
    fn reduce_defer(&self, defer_labels: &IndexSet<String>) -> Result<Self, FederationError> {
        let mut reduce_defer = SelectionSet::empty(self.schema.clone(), self.type_position.clone());
        for selection in self.selections.values() {
            reduce_defer.add_local_selection(&selection.reduce_defer(defer_labels)?)?;
        }
        Ok(reduce_defer)
    }

    fn has_defer(&self) -> bool {
        self.selections.values().any(|s| s.has_defer())
    }

    fn normalize_defer(self, normalizer: &mut DeferNormalizer) -> Result<Self, FederationError> {
        let Self {
            schema,
            type_position,
            selections,
        } = self;
        Arc::unwrap_or_clone(selections)
            .into_values()
            .map(|sel| sel.normalize_defer(normalizer))
            .try_collect()
            .map(|selections| Self {
                schema,
                type_position,
                selections: Arc::new(selections),
            })
    }
}

impl Operation {
    fn has_defer(&self) -> bool {
        self.selection_set.has_defer()
    }

    /// Create a new operation without specific @defer(label:) directive applications.
    pub(crate) fn reduce_defer(
        mut self,
        labels: &IndexSet<String>,
    ) -> Result<Self, FederationError> {
        if self.has_defer() {
            self.selection_set = self.selection_set.reduce_defer(labels)?;
        }
        Ok(self)
    }

    /// Returns this operation but modified to "normalize" all the @defer applications.
    ///
    /// "Normalized" in this context means that all the `@defer` application in the resulting
    /// operation will:
    ///  - have a (unique) label. Which implies that this method generates a label for any `@defer`
    ///    not having a label.
    ///  - have a non-trivial `if` condition, if any. By non-trivial, we mean that the condition
    ///    will be a variable and not an hard-coded `true` or `false`. To do this, this method will
    ///    remove the condition of any `@defer` that has `if: true`, and will completely remove any
    ///    `@defer` application that has `if: false`.
    ///
    /// Defer normalization does not support named fragment definitions, so it must only be called
    /// if the operation had its fragments expanded. In effect, it means that this method may
    /// modify the operation in a way that prevents fragments from being reused in
    /// `.reuse_fragments()`.
    pub(crate) fn with_normalized_defer(mut self) -> Result<NormalizedDefer, FederationError> {
        if self.has_defer() {
            let mut normalizer = DeferNormalizer::new(&self.selection_set)?;
            self.selection_set = self.selection_set.normalize_defer(&mut normalizer)?;
            Ok(NormalizedDefer {
                operation: self,
                has_defers: true,
                assigned_defer_labels: normalizer.assigned_labels,
                defer_conditions: normalizer.conditions,
            })
        } else {
            Ok(NormalizedDefer {
                operation: self,
                has_defers: false,
                assigned_defer_labels: IndexSet::default(),
                defer_conditions: IndexMap::default(),
            })
        }
    }
}

// Collect used variables from operation types.

pub(crate) struct VariableCollector<'s> {
    variables: IndexSet<&'s Name>,
}

impl<'s> VariableCollector<'s> {
    pub(crate) fn new() -> Self {
        Self {
            variables: Default::default(),
        }
    }

    fn visit_value(&mut self, value: &'s executable::Value) {
        match value {
            executable::Value::Variable(v) => {
                self.variables.insert(v);
            }
            executable::Value::List(list) => {
                for value in list {
                    self.visit_value(value);
                }
            }
            executable::Value::Object(object) => {
                for (_key, value) in object {
                    self.visit_value(value);
                }
            }
            _ => {}
        }
    }

    fn visit_directive(&mut self, directive: &'s Directive) {
        for arg in directive.arguments.iter() {
            self.visit_value(&arg.value);
        }
    }

    pub(crate) fn visit_directive_list(&mut self, directives: &'s executable::DirectiveList) {
        for dir in directives.iter() {
            self.visit_directive(dir);
        }
    }

    fn visit_field(&mut self, field: &'s Field) {
        for arg in field.arguments.iter() {
            self.visit_value(&arg.value);
        }
        self.visit_directive_list(&field.directives);
    }

    fn visit_field_selection(&mut self, selection: &'s FieldSelection) {
        self.visit_field(&selection.field);
        if let Some(set) = &selection.selection_set {
            self.visit_selection_set(set);
        }
    }

    fn visit_inline_fragment(&mut self, fragment: &'s InlineFragment) {
        self.visit_directive_list(&fragment.directives);
    }

    fn visit_inline_fragment_selection(&mut self, selection: &'s InlineFragmentSelection) {
        self.visit_inline_fragment(&selection.inline_fragment);
        self.visit_selection_set(&selection.selection_set);
    }

    fn visit_selection(&mut self, selection: &'s Selection) {
        match selection {
            Selection::Field(field) => self.visit_field_selection(field),
            Selection::InlineFragment(frag) => self.visit_inline_fragment_selection(frag),
        }
    }

    pub(crate) fn visit_selection_set(&mut self, selection_set: &'s SelectionSet) {
        for selection in selection_set.iter() {
            self.visit_selection(selection);
        }
    }

    /// Consume the collector and return the collected names.
    pub(crate) fn into_inner(self) -> IndexSet<&'s Name> {
        self.variables
    }
}

impl SelectionSet {
    /// Returns the variable names that are used by this selection set, including through fragment
    /// spreads.
    #[cfg(test)]
    pub(crate) fn used_variables(&self) -> IndexSet<&'_ Name> {
        let mut collector = VariableCollector::new();
        collector.visit_selection_set(self);
        collector.into_inner()
    }
}

// Conversion between apollo-rs and apollo-federation types.

impl TryFrom<&Operation> for executable::Operation {
    type Error = FederationError;

    fn try_from(normalized_operation: &Operation) -> Result<Self, Self::Error> {
        let operation_type: executable::OperationType = normalized_operation.root_kind.into();
        Ok(Self {
            operation_type,
            name: normalized_operation.name.clone(),
            variables: normalized_operation.variables.deref().clone(),
            directives: normalized_operation.directives.iter().cloned().collect(),
            selection_set: (&normalized_operation.selection_set).try_into()?,
        })
    }
}

impl TryFrom<&SelectionSet> for executable::SelectionSet {
    type Error = FederationError;

    fn try_from(val: &SelectionSet) -> Result<Self, Self::Error> {
        let mut flattened = vec![];
        for normalized_selection in val.selections.values() {
            let selection: executable::Selection = normalized_selection.try_into()?;
            if let executable::Selection::Field(field) = &selection {
                if field.name == *INTROSPECTION_TYPENAME_FIELD_NAME
                    && field.directives.is_empty()
                    && field.alias.is_none()
                {
                    // Move the plain __typename to the start of the selection set.
                    // This looks nicer, and matches existing tests.
                    // Note: The plain-ness is also defined in `Field::is_plain_typename_field`.
                    // PORT_NOTE: JS does this in `selectionsInPrintOrder`
                    flattened.insert(0, selection);
                    continue;
                }
            }
            flattened.push(selection);
        }
        if flattened.is_empty() {
            // In theory, for valid operations, we shouldn't have empty selection sets (field
            // selections whose type is a leaf will have an undefined selection set, not an empty
            // one). We do "abuse" this a bit however when create query "witness" during
            // composition validation where, to make it easier for users to locate the issue, we
            // want the created witness query to stop where the validation problem lies, even if
            // we're not on a leaf type. To make this look nice and explicit, we handle that case
            // by create a fake selection set that just contains an ellipsis, indicate there is
            // supposed to be more but we elided it for clarity. And yes, the whole thing is a bit
            // of a hack, albeit a convenient one.
            flattened.push(ellipsis_field()?);
        }
        Ok(Self {
            ty: val.type_position.type_name().clone(),
            selections: flattened,
        })
    }
}

/// Create a synthetic field named "...".
fn ellipsis_field() -> Result<executable::Selection, FederationError> {
    let field_name = Name::new_unchecked("...");
    let field_def = ast::FieldDefinition {
        description: None,
        ty: ty!(String),
        name: field_name.clone(),
        arguments: vec![],
        directives: Default::default(),
    };
    Ok(executable::Selection::Field(Node::new(executable::Field {
        definition: Node::new(field_def),
        alias: None,
        name: field_name,
        arguments: vec![],
        directives: Default::default(),
        selection_set: executable::SelectionSet::new(GRAPHQL_STRING_TYPE_NAME),
    })))
}

impl TryFrom<&Selection> for executable::Selection {
    type Error = FederationError;

    fn try_from(val: &Selection) -> Result<Self, Self::Error> {
        Ok(match val {
            Selection::Field(normalized_field_selection) => executable::Selection::Field(
                Node::new(normalized_field_selection.deref().try_into()?),
            ),
            Selection::InlineFragment(normalized_inline_fragment_selection) => {
                executable::Selection::InlineFragment(Node::new(
                    normalized_inline_fragment_selection.deref().try_into()?,
                ))
            }
        })
    }
}

impl TryFrom<&Field> for executable::Field {
    type Error = FederationError;

    fn try_from(normalized_field: &Field) -> Result<Self, Self::Error> {
        let definition = normalized_field
            .field_position
            .get(normalized_field.schema.schema())?
            .node
            .to_owned();
        let selection_set = executable::SelectionSet {
            ty: definition.ty.inner_named_type().clone(),
            selections: vec![],
        };
        Ok(Self {
            definition,
            alias: normalized_field.alias.to_owned(),
            name: normalized_field.name().to_owned(),
            arguments: normalized_field.arguments.deref().to_owned(),
            directives: normalized_field.directives.iter().cloned().collect(),
            selection_set,
        })
    }
}

impl TryFrom<&FieldSelection> for executable::Field {
    type Error = FederationError;

    fn try_from(val: &FieldSelection) -> Result<Self, Self::Error> {
        let mut field = Self::try_from(&val.field)?;
        if let Some(selection_set) = &val.selection_set {
            field.selection_set = selection_set.try_into()?;
        }
        Ok(field)
    }
}

impl TryFrom<&InlineFragment> for executable::InlineFragment {
    type Error = FederationError;

    fn try_from(normalized_inline_fragment: &InlineFragment) -> Result<Self, Self::Error> {
        let type_condition = normalized_inline_fragment
            .type_condition_position
            .as_ref()
            .map(|pos| pos.type_name().clone());
        let ty = type_condition.clone().unwrap_or_else(|| {
            normalized_inline_fragment
                .parent_type_position
                .type_name()
                .clone()
        });
        Ok(Self {
            type_condition,
            directives: normalized_inline_fragment
                .directives
                .iter()
                .cloned()
                .collect(),
            selection_set: executable::SelectionSet {
                ty,
                selections: Vec::new(),
            },
        })
    }
}

impl TryFrom<&InlineFragmentSelection> for executable::InlineFragment {
    type Error = FederationError;

    fn try_from(val: &InlineFragmentSelection) -> Result<Self, Self::Error> {
        Ok(Self {
            selection_set: (&val.selection_set).try_into()?,
            ..Self::try_from(&val.inline_fragment)?
        })
    }
}

impl TryFrom<Operation> for Valid<executable::ExecutableDocument> {
    type Error = FederationError;

    fn try_from(value: Operation) -> Result<Self, Self::Error> {
        let operation = executable::Operation::try_from(&value)?;
        let mut document = executable::ExecutableDocument::new();
        document.operations.insert(operation);
        coerce_executable_values(value.schema.schema(), &mut document);
        Ok(document.validate(value.schema.schema())?)
    }
}

// Display implementations for the operation types.

impl Display for Operation {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let operation: executable::Operation = match self.try_into() {
            Ok(operation) => operation,
            Err(_) => return Err(std::fmt::Error),
        };
        operation.serialize().fmt(f)
    }
}

impl Display for SelectionSet {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let selection_set: executable::SelectionSet = match self.try_into() {
            Ok(selection_set) => selection_set,
            Err(_) => return Err(std::fmt::Error),
        };
        selection_set.serialize().no_indent().fmt(f)
    }
}

pub(crate) struct FieldSetDisplay<T: AsRef<SelectionSet>>(pub(crate) T);

impl<T: AsRef<SelectionSet>> Display for FieldSetDisplay<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let selection_set: executable::SelectionSet = match self.0.as_ref().try_into() {
            Ok(selection_set) => selection_set,
            Err(_) => return Err(std::fmt::Error),
        };
        FieldSet {
            sources: Default::default(),
            selection_set,
        }
        .serialize()
        .no_indent()
        .fmt(f)
    }
}

impl Display for Selection {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let selection: executable::Selection = match self.try_into() {
            Ok(selection) => selection,
            Err(_) => return Err(std::fmt::Error),
        };
        selection.serialize().no_indent().fmt(f)
    }
}

impl Display for FieldSelection {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let field: executable::Field = match self.try_into() {
            Ok(field) => field,
            Err(_) => return Err(std::fmt::Error),
        };
        field.serialize().no_indent().fmt(f)
    }
}

impl Display for InlineFragmentSelection {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let inline_fragment: executable::InlineFragment = match self.try_into() {
            Ok(inline_fragment) => inline_fragment,
            Err(_) => return Err(std::fmt::Error),
        };
        inline_fragment.serialize().no_indent().fmt(f)
    }
}

impl Display for Field {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        // We create a selection with an empty selection set here, relying on `apollo-rs` to skip
        // serializing it when empty. Note we're implicitly relying on the lack of type-checking
        // in both `FieldSelection` and `Field` display logic (specifically, we rely on
        // them not checking whether it is valid for the selection set to be empty).
        self.clone().with_subselection(None).fmt(f)
    }
}

impl Display for InlineFragment {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        // We can't use the same trick we did with `Field`'s display logic, since
        // selection sets are non-optional for inline fragment selections.
        let data = self;
        if let Some(type_name) = &data.type_condition_position {
            f.write_str("... on ")?;
            f.write_str(type_name.type_name())?;
        } else {
            f.write_str("...")?;
        }
        data.directives.serialize().no_indent().fmt(f)
    }
}

/// Holds normalized selection sets of provided fragments.
///
/// PORT_NOTE: The JS codebase combined the fragment spread's directives with the fragment
/// definition's directives. This was invalid GraphQL as those directives may not be applicable
/// on different locations. Fragment directives are currently ignored. We validate whether
/// fragment spread directives can be applied to inline fragment and raise an error if they
/// are not applicable.
#[derive(Default)]
pub(crate) struct FragmentSpreadCache {
    fragment_selection_sets: Arc<HashMap<Name, SelectionSet>>,
}

impl FragmentSpreadCache {
    // in order to normalize selection sets, we need to process them in dependency order
    fn init(
        fragments: &IndexMap<Name, Node<Fragment>>,
        schema: &ValidFederationSchema,
        check_cancellation: &dyn Fn() -> Result<(), SingleFederationError>,
    ) -> Self {
        FragmentSpreadCache::normalize_in_dependency_order(fragments, schema, check_cancellation)
    }

    fn insert(&mut self, fragment_name: &Name, selection_set: SelectionSet) {
        Arc::make_mut(&mut self.fragment_selection_sets)
            .insert(fragment_name.clone(), selection_set);
    }

    pub(crate) fn get(&self, name: &str) -> Option<&SelectionSet> {
        self.fragment_selection_sets.get(name)
    }

    pub(crate) fn contains(&self, name: &str) -> bool {
        self.fragment_selection_sets.contains_key(name)
    }

    // We normalize passed in fragments in their dependency order, i.e. if a fragment A uses another fragment B, then we will
    // normalize B _before_ attempting to normalize A. Normalized fragments have access to previously normalized fragments.
    fn normalize_in_dependency_order(
        fragments: &IndexMap<Name, Node<Fragment>>,
        schema: &ValidFederationSchema,
        check_cancellation: &dyn Fn() -> Result<(), SingleFederationError>,
    ) -> FragmentSpreadCache {
        struct FragmentDependencies {
            fragment: Node<Fragment>,
            depends_on: Vec<Name>,
        }

        // Note: We use IndexMap to stabilize the ordering of the result so we can
        // normalize them in order.
        let mut fragments_map: IndexMap<Name, FragmentDependencies> = IndexMap::default();
        for fragment in fragments.values() {
            let mut fragment_usages = IndexMap::default();
            FragmentSpreadCache::collect_fragment_usages(
                &fragment.selection_set,
                &mut fragment_usages,
            );
            let usages: Vec<Name> = fragment_usages.keys().cloned().collect::<Vec<Name>>();
            fragments_map.insert(
                fragment.name.clone(),
                FragmentDependencies {
                    fragment: fragment.clone(),
                    depends_on: usages,
                },
            );
        }

        let mut removed_fragments: IndexSet<Name> = IndexSet::default();
        let mut cache = FragmentSpreadCache::default();
        while !fragments_map.is_empty() {
            // Note that graphQL specifies that named fragments cannot have cycles (https://spec.graphql.org/draft/#sec-Fragment-spreads-must-not-form-cycles)
            // and so we're guaranteed that on every iteration, at least one element of the map is removed (so the `while` loop will terminate).
            fragments_map.retain(|name, info| {
                let can_remove = info
                    .depends_on
                    .iter()
                    .all(|n| cache.contains(n) || removed_fragments.contains(n));
                if can_remove {
                    if let Ok(normalized) = SelectionSet::from_selection_set(
                        &info.fragment.selection_set,
                        &cache,
                        schema,
                        check_cancellation,
                    ) {
                        cache.insert(&info.fragment.name, normalized);
                    } else {
                        removed_fragments.insert(name.clone());
                    }
                }
                // keep only the elements that cannot be removed
                !can_remove
            });
        }
        cache
    }
    /// Just like our `SelectionSet::used_fragments`, but with apollo-compiler types
    fn collect_fragment_usages(
        selection_set: &executable::SelectionSet,
        aggregator: &mut IndexMap<Name, u32>,
    ) {
        selection_set.selections.iter().for_each(|s| match s {
            executable::Selection::Field(f) => {
                FragmentSpreadCache::collect_fragment_usages(&f.selection_set, aggregator);
            }
            executable::Selection::InlineFragment(i) => {
                FragmentSpreadCache::collect_fragment_usages(&i.selection_set, aggregator);
            }
            executable::Selection::FragmentSpread(f) => {
                let current_count = aggregator.entry(f.fragment_name.clone()).or_default();
                *current_count += 1;
            }
        })
    }
}

/// Normalizes the selection set of the specified operation.
///
/// This method applies the following transformations:
/// - Merge selections with the same normalization "key".
/// - Expand fragment spreads into inline fragments.
/// - Remove `__schema` or `__type` introspection fields at all levels, as these shouldn't be
///   handled by query planning.
/// - Hoist fragment spreads/inline fragments into their parents if they have no directives and
///   their parent type matches.
pub(crate) fn normalize_operation(
    operation: &executable::Operation,
    fragments: &IndexMap<Name, Node<Fragment>>,
    schema: &ValidFederationSchema,
    interface_types_with_interface_objects: &IndexSet<InterfaceTypeDefinitionPosition>,
    check_cancellation: &dyn Fn() -> Result<(), SingleFederationError>,
) -> Result<Operation, FederationError> {
    let fragment_cache = FragmentSpreadCache::init(fragments, schema, check_cancellation);
    let mut normalized_selection_set = SelectionSet::from_selection_set(
        &operation.selection_set,
        &fragment_cache,
        schema,
        check_cancellation,
    )?;
    // We clear up the fragments since we've expanded all.
    // Also note that expanding fragment usually generate unnecessary fragments/inefficient
    // selections, so it basically always make sense to flatten afterwards.
    // PORT_NOTE: This was done in `Operation.expandAllFragments`, but it's moved here.
    normalized_selection_set = normalized_selection_set
        .flatten_unnecessary_fragments(&normalized_selection_set.type_position, schema)?;
    remove_introspection(&mut normalized_selection_set);
    normalized_selection_set.optimize_sibling_typenames(interface_types_with_interface_objects)?;

    let normalized_operation = Operation {
        schema: schema.clone(),
        root_kind: operation.operation_type.into(),
        name: operation.name.clone(),
        variables: Arc::new(operation.variables.clone()),
        directives: operation.directives.clone().into(),
        selection_set: normalized_selection_set,
    };
    Ok(normalized_operation)
}

// PORT_NOTE: This is a port of `withoutIntrospection` from JS version.
fn remove_introspection(selection_set: &mut SelectionSet) {
    // Note that, because we only apply this to the top-level selections, we skip all
    // introspection, including __typename. In general, we don't want to ignore __typename during
    // query plans, but at top-level, we can let the router execution deal with it rather than
    // querying some service for that.

    Arc::make_mut(&mut selection_set.selections).retain(|_, selection| {
        !matches!(selection,
            Selection::Field(field_selection) if
                field_selection.field.field_position.is_introspection_typename_field()
        )
    });
}

/// Check if the runtime types of two composite types intersect.
///
/// This avoids using `possible_runtime_types` and instead implements fast paths.
fn runtime_types_intersect(
    type1: &CompositeTypeDefinitionPosition,
    type2: &CompositeTypeDefinitionPosition,
    schema: &ValidFederationSchema,
) -> bool {
    use CompositeTypeDefinitionPosition::*;
    match (type1, type2) {
        (Object(left), Object(right)) => left == right,
        (Object(object), Union(union_)) | (Union(union_), Object(object)) => union_
            .get(schema.schema())
            .is_ok_and(|union_| union_.members.contains(&object.type_name)),
        (Object(object), Interface(interface)) | (Interface(interface), Object(object)) => schema
            .referencers()
            .get_interface_type(&interface.type_name)
            .is_ok_and(|referencers| referencers.object_types.contains(object)),
        (Union(left), Union(right)) if left == right => true,
        (Union(left), Union(right)) => {
            match (left.get(schema.schema()), right.get(schema.schema())) {
                (Ok(left), Ok(right)) => left.members.intersection(&right.members).next().is_some(),
                _ => false,
            }
        }
        (Interface(left), Interface(right)) if left == right => true,
        (Interface(left), Interface(right)) => {
            let r = schema.referencers();
            match (
                r.get_interface_type(&left.type_name),
                r.get_interface_type(&right.type_name),
            ) {
                (Ok(left), Ok(right)) => left
                    .object_types
                    .intersection(&right.object_types)
                    .next()
                    .is_some(),
                _ => false,
            }
        }
        (Union(union_), Interface(interface)) | (Interface(interface), Union(union_)) => match (
            union_.get(schema.schema()),
            schema
                .referencers()
                .get_interface_type(&interface.type_name),
        ) {
            (Ok(union_), Ok(referencers)) => referencers
                .object_types
                .iter()
                .any(|implementer| union_.members.contains(&implementer.type_name)),
            _ => false,
        },
    }
}
