use std::collections::HashMap;
use std::str::FromStr;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use jsonpath_rust::JsonPathInst;
use opentelemetry::KeyValue;
use opentelemetry::metrics::Meter;
use opentelemetry::metrics::MeterProvider;
use parking_lot::Mutex;
use paste::paste;
use serde_json::Value;

use crate::Configuration;
use crate::metrics::meter_provider;
use crate::uplink::license_enforcement::LicenseState;

type InstrumentMap = HashMap<String, (u64, HashMap<String, opentelemetry::Value>)>;

pub(crate) struct Metrics {
    _instruments: Vec<opentelemetry::metrics::ObservableGauge<u64>>,
}

struct InstrumentData {
    data: InstrumentMap,
    meter: Meter,
}

impl Default for InstrumentData {
    fn default() -> Self {
        InstrumentData {
            meter: meter_provider().meter("apollo/router"),
            data: Default::default(),
        }
    }
}

impl Metrics {
    pub(crate) fn new(configuration: &Configuration, license_state: &LicenseState) -> Metrics {
        let mut data = InstrumentData::default();

        // Env variables and unit tests don't mix.
        data.populate_cli_instrument();
        data.populate_config_instruments(
            configuration
                .validated_yaml
                .as_ref()
                .unwrap_or(&serde_json::Value::Null),
        );
        data.populate_license_instrument(license_state);
        data.populate_user_plugins_instrument(configuration);

        data.into()
    }
}

impl InstrumentData {
    fn get_first_key_from_path(
        attributes: &mut HashMap<String, opentelemetry::Value>,
        attr_name: &str,
        path: &str,
        value: &Value,
    ) {
        if let Ok(json_path) = JsonPathInst::from_str(path) {
            let value_at_path = json_path.find_slice(value).into_iter().next();
            if let Some(Value::Object(children)) = value_at_path.as_deref() {
                if let Some(first_key) = children.keys().next() {
                    attributes.insert(attr_name.to_string(), first_key.clone().into());
                }
            }
        }
    }

    fn get_value_from_path(
        attributes: &mut HashMap<String, opentelemetry::Value>,
        attr_name: &str,
        path: &str,
        value: &Value,
    ) {
        let attr_name = attr_name.to_string();
        match JsonPathInst::from_str(path)
            .expect("json path must be valid")
            .find_slice(value)
            .into_iter()
            .next()
            .as_deref()
        {
            // If the value is an object we can only state that it is set, but not what it is set to.
            Some(Value::Object(_value)) => {
                attributes.insert(attr_name, true.into());
            }
            Some(Value::Array(value)) if !value.is_empty() => {
                attributes.insert(attr_name, true.into());
            }
            // Scalars can be logged as is.
            Some(Value::Number(value)) if value.is_f64() => {
                attributes.insert(attr_name, value.as_f64().expect("checked, qed").into());
            }
            Some(Value::Number(value)) if value.is_i64() => {
                attributes.insert(attr_name, value.as_i64().expect("checked, qed").into());
            }
            // Note that we convert u64 to i64 because opentelemetry does not support u64 as an attribute.
            Some(Value::Number(value)) => {
                attributes.insert(
                    attr_name,
                    (value.as_u64().expect("checked, qed") as i64).into(),
                );
            }
            Some(Value::String(value)) => {
                attributes.insert(attr_name, value.clone().into());
            }
            Some(Value::Bool(value)) => {
                attributes.insert(attr_name, (*value).into());
            }

            // If the value is not set we don't specify the attribute.
            None => {
                attributes.insert(attr_name, false.into());
            }

            _ => {}
        };
    }

    pub(crate) fn populate_config_instruments(&mut self, yaml: &serde_json::Value) {
        /// This macro will query the config json for a primary metric and optionally metric attributes.
        ///
        /// The reason we use jsonpath_rust is that jsonpath_lib has correctness issues and looks abandoned.
        /// We should consider converting the rest of the codebase to use jsonpath_rust.
        ///
        /// Example usage:
        /// ```rust,ignore
        /// populate_config_instrument!(
        ///     apollo.router.config.authorization, // The metric name
        ///     "$.authorization", // The path into the config
        ///     opt.require_authentication, // The name of the attribute
        ///     "$[?(@.require_authentication == true)]" // The path for the attribute relative to the metric
        /// );
        /// ```
        macro_rules! populate_config_instrument {
            ($($metric:ident).+, $path:literal) => {
                let instrument_name = stringify!($($metric).+).to_string();
                self.data.entry(instrument_name.clone()).or_insert_with(|| {
                    if JsonPathInst::from_str($path).expect("json path must be valid").find_slice(yaml).first().is_some() {
                        (1, HashMap::new())
                    }
                    else {
                        (0, HashMap::new())
                    }
                });
            };
            ($($metric:ident).+, $path:literal, $($($attr:ident).+, $attr_path:literal),+) => {
                let instrument_name = stringify!($($metric).+).to_string();
                self.data.entry(instrument_name).or_insert_with(|| {
                    if let Some(value) = JsonPathInst::from_str($path).expect("json path must be valid").find_slice(yaml).first() {
                        paste!{
                            let mut attributes = HashMap::new();
                            $(
                            let attr_name = stringify!([<$($attr __ )+>]).to_string();
                            Self::get_value_from_path(&mut attributes, &attr_name, $attr_path, value);)+
                            (1, attributes)
                        }
                    }
                    else {
                        paste!{
                            let mut attributes = HashMap::new();
                            $(
                                let attr_name = stringify!([<$($attr __ )+>]).to_string();
                                attributes.insert(attr_name, false.into());
                            )+
                            (0, attributes)
                        }
                    }
                });

            };
        }

        populate_config_instrument!(
            apollo.router.config.defer,
            "$.supergraph[?(@.defer_support == true)]"
        );
        populate_config_instrument!(
            apollo.router.config.authentication.jwt,
            "$.authentication[?(@..jwt)]"
        );
        populate_config_instrument!(
            apollo.router.config.authentication.aws.sigv4,
            "$.authentication[?(@.subgraph..aws_sig_v4)]"
        );
        populate_config_instrument!(
            apollo.router.config.authorization,
            "$.authorization",
            opt.require_authentication,
            "$[?(@.require_authentication == true)]",
            opt.directives,
            "$.directives[?(@.enabled == true)]"
        );
        populate_config_instrument!(
            apollo.router.config.coprocessor,
            "$.coprocessor",
            opt.router.request,
            "$.router.request",
            opt.router.response,
            "$.router.response",
            // Note that supergraph is not supported yet so these will always be empty
            opt.supergraph.request,
            "$.supergraph.response",
            opt.supergraph.response,
            "$.supergraph.request",
            opt.subgraph.request,
            "$.subgraph..request",
            opt.subgraph.response,
            "$.subgraph..response"
        );
        populate_config_instrument!(
            apollo.router.config.rhai,
            "$.rhai",
            opt.scripts,
            "$[?(@.scripts)]",
            opt.main,
            "$[?(@.main)]"
        );
        populate_config_instrument!(
            apollo.router.config.persisted_queries,
            "$.persisted_queries[?(@.enabled == true)]",
            opt.log_unknown,
            "$[?(@.log_unknown == true)]",
            opt.safelist.require_id,
            "$[?(@.safelist.require_id == true)]",
            opt.safelist.enabled,
            "$[?(@.safelist.enabled == true)]"
        );

        populate_config_instrument!(
            apollo.router.config.subscriptions,
            "$.subscription[?(@.enabled == true)]",
            opt.mode.passthrough,
            "$.mode.passthrough",
            opt.mode.callback,
            "$.mode.callback",
            opt.deduplication,
            "$[?(@.enable_deduplication == true)]",
            opt.max_opened,
            "$[?(@.max_opened_subscriptions)]",
            opt.queue_capacity,
            "$[?(@.queue_capacity)]"
        );

        populate_config_instrument!(
            apollo.router.config.limits,
            "$.limits",
            opt.operation.max_depth,
            "$[?(@.max_depth)]",
            opt.operation.max_aliases,
            "$[?(@.max_aliases)]",
            opt.operation.max_height,
            "$[?(@.max_height)]",
            opt.operation.max_root_fields,
            "$[?(@.max_root_fields)]",
            opt.operation.warn_only,
            "$[?(@.warn_only)]",
            opt.parser.max_recursion,
            "$[?(@.parser_max_recursion)]",
            opt.parser.max_tokens,
            "$[?(@.parser_max_tokens)]",
            opt.request.max_size,
            "$[?(@.http_max_request_bytes)]"
        );
        populate_config_instrument!(
            apollo.router.config.apq,
            "$.apq[?(@.enabled==true)]",
            opt.router.cache.redis,
            "$.router.cache.redis",
            opt.router.cache.in_memory,
            "$.router.cache.in_memory",
            opt.subgraph,
            "$.subgraph..enabled[?(@ == true)]"
        );
        populate_config_instrument!(
            apollo.router.config.tls,
            "$.tls",
            opt.router.tls.server,
            "$.supergraph",
            opt.router.tls.subgraph.ca_override,
            "$[?(@.subgraph..certificate_authorities)]",
            opt.router.tls.subgraph.client_authentication,
            "$.subgraph..client_authentication"
        );
        populate_config_instrument!(
            apollo.router.config.traffic_shaping,
            "$.traffic_shaping",
            opt.router.timeout,
            "$$[?(@.router.timeout)]",
            opt.router.rate_limit,
            "$.router.global_rate_limit",
            opt.subgraph.timeout,
            "$[?(@.all.timeout || @.subgraphs..timeout)]",
            opt.subgraph.rate_limit,
            "$[?(@.all.global_rate_limit || @.subgraphs..global_rate_limit)]",
            opt.subgraph.http2,
            "$[?(@.all.experimental_http2 == 'enable' || @.all.experimental_http2 == 'http2only' || @.subgraphs..experimental_http2 == 'enable' || @.subgraphs..experimental_http2 == 'http2only')]",
            opt.subgraph.compression,
            "$[?(@.all.compression || @.subgraphs..compression)]",
            opt.subgraph.deduplicate_query,
            "$[?(@.all.deduplicate_query == true || @.subgraphs..deduplicate_query == true)]"
        );

        populate_config_instrument!(
            apollo.router.config.entity_cache,
            "$.preview_entity_cache",
            opt.enabled,
            "$[?(@.enabled)]",
            opt.subgraph.enabled,
            "$[?(@.subgraph.all.enabled)]",
            opt.subgraph.enabled,
            "$[?(@.subgraph.subgraphs..enabled)]",
            opt.subgraph.ttl,
            "$[?(@.subgraph.all.ttl || @.subgraph.subgraphs..ttl)]",
            opt.subgraph.invalidation.enabled,
            "$[?(@.subgraph.all.invalidation.enabled || @.subgraph.subgraphs..invalidation.enabled)]"
        );
        populate_config_instrument!(
            apollo.router.config.telemetry,
            "$..telemetry[?(@..endpoint || @.metrics.prometheus.enabled == true)]",
            opt.metrics.otlp,
            "$..metrics.otlp[?(@.endpoint)]",
            opt.metrics.prometheus,
            "$..metrics.prometheus[?(@.enabled==true)]",
            opt.tracing.otlp,
            "$..tracing.otlp[?(@.enabled==true)]",
            opt.tracing.datadog,
            "$..tracing.datadog[?(@.enabled==true)]",
            opt.tracing.zipkin,
            "$..tracing.zipkin[?(@.enabled==true)]",
            opt.events,
            "$..events",
            opt.events.router,
            "$..events.router",
            opt.events.supergraph,
            "$..events.supergraph",
            opt.events.subgraph,
            "$..events.subgraph",
            opt.events.connector,
            "$..events.connector",
            opt.instruments,
            "$..instruments",
            opt.instruments.router,
            "$..instruments.router",
            opt.instruments.supergraph,
            "$..instruments.supergraph",
            opt.instruments.subgraph,
            "$..instruments.subgraph",
            opt.instruments.connector,
            "$..instruments.connector",
            opt.instruments.graphql,
            "$..instruments.graphql",
            opt.instruments.default_attribute_requirement_level,
            "$..instruments.default_attribute_requirement_level",
            opt.spans,
            "$..spans",
            opt.spans.mode,
            "$..spans.mode",
            opt.spans.default_attribute_requirement_level,
            "$..spans.default_attribute_requirement_level",
            opt.spans.router,
            "$..spans.router",
            opt.spans.subgraph,
            "$..spans.subgraph",
            opt.spans.supergraph,
            "$..spans.supergraph"
        );

        populate_config_instrument!(
            apollo.router.config.batching,
            "$.batching[?(@.enabled == true)]",
            opt.mode,
            "$.mode"
        );

        populate_config_instrument!(
            apollo.router.config.file_uploads.multipart,
            "$.preview_file_uploads[?(@.enabled == true)].protocols.multipart[?(@.enabled == true)]",
            opt.limits.max_file_size,
            "$.limits.max_file_size",
            opt.limits.max_files,
            "$.limits.max_files"
        );

        populate_config_instrument!(
            apollo.router.config.demand_control,
            "$.demand_control[?(@.enabled == true)]",
            opt.mode,
            "$.mode"
        );

        populate_config_instrument!(
            apollo.router.config.apollo_telemetry_options,
            "$.telemetry.apollo",
            opt.signature_normalization_algorithm,
            "$.signature_normalization_algorithm",
            opt.metrics_reference_mode,
            "$.metrics_reference_mode",
            opt.errors.preview_extended_error_metrics,
            "$.errors.preview_extended_error_metrics"
        );

        populate_config_instrument!(
            apollo.router.config.connectors,
            "$.connectors",
            opt.debug_extensions,
            "$[?(@.debug_extensions == true)]",
            opt.expose_sources_in_context,
            "$[?(@.expose_sources_in_context == true)]",
            opt.max_requests_per_operation_per_source,
            "$[?(@.max_requests_per_operation_per_source)]",
            opt.subgraph.config,
            "$[?(@.subgraphs..['$config'])]",
            opt.source.override_url,
            "$[?(@.subgraphs..sources..override_url)]",
            opt.source.max_requests_per_operation,
            "$[?(@.subgraphs..sources..max_requests_per_operation)]"
        );

        // We need to update the entry we just made because the selected strategy is a named object in the config.
        // The jsonpath spec doesn't include a utility for getting the keys out of an object, so we do it manually.
        if let Some((_, demand_control_attributes)) =
            self.data.get_mut("apollo.router.config.demand_control")
        {
            Self::get_first_key_from_path(
                demand_control_attributes,
                "opt.strategy",
                "$.demand_control[?(@.enabled == true)].strategy",
                yaml,
            );
        }
    }

    fn populate_cli_instrument(&mut self) {
        fn mutex_is_some(mutex: &Mutex<Option<String>>) -> opentelemetry::Value {
            if cfg!(test) {
                true.into()
            } else {
                mutex.lock().is_some().into()
            }
        }
        fn atomic_is_true(atomic: &AtomicBool) -> opentelemetry::Value {
            if cfg!(test) {
                true.into()
            } else {
                atomic.load(Ordering::Relaxed).into()
            }
        }
        let mut attributes = HashMap::new();
        attributes.insert(
            "opt.apollo.key".to_string(),
            mutex_is_some(&crate::services::APOLLO_KEY),
        );
        attributes.insert(
            "opt.apollo.graph_ref".to_string(),
            mutex_is_some(&crate::services::APOLLO_GRAPH_REF),
        );
        attributes.insert(
            "opt.apollo.license".to_string(),
            atomic_is_true(&crate::executable::APOLLO_ROUTER_LICENCE_IS_SET),
        );
        attributes.insert(
            "opt.apollo.license.path".to_string(),
            atomic_is_true(&crate::executable::APOLLO_ROUTER_LICENCE_PATH_IS_SET),
        );
        attributes.insert(
            "opt.apollo.supergraph.urls".to_string(),
            atomic_is_true(&crate::executable::APOLLO_ROUTER_SUPERGRAPH_URLS_IS_SET),
        );
        attributes.insert(
            "opt.apollo.supergraph.path".to_string(),
            atomic_is_true(&crate::executable::APOLLO_ROUTER_SUPERGRAPH_PATH_IS_SET),
        );
        attributes.insert(
            "opt.apollo.dev".to_string(),
            atomic_is_true(&crate::executable::APOLLO_ROUTER_DEV_MODE),
        );
        attributes.insert(
            "opt.security.recursive_selections".to_string(),
            crate::services::layers::query_analysis::recursive_selections_check_enabled().into(),
        );
        attributes.insert(
            "opt.security.non_local_selections".to_string(),
            crate::query_planner::query_planner_service::non_local_selections_check_enabled()
                .into(),
        );

        self.data
            .insert("apollo.router.config.env".to_string(), (1, attributes));
    }

    pub(crate) fn populate_license_instrument(&mut self, license_state: &LicenseState) {
        self.data.insert(
            "apollo.router.lifecycle.license".to_string(),
            (
                1,
                [(
                    "license.state".to_string(),
                    license_state.to_string().into(),
                )]
                .into(),
            ),
        );
    }

    pub(crate) fn populate_user_plugins_instrument(&mut self, configuration: &Configuration) {
        self.data.insert(
            "apollo.router.config.custom_plugins".to_string(),
            (
                configuration
                    .plugins
                    .plugins
                    .as_ref()
                    .map(|configuration| {
                        configuration
                            .keys()
                            .filter(|k| !k.starts_with("cloud_router."))
                            .count()
                    })
                    .unwrap_or_default() as u64,
                [].into(),
            ),
        );
    }
}

impl From<InstrumentData> for Metrics {
    fn from(data: InstrumentData) -> Self {
        Metrics {
            _instruments: data
                .data
                .into_iter()
                .map(|(metric_name, (value, attributes))| {
                    let attributes: Vec<_> = attributes
                        .into_iter()
                        .map(|(k, v)| KeyValue::new(k.trim_end_matches("__").replace("__", "."), v))
                        .collect();
                    data.meter
                        .u64_observable_gauge(metric_name)
                        .with_callback(move |observer| {
                            observer.observe(value, &attributes);
                        })
                        .init()
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod test {
    use rust_embed::RustEmbed;
    use serde_json::json;

    use crate::configuration::metrics::InstrumentData;
    use crate::configuration::metrics::Metrics;
    use crate::uplink::license_enforcement::LicenseLimits;
    use crate::uplink::license_enforcement::LicenseState;

    #[derive(RustEmbed)]
    #[folder = "src/configuration/testdata/metrics"]
    struct Asset;

    #[test]
    fn test_metrics() {
        for file_name in Asset::iter() {
            let source = Asset::get(&file_name).expect("test file must exist");
            let input = std::str::from_utf8(&source.data)
                .expect("expected utf8")
                .to_string();
            let yaml = &serde_yaml::from_str::<serde_json::Value>(&input)
                .expect("config must be valid yaml");

            let mut data = InstrumentData::default();
            data.populate_config_instruments(yaml);
            let _metrics: Metrics = data.into();
            assert_non_zero_metrics_snapshot!(file_name);
        }
    }

    #[test]
    fn test_env_metrics() {
        let mut data = InstrumentData::default();
        data.populate_cli_instrument();
        let _metrics: Metrics = data.into();
        assert_non_zero_metrics_snapshot!();
    }

    #[test]
    fn test_license_warn() {
        let mut data = InstrumentData::default();
        data.populate_license_instrument(&LicenseState::LicensedWarn {
            limits: Some(LicenseLimits::default()),
        });
        let _metrics: Metrics = data.into();
        assert_non_zero_metrics_snapshot!();
    }

    #[test]
    fn test_license_halt() {
        let mut data = InstrumentData::default();
        data.populate_license_instrument(&LicenseState::LicensedHalt {
            limits: Some(LicenseLimits::default()),
        });
        let _metrics: Metrics = data.into();
        assert_non_zero_metrics_snapshot!();
    }

    #[test]
    fn test_custom_plugin() {
        let mut configuration = crate::Configuration::default();
        let mut custom_plugins = serde_json::Map::new();
        custom_plugins.insert("name".to_string(), json!("test"));
        configuration.plugins.plugins = Some(custom_plugins);
        let mut data = InstrumentData::default();
        data.populate_user_plugins_instrument(&configuration);
        let _metrics: Metrics = data.into();
        assert_non_zero_metrics_snapshot!();
    }

    #[test]
    fn test_ignore_cloud_router_plugins() {
        let mut configuration = crate::Configuration::default();
        let mut custom_plugins = serde_json::Map::new();
        custom_plugins.insert("name".to_string(), json!("test"));
        custom_plugins.insert("cloud_router.".to_string(), json!("test"));
        configuration.plugins.plugins = Some(custom_plugins);
        let mut data = InstrumentData::default();
        data.populate_user_plugins_instrument(&configuration);
        let _metrics: Metrics = data.into();
        assert_non_zero_metrics_snapshot!();
    }
}
