use apollo_compiler::ExecutableDocument;
use apollo_compiler::Node;
use apollo_compiler::executable::Field;
use apollo_compiler::executable::Operation;
use apollo_compiler::executable::Selection;
use apollo_compiler::schema::Value;
use serde_json::Number;
use serde_json_bytes::ByteString;
use serde_json_bytes::Map;
use serde_json_bytes::Value as JSONValue;
use tower::BoxError;

use super::ENTITIES;
use super::MakeRequestError;

pub(super) fn field_arguments_map(
    field: &Node<Field>,
    variables: &Map<ByteString, JSONValue>,
) -> Result<Map<ByteString, JSONValue>, BoxError> {
    let mut arguments = Map::new();

    for argument in field.arguments.iter() {
        arguments.insert(
            argument.name.as_str(),
            argument_value_to_json(&argument.value, variables)?,
        );
    }

    for argument_def in field.definition.arguments.iter() {
        if let Some(value) = argument_def.default_value.as_ref() {
            if !arguments.contains_key(argument_def.name.as_str()) {
                arguments.insert(
                    argument_def.name.as_str(),
                    argument_value_to_json(value, variables).map_err(|err| {
                        format!(
                            "failed to convert default value on {}({}:) to json: {}",
                            field.definition.name, argument_def.name, err
                        )
                    })?,
                );
            }
        }
    }

    Ok(arguments)
}

pub(super) fn argument_value_to_json(
    value: &apollo_compiler::ast::Value,
    variables: &Map<ByteString, JSONValue>,
) -> Result<JSONValue, BoxError> {
    match value {
        Value::Null => Ok(JSONValue::Null),
        Value::Enum(e) => Ok(JSONValue::String(e.as_str().into())),
        Value::Variable(name) => variables.get(name.as_str()).cloned().ok_or_else(|| {
            BoxError::from(format!(
                "variable {name} used in operation but not defined in variables"
            ))
        }),
        Value::String(s) => Ok(JSONValue::String(s.as_str().into())),
        Value::Float(f) => Ok(JSONValue::Number(
            Number::from_f64(
                f.try_to_f64()
                    .map_err(|_| BoxError::from("Failed to parse float"))?,
            )
            .ok_or_else(|| BoxError::from("Failed to parse float"))?,
        )),
        Value::Int(i) => Ok(JSONValue::Number(Number::from(
            i.try_to_i32().map_err(|_| "Failed to parse int")?,
        ))),
        Value::Boolean(b) => Ok(JSONValue::Bool(*b)),
        Value::List(l) => Ok(JSONValue::Array(
            l.iter()
                .map(|v| argument_value_to_json(v, variables))
                .collect::<Result<Vec<_>, _>>()?,
        )),
        Value::Object(o) => Ok(JSONValue::Object(
            o.iter()
                .map(|(k, v)| argument_value_to_json(v, variables).map(|v| (k.as_str().into(), v)))
                .collect::<Result<Map<_, _>, _>>()?,
        )),
    }
}

/// Looks for _entities near the root of the operation. Also looks for
/// __typename within the _entities selection — if it was selected, then we
/// don't have a interfaceObject query.
pub(super) fn get_entity_fields<'a>(
    document: &'a ExecutableDocument,
    op: &'a Node<Operation>,
) -> Result<(&'a Node<Field>, bool), MakeRequestError> {
    use MakeRequestError::*;

    let root_field = op
        .selection_set
        .selections
        .iter()
        .find_map(|s| match s {
            Selection::Field(f) if f.name == ENTITIES => Some(f),
            _ => None,
        })
        .ok_or_else(|| InvalidOperation("missing entities root field".into()))?;

    let mut typename_requested = false;

    for selection in root_field.selection_set.selections.iter() {
        match selection {
            Selection::Field(f) => {
                if f.name == "__typename" {
                    typename_requested = true;
                }
            }
            Selection::FragmentSpread(f) => {
                let fragment = document
                    .fragments
                    .get(f.fragment_name.as_str())
                    .ok_or_else(|| InvalidOperation("missing fragment".into()))?;
                for selection in fragment.selection_set.selections.iter() {
                    match selection {
                        Selection::Field(f) => {
                            if f.name == "__typename" {
                                typename_requested = true;
                            }
                        }
                        Selection::FragmentSpread(_) | Selection::InlineFragment(_) => {}
                    }
                }
            }
            Selection::InlineFragment(f) => {
                for selection in f.selection_set.selections.iter() {
                    match selection {
                        Selection::Field(f) => {
                            if f.name == "__typename" {
                                typename_requested = true;
                            }
                        }
                        Selection::FragmentSpread(_) | Selection::InlineFragment(_) => {}
                    }
                }
            }
        }
    }

    Ok((root_field, typename_requested))
}
