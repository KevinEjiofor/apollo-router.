//! Query processing.
//!
//! Parsing, formatting and manipulation of queries.
#![allow(clippy::mutable_key_type)]

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;

use apollo_compiler::ExecutableDocument;
use apollo_compiler::executable;
use apollo_compiler::schema::ExtendedType;
use derivative::Derivative;
use indexmap::IndexSet;
use serde::Deserialize;
use serde::Serialize;
use serde_json_bytes::ByteString;
use tracing::level_filters::LevelFilter;

use self::subselections::BooleanValues;
use self::subselections::SubSelectionKey;
use self::subselections::SubSelectionValue;
use super::Fragment;
use super::QueryHash;
use crate::Configuration;
use crate::error::FetchError;
use crate::graphql::Error;
use crate::graphql::Request;
use crate::graphql::Response;
use crate::json_ext::Object;
use crate::json_ext::Path;
use crate::json_ext::ResponsePathElement;
use crate::json_ext::Value;
use crate::plugins::authorization::UnauthorizedPaths;
use crate::query_planner::fetch::OperationKind;
use crate::services::layers::query_analysis::ParsedDocument;
use crate::services::layers::query_analysis::ParsedDocumentInner;
use crate::services::layers::query_analysis::get_operation;
use crate::spec::FieldType;
use crate::spec::Fragments;
use crate::spec::InvalidValue;
use crate::spec::Schema;
use crate::spec::Selection;
use crate::spec::SpecError;
use crate::spec::schema::ApiSchema;

pub(crate) mod subselections;
pub(crate) mod transform;
pub(crate) mod traverse;

pub(crate) const TYPENAME: &str = "__typename";
pub(crate) const ERROR_CODE_RESPONSE_VALIDATION: &str = "RESPONSE_VALIDATION_FAILED";
pub(crate) const EXTENSIONS_VALUE_COMPLETION_KEY: &str = "valueCompletion";

/// A GraphQL query.
#[derive(Derivative, Serialize, Deserialize)]
#[derivative(PartialEq, Hash, Eq, Debug)]
pub(crate) struct Query {
    pub(crate) string: String,
    #[derivative(PartialEq = "ignore", Hash = "ignore")]
    pub(crate) fragments: Fragments,
    #[derivative(PartialEq = "ignore", Hash = "ignore")]
    pub(crate) operation: Operation,
    #[derivative(PartialEq = "ignore", Hash = "ignore")]
    pub(crate) subselections: HashMap<SubSelectionKey, SubSelectionValue>,
    #[derivative(PartialEq = "ignore", Hash = "ignore")]
    pub(crate) unauthorized: UnauthorizedPaths,
    #[derivative(PartialEq = "ignore", Hash = "ignore")]
    pub(crate) filtered_query: Option<Arc<Query>>,
    #[derivative(PartialEq = "ignore", Hash = "ignore")]
    pub(crate) defer_stats: DeferStats,
    #[derivative(PartialEq = "ignore", Hash = "ignore")]
    pub(crate) is_original: bool,

    /// This is a hash that depends on:
    /// - the query itself
    /// - the schema
    #[derivative(PartialEq = "ignore", Hash = "ignore")]
    pub(crate) schema_aware_hash: QueryHash,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct DeferStats {
    /// Is `@defer` used at all (except `@defer(if=false)`)
    pub(crate) has_defer: bool,

    /// Is `@defer` used without `if` (or `@defer(if=true)`)
    pub(crate) has_unconditional_defer: bool,

    /// Names of boolean variables used in `@defer(if=$var)`
    pub(crate) conditional_defer_variable_names: IndexSet<String>,
}

impl Query {
    /// Returns an empty query. This should be used somewhat carefully and only in tests.
    /// Other parts of the router may not handle empty queries properly.
    ///
    /// FIXME: This should be marked cfg(test) but it's used in places where adding cfg(test) is tricky.
    pub(crate) fn empty_for_tests() -> Self {
        Self {
            string: String::new(),
            fragments: Fragments {
                map: HashMap::new(),
            },
            operation: Operation::empty(),
            subselections: HashMap::new(),
            unauthorized: UnauthorizedPaths::default(),
            filtered_query: None,
            defer_stats: DeferStats {
                has_defer: false,
                has_unconditional_defer: false,
                conditional_defer_variable_names: IndexSet::default(),
            },
            is_original: true,
            schema_aware_hash: QueryHash::default(),
        }
    }

    /// Re-format the response value to match this query.
    ///
    /// This will discard unrequested fields and re-order the output to match the order of the
    /// query.
    #[tracing::instrument(skip_all, level = "trace")]
    pub(crate) fn format_response(
        &self,
        response: &mut Response,
        variables: Object,
        schema: &ApiSchema,
        defer_conditions: BooleanValues,
    ) -> Vec<Path> {
        let data = std::mem::take(&mut response.data);

        match data {
            Some(Value::Object(mut input)) => {
                if self.is_deferred(defer_conditions) {
                    // Get subselection from hashmap
                    match self.subselections.get(&SubSelectionKey {
                        defer_label: response.label.clone(),
                        defer_conditions,
                    }) {
                        Some(subselection) => {
                            let mut output =
                                Object::with_capacity(subselection.selection_set.len());
                            let mut parameters = FormatParameters {
                                variables: &variables,
                                schema,
                                errors: Vec::new(),
                                nullified: Vec::new(),
                            };

                            response.data = Some(
                                match self.apply_root_selection_set(
                                    &subselection.type_name,
                                    &subselection.selection_set,
                                    &mut parameters,
                                    &mut input,
                                    &mut output,
                                    &mut Vec::new(),
                                ) {
                                    Ok(()) => output.into(),
                                    Err(InvalidValue) => Value::Null,
                                },
                            );

                            if !parameters.errors.is_empty() {
                                if let Ok(value) = serde_json_bytes::to_value(&parameters.errors) {
                                    response
                                        .extensions
                                        .insert(EXTENSIONS_VALUE_COMPLETION_KEY, value);
                                }
                            }

                            return parameters.nullified;
                        }
                        None => {
                            response.data = Some(Value::Object(Object::default()));
                            return vec![];
                        }
                    }
                } else {
                    let mut output = Object::with_capacity(self.operation.selection_set.len());

                    let all_variables = if self.operation.variables.is_empty() {
                        variables
                    } else {
                        self.operation
                            .variables
                            .iter()
                            .filter_map(|(k, Variable { default_value, .. })| {
                                default_value.as_ref().map(|v| (k, v))
                            })
                            .chain(variables.iter())
                            .map(|(k, v)| (k.clone(), v.clone()))
                            .collect()
                    };

                    let operation_type_name = schema
                        .root_operation(self.operation.kind.into())
                        .map(|name| name.as_str())
                        .unwrap_or(self.operation.kind.default_type_name());
                    let mut parameters = FormatParameters {
                        variables: &all_variables,
                        schema,
                        errors: Vec::new(),
                        nullified: Vec::new(),
                    };

                    response.data = Some(
                        match self.apply_root_selection_set(
                            operation_type_name,
                            &self.operation.selection_set,
                            &mut parameters,
                            &mut input,
                            &mut output,
                            &mut Vec::new(),
                        ) {
                            Ok(()) => output.into(),
                            Err(InvalidValue) => Value::Null,
                        },
                    );
                    if !parameters.errors.is_empty() {
                        if let Ok(value) = serde_json_bytes::to_value(&parameters.errors) {
                            response
                                .extensions
                                .insert(EXTENSIONS_VALUE_COMPLETION_KEY, value);
                        }
                    }

                    return parameters.nullified;
                }
            }
            Some(Value::Null) => {
                response.data = Some(Value::Null);
                return vec![];
            }
            _ => {
                failfast_debug!("invalid type for data in response. data: {:#?}", data);
            }
        }

        response.data = Some(Value::Null);

        vec![]
    }

    pub(crate) fn parse_document(
        query: &str,
        operation_name: Option<&str>,
        schema: &Schema,
        configuration: &Configuration,
    ) -> Result<ParsedDocument, SpecError> {
        let parser = &mut apollo_compiler::parser::Parser::new()
            .recursion_limit(configuration.limits.parser_max_recursion)
            .token_limit(configuration.limits.parser_max_tokens);
        let ast = match parser.parse_ast(query, "query.graphql") {
            Ok(ast) => ast,
            Err(errors) => {
                return Err(SpecError::ParseError(errors.into()));
            }
        };

        let api_schema = schema.api_schema();
        let executable_document = match ast.to_executable_validate(api_schema) {
            Ok(doc) => doc,
            Err(errors) => {
                return Err(SpecError::ValidationError(errors.into()));
            }
        };

        // Trace log recursion limit data
        let recursion_limit = parser.recursion_reached();
        tracing::trace!(?recursion_limit, "recursion limit data");

        let hash = schema.schema_id.operation_hash(query, operation_name);
        ParsedDocumentInner::new(
            ast,
            Arc::new(executable_document),
            operation_name,
            Arc::new(hash),
        )
    }

    #[cfg(test)]
    pub(crate) fn parse(
        query_text: impl Into<String>,
        operation_name: Option<&str>,
        schema: &Schema,
        configuration: &Configuration,
    ) -> Result<Self, tower::BoxError> {
        let query_text = query_text.into();

        let doc = Self::parse_document(&query_text, operation_name, schema, configuration)?;
        let (fragments, operation, defer_stats, schema_aware_hash) =
            Self::extract_query_information(schema, &query_text, &doc.executable, operation_name)?;

        Ok(Query {
            string: query_text,
            fragments,
            operation,
            subselections: HashMap::new(),
            unauthorized: UnauthorizedPaths::default(),
            filtered_query: None,
            defer_stats,
            is_original: true,
            schema_aware_hash,
        })
    }

    /// Extract serializable data structures from the apollo-compiler HIR.
    pub(crate) fn extract_query_information(
        schema: &Schema,
        query_text: &str,
        document: &ExecutableDocument,
        operation_name: Option<&str>,
    ) -> Result<(Fragments, Operation, DeferStats, QueryHash), SpecError> {
        let mut defer_stats = DeferStats {
            has_defer: false,
            has_unconditional_defer: false,
            conditional_defer_variable_names: IndexSet::default(),
        };
        let fragments = Fragments::from_hir(document, schema, &mut defer_stats)?;
        let operation = get_operation(document, operation_name)?;
        let operation = Operation::from_hir(&operation, schema, &mut defer_stats, &fragments)?;
        let hash = schema.schema_id.operation_hash(query_text, operation_name);

        Ok((fragments, operation, defer_stats, hash))
    }

    #[allow(clippy::too_many_arguments)]
    fn format_value<'a: 'b, 'b>(
        &'a self,
        parameters: &mut FormatParameters,
        field_type: &executable::Type,
        input: &mut Value,
        output: &mut Value,
        path: &mut Vec<ResponsePathElement<'b>>,
        parent_type: &executable::Type,
        selection_set: &'a [Selection],
    ) -> Result<(), InvalidValue> {
        // for every type, if we have an invalid value, we will replace it with null
        // and return Ok(()), because values are optional by default
        match field_type {
            // for non null types, we validate with the inner type, then if we get an InvalidValue
            // we set it to null and immediately return an error instead of Ok(()), because we
            // want the error to go up until the next nullable parent
            executable::Type::NonNullNamed(_) | executable::Type::NonNullList(_) => {
                let inner_type = match field_type {
                    executable::Type::NonNullList(ty) => ty.clone().list(),
                    executable::Type::NonNullNamed(name) => executable::Type::Named(name.clone()),
                    _ => unreachable!(),
                };
                match self.format_value(
                    parameters,
                    &inner_type,
                    input,
                    output,
                    path,
                    field_type,
                    selection_set,
                ) {
                    Err(_) => Err(InvalidValue),
                    Ok(_) => {
                        if output.is_null() {
                            let message = match path.last() {
                                Some(ResponsePathElement::Key(k)) => format!(
                                    "Cannot return null for non-nullable field {parent_type}.{k}"
                                ),
                                Some(ResponsePathElement::Index(i)) => format!(
                                    "Cannot return null for non-nullable array element of type {inner_type} at index {i}"
                                ),
                                _ => todo!(),
                            };
                            parameters.errors.push(
                                Error::builder()
                                    .message(message)
                                    .path(Path::from_response_slice(path))
                                    .build(),
                            );

                            Err(InvalidValue)
                        } else {
                            Ok(())
                        }
                    }
                }
            }

            // if the list contains nonnullable types, we will receive a Err(InvalidValue)
            // and should replace the entire list with null
            // if the types are nullable, the inner call to filter_errors will take care
            // of setting the current entry to null
            executable::Type::List(inner_type) => match input {
                Value::Array(input_array) => {
                    if output.is_null() {
                        *output = Value::Array(vec![Value::Null; input_array.len()]);
                    }
                    let output_array = output.as_array_mut().ok_or(InvalidValue)?;
                    match input_array
                        .iter_mut()
                        .enumerate()
                        .try_for_each(|(i, element)| {
                            path.push(ResponsePathElement::Index(i));
                            let res = self.format_value(
                                parameters,
                                inner_type,
                                element,
                                &mut output_array[i],
                                path,
                                field_type,
                                selection_set,
                            );
                            path.pop();
                            res
                        }) {
                        Err(InvalidValue) => {
                            parameters.nullified.push(Path::from_response_slice(path));
                            *output = Value::Null;
                            Ok(())
                        }
                        Ok(()) => Ok(()),
                    }
                }
                _ => Ok(()),
            },
            executable::Type::Named(name) if name == "Int" => {
                let opt = if input.is_i64() {
                    input.as_i64().and_then(|i| i32::try_from(i).ok())
                } else if input.is_u64() {
                    input.as_i64().and_then(|i| i32::try_from(i).ok())
                } else {
                    None
                };

                // if the value is invalid, we do not insert it in the output object
                // which is equivalent to inserting null
                if opt.is_some() {
                    *output = input.clone();
                } else {
                    *output = Value::Null;
                }
                Ok(())
            }
            executable::Type::Named(name) if name == "Float" => {
                if input.as_f64().is_some() {
                    *output = input.clone();
                } else {
                    *output = Value::Null;
                }
                Ok(())
            }
            executable::Type::Named(name) if name == "Boolean" => {
                if input.as_bool().is_some() {
                    *output = input.clone();
                } else {
                    *output = Value::Null;
                }
                Ok(())
            }
            executable::Type::Named(name) if name == "String" => {
                if input.as_str().is_some() {
                    *output = input.clone();
                } else {
                    *output = Value::Null;
                }
                Ok(())
            }
            executable::Type::Named(name) if name == "Id" => {
                if input.is_string() || input.is_i64() || input.is_u64() || input.is_f64() {
                    *output = input.clone();
                } else {
                    *output = Value::Null;
                }
                Ok(())
            }
            executable::Type::Named(type_name) => {
                // we cannot know about the expected format of custom scalars
                // so we must pass them directly to the client
                match parameters.schema.types.get(type_name) {
                    Some(ExtendedType::Scalar(_)) => {
                        *output = input.clone();
                        return Ok(());
                    }
                    Some(ExtendedType::Enum(enum_type)) => {
                        return match input.as_str() {
                            Some(s) => {
                                if enum_type.values.contains_key(s) {
                                    *output = input.clone();
                                    Ok(())
                                } else {
                                    *output = Value::Null;
                                    Ok(())
                                }
                            }
                            None => {
                                *output = Value::Null;
                                Ok(())
                            }
                        };
                    }
                    _ => {}
                }

                match input {
                    Value::Object(input_object) => {
                        if let Some(input_type) =
                            input_object.get(TYPENAME).and_then(|val| val.as_str())
                        {
                            // If there is a __typename, make sure the pointed type is a valid type of the schema. Otherwise, something is wrong, and in case we might
                            // be inadvertently leaking some data for an @inacessible type or something, nullify the whole object. However, do note that due to `@interfaceObject`,
                            // some subgraph can have returned a __typename that is the name of an interface in the supergraph, and this is fine (that is, we should not
                            // return such a __typename to the user, but as long as it's not returned, having it in the internal data is ok and sometimes expected).
                            let Some(ExtendedType::Object(_) | ExtendedType::Interface(_)) =
                                parameters.schema.types.get(input_type)
                            else {
                                parameters.nullified.push(Path::from_response_slice(path));
                                *output = Value::Null;
                                return Ok(());
                            };
                        }

                        if output.is_null() {
                            *output = Value::Object(Object::with_capacity(selection_set.len()));
                        }
                        let output_object = output.as_object_mut().ok_or(InvalidValue)?;

                        let typename = input_object
                            .get(TYPENAME)
                            .and_then(|val| val.as_str())
                            .and_then(|s| apollo_compiler::ast::NamedType::new(s).ok())
                            .map(apollo_compiler::ast::Type::Named);

                        let current_type =
                            match parameters.schema.types.get(field_type.inner_named_type()) {
                                Some(ExtendedType::Interface(..) | ExtendedType::Union(..)) => {
                                    typename.as_ref().unwrap_or(field_type)
                                }
                                _ => field_type,
                            };

                        if self
                            .apply_selection_set(
                                selection_set,
                                parameters,
                                input_object,
                                output_object,
                                path,
                                current_type,
                            )
                            .is_err()
                        {
                            parameters.nullified.push(Path::from_response_slice(path));
                            *output = Value::Null;
                        }

                        Ok(())
                    }
                    _ => {
                        parameters.nullified.push(Path::from_response_slice(path));
                        *output = Value::Null;
                        Ok(())
                    }
                }
            }
        }
    }

    fn apply_selection_set<'a: 'b, 'b>(
        &'a self,
        selection_set: &'a [Selection],
        parameters: &mut FormatParameters,
        input: &mut Object,
        output: &mut Object,
        path: &mut Vec<ResponsePathElement<'b>>,
        // the type under which we apply selections
        current_type: &executable::Type,
    ) -> Result<(), InvalidValue> {
        // For skip and include, using .unwrap_or is legit here because
        // validate_variables should have already checked that
        // the variable is present and it is of the correct type
        for selection in selection_set {
            match selection {
                Selection::Field {
                    name,
                    alias,
                    selection_set,
                    field_type,
                    include_skip,
                } => {
                    let field_name = alias.as_ref().unwrap_or(name);
                    if include_skip.should_skip(parameters.variables) {
                        continue;
                    }

                    if name.as_str() == TYPENAME {
                        let object_type = parameters
                            .schema
                            .get_object(current_type.inner_named_type())
                            .or_else(|| {
                                let input_value = input.get(field_name.as_str())?.as_str()?;
                                parameters.schema.get_object(input_value)
                            });

                        if let Some(object_type) = object_type {
                            output.insert((*field_name).clone(), object_type.name.as_str().into());
                        } else {
                            return Err(InvalidValue);
                        }
                        continue;
                    }

                    if let Some(input_value) = input.get_mut(field_name.as_str()) {
                        // if there's already a value for that key in the output it means either:
                        // - the value is a scalar and was moved into output using take(), replacing
                        // the input value with Null
                        // - the value was already null and is already present in output
                        // if we expect an object or list at that key, output will already contain
                        // an object or list and then input_value cannot be null
                        if input_value.is_null() && output.contains_key(field_name.as_str()) {
                            continue;
                        }

                        let selection_set = selection_set.as_deref().unwrap_or_default();
                        let output_value =
                            output.entry((*field_name).clone()).or_insert(Value::Null);

                        path.push(ResponsePathElement::Key(field_name.as_str()));
                        let res = self.format_value(
                            parameters,
                            &field_type.0,
                            input_value,
                            output_value,
                            path,
                            current_type,
                            selection_set,
                        );
                        path.pop();
                        res?
                    } else {
                        if !output.contains_key(field_name.as_str()) {
                            output.insert((*field_name).clone(), Value::Null);
                        }
                        if field_type.is_non_null() {
                            parameters.errors.push(
                                Error::builder()
                                    .message(format!(
                                        "Cannot return null for non-nullable field {current_type}.{}",
                                        field_name.as_str()
                                    ))
                                    .path(Path::from_response_slice(path))
                                    .build()
                            );

                            return Err(InvalidValue);
                        }
                    }
                }
                Selection::InlineFragment {
                    type_condition,
                    selection_set,
                    include_skip,
                    defer: _,
                    defer_label: _,
                    known_type: _,
                } => {
                    if include_skip.should_skip(parameters.variables) {
                        continue;
                    }

                    let is_apply = current_type.inner_named_type().as_str()
                        == type_condition.as_str()
                        || parameters
                            .schema
                            .is_subtype(type_condition, current_type.inner_named_type().as_str());

                    if is_apply {
                        // if this is the filtered query, we must keep the __typename field because the original query must know the type
                        if !self.is_original {
                            if let Some(input_type) = input.get(TYPENAME) {
                                output.insert(TYPENAME, input_type.clone());
                            }
                        }

                        self.apply_selection_set(
                            selection_set,
                            parameters,
                            input,
                            output,
                            path,
                            current_type,
                        )?;
                    }
                }
                Selection::FragmentSpread {
                    name,
                    known_type: _,
                    include_skip,
                    defer: _,
                    defer_label: _,
                } => {
                    if include_skip.should_skip(parameters.variables) {
                        continue;
                    }

                    if let Some(Fragment {
                        type_condition,
                        selection_set,
                    }) = self.fragments.get(name)
                    {
                        let is_apply = current_type.inner_named_type().as_str()
                            == type_condition.as_str()
                            || parameters.schema.is_subtype(
                                type_condition,
                                current_type.inner_named_type().as_str(),
                            );

                        if is_apply {
                            // if this is the filtered query, we must keep the __typename field because the original query must know the type
                            if !self.is_original {
                                if let Some(input_type) = input.get(TYPENAME) {
                                    output.insert(TYPENAME, input_type.clone());
                                }
                            }

                            self.apply_selection_set(
                                selection_set,
                                parameters,
                                input,
                                output,
                                path,
                                current_type,
                            )?;
                        }
                    } else {
                        // the fragment should have been already checked with the schema
                        failfast_debug!("missing fragment named: {}", name);
                    }
                }
            }
        }

        Ok(())
    }

    fn apply_root_selection_set<'a: 'b, 'b>(
        &'a self,
        root_type_name: &str,
        selection_set: &'a [Selection],
        parameters: &mut FormatParameters,
        input: &mut Object,
        output: &mut Object,
        path: &mut Vec<ResponsePathElement<'b>>,
    ) -> Result<(), InvalidValue> {
        for selection in selection_set {
            match selection {
                Selection::Field {
                    name,
                    alias,
                    selection_set,
                    field_type,
                    include_skip,
                } => {
                    if include_skip.should_skip(parameters.variables) {
                        continue;
                    }

                    let field_name = alias.as_ref().unwrap_or(name);
                    let field_name_str = field_name.as_str();

                    if name.as_str() == TYPENAME {
                        if !output.contains_key(field_name_str) {
                            output.insert(field_name.clone(), Value::String(root_type_name.into()));
                        }
                    } else if let Some(input_value) = input.get_mut(field_name_str) {
                        // if there's already a value for that key in the output it means either:
                        // - the value is a scalar and was moved into output using take(), replacing
                        // the input value with Null
                        // - the value was already null and is already present in output
                        // if we expect an object or list at that key, output will already contain
                        // an object or list and then input_value cannot be null
                        if input_value.is_null() && output.contains_key(field_name_str) {
                            continue;
                        }

                        let selection_set = selection_set.as_deref().unwrap_or_default();
                        let output_value =
                            output.entry((*field_name).clone()).or_insert(Value::Null);
                        path.push(ResponsePathElement::Key(field_name_str));
                        let res = self.format_value(
                            parameters,
                            &field_type.0,
                            input_value,
                            output_value,
                            path,
                            &field_type.0,
                            selection_set,
                        );
                        path.pop();
                        res?
                    } else if field_type.is_non_null() {
                        parameters.errors.push(
                            Error::builder()
                                .message(format!(
                                    "Cannot return null for non-nullable field {}.{field_name_str}",
                                    root_type_name
                                ))
                                .path(Path::from_response_slice(path))
                                .build(),
                        );
                        return Err(InvalidValue);
                    } else {
                        output.insert(field_name.clone(), Value::Null);
                    }
                }
                Selection::InlineFragment {
                    type_condition,
                    selection_set,
                    include_skip,
                    ..
                } => {
                    if include_skip.should_skip(parameters.variables) {
                        continue;
                    }

                    // check if the fragment matches the input type directly, and if not, check if the
                    // input type is a subtype of the fragment's type condition (interface, union)
                    let is_apply = (root_type_name == type_condition.as_str())
                        || parameters.schema.is_subtype(type_condition, root_type_name);

                    if is_apply {
                        self.apply_root_selection_set(
                            root_type_name,
                            selection_set,
                            parameters,
                            input,
                            output,
                            path,
                        )?;
                    }
                }
                Selection::FragmentSpread {
                    name,
                    known_type: _,
                    include_skip,
                    defer: _,
                    defer_label: _,
                } => {
                    if include_skip.should_skip(parameters.variables) {
                        continue;
                    }

                    if let Some(Fragment {
                        type_condition,
                        selection_set,
                    }) = self.fragments.get(name)
                    {
                        // check if the fragment matches the input type directly, and if not, check if the
                        // input type is a subtype of the fragment's type condition (interface, union)
                        let is_apply = (root_type_name == type_condition.as_str())
                            || parameters.schema.is_subtype(type_condition, root_type_name);

                        if is_apply {
                            self.apply_root_selection_set(
                                root_type_name,
                                selection_set,
                                parameters,
                                input,
                                output,
                                path,
                            )?;
                        }
                    } else {
                        // the fragment should have been already checked with the schema
                        failfast_debug!("missing fragment named: {}", name);
                    }
                }
            }
        }

        Ok(())
    }

    /// Validate a [`Request`]'s variables against this [`Query`] using a provided [`Schema`].
    #[tracing::instrument(skip_all, level = "trace")]
    pub(crate) fn validate_variables(
        &self,
        request: &Request,
        schema: &Schema,
    ) -> Result<(), Response> {
        if LevelFilter::current() >= LevelFilter::DEBUG {
            let known_variables = self
                .operation
                .variables
                .keys()
                .map(|k| k.as_str())
                .collect();
            let provided_variables = request
                .variables
                .keys()
                .map(|k| k.as_str())
                .collect::<HashSet<_>>();
            let unknown_variables = provided_variables
                .difference(&known_variables)
                .collect::<Vec<_>>();
            if !unknown_variables.is_empty() {
                failfast_debug!(
                    "Received variable unknown to the query: {:?}",
                    unknown_variables,
                );
            }
        }

        let errors = self
            .operation
            .variables
            .iter()
            .filter_map(
                |(
                    name,
                    Variable {
                        field_type: ty,
                        default_value,
                    },
                )| {
                    let value = request
                        .variables
                        .get(name.as_str())
                        .or(default_value.as_ref());
                    let path = super::JsonValuePath::Variable {
                        name: name.as_str(),
                    };
                    ty.validate_input_value(value, schema, &path)
                        .err()
                        .map(|message| {
                            FetchError::ValidationInvalidTypeVariable {
                                name: name.clone(),
                                message,
                            }
                            .to_graphql_error(None)
                        })
                },
            )
            .collect::<Vec<_>>();

        if errors.is_empty() {
            Ok(())
        } else {
            Err(Response::builder().errors(errors).build())
        }
    }

    pub(crate) fn variable_value<'a>(
        &'a self,
        variable_name: &str,
        variables: &'a Object,
    ) -> Option<&'a Value> {
        variables
            .get(variable_name)
            .or_else(|| self.default_variable_value(variable_name))
    }

    pub(crate) fn default_variable_value(&self, variable_name: &str) -> Option<&Value> {
        self.operation
            .variables
            .get(variable_name)
            .and_then(|Variable { default_value, .. }| default_value.as_ref())
    }

    pub(crate) fn contains_error_path(
        &self,
        label: &Option<String>,
        path: &Path,
        defer_conditions: BooleanValues,
    ) -> bool {
        let selection_set = match self.subselections.get(&SubSelectionKey {
            defer_label: label.clone(),
            defer_conditions,
        }) {
            Some(subselection) => &subselection.selection_set,
            None => &self.operation.selection_set,
        };
        let match_length = selection_set
            .iter()
            .map(|selection| selection.matching_error_path_length(&path.0, &self.fragments))
            .max()
            .unwrap_or(0);
        path.len() == match_length
    }

    pub(crate) fn matching_error_path_length(&self, path: &Path) -> usize {
        self.operation
            .selection_set
            .iter()
            .map(|selection| selection.matching_error_path_length(&path.0, &self.fragments))
            .max()
            .unwrap_or(0)
    }

    pub(crate) fn defer_variables_set(&self, variables: &Object) -> BooleanValues {
        let mut bits = 0_u32;
        for (i, variable) in self
            .defer_stats
            .conditional_defer_variable_names
            .iter()
            .enumerate()
        {
            let value = variables
                .get(variable.as_str())
                .or_else(|| self.default_variable_value(variable));

            if matches!(value, Some(serde_json_bytes::Value::Bool(true))) {
                bits |= 1 << i;
            }
        }

        BooleanValues { bits }
    }

    pub(crate) fn is_deferred(&self, defer_conditions: BooleanValues) -> bool {
        self.defer_stats.has_unconditional_defer || defer_conditions.bits != 0
    }
}

/// Intermediate structure for arguments passed through the entire formatting
struct FormatParameters<'a> {
    variables: &'a Object,
    errors: Vec<Error>,
    nullified: Vec<Path>,
    schema: &'a ApiSchema,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Operation {
    pub(crate) name: Option<String>,
    kind: OperationKind,
    type_name: String,
    pub(crate) selection_set: Vec<Selection>,
    variables: HashMap<ByteString, Variable>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Variable {
    field_type: FieldType,
    default_value: Option<Value>,
}

impl Operation {
    fn empty() -> Self {
        Self {
            name: None,
            kind: OperationKind::Query,
            type_name: "".into(),
            selection_set: Vec::new(),
            variables: HashMap::new(),
        }
    }

    pub(crate) fn from_hir(
        operation: &executable::Operation,
        schema: &Schema,
        defer_stats: &mut DeferStats,
        fragments: &Fragments,
    ) -> Result<Self, SpecError> {
        let name = operation.name.as_ref().map(|s| s.as_str().to_owned());
        let kind = operation.operation_type.into();
        let type_name = schema.root_operation_name(kind).to_owned();

        let selection_set = operation
            .selection_set
            .selections
            .iter()
            .filter_map(|selection| {
                Selection::from_hir(selection, &type_name, schema, 0, defer_stats, fragments)
                    .transpose()
            })
            .collect::<Result<_, _>>()?;
        let variables = operation
            .variables
            .iter()
            .map(|variable| {
                let name = variable.name.as_str().into();
                let variable = Variable {
                    field_type: variable.ty.as_ref().into(),
                    default_value: variable
                        .default_value
                        .as_ref()
                        .and_then(|v| parse_hir_value(v)),
                };
                (name, variable)
            })
            .collect();

        Ok(Operation {
            selection_set,
            name,
            type_name,
            variables,
            kind,
        })
    }

    pub(crate) fn kind(&self) -> &OperationKind {
        &self.kind
    }
}

pub(crate) fn parse_hir_value(value: &executable::Value) -> Option<Value> {
    match value {
        executable::Value::Variable(_) => None,
        executable::Value::Int(value) => Some(value.as_str().parse::<i64>().ok()?.into()),
        executable::Value::Float(value) => Some(value.try_to_f64().ok()?.into()),
        executable::Value::Null => Some(Value::Null),
        executable::Value::String(value) => Some(value.as_str().into()),
        executable::Value::Boolean(value) => Some((*value).into()),
        executable::Value::Enum(value) => Some(value.as_str().into()),
        executable::Value::List(value) => value.iter().map(|v| parse_hir_value(v)).collect(),
        executable::Value::Object(value) => value
            .iter()
            .map(|(k, v)| Some((k.as_str(), parse_hir_value(v)?)))
            .collect(),
    }
}

#[cfg(test)]
mod tests;
