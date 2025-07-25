use events::EventOn;
use opentelemetry::KeyValue;
use opentelemetry::Value;
use opentelemetry::baggage::BaggageExt;
use opentelemetry::trace::TraceContextExt;
use opentelemetry::trace::TraceId;
use paste::paste;
use tower::BoxError;
use tracing::Span;

use super::otel::OpenTelemetrySpanExt;
use super::otlp::TelemetryDataKind;
use crate::Context;
use crate::plugins::telemetry::config::AttributeValue;
use crate::plugins::telemetry::config_new::attributes::DefaultAttributeRequirementLevel;

/// These modules contain a new config structure for telemetry that will progressively move to
pub(crate) mod attributes;
pub(crate) mod conditions;

pub(crate) mod cache;
mod conditional;
pub(crate) mod connector;
pub(crate) mod cost;
pub(crate) mod events;
pub(crate) mod extendable;
pub(crate) mod graphql;
pub(crate) mod http_common;
pub(crate) mod http_server;
pub(crate) mod instruments;
pub(crate) mod logging;
pub(crate) mod router;
pub(crate) mod selectors;
pub(crate) mod spans;
pub(crate) mod subgraph;
pub(crate) mod supergraph;

pub(crate) trait Selectors<Request, Response, EventResponse> {
    fn on_request(&self, request: &Request) -> Vec<KeyValue>;
    fn on_response(&self, response: &Response) -> Vec<KeyValue>;
    fn on_response_event(&self, _response: &EventResponse, _ctx: &Context) -> Vec<KeyValue> {
        Vec::with_capacity(0)
    }
    fn on_error(&self, error: &BoxError, ctx: &Context) -> Vec<KeyValue>;
    fn on_response_field(
        &self,
        _attrs: &mut Vec<KeyValue>,
        _ty: &apollo_compiler::executable::NamedType,
        _field: &apollo_compiler::executable::Field,
        _value: &serde_json_bytes::Value,
        _ctx: &Context,
    ) {
    }
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Stage {
    Request,
    Response,
    ResponseEvent,
    ResponseField,
    Error,
    Drop,
}

impl std::fmt::Display for Stage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Stage::Request => write!(f, "request"),
            Stage::Response => write!(f, "response"),
            Stage::ResponseEvent => write!(f, "response_event"),
            Stage::ResponseField => write!(f, "response_field"),
            Stage::Error => write!(f, "error"),
            Stage::Drop => write!(f, "drop"),
        }
    }
}

impl From<EventOn> for Stage {
    fn from(value: EventOn) -> Self {
        match value {
            EventOn::Request => Self::Request,
            EventOn::Response => Self::Response,
            EventOn::EventResponse => Self::ResponseEvent,
            EventOn::Error => Self::Error,
        }
    }
}

pub(crate) trait Selector: std::fmt::Debug {
    type Request;
    type Response;
    type EventResponse;

    fn on_request(&self, request: &Self::Request) -> Option<opentelemetry::Value>;
    fn on_response(&self, response: &Self::Response) -> Option<opentelemetry::Value>;
    fn on_response_event(
        &self,
        _response: &Self::EventResponse,
        _ctx: &Context,
    ) -> Option<opentelemetry::Value> {
        None
    }
    fn on_error(&self, error: &BoxError, ctx: &Context) -> Option<opentelemetry::Value>;
    fn on_response_field(
        &self,
        _ty: &apollo_compiler::executable::NamedType,
        _field: &apollo_compiler::executable::Field,
        _value: &serde_json_bytes::Value,
        _ctx: &Context,
    ) -> Option<opentelemetry::Value> {
        None
    }

    fn on_drop(&self) -> Option<Value> {
        None
    }

    fn is_active(&self, stage: Stage) -> bool;
}

pub(crate) trait DefaultForLevel {
    /// Don't call this directly, use `defaults_for_levels` instead.
    fn defaults_for_level(
        &mut self,
        requirement_level: DefaultAttributeRequirementLevel,
        kind: TelemetryDataKind,
    );
    fn defaults_for_levels(
        &mut self,
        requirement_level: DefaultAttributeRequirementLevel,
        kind: TelemetryDataKind,
    ) {
        match requirement_level {
            DefaultAttributeRequirementLevel::None => {}
            DefaultAttributeRequirementLevel::Required => {
                self.defaults_for_level(DefaultAttributeRequirementLevel::Required, kind)
            }
            DefaultAttributeRequirementLevel::Recommended => {
                self.defaults_for_level(DefaultAttributeRequirementLevel::Required, kind);
                self.defaults_for_level(DefaultAttributeRequirementLevel::Recommended, kind);
            }
        }
    }
}

pub(crate) trait DatadogId {
    fn to_datadog(&self) -> String;
}
impl DatadogId for TraceId {
    fn to_datadog(&self) -> String {
        let mut bytes: [u8; 8] = Default::default();
        bytes.copy_from_slice(&self.to_bytes()[8..16]);
        u64::from_be_bytes(bytes).to_string()
    }
}

pub(crate) fn trace_id() -> Option<TraceId> {
    let context = Span::current().context();
    let span = context.span();
    let span_context = span.span_context();
    if span_context.is_valid() {
        Some(span_context.trace_id())
    } else {
        crate::tracer::TraceId::current().map(|trace_id| TraceId::from(trace_id.to_u128()))
    }
}

pub(crate) fn get_baggage(key: &str) -> Option<opentelemetry::Value> {
    let context = Span::current().context();
    let baggage = context.baggage();
    baggage.get(key).cloned()
}

pub(crate) trait ToOtelValue {
    fn maybe_to_otel_value(&self) -> Option<opentelemetry::Value>;
}
impl ToOtelValue for &Option<AttributeValue> {
    fn maybe_to_otel_value(&self) -> Option<opentelemetry::Value> {
        self.as_ref().map(|v| v.clone().into())
    }
}

macro_rules! impl_to_otel_value {
    ($type:ty) => {
        paste! {
            impl ToOtelValue for $type {
                fn maybe_to_otel_value(&self) -> Option<opentelemetry::Value> {
                    match self {
                        $type::Bool(value) => Some((*value).into()),
                        $type::Number(value) if value.is_f64() => {
                            value.as_f64().map(opentelemetry::Value::from)
                        }
                        $type::Number(value) if value.is_i64() => {
                            value.as_i64().map(opentelemetry::Value::from)
                        }
                        $type::String(value) => Some(value.as_str().to_string().into()),
                        $type::Array(value) => {
                            // Arrays must be uniform in value
                            if value.iter().all(|v| v.is_i64()) {
                                Some(opentelemetry::Value::Array(opentelemetry::Array::I64(
                                    value.iter().filter_map(|v| v.as_i64()).collect(),
                                )))
                            } else if value.iter().all(|v| v.is_f64()) {
                                Some(opentelemetry::Value::Array(opentelemetry::Array::F64(
                                    value.iter().filter_map(|v| v.as_f64()).collect(),
                                )))
                            } else if value.iter().all(|v| v.is_boolean()) {
                                Some(opentelemetry::Value::Array(opentelemetry::Array::Bool(
                                    value.iter().filter_map(|v| v.as_bool()).collect(),
                                )))
                            } else if value.iter().all(|v| v.is_object()) {
                                Some(opentelemetry::Value::Array(opentelemetry::Array::String(
                                    value.iter().map(|v| v.to_string().into()).collect(),
                                )))
                            } else if value.iter().all(|v| v.is_string()) {
                                Some(opentelemetry::Value::Array(opentelemetry::Array::String(
                                    value
                                        .iter()
                                        .filter_map(|v| v.as_str())
                                        .map(|v| v.to_string().into())
                                        .collect(),
                                )))
                            } else {
                                Some(serde_json::to_string(value).ok()?.into())
                            }
                        }
                        $type::Object(value) => Some(serde_json::to_string(value).ok()?.into()),
                        _ => None
                    }
                }
            }
        }
    };
}
impl_to_otel_value!(serde_json_bytes::Value);
impl_to_otel_value!(serde_json::Value);

impl From<opentelemetry::Value> for AttributeValue {
    fn from(value: opentelemetry::Value) -> Self {
        match value {
            opentelemetry::Value::Bool(v) => AttributeValue::Bool(v),
            opentelemetry::Value::I64(v) => AttributeValue::I64(v),
            opentelemetry::Value::F64(v) => AttributeValue::F64(v),
            opentelemetry::Value::String(v) => AttributeValue::String(v.into()),
            opentelemetry::Value::Array(v) => AttributeValue::Array(v.into()),
        }
    }
}

#[cfg(test)]
mod test {
    use std::sync::OnceLock;

    use apollo_compiler::Node;
    use apollo_compiler::ast::FieldDefinition;
    use apollo_compiler::ast::NamedType;
    use apollo_compiler::executable::Field;
    use apollo_compiler::name;
    use opentelemetry::Context;
    use opentelemetry::StringValue;
    use opentelemetry::trace::SpanContext;
    use opentelemetry::trace::SpanId;
    use opentelemetry::trace::TraceContextExt;
    use opentelemetry::trace::TraceFlags;
    use opentelemetry::trace::TraceId;
    use opentelemetry::trace::TraceState;
    use serde_json::json;
    use tracing::span;
    use tracing_subscriber::layer::SubscriberExt;

    use crate::plugins::telemetry::config_new::DatadogId;
    use crate::plugins::telemetry::config_new::ToOtelValue;
    use crate::plugins::telemetry::config_new::trace_id;
    use crate::plugins::telemetry::otel;

    pub(crate) fn field() -> &'static Field {
        static FIELD: OnceLock<Field> = OnceLock::new();
        FIELD.get_or_init(|| {
            Field::new(
                name!("field_name"),
                Node::new(FieldDefinition {
                    description: None,
                    name: name!("field_name"),
                    arguments: vec![],
                    ty: apollo_compiler::ty!(field_type),
                    directives: Default::default(),
                }),
            )
        })
    }
    pub(crate) fn ty() -> NamedType {
        name!("type_name")
    }

    #[test]
    fn dd_convert() {
        let trace_id = TraceId::from_hex("234e10d9e749a0a19e94ac0e4a896aee").unwrap();
        let dd_id = trace_id.to_datadog();
        assert_eq!(dd_id, "11426947331925830382");
    }

    #[test]
    fn test_trace_id() {
        // Create a span with a trace ID
        let subscriber = tracing_subscriber::registry().with(otel::layer());
        tracing::subscriber::with_default(subscriber, || {
            let span_context = SpanContext::new(
                TraceId::from(42),
                SpanId::from_u64(42),
                TraceFlags::default(),
                false,
                TraceState::default(),
            );
            let _context = Context::current()
                .with_remote_span_context(span_context)
                .attach();
            let span = span!(tracing::Level::INFO, "test");
            let _guard = span.enter();
            let trace_id = trace_id();
            assert_eq!(trace_id, Some(TraceId::from_u128(42)));
        });
    }

    #[test]
    fn test_baggage() {
        // Create a span with a trace ID
        let subscriber = tracing_subscriber::registry().with(otel::layer());
        tracing::subscriber::with_default(subscriber, || {
            let span_context = SpanContext::new(
                TraceId::from_u128(42),
                SpanId::from_u64(42),
                TraceFlags::default(),
                false,
                TraceState::default(),
            );
            let _context = Context::current()
                .with_remote_span_context(span_context)
                .attach();
            let span = span!(tracing::Level::INFO, "test");
            let _guard = span.enter();
            let trace_id = trace_id();
            assert_eq!(trace_id, Some(TraceId::from_u128(42)));
        });
    }

    #[test]
    fn maybe_to_otel_value() {
        assert_eq!(json!("string").maybe_to_otel_value(), Some("string".into()));
        assert_eq!(json!(1).maybe_to_otel_value(), Some(1.into()));
        assert_eq!(json!(1.0).maybe_to_otel_value(), Some(1.0.into()));
        assert_eq!(json!(true).maybe_to_otel_value(), Some(true.into()));

        assert_eq!(
            json!(["string1", "string2"]).maybe_to_otel_value(),
            Some(opentelemetry::Value::Array(
                vec![
                    StringValue::from("string1".to_string()),
                    StringValue::from("string2".to_string())
                ]
                .into()
            ))
        );
        assert_eq!(
            json!([1, 2]).maybe_to_otel_value(),
            Some(opentelemetry::Value::Array(vec![1i64, 2i64].into()))
        );
        assert_eq!(
            json!([1.0, 2.0]).maybe_to_otel_value(),
            Some(opentelemetry::Value::Array(vec![1.0, 2.0].into()))
        );
        assert_eq!(
            json!([true, false]).maybe_to_otel_value(),
            Some(opentelemetry::Value::Array(vec![true, false].into()))
        );

        // Arrays must be uniform
        assert_eq!(
            json!(["1", 1]).maybe_to_otel_value(),
            Some(r#"["1",1]"#.to_string().into())
        );
        assert_eq!(
            json!([1.0, 1]).maybe_to_otel_value(),
            Some(r#"[1.0,1]"#.to_string().into())
        );
    }
}
