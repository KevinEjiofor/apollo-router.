use std::sync::LazyLock;

use apollo_compiler::ast::Value;
use apollo_compiler::name;
use apollo_compiler::schema::Type;

use crate::schema::FederationSchema;

#[allow(dead_code)]
#[derive(Clone, Copy)]
pub(crate) enum ArgumentCompositionStrategy {
    Max,
    Min,
    Sum,
    Intersection,
    Union,
}

pub(crate) static MAX_STRATEGY: LazyLock<MaxArgumentCompositionStrategy> =
    LazyLock::new(MaxArgumentCompositionStrategy::new);
pub(crate) static MIN_STRATEGY: LazyLock<MinArgumentCompositionStrategy> =
    LazyLock::new(MinArgumentCompositionStrategy::new);
pub(crate) static SUM_STRATEGY: LazyLock<SumArgumentCompositionStrategy> =
    LazyLock::new(SumArgumentCompositionStrategy::new);
pub(crate) static INTERSECTION_STRATEGY: LazyLock<IntersectionArgumentCompositionStrategy> =
    LazyLock::new(|| IntersectionArgumentCompositionStrategy {});
pub(crate) static UNION_STRATEGY: LazyLock<UnionArgumentCompositionStrategy> =
    LazyLock::new(|| UnionArgumentCompositionStrategy {});

impl ArgumentCompositionStrategy {
    pub(crate) fn name(&self) -> &str {
        match self {
            Self::Max => MAX_STRATEGY.name(),
            Self::Min => MIN_STRATEGY.name(),
            Self::Sum => SUM_STRATEGY.name(),
            Self::Intersection => INTERSECTION_STRATEGY.name(),
            Self::Union => UNION_STRATEGY.name(),
        }
    }

    pub(crate) fn is_type_supported(
        &self,
        schema: &FederationSchema,
        ty: &Type,
    ) -> Result<(), String> {
        match self {
            Self::Max => MAX_STRATEGY.is_type_supported(schema, ty),
            Self::Min => MIN_STRATEGY.is_type_supported(schema, ty),
            Self::Sum => SUM_STRATEGY.is_type_supported(schema, ty),
            Self::Intersection => INTERSECTION_STRATEGY.is_type_supported(schema, ty),
            Self::Union => UNION_STRATEGY.is_type_supported(schema, ty),
        }
    }

    pub(crate) fn merge_values(&self, values: &[Value]) -> Value {
        match self {
            Self::Max => MAX_STRATEGY.merge_values(values),
            Self::Min => MIN_STRATEGY.merge_values(values),
            Self::Sum => SUM_STRATEGY.merge_values(values),
            Self::Intersection => INTERSECTION_STRATEGY.merge_values(values),
            Self::Union => UNION_STRATEGY.merge_values(values),
        }
    }
}

//////////////////////////////////////////////////////////////////////////////
// Implementation of ArgumentCompositionStrategy's

/// Argument composition strategy for directives.
///
/// Determines how to compose the merged argument value from directive
/// applications across subgraph schemas.
pub(crate) trait ArgumentComposition {
    /// The name of the validator.
    fn name(&self) -> &str;
    /// Is the type `ty`  supported by this validator?
    fn is_type_supported(&self, schema: &FederationSchema, ty: &Type) -> Result<(), String>;
    /// Assumes that schemas are validated by `is_type_supported`.
    fn merge_values(&self, values: &[Value]) -> Value;
}

#[derive(Clone)]
pub(crate) struct FixedTypeSupportValidator {
    pub(crate) supported_types: Vec<Type>,
}

impl FixedTypeSupportValidator {
    fn is_type_supported(&self, _schema: &FederationSchema, ty: &Type) -> Result<(), String> {
        if self.supported_types.contains(ty) {
            return Ok(());
        }

        let expected_types: Vec<String> = self
            .supported_types
            .iter()
            .map(|ty| ty.to_string())
            .collect();
        Err(format!("type(s) {}", expected_types.join(", ")))
    }
}

fn int_type() -> Type {
    Type::Named(name!("Int"))
}

fn support_any_non_null_array(ty: &Type) -> Result<(), String> {
    if !ty.is_non_null() || !ty.is_list() {
        Err("non-nullable list types of any type".to_string())
    } else {
        Ok(())
    }
}

// MAX
#[derive(Clone)]
pub(crate) struct MaxArgumentCompositionStrategy {
    validator: FixedTypeSupportValidator,
}

impl MaxArgumentCompositionStrategy {
    fn new() -> Self {
        Self {
            validator: FixedTypeSupportValidator {
                supported_types: vec![int_type().non_null()],
            },
        }
    }
}

impl ArgumentComposition for MaxArgumentCompositionStrategy {
    fn name(&self) -> &str {
        "MAX"
    }

    fn is_type_supported(&self, schema: &FederationSchema, ty: &Type) -> Result<(), String> {
        self.validator.is_type_supported(schema, ty)
    }

    // TODO: check if this needs to be an Result<Value> to avoid the panic!()
    // https://apollographql.atlassian.net/browse/FED-170
    fn merge_values(&self, values: &[Value]) -> Value {
        values
            .iter()
            .map(|val| match val {
                Value::Int(i) => i.try_to_i32().unwrap(),
                _ => panic!("Unexpected value type"),
            })
            .max()
            .unwrap_or_default()
            .into()
    }
}

// MIN
#[derive(Clone)]
pub(crate) struct MinArgumentCompositionStrategy {
    validator: FixedTypeSupportValidator,
}

impl MinArgumentCompositionStrategy {
    fn new() -> Self {
        Self {
            validator: FixedTypeSupportValidator {
                supported_types: vec![int_type().non_null()],
            },
        }
    }
}

impl ArgumentComposition for MinArgumentCompositionStrategy {
    fn name(&self) -> &str {
        "MIN"
    }

    fn is_type_supported(&self, schema: &FederationSchema, ty: &Type) -> Result<(), String> {
        self.validator.is_type_supported(schema, ty)
    }

    // TODO: check if this needs to be an Result<Value> to avoid the panic!()
    // https://apollographql.atlassian.net/browse/FED-170
    fn merge_values(&self, values: &[Value]) -> Value {
        values
            .iter()
            .map(|val| match val {
                Value::Int(i) => i.try_to_i32().unwrap(),
                _ => panic!("Unexpected value type"),
            })
            .min()
            .unwrap_or_default()
            .into()
    }
}

// SUM
#[derive(Clone)]
pub(crate) struct SumArgumentCompositionStrategy {
    validator: FixedTypeSupportValidator,
}

impl SumArgumentCompositionStrategy {
    fn new() -> Self {
        Self {
            validator: FixedTypeSupportValidator {
                supported_types: vec![int_type().non_null()],
            },
        }
    }
}

impl ArgumentComposition for SumArgumentCompositionStrategy {
    fn name(&self) -> &str {
        "SUM"
    }

    fn is_type_supported(&self, schema: &FederationSchema, ty: &Type) -> Result<(), String> {
        self.validator.is_type_supported(schema, ty)
    }

    // TODO: check if this needs to be an Result<Value> to avoid the panic!()
    // https://apollographql.atlassian.net/browse/FED-170
    fn merge_values(&self, values: &[Value]) -> Value {
        values
            .iter()
            .map(|val| match val {
                Value::Int(i) => i.try_to_i32().unwrap(),
                _ => panic!("Unexpected value type"),
            })
            .sum::<i32>()
            .into()
    }
}

// INTERSECTION
#[derive(Clone)]
pub(crate) struct IntersectionArgumentCompositionStrategy {}

impl ArgumentComposition for IntersectionArgumentCompositionStrategy {
    fn name(&self) -> &str {
        "INTERSECTION"
    }

    fn is_type_supported(&self, _schema: &FederationSchema, ty: &Type) -> Result<(), String> {
        support_any_non_null_array(ty)
    }

    // TODO: check if this needs to be an Result<Value> to avoid the panic!()
    // https://apollographql.atlassian.net/browse/FED-170
    fn merge_values(&self, values: &[Value]) -> Value {
        // Each item in `values` must be a Value::List(...).
        values
            .split_first()
            .map(|(first, rest)| {
                let first_ls = first.as_list().unwrap();
                // Not a super efficient implementation, but we don't expect large problem sizes.
                let mut result = first_ls.to_vec();
                for val in rest {
                    let val_ls = val.as_list().unwrap();
                    result.retain(|result_item| val_ls.contains(result_item));
                }
                Value::List(result)
            })
            .unwrap()
    }
}

// UNION
#[derive(Clone)]
pub(crate) struct UnionArgumentCompositionStrategy {}

impl ArgumentComposition for UnionArgumentCompositionStrategy {
    fn name(&self) -> &str {
        "UNION"
    }

    fn is_type_supported(&self, _schema: &FederationSchema, ty: &Type) -> Result<(), String> {
        support_any_non_null_array(ty)
    }

    // TODO: check if this needs to be an Result<Value> to avoid the panic!()
    // https://apollographql.atlassian.net/browse/FED-170
    fn merge_values(&self, values: &[Value]) -> Value {
        // Each item in `values` must be a Value::List(...).
        // Not a super efficient implementation, but we don't expect large problem sizes.
        let mut result = Vec::new();
        for val in values {
            let val_ls = val.as_list().unwrap();
            for x in val_ls.iter() {
                if !result.contains(x) {
                    result.push(x.clone());
                }
            }
        }
        Value::List(result)
    }
}
