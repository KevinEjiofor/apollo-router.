use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt;
use std::io;

use opentelemetry::Array;
use opentelemetry::Key;
use opentelemetry::KeyValue;
use opentelemetry::Value;
use opentelemetry_sdk::Resource;
use serde::ser::SerializeMap;
use serde::ser::Serializer as _;
use serde_json::Serializer;
use tracing_core::Event;
use tracing_core::Subscriber;
use tracing_serde::AsSerde;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::registry::SpanRef;

use super::APOLLO_CONNECTOR_PREFIX;
use super::APOLLO_PRIVATE_PREFIX;
use super::EXCLUDED_ATTRIBUTES;
use super::EventFormatter;
use super::get_trace_and_span_id;
use crate::plugins::telemetry::config::AttributeValue;
use crate::plugins::telemetry::config::TraceIdFormat;
use crate::plugins::telemetry::config_new::logging::DisplayTraceIdFormat;
use crate::plugins::telemetry::config_new::logging::JsonFormat;
use crate::plugins::telemetry::dynamic_attribute::EventAttributes;
use crate::plugins::telemetry::dynamic_attribute::LogAttributes;
use crate::plugins::telemetry::formatters::to_list;
use crate::plugins::telemetry::otel::OtelData;

#[derive(Debug)]
pub(crate) struct Json {
    config: JsonFormat,
    resource: Vec<(String, serde_json::Value)>,
    excluded_attributes: HashSet<&'static str>,
}

impl Json {
    pub(crate) fn new(resource: Resource, config: JsonFormat) -> Self {
        Self {
            resource: to_list(resource),
            config,
            excluded_attributes: EXCLUDED_ATTRIBUTES.into(),
        }
    }
}

impl Default for Json {
    fn default() -> Self {
        Self {
            config: Default::default(),
            resource: Default::default(),
            excluded_attributes: EXCLUDED_ATTRIBUTES.into(),
        }
    }
}

struct SerializableResources<'a>(&'a Vec<(String, serde_json::Value)>);

impl serde::ser::Serialize for SerializableResources<'_> {
    fn serialize<Ser>(&self, serializer_o: Ser) -> Result<Ser::Ok, Ser::Error>
    where
        Ser: serde::ser::Serializer,
    {
        let mut serializer = serializer_o.serialize_map(Some(self.0.len()))?;

        for (key, val) in self.0 {
            serializer.serialize_entry(key, val)?;
        }

        serializer.end()
    }
}

struct SerializableContext<'a, 'b, Span>(Option<SpanRef<'a, Span>>, &'b HashSet<&'static str>)
where
    Span: Subscriber + for<'lookup> tracing_subscriber::registry::LookupSpan<'lookup>;

impl<Span> serde::ser::Serialize for SerializableContext<'_, '_, Span>
where
    Span: Subscriber + for<'lookup> tracing_subscriber::registry::LookupSpan<'lookup>,
{
    fn serialize<Ser>(&self, serializer_o: Ser) -> Result<Ser::Ok, Ser::Error>
    where
        Ser: serde::ser::Serializer,
    {
        use serde::ser::SerializeSeq;
        let mut serializer = serializer_o.serialize_seq(None)?;

        if let Some(leaf_span) = &self.0 {
            for span in leaf_span.scope().from_root() {
                // TODO: Here in the future we could try to memoize parent spans of the current span to not re serialize eveything if another log happens in the same span
                serializer.serialize_element(&SerializableSpan(&span, self.1))?;
            }
        }

        serializer.end()
    }
}

struct SerializableSpan<'a, 'b, Span>(
    &'b tracing_subscriber::registry::SpanRef<'a, Span>,
    &'b HashSet<&'static str>,
)
where
    Span: for<'lookup> tracing_subscriber::registry::LookupSpan<'lookup>;

impl<Span> serde::ser::Serialize for SerializableSpan<'_, '_, Span>
where
    Span: for<'lookup> tracing_subscriber::registry::LookupSpan<'lookup>,
{
    fn serialize<Ser>(&self, serializer: Ser) -> Result<Ser::Ok, Ser::Error>
    where
        Ser: serde::ser::Serializer,
    {
        let mut serializer = serializer.serialize_map(None)?;

        let ext = self.0.extensions();

        // Get otel attributes
        {
            let otel_attributes = ext
                .get::<OtelData>()
                .and_then(|otel_data| otel_data.builder.attributes.as_ref());
            if let Some(otel_attributes) = otel_attributes {
                for kv in otel_attributes.iter().filter(|kv| {
                    let key_name = kv.key.as_str();
                    !key_name.starts_with(APOLLO_PRIVATE_PREFIX)
                        && !key_name.starts_with(APOLLO_CONNECTOR_PREFIX)
                        && !self.1.contains(&key_name)
                }) {
                    serializer.serialize_entry(kv.key.as_str(), &kv.value.as_str())?;
                }
            }
        }
        // Get custom dynamic attributes
        {
            let custom_attributes = ext.get::<LogAttributes>().map(|attrs| attrs.attributes());
            if let Some(custom_attributes) = custom_attributes {
                #[cfg(test)]
                let custom_attributes: Vec<&opentelemetry::KeyValue> = {
                    let mut my_custom_attributes: Vec<&opentelemetry::KeyValue> =
                        custom_attributes.iter().collect();
                    my_custom_attributes.sort_by_key(|kv| &kv.key);
                    my_custom_attributes
                };
                for kv in custom_attributes.iter().filter(|kv| {
                    let key_name = kv.key.as_str();
                    !key_name.starts_with(APOLLO_PRIVATE_PREFIX)
                        && !key_name.starts_with(APOLLO_CONNECTOR_PREFIX)
                        && !self.1.contains(&key_name)
                }) {
                    match &kv.value {
                        Value::Bool(value) => {
                            serializer.serialize_entry(kv.key.as_str(), value)?;
                        }
                        Value::I64(value) => {
                            serializer.serialize_entry(kv.key.as_str(), value)?;
                        }
                        Value::F64(value) => {
                            serializer.serialize_entry(kv.key.as_str(), value)?;
                        }
                        Value::String(value) => {
                            serializer.serialize_entry(kv.key.as_str(), value.as_str())?;
                        }
                        Value::Array(Array::Bool(array)) => {
                            serializer.serialize_entry(kv.key.as_str(), array)?;
                        }
                        Value::Array(Array::I64(array)) => {
                            serializer.serialize_entry(kv.key.as_str(), array)?;
                        }
                        Value::Array(Array::F64(array)) => {
                            serializer.serialize_entry(kv.key.as_str(), array)?;
                        }
                        Value::Array(Array::String(array)) => {
                            let array = array.iter().map(|a| a.as_str()).collect::<Vec<_>>();
                            serializer.serialize_entry(kv.key.as_str(), &array)?;
                        }
                    }
                }
            }
        }

        serializer.serialize_entry("name", self.0.metadata().name())?;
        serializer.end()
    }
}

impl<S> EventFormatter<S> for Json
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    fn format_event<W>(
        &self,
        ctx: &Context<'_, S>,
        writer: &mut W,
        event: &Event<'_>,
    ) -> fmt::Result
    where
        S: Subscriber + for<'a> LookupSpan<'a>,
        W: std::fmt::Write,
    {
        let meta = event.metadata();

        let mut visit = || {
            let mut serializer = Serializer::new(WriteAdaptor::new(writer));

            let mut serializer = serializer.serialize_map(None)?;

            if self.config.display_timestamp {
                #[cfg(test)]
                {
                    serializer.serialize_entry("timestamp", "[timestamp]")?;
                }
                #[cfg(not(test))]
                {
                    let timestamp = time::OffsetDateTime::now_utc()
                        .format(&time::format_description::well_known::Iso8601::DEFAULT)
                        .map_err(|e| serde::ser::Error::custom(e.to_string()))?;
                    serializer.serialize_entry("timestamp", &timestamp)?;
                }
            }

            if self.config.display_level {
                serializer.serialize_entry("level", &meta.level().as_serde())?;
            }

            let current_span = event
                .parent()
                .and_then(|id| ctx.span(id))
                .or_else(|| ctx.lookup_current());

            if let Some(ref span) = current_span {
                if let Some((trace_id, span_id)) = get_trace_and_span_id(span) {
                    let trace_id = match self.config.display_trace_id {
                        DisplayTraceIdFormat::Bool(true)
                        | DisplayTraceIdFormat::TraceIdFormat(TraceIdFormat::Hexadecimal)
                        | DisplayTraceIdFormat::TraceIdFormat(TraceIdFormat::OpenTelemetry) => {
                            Some(TraceIdFormat::Hexadecimal.format(trace_id))
                        }
                        DisplayTraceIdFormat::TraceIdFormat(TraceIdFormat::Decimal) => {
                            Some(TraceIdFormat::Decimal.format(trace_id))
                        }
                        DisplayTraceIdFormat::TraceIdFormat(TraceIdFormat::Datadog) => {
                            Some(TraceIdFormat::Datadog.format(trace_id))
                        }
                        DisplayTraceIdFormat::TraceIdFormat(TraceIdFormat::Uuid) => {
                            Some(TraceIdFormat::Uuid.format(trace_id))
                        }
                        DisplayTraceIdFormat::Bool(false) => None,
                    };
                    if let Some(trace_id) = trace_id {
                        serializer
                            .serialize_entry("trace_id", &trace_id)
                            .unwrap_or(());
                    }
                    if self.config.display_span_id {
                        serializer
                            .serialize_entry("span_id", &span_id.to_string())
                            .unwrap_or(());
                    }
                };
                let event_attributes = {
                    let mut extensions = span.extensions_mut();
                    let otel_data = extensions.get_mut::<OtelData>();
                    let attrs = otel_data.and_then(|od| od.event_attributes.take());
                    match attrs {
                        Some(attrs) => Some(attrs),
                        None => {
                            let event_attributes = extensions.get_mut::<EventAttributes>();
                            event_attributes.map(|event_attributes| {
                                event_attributes
                                    .take()
                                    .into_iter()
                                    .map(|KeyValue { key, value }| (key, value))
                                    .collect()
                            })
                        }
                    }
                };
                if let Some(event_attributes) = event_attributes {
                    for (key, value) in event_attributes {
                        serializer.serialize_entry(key.as_str(), &AttributeValue::from(value))?;
                    }
                }
            }

            if !self.config.span_attributes.is_empty() {
                for (key, value) in
                    extract_span_attributes(ctx.lookup_current(), &self.config.span_attributes)
                {
                    serializer.serialize_entry(key.as_str(), &AttributeValue::from(value))?;
                }
            }

            let mut visitor = tracing_serde::SerdeMapVisitor::new(serializer);
            event.record(&mut visitor);

            serializer = visitor.take_serializer()?;

            if self.config.display_target {
                serializer.serialize_entry("target", meta.target())?;
            }

            if self.config.display_filename {
                if let Some(filename) = meta.file() {
                    serializer.serialize_entry("filename", filename)?;
                }
            }

            if self.config.display_line_number {
                if let Some(line_number) = meta.line() {
                    serializer.serialize_entry("line_number", &line_number)?;
                }
            }
            if self.config.display_current_span {
                if let Some(ref span) = current_span {
                    serializer
                        .serialize_entry("span", &SerializableSpan(span, &self.excluded_attributes))
                        .unwrap_or(());
                }
            }

            // dd.trace_id is special. It must appear as a root attribute on log lines, so we need to extract it from the root span.
            // We're just going to assume if it's there then we should output it, as the user will have to have configured it to be there.
            if let Some(span) = &current_span {
                if let Some(dd_trace_id) = extract_dd_trace_id(span) {
                    serializer.serialize_entry("dd.trace_id", &dd_trace_id)?;
                }
            }
            if self.config.display_span_list && current_span.is_some() {
                serializer.serialize_entry(
                    "spans",
                    &SerializableContext(ctx.lookup_current(), &self.excluded_attributes),
                )?;
            }

            if self.config.display_resource {
                serializer.serialize_entry("resource", &SerializableResources(&self.resource))?;
            }

            serializer.end()
        };

        visit().map_err(|_| fmt::Error)?;
        writeln!(writer)
    }
}

fn extract_dd_trace_id<'a, 'b, T: LookupSpan<'a>>(span: &SpanRef<'a, T>) -> Option<String> {
    let mut dd_trace_id = None;
    let mut root = span.scope().from_root();
    if let Some(root_span) = root.next() {
        let ext = root_span.extensions();
        // Extract dd_trace_id, this could be in otel data or log attributes
        if let Some(otel_data) = ext.get::<OtelData>() {
            if let Some(attributes) = otel_data.builder.attributes.as_ref() {
                if let Some(kv) = attributes
                    .iter()
                    .find(|kv| kv.key.as_str() == "dd.trace_id")
                {
                    dd_trace_id = Some(kv.value.to_string());
                }
            }
        };

        if dd_trace_id.is_none() {
            if let Some(log_attr) = ext.get::<LogAttributes>() {
                if let Some(kv) = log_attr
                    .attributes()
                    .iter()
                    .find(|kv| kv.key.as_str() == "dd.trace_id")
                {
                    dd_trace_id = Some(kv.value.to_string());
                }
            }
        }
    }
    dd_trace_id
}

fn extract_span_attributes<'a, 'b, Span>(
    current: Option<SpanRef<'a, Span>>,
    include_attributes: &HashSet<String>,
) -> HashMap<Key, Value>
where
    Span: for<'lookup> tracing_subscriber::registry::LookupSpan<'lookup>,
{
    let mut attributes = HashMap::new();
    if let Some(leaf_span) = &current {
        for span in leaf_span.scope().from_root() {
            let ext = span.extensions();

            // Get otel attributes
            {
                let otel_attributes = ext
                    .get::<OtelData>()
                    .and_then(|otel_data| otel_data.builder.attributes.as_ref());
                if let Some(otel_attributes) = otel_attributes {
                    attributes.extend(
                        otel_attributes
                            .iter()
                            .filter(|kv| {
                                let key_name = kv.key.as_str();
                                !key_name.starts_with(APOLLO_PRIVATE_PREFIX)
                                    && !key_name.starts_with(APOLLO_CONNECTOR_PREFIX)
                                    && include_attributes.contains(key_name)
                            })
                            .map(|kv| (kv.key.clone(), kv.value.clone())),
                    );
                }
            }
            // Get custom dynamic attributes
            {
                let custom_attributes = ext.get::<LogAttributes>().map(|attrs| attrs.attributes());
                if let Some(custom_attributes) = custom_attributes {
                    #[cfg(test)]
                    let custom_attributes: Vec<&opentelemetry::KeyValue> = {
                        let mut my_custom_attributes: Vec<&opentelemetry::KeyValue> =
                            custom_attributes.iter().collect();
                        my_custom_attributes.sort_by_key(|kv| &kv.key);
                        my_custom_attributes
                    };
                    #[allow(clippy::into_iter_on_ref)]
                    attributes.extend(
                        custom_attributes
                            .into_iter()
                            .filter(|kv| {
                                let key_name = kv.key.as_str();
                                !key_name.starts_with(APOLLO_PRIVATE_PREFIX)
                                    && !key_name.starts_with(APOLLO_CONNECTOR_PREFIX)
                                    && include_attributes.contains(key_name)
                            })
                            .map(|kv| (kv.key.clone(), kv.value.clone())),
                    );
                }
            }
        }
    }

    attributes
}

struct WriteAdaptor<'a> {
    fmt_write: &'a mut dyn fmt::Write,
}

impl<'a> WriteAdaptor<'a> {
    fn new(fmt_write: &'a mut dyn fmt::Write) -> Self {
        Self { fmt_write }
    }
}

impl io::Write for WriteAdaptor<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let s =
            std::str::from_utf8(buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        self.fmt_write.write_str(s).map_err(io::Error::other)?;

        Ok(s.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl fmt::Debug for WriteAdaptor<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad("WriteAdaptor { .. }")
    }
}

#[cfg(test)]
mod test {
    use tracing::subscriber;
    use tracing_core::Event;
    use tracing_core::Subscriber;
    use tracing_subscriber::Layer;
    use tracing_subscriber::Registry;
    use tracing_subscriber::layer::Context;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::registry::LookupSpan;

    use crate::plugins::telemetry::dynamic_attribute::DynAttributeLayer;
    use crate::plugins::telemetry::dynamic_attribute::SpanDynAttribute;
    use crate::plugins::telemetry::formatters::json::extract_dd_trace_id;
    use crate::plugins::telemetry::otel;

    struct RequiresDatadogLayer;
    impl<S> Layer<S> for RequiresDatadogLayer
    where
        S: Subscriber + for<'lookup> LookupSpan<'lookup>,
    {
        fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
            let current_span = event
                .parent()
                .and_then(|id| ctx.span(id))
                .or_else(|| ctx.lookup_current())
                .expect("current span expected");
            let extracted = extract_dd_trace_id(&current_span);
            assert_eq!(extracted, Some("1234".to_string()), "should have trace id");
        }
    }

    #[test]
    fn test_extract_dd_trace_id_span_attribute() {
        subscriber::with_default(
            Registry::default()
                .with(RequiresDatadogLayer)
                .with(otel::layer().force_sampling()),
            || {
                let root_span = tracing::info_span!("root", dd.trace_id = "1234");
                let _root_span = root_span.enter();
                tracing::info!("test");
            },
        );
    }

    #[test]
    fn test_extract_dd_trace_id_dyn_attribute() {
        subscriber::with_default(
            Registry::default()
                .with(RequiresDatadogLayer)
                .with(DynAttributeLayer)
                .with(otel::layer().force_sampling()),
            || {
                let root_span = tracing::info_span!("root");
                root_span.set_span_dyn_attribute("dd.trace_id".into(), "1234".into());
                let _root_span = root_span.enter();
                tracing::info!("test");
            },
        );
    }

    #[test]
    #[should_panic(expected = "should have trace id")]
    fn test_missing_dd_attribute() {
        subscriber::with_default(
            Registry::default()
                .with(RequiresDatadogLayer)
                .with(DynAttributeLayer)
                .with(otel::layer().force_sampling()),
            || {
                let root_span = tracing::info_span!("root");
                let _root_span = root_span.enter();
                tracing::info!("test");
            },
        );
    }
}
