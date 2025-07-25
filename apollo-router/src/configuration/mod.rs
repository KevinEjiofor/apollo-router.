//! Logic for loading configuration in to an object model
use std::fmt;
use std::hash::Hash;
use std::io;
use std::io::BufReader;
use std::iter;
use std::net::IpAddr;
use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::num::NonZeroUsize;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use connector::ConnectorConfiguration;
use derivative::Derivative;
use displaydoc::Display;
use itertools::Either;
use itertools::Itertools;
use once_cell::sync::Lazy;
pub(crate) use persisted_queries::PersistedQueries;
pub(crate) use persisted_queries::PersistedQueriesPrewarmQueryPlanCache;
#[cfg(test)]
pub(crate) use persisted_queries::PersistedQueriesSafelist;
use regex::Regex;
use rustls::ServerConfig;
use rustls::pki_types::CertificateDer;
use rustls::pki_types::PrivateKeyDer;
use schemars::JsonSchema;
use schemars::r#gen::SchemaGenerator;
use schemars::schema::ObjectValidation;
use schemars::schema::Schema;
use schemars::schema::SchemaObject;
use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde_json::Map;
use serde_json::Value;
use sha2::Digest;
use thiserror::Error;

use self::cors::Cors;
use self::expansion::Expansion;
pub(crate) use self::experimental::Discussed;
pub(crate) use self::schema::generate_config_schema;
pub(crate) use self::schema::generate_upgrade;
pub(crate) use self::schema::validate_yaml_configuration;
use self::server::Server;
use self::subgraph::SubgraphConfiguration;
use crate::ApolloRouterError;
use crate::cache::DEFAULT_CACHE_CAPACITY;
use crate::configuration::cooperative_cancellation::CooperativeCancellation;
use crate::graphql;
use crate::notification::Notify;
use crate::plugin::plugins;
use crate::plugins::healthcheck::Config as HealthCheck;
#[cfg(test)]
use crate::plugins::healthcheck::test_listen;
use crate::plugins::limits;
use crate::plugins::subscription::APOLLO_SUBSCRIPTION_PLUGIN;
use crate::plugins::subscription::APOLLO_SUBSCRIPTION_PLUGIN_NAME;
use crate::plugins::subscription::SubscriptionConfig;
use crate::uplink::UplinkConfig;

pub(crate) mod connector;
pub(crate) mod cooperative_cancellation;
pub(crate) mod cors;
pub(crate) mod expansion;
mod experimental;
pub(crate) mod metrics;
pub(crate) mod mode;
mod persisted_queries;
pub(crate) mod schema;
pub(crate) mod server;
pub(crate) mod shared;
pub(crate) mod subgraph;
#[cfg(test)]
mod tests;
mod upgrade;
mod yaml;

// TODO: Talk it through with the teams
static HEARTBEAT_TIMEOUT_DURATION_SECONDS: u64 = 15;

static SUPERGRAPH_ENDPOINT_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?P<first_path>.*/)(?P<sub_path>.+)\*$")
        .expect("this regex to check the path is valid")
});

/// Configuration error.
#[derive(Debug, Error, Display)]
#[non_exhaustive]
pub enum ConfigurationError {
    /// could not expand variable: {key}, {cause}
    CannotExpandVariable { key: String, cause: String },
    /// could not expand variable: {key}. Variables must be prefixed with one of '{supported_modes}' followed by '.' e.g. 'env.'
    UnknownExpansionMode {
        key: String,
        supported_modes: String,
    },
    /// unknown plugin {0}
    PluginUnknown(String),
    /// plugin {plugin} could not be configured: {error}
    PluginConfiguration { plugin: String, error: String },
    /// {message}: {error}
    InvalidConfiguration {
        message: &'static str,
        error: String,
    },
    /// could not deserialize configuration: {0}
    DeserializeConfigError(serde_json::Error),

    /// APOLLO_ROUTER_CONFIG_SUPPORTED_MODES must be of the format env,file,... Possible modes are 'env' and 'file'.
    InvalidExpansionModeConfig,

    /// could not migrate configuration: {error}.
    MigrationFailure { error: String },

    /// could not load certificate authorities: {error}
    CertificateAuthorities { error: String },
}

impl From<proteus::Error> for ConfigurationError {
    fn from(error: proteus::Error) -> Self {
        Self::MigrationFailure {
            error: error.to_string(),
        }
    }
}

impl From<proteus::parser::Error> for ConfigurationError {
    fn from(error: proteus::parser::Error) -> Self {
        Self::MigrationFailure {
            error: error.to_string(),
        }
    }
}

/// The configuration for the router.
///
/// Can be created through `serde::Deserialize` from various formats,
/// or inline in Rust code with `serde_json::json!` and `serde_json::from_value`.
#[derive(Clone, Derivative, Serialize, JsonSchema)]
#[derivative(Debug)]
// We can't put a global #[serde(default)] here because of the Default implementation using `from_str` which use deserialize
pub struct Configuration {
    /// The raw configuration value.
    #[serde(skip)]
    pub(crate) validated_yaml: Option<Value>,

    /// Health check configuration
    #[serde(default)]
    pub(crate) health_check: HealthCheck,

    /// Sandbox configuration
    #[serde(default)]
    pub(crate) sandbox: Sandbox,

    /// Homepage configuration
    #[serde(default)]
    pub(crate) homepage: Homepage,

    /// Configuration for the server
    #[serde(default)]
    pub(crate) server: Server,

    /// Configuration for the supergraph
    #[serde(default)]
    pub(crate) supergraph: Supergraph,

    /// Cross origin request headers.
    #[serde(default)]
    pub(crate) cors: Cors,

    #[serde(default)]
    pub(crate) tls: Tls,

    /// Configures automatic persisted queries
    #[serde(default)]
    pub(crate) apq: Apq,

    /// Configures managed persisted queries
    #[serde(default)]
    pub persisted_queries: PersistedQueries,

    /// Configuration for operation limits, parser limits, HTTP limits, etc.
    #[serde(default)]
    pub(crate) limits: limits::Config,

    /// Configuration for chaos testing, trying to reproduce bugs that require uncommon conditions.
    /// You probably don’t want this in production!
    #[serde(default)]
    pub(crate) experimental_chaos: Chaos,

    /// Plugin configuration
    #[serde(default)]
    pub(crate) plugins: UserPlugins,

    /// Built-in plugin configuration. Built in plugins are pushed to the top level of config.
    #[serde(default)]
    #[serde(flatten)]
    pub(crate) apollo_plugins: ApolloPlugins,

    /// Uplink configuration.
    #[serde(skip)]
    pub uplink: Option<UplinkConfig>,

    #[serde(default, skip_serializing, skip_deserializing)]
    pub(crate) notify: Notify<String, graphql::Response>,

    /// Batching configuration.
    #[serde(default)]
    pub(crate) batching: Batching,

    /// Type conditioned fetching configuration.
    #[serde(default)]
    pub(crate) experimental_type_conditioned_fetching: bool,
}

impl PartialEq for Configuration {
    fn eq(&self, other: &Self) -> bool {
        self.validated_yaml == other.validated_yaml
    }
}

impl<'de> serde::Deserialize<'de> for Configuration {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // This intermediate structure will allow us to deserialize a Configuration
        // yet still exercise the Configuration validation function
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct AdHocConfiguration {
            health_check: HealthCheck,
            sandbox: Sandbox,
            homepage: Homepage,
            server: Server,
            supergraph: Supergraph,
            cors: Cors,
            plugins: UserPlugins,
            #[serde(flatten)]
            apollo_plugins: ApolloPlugins,
            tls: Tls,
            apq: Apq,
            persisted_queries: PersistedQueries,
            limits: limits::Config,
            experimental_chaos: Chaos,
            batching: Batching,
            experimental_type_conditioned_fetching: bool,
        }
        let mut ad_hoc: AdHocConfiguration = serde::Deserialize::deserialize(deserializer)?;

        let notify = Configuration::notify(&ad_hoc.apollo_plugins.plugins)
            .map_err(|e| serde::de::Error::custom(e.to_string()))?;

        // Allow the limits plugin to use the configuration from the configuration struct.
        // This means that the limits plugin will get the regular configuration via plugin init.
        ad_hoc.apollo_plugins.plugins.insert(
            "limits".to_string(),
            serde_json::to_value(&ad_hoc.limits).unwrap(),
        );
        ad_hoc.apollo_plugins.plugins.insert(
            "health_check".to_string(),
            serde_json::to_value(&ad_hoc.health_check).unwrap(),
        );

        // Use a struct literal instead of a builder to ensure this is exhaustive
        Configuration {
            health_check: ad_hoc.health_check,
            sandbox: ad_hoc.sandbox,
            homepage: ad_hoc.homepage,
            server: ad_hoc.server,
            supergraph: ad_hoc.supergraph,
            cors: ad_hoc.cors,
            tls: ad_hoc.tls,
            apq: ad_hoc.apq,
            persisted_queries: ad_hoc.persisted_queries,
            limits: ad_hoc.limits,
            experimental_chaos: ad_hoc.experimental_chaos,
            experimental_type_conditioned_fetching: ad_hoc.experimental_type_conditioned_fetching,
            plugins: ad_hoc.plugins,
            apollo_plugins: ad_hoc.apollo_plugins,
            batching: ad_hoc.batching,

            // serde(skip)
            notify,
            uplink: None,
            validated_yaml: None,
        }
        .validate()
        .map_err(|e| serde::de::Error::custom(e.to_string()))
    }
}

pub(crate) const APOLLO_PLUGIN_PREFIX: &str = "apollo.";

fn default_graphql_listen() -> ListenAddr {
    SocketAddr::from_str("127.0.0.1:4000").unwrap().into()
}

#[cfg(test)]
#[buildstructor::buildstructor]
impl Configuration {
    #[builder]
    pub(crate) fn new(
        supergraph: Option<Supergraph>,
        health_check: Option<HealthCheck>,
        sandbox: Option<Sandbox>,
        homepage: Option<Homepage>,
        cors: Option<Cors>,
        plugins: Map<String, Value>,
        apollo_plugins: Map<String, Value>,
        tls: Option<Tls>,
        apq: Option<Apq>,
        persisted_query: Option<PersistedQueries>,
        operation_limits: Option<limits::Config>,
        chaos: Option<Chaos>,
        uplink: Option<UplinkConfig>,
        experimental_type_conditioned_fetching: Option<bool>,
        batching: Option<Batching>,
        server: Option<Server>,
    ) -> Result<Self, ConfigurationError> {
        let notify = Self::notify(&apollo_plugins)?;

        let conf = Self {
            validated_yaml: Default::default(),
            supergraph: supergraph.unwrap_or_default(),
            server: server.unwrap_or_default(),
            health_check: health_check.unwrap_or_default(),
            sandbox: sandbox.unwrap_or_default(),
            homepage: homepage.unwrap_or_default(),
            cors: cors.unwrap_or_default(),
            apq: apq.unwrap_or_default(),
            persisted_queries: persisted_query.unwrap_or_default(),
            limits: operation_limits.unwrap_or_default(),
            experimental_chaos: chaos.unwrap_or_default(),
            plugins: UserPlugins {
                plugins: Some(plugins),
            },
            apollo_plugins: ApolloPlugins {
                plugins: apollo_plugins,
            },
            tls: tls.unwrap_or_default(),
            uplink,
            batching: batching.unwrap_or_default(),
            experimental_type_conditioned_fetching: experimental_type_conditioned_fetching
                .unwrap_or_default(),
            notify,
        };

        conf.validate()
    }
}

impl Configuration {
    pub(crate) fn hash(&self) -> String {
        let mut hasher = sha2::Sha256::new();
        let defaulted_raw = self
            .validated_yaml
            .as_ref()
            .map(|s| serde_yaml::to_string(s).expect("config was not serializable"))
            .unwrap_or_default();
        hasher.update(defaulted_raw);
        let hash: String = format!("{:x}", hasher.finalize());
        hash
    }

    fn notify(
        apollo_plugins: &Map<String, Value>,
    ) -> Result<Notify<String, graphql::Response>, ConfigurationError> {
        if cfg!(test) {
            return Ok(Notify::for_tests());
        }
        let notify_queue_cap = match apollo_plugins.get(APOLLO_SUBSCRIPTION_PLUGIN_NAME) {
            Some(plugin_conf) => {
                let conf = serde_json::from_value::<SubscriptionConfig>(plugin_conf.clone())
                    .map_err(|err| ConfigurationError::PluginConfiguration {
                        plugin: APOLLO_SUBSCRIPTION_PLUGIN.to_string(),
                        error: format!("{err:?}"),
                    })?;
                conf.queue_capacity
            }
            None => None,
        };
        Ok(Notify::builder()
            .and_queue_size(notify_queue_cap)
            .ttl(Duration::from_secs(HEARTBEAT_TIMEOUT_DURATION_SECONDS))
            .heartbeat_error_message(
                graphql::Response::builder()
                .errors(vec![
                    graphql::Error::builder()
                    .message("the connection has been closed because it hasn't heartbeat for a while")
                    .extension_code("SUBSCRIPTION_HEARTBEAT_ERROR")
                    .build()
                ])
                .build()
            ).build())
    }

    pub(crate) fn rust_query_planner_config(
        &self,
    ) -> apollo_federation::query_plan::query_planner::QueryPlannerConfig {
        use apollo_federation::query_plan::query_planner::QueryPlanIncrementalDeliveryConfig;
        use apollo_federation::query_plan::query_planner::QueryPlannerConfig;
        use apollo_federation::query_plan::query_planner::QueryPlannerDebugConfig;

        let max_evaluated_plans = self
            .supergraph
            .query_planning
            .experimental_plans_limit
            // Fails if experimental_plans_limit is zero; use our default.
            .and_then(NonZeroU32::new)
            .unwrap_or(NonZeroU32::new(10_000).expect("it is not zero"));

        QueryPlannerConfig {
            subgraph_graphql_validation: false,
            generate_query_fragments: self.supergraph.generate_query_fragments,
            incremental_delivery: QueryPlanIncrementalDeliveryConfig {
                enable_defer: self.supergraph.defer_support,
            },
            type_conditioned_fetching: self.experimental_type_conditioned_fetching,
            debug: QueryPlannerDebugConfig {
                max_evaluated_plans,
                paths_limit: self.supergraph.query_planning.experimental_paths_limit,
            },
        }
    }
}

impl Default for Configuration {
    fn default() -> Self {
        // We want to trigger all defaulting logic so don't use the raw builder.
        Configuration::from_str("").expect("default configuration must be valid")
    }
}

#[cfg(test)]
#[buildstructor::buildstructor]
impl Configuration {
    #[builder]
    pub(crate) fn fake_new(
        supergraph: Option<Supergraph>,
        health_check: Option<HealthCheck>,
        sandbox: Option<Sandbox>,
        homepage: Option<Homepage>,
        cors: Option<Cors>,
        plugins: Map<String, Value>,
        apollo_plugins: Map<String, Value>,
        tls: Option<Tls>,
        notify: Option<Notify<String, graphql::Response>>,
        apq: Option<Apq>,
        persisted_query: Option<PersistedQueries>,
        operation_limits: Option<limits::Config>,
        chaos: Option<Chaos>,
        uplink: Option<UplinkConfig>,
        batching: Option<Batching>,
        experimental_type_conditioned_fetching: Option<bool>,
        server: Option<Server>,
    ) -> Result<Self, ConfigurationError> {
        let configuration = Self {
            validated_yaml: Default::default(),
            server: server.unwrap_or_default(),
            supergraph: supergraph.unwrap_or_else(|| Supergraph::fake_builder().build()),
            health_check: health_check.unwrap_or_else(|| HealthCheck::builder().build()),
            sandbox: sandbox.unwrap_or_else(|| Sandbox::fake_builder().build()),
            homepage: homepage.unwrap_or_else(|| Homepage::fake_builder().build()),
            cors: cors.unwrap_or_default(),
            limits: operation_limits.unwrap_or_default(),
            experimental_chaos: chaos.unwrap_or_default(),
            plugins: UserPlugins {
                plugins: Some(plugins),
            },
            apollo_plugins: ApolloPlugins {
                plugins: apollo_plugins,
            },
            tls: tls.unwrap_or_default(),
            notify: notify.unwrap_or_default(),
            apq: apq.unwrap_or_default(),
            persisted_queries: persisted_query.unwrap_or_default(),
            uplink,
            experimental_type_conditioned_fetching: experimental_type_conditioned_fetching
                .unwrap_or_default(),
            batching: batching.unwrap_or_default(),
        };

        configuration.validate()
    }
}

impl Configuration {
    pub(crate) fn validate(self) -> Result<Self, ConfigurationError> {
        // Sandbox and Homepage cannot be both enabled
        if self.sandbox.enabled && self.homepage.enabled {
            return Err(ConfigurationError::InvalidConfiguration {
                message: "sandbox and homepage cannot be enabled at the same time",
                error: "disable the homepage if you want to enable sandbox".to_string(),
            });
        }
        // Sandbox needs Introspection to be enabled
        if self.sandbox.enabled && !self.supergraph.introspection {
            return Err(ConfigurationError::InvalidConfiguration {
                message: "sandbox requires introspection",
                error: "sandbox needs introspection to be enabled".to_string(),
            });
        }
        if !self.supergraph.path.starts_with('/') {
            return Err(ConfigurationError::InvalidConfiguration {
                message: "invalid 'server.graphql_path' configuration",
                error: format!(
                    "'{}' is invalid, it must be an absolute path and start with '/', you should try with '/{}'",
                    self.supergraph.path, self.supergraph.path
                ),
            });
        }
        if self.supergraph.path.ends_with('*')
            && !self.supergraph.path.ends_with("/*")
            && !SUPERGRAPH_ENDPOINT_REGEX.is_match(&self.supergraph.path)
        {
            return Err(ConfigurationError::InvalidConfiguration {
                message: "invalid 'server.graphql_path' configuration",
                error: format!(
                    "'{}' is invalid, you can only set a wildcard after a '/'",
                    self.supergraph.path
                ),
            });
        }
        if self.supergraph.path.contains("/*/") {
            return Err(ConfigurationError::InvalidConfiguration {
                message: "invalid 'server.graphql_path' configuration",
                error: format!(
                    "'{}' is invalid, if you need to set a path like '/*/graphql' then specify it as a path parameter with a name, for example '/:my_project_key/graphql'",
                    self.supergraph.path
                ),
            });
        }

        // PQs.
        if self.persisted_queries.enabled {
            if self.persisted_queries.safelist.enabled && self.apq.enabled {
                return Err(ConfigurationError::InvalidConfiguration {
                    message: "apqs must be disabled to enable safelisting",
                    error: "either set persisted_queries.safelist.enabled: false or apq.enabled: false in your router yaml configuration".into()
                });
            } else if !self.persisted_queries.safelist.enabled
                && self.persisted_queries.safelist.require_id
            {
                return Err(ConfigurationError::InvalidConfiguration {
                    message: "safelist must be enabled to require IDs",
                    error: "either set persisted_queries.safelist.enabled: true or persisted_queries.safelist.require_id: false in your router yaml configuration".into()
                });
            }
        } else {
            // If the feature isn't enabled, sub-features shouldn't be.
            if self.persisted_queries.safelist.enabled {
                return Err(ConfigurationError::InvalidConfiguration {
                    message: "persisted queries must be enabled to enable safelisting",
                    error: "either set persisted_queries.safelist.enabled: false or persisted_queries.enabled: true in your router yaml configuration".into()
                });
            } else if self.persisted_queries.log_unknown {
                return Err(ConfigurationError::InvalidConfiguration {
                    message: "persisted queries must be enabled to enable logging unknown operations",
                    error: "either set persisted_queries.log_unknown: false or persisted_queries.enabled: true in your router yaml configuration".into()
                });
            }
        }

        Ok(self)
    }
}

/// Parse configuration from a string in YAML syntax
impl FromStr for Configuration {
    type Err = ConfigurationError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        schema::validate_yaml_configuration(s, Expansion::default()?, schema::Mode::Upgrade)?
            .validate()
    }
}

fn gen_schema(
    plugins: schemars::Map<String, Schema>,
    hidden_plugins: Option<schemars::Map<String, Schema>>,
) -> Schema {
    let plugins_object = SchemaObject {
        object: Some(Box::new(ObjectValidation {
            properties: plugins,
            additional_properties: Option::Some(Box::new(Schema::Bool(false))),
            pattern_properties: hidden_plugins
                .unwrap_or_default()
                .into_iter()
                // Wrap plugin name with regex start/end to enforce exact match
                .map(|(k, v)| (format!("^{}$", k), v))
                .collect(),
            ..Default::default()
        })),
        ..Default::default()
    };

    Schema::Object(plugins_object)
}

/// Plugins provided by Apollo.
///
/// These plugins are processed prior to user plugins. Also, their configuration
/// is "hoisted" to the top level of the config rather than being processed
/// under "plugins" as for user plugins.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(transparent)]
pub(crate) struct ApolloPlugins {
    pub(crate) plugins: Map<String, Value>,
}

impl JsonSchema for ApolloPlugins {
    fn schema_name() -> String {
        stringify!(Plugins).to_string()
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        // This is a manual implementation of Plugins schema to allow plugins that have been registered at
        // compile time to be picked up.

        let (plugin_entries, hidden_plugin_entries): (Vec<_>, Vec<_>) = crate::plugin::plugins()
            .sorted_by_key(|factory| factory.name.clone())
            .filter(|factory| factory.name.starts_with(APOLLO_PLUGIN_PREFIX))
            .partition_map(|factory| {
                let key = factory.name[APOLLO_PLUGIN_PREFIX.len()..].to_string();
                let schema = factory.create_schema(generator);
                // Separate any plugins we're hiding
                if factory.hidden_from_config_json_schema {
                    Either::Right((key, schema))
                } else {
                    Either::Left((key, schema))
                }
            });
        gen_schema(
            plugin_entries.into_iter().collect(),
            Some(hidden_plugin_entries.into_iter().collect()),
        )
    }
}

/// Plugins provided by a user.
///
/// These plugins are compiled into a router by and their configuration is performed
/// under the "plugins" section.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(transparent)]
pub(crate) struct UserPlugins {
    pub(crate) plugins: Option<Map<String, Value>>,
}

impl JsonSchema for UserPlugins {
    fn schema_name() -> String {
        stringify!(Plugins).to_string()
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        // This is a manual implementation of Plugins schema to allow plugins that have been registered at
        // compile time to be picked up.

        let plugins = crate::plugin::plugins()
            .sorted_by_key(|factory| factory.name.clone())
            .filter(|factory| !factory.name.starts_with(APOLLO_PLUGIN_PREFIX))
            .map(|factory| (factory.name.to_string(), factory.create_schema(generator)))
            .collect::<schemars::Map<String, Schema>>();
        gen_schema(plugins, None)
    }
}

/// Configuration options pertaining to the supergraph server component.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[serde(default)]
pub(crate) struct Supergraph {
    /// The socket address and port to listen on
    /// Defaults to 127.0.0.1:4000
    pub(crate) listen: ListenAddr,

    /// The timeout for shutting down connections during a router shutdown or a schema reload.
    #[serde(deserialize_with = "humantime_serde::deserialize")]
    #[schemars(with = "String", default = "default_connection_shutdown_timeout")]
    pub(crate) connection_shutdown_timeout: Duration,

    /// The HTTP path on which GraphQL requests will be served.
    /// default: "/"
    pub(crate) path: String,

    /// Enable introspection
    /// Default: false
    pub(crate) introspection: bool,

    /// Enable QP generation of fragments for subgraph requests
    /// Default: true
    pub(crate) generate_query_fragments: bool,

    /// Set to false to disable defer support
    pub(crate) defer_support: bool,

    /// Query planning options
    pub(crate) query_planning: QueryPlanning,

    /// abort request handling when the client drops the connection.
    /// Default: false.
    /// When set to true, some parts of the request pipeline like telemetry will not work properly,
    /// but request handling will stop immediately when the client connection is closed.
    pub(crate) early_cancel: bool,

    /// Log a message if the client closes the connection before the response is sent.
    /// Default: false.
    pub(crate) experimental_log_on_broken_pipe: bool,
}

const fn default_generate_query_fragments() -> bool {
    true
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Auto {
    Auto,
}

fn default_defer_support() -> bool {
    true
}

#[buildstructor::buildstructor]
impl Supergraph {
    #[builder]
    pub(crate) fn new(
        listen: Option<ListenAddr>,
        path: Option<String>,
        connection_shutdown_timeout: Option<Duration>,
        introspection: Option<bool>,
        defer_support: Option<bool>,
        query_planning: Option<QueryPlanning>,
        generate_query_fragments: Option<bool>,
        early_cancel: Option<bool>,
        experimental_log_on_broken_pipe: Option<bool>,
    ) -> Self {
        Self {
            listen: listen.unwrap_or_else(default_graphql_listen),
            path: path.unwrap_or_else(default_graphql_path),
            connection_shutdown_timeout: connection_shutdown_timeout
                .unwrap_or_else(default_connection_shutdown_timeout),
            introspection: introspection.unwrap_or_else(default_graphql_introspection),
            defer_support: defer_support.unwrap_or_else(default_defer_support),
            query_planning: query_planning.unwrap_or_default(),
            generate_query_fragments: generate_query_fragments
                .unwrap_or_else(default_generate_query_fragments),
            early_cancel: early_cancel.unwrap_or_default(),
            experimental_log_on_broken_pipe: experimental_log_on_broken_pipe.unwrap_or_default(),
        }
    }
}

#[cfg(test)]
#[buildstructor::buildstructor]
impl Supergraph {
    #[builder]
    pub(crate) fn fake_new(
        listen: Option<ListenAddr>,
        path: Option<String>,
        connection_shutdown_timeout: Option<Duration>,
        introspection: Option<bool>,
        defer_support: Option<bool>,
        query_planning: Option<QueryPlanning>,
        generate_query_fragments: Option<bool>,
        early_cancel: Option<bool>,
        experimental_log_on_broken_pipe: Option<bool>,
    ) -> Self {
        Self {
            listen: listen.unwrap_or_else(test_listen),
            path: path.unwrap_or_else(default_graphql_path),
            connection_shutdown_timeout: connection_shutdown_timeout
                .unwrap_or_else(default_connection_shutdown_timeout),
            introspection: introspection.unwrap_or_else(default_graphql_introspection),
            defer_support: defer_support.unwrap_or_else(default_defer_support),
            query_planning: query_planning.unwrap_or_default(),
            generate_query_fragments: generate_query_fragments
                .unwrap_or_else(default_generate_query_fragments),
            early_cancel: early_cancel.unwrap_or_default(),
            experimental_log_on_broken_pipe: experimental_log_on_broken_pipe.unwrap_or_default(),
        }
    }
}

impl Default for Supergraph {
    fn default() -> Self {
        Self::builder().build()
    }
}

impl Supergraph {
    /// To sanitize the path for axum router
    pub(crate) fn sanitized_path(&self) -> String {
        let mut path = self.path.clone();
        if self.path.ends_with("/*") {
            // Needed for axum (check the axum docs for more information about wildcards https://docs.rs/axum/latest/axum/struct.Router.html#wildcards)
            path = format!("{}router_extra_path", self.path);
        } else if SUPERGRAPH_ENDPOINT_REGEX.is_match(&self.path) {
            let new_path = SUPERGRAPH_ENDPOINT_REGEX
                .replace(&self.path, "${first_path}${sub_path}{supergraph_route}");
            path = new_path.to_string();
        }

        path
    }
}

/// Router level (APQ) configuration
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct Router {
    #[serde(default)]
    pub(crate) cache: Cache,
}

/// Automatic Persisted Queries (APQ) configuration
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct Apq {
    /// Activates Automatic Persisted Queries (enabled by default)
    pub(crate) enabled: bool,

    pub(crate) router: Router,

    pub(crate) subgraph: SubgraphConfiguration<SubgraphApq>,
}

#[cfg(test)]
#[buildstructor::buildstructor]
impl Apq {
    #[builder]
    pub(crate) fn fake_new(enabled: Option<bool>) -> Self {
        Self {
            enabled: enabled.unwrap_or_else(default_apq),
            ..Default::default()
        }
    }
}

/// Subgraph level Automatic Persisted Queries (APQ) configuration
#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct SubgraphApq {
    /// Enable
    pub(crate) enabled: bool,
}

fn default_apq() -> bool {
    true
}

impl Default for Apq {
    fn default() -> Self {
        Self {
            enabled: default_apq(),
            router: Default::default(),
            subgraph: Default::default(),
        }
    }
}

/// Query planning cache configuration
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, Default)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct QueryPlanning {
    /// Cache configuration
    pub(crate) cache: QueryPlanCache,
    /// Warms up the cache on reloads by running the query plan over
    /// a list of the most used queries (from the in memory cache)
    /// Configures the number of queries warmed up. Defaults to 1/3 of
    /// the in memory cache
    pub(crate) warmed_up_queries: Option<usize>,

    /// Sets a limit to the number of generated query plans.
    /// The planning process generates many different query plans as it
    /// explores the graph, and the list can grow large. By using this
    /// limit, we prevent that growth and still get a valid query plan,
    /// but it may not be the optimal one.
    ///
    /// The default limit is set to 10000, but it may change in the future
    pub(crate) experimental_plans_limit: Option<u32>,

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
    pub(crate) experimental_paths_limit: Option<u32>,

    /// If cache warm up is configured, this will allow the router to keep a query plan created with
    /// the old schema, if it determines that the schema update does not affect the corresponding query
    pub(crate) experimental_reuse_query_plans: bool,

    /// Configures cooperative cancellation of query planning
    ///
    /// See [`CooperativeCancellation`] for more details.
    pub(crate) experimental_cooperative_cancellation: CooperativeCancellation,
}

#[buildstructor::buildstructor]
impl QueryPlanning {
    #[builder]
    #[allow(dead_code)]
    pub(crate) fn new(
        cache: Option<QueryPlanCache>,
        warmed_up_queries: Option<usize>,
        experimental_plans_limit: Option<u32>,
        experimental_paths_limit: Option<u32>,
        experimental_reuse_query_plans: Option<bool>,
        experimental_cooperative_cancellation: Option<CooperativeCancellation>,
    ) -> Self {
        Self {
            cache: cache.unwrap_or_default(),
            warmed_up_queries,
            experimental_plans_limit,
            experimental_paths_limit,
            experimental_reuse_query_plans: experimental_reuse_query_plans.unwrap_or_default(),
            experimental_cooperative_cancellation: experimental_cooperative_cancellation
                .unwrap_or_default(),
        }
    }
}

/// Cache configuration
#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct QueryPlanCache {
    /// Configures the in memory cache (always active)
    pub(crate) in_memory: InMemoryCache,
    /// Configures and activates the Redis cache
    pub(crate) redis: Option<QueryPlanRedisCache>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
/// Redis cache configuration
pub(crate) struct QueryPlanRedisCache {
    /// List of URLs to the Redis cluster
    pub(crate) urls: Vec<url::Url>,

    /// Redis username if not provided in the URLs. This field takes precedence over the username in the URL
    pub(crate) username: Option<String>,
    /// Redis password if not provided in the URLs. This field takes precedence over the password in the URL
    pub(crate) password: Option<String>,

    #[serde(deserialize_with = "humantime_serde::deserialize", default)]
    #[schemars(with = "Option<String>", default)]
    /// Redis request timeout (default: 2ms)
    pub(crate) timeout: Option<Duration>,

    #[serde(
        deserialize_with = "humantime_serde::deserialize",
        default = "default_query_plan_cache_ttl"
    )]
    #[schemars(with = "Option<String>", default = "default_query_plan_cache_ttl")]
    /// TTL for entries
    pub(crate) ttl: Duration,

    /// namespace used to prefix Redis keys
    pub(crate) namespace: Option<String>,

    #[serde(default)]
    /// TLS client configuration
    pub(crate) tls: Option<TlsClient>,

    #[serde(default = "default_required_to_start")]
    /// Prevents the router from starting if it cannot connect to Redis
    pub(crate) required_to_start: bool,

    #[serde(default = "default_reset_ttl")]
    /// When a TTL is set on a key, reset it when reading the data from that key
    pub(crate) reset_ttl: bool,

    #[serde(default = "default_query_planner_cache_pool_size")]
    /// The size of the Redis connection pool
    pub(crate) pool_size: u32,
}

fn default_query_plan_cache_ttl() -> Duration {
    // Default TTL set to 30 days
    Duration::from_secs(86400 * 30)
}

fn default_query_planner_cache_pool_size() -> u32 {
    1
}

/// Cache configuration
#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct Cache {
    /// Configures the in memory cache (always active)
    pub(crate) in_memory: InMemoryCache,
    /// Configures and activates the Redis cache
    pub(crate) redis: Option<RedisCache>,
}

impl From<QueryPlanCache> for Cache {
    fn from(value: QueryPlanCache) -> Self {
        Cache {
            in_memory: value.in_memory,
            redis: value.redis.map(Into::into),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
/// In memory cache configuration
pub(crate) struct InMemoryCache {
    /// Number of entries in the Least Recently Used cache
    pub(crate) limit: NonZeroUsize,
}

impl Default for InMemoryCache {
    fn default() -> Self {
        Self {
            limit: DEFAULT_CACHE_CAPACITY,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
/// Redis cache configuration
pub(crate) struct RedisCache {
    /// List of URLs to the Redis cluster
    pub(crate) urls: Vec<url::Url>,

    /// Redis username if not provided in the URLs. This field takes precedence over the username in the URL
    pub(crate) username: Option<String>,
    /// Redis password if not provided in the URLs. This field takes precedence over the password in the URL
    pub(crate) password: Option<String>,

    #[serde(deserialize_with = "humantime_serde::deserialize", default)]
    #[schemars(with = "Option<String>", default)]
    /// Redis request timeout (default: 2ms)
    pub(crate) timeout: Option<Duration>,

    #[serde(deserialize_with = "humantime_serde::deserialize", default)]
    #[schemars(with = "Option<String>", default)]
    /// TTL for entries
    pub(crate) ttl: Option<Duration>,

    /// namespace used to prefix Redis keys
    pub(crate) namespace: Option<String>,

    #[serde(default)]
    /// TLS client configuration
    pub(crate) tls: Option<TlsClient>,

    #[serde(default = "default_required_to_start")]
    /// Prevents the router from starting if it cannot connect to Redis
    pub(crate) required_to_start: bool,

    #[serde(default = "default_reset_ttl")]
    /// When a TTL is set on a key, reset it when reading the data from that key
    pub(crate) reset_ttl: bool,

    #[serde(default = "default_pool_size")]
    /// The size of the Redis connection pool
    pub(crate) pool_size: u32,
}

fn default_required_to_start() -> bool {
    false
}

fn default_pool_size() -> u32 {
    1
}

impl From<QueryPlanRedisCache> for RedisCache {
    fn from(value: QueryPlanRedisCache) -> Self {
        RedisCache {
            urls: value.urls,
            username: value.username,
            password: value.password,
            timeout: value.timeout,
            ttl: Some(value.ttl),
            namespace: value.namespace,
            tls: value.tls,
            required_to_start: value.required_to_start,
            reset_ttl: value.reset_ttl,
            pool_size: value.pool_size,
        }
    }
}

fn default_reset_ttl() -> bool {
    true
}

/// TLS related configuration options.
#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[serde(default)]
pub(crate) struct Tls {
    /// TLS server configuration
    ///
    /// this will affect the GraphQL endpoint and any other endpoint targeting the same listen address
    pub(crate) supergraph: Option<Arc<TlsSupergraph>>,
    pub(crate) subgraph: SubgraphConfiguration<TlsClient>,
    pub(crate) connector: ConnectorConfiguration<TlsClient>,
}

/// Configuration options pertaining to the supergraph server component.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct TlsSupergraph {
    /// server certificate in PEM format
    #[serde(deserialize_with = "deserialize_certificate", skip_serializing)]
    #[schemars(with = "String")]
    pub(crate) certificate: CertificateDer<'static>,
    /// server key in PEM format
    #[serde(deserialize_with = "deserialize_key", skip_serializing)]
    #[schemars(with = "String")]
    pub(crate) key: PrivateKeyDer<'static>,
    /// list of certificate authorities in PEM format
    #[serde(deserialize_with = "deserialize_certificate_chain", skip_serializing)]
    #[schemars(with = "String")]
    pub(crate) certificate_chain: Vec<CertificateDer<'static>>,
}

impl TlsSupergraph {
    pub(crate) fn tls_config(&self) -> Result<Arc<rustls::ServerConfig>, ApolloRouterError> {
        let mut certificates = vec![self.certificate.clone()];
        certificates.extend(self.certificate_chain.iter().cloned());

        let mut config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certificates, self.key.clone_key())
            .map_err(ApolloRouterError::Rustls)?;
        config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];

        Ok(Arc::new(config))
    }
}

fn deserialize_certificate<'de, D>(deserializer: D) -> Result<CertificateDer<'static>, D::Error>
where
    D: Deserializer<'de>,
{
    let data = String::deserialize(deserializer)?;

    load_certs(&data)
        .map_err(serde::de::Error::custom)
        .and_then(|mut certs| {
            if certs.len() > 1 {
                Err(serde::de::Error::custom("expected exactly one certificate"))
            } else {
                certs
                    .pop()
                    .ok_or(serde::de::Error::custom("expected exactly one certificate"))
            }
        })
}

fn deserialize_certificate_chain<'de, D>(
    deserializer: D,
) -> Result<Vec<CertificateDer<'static>>, D::Error>
where
    D: Deserializer<'de>,
{
    let data = String::deserialize(deserializer)?;

    load_certs(&data).map_err(serde::de::Error::custom)
}

fn deserialize_key<'de, D>(deserializer: D) -> Result<PrivateKeyDer<'static>, D::Error>
where
    D: Deserializer<'de>,
{
    let data = String::deserialize(deserializer)?;

    load_key(&data).map_err(serde::de::Error::custom)
}

#[derive(thiserror::Error, Debug)]
#[error("could not load TLS certificate: {0}")]
struct LoadCertError(std::io::Error);

pub(crate) fn load_certs(data: &str) -> io::Result<Vec<CertificateDer<'static>>> {
    rustls_pemfile::certs(&mut BufReader::new(data.as_bytes()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, LoadCertError(error)))
}

pub(crate) fn load_key(data: &str) -> io::Result<PrivateKeyDer<'static>> {
    let mut reader = BufReader::new(data.as_bytes());
    let mut key_iterator = iter::from_fn(|| rustls_pemfile::read_one(&mut reader).transpose());

    let private_key = match key_iterator.next() {
        Some(Ok(rustls_pemfile::Item::Pkcs1Key(key))) => PrivateKeyDer::from(key),
        Some(Ok(rustls_pemfile::Item::Pkcs8Key(key))) => PrivateKeyDer::from(key),
        Some(Ok(rustls_pemfile::Item::Sec1Key(key))) => PrivateKeyDer::from(key),
        Some(Err(e)) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("could not parse the key: {e}"),
            ));
        }
        Some(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "expected a private key",
            ));
        }
        None => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "could not find a private key",
            ));
        }
    };

    if key_iterator.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected exactly one private key",
        ));
    }
    Ok(private_key)
}

/// Configuration options pertaining to the subgraph server component.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[serde(default)]
pub(crate) struct TlsClient {
    /// list of certificate authorities in PEM format
    pub(crate) certificate_authorities: Option<String>,
    /// client certificate authentication
    pub(crate) client_authentication: Option<Arc<TlsClientAuth>>,
}

#[buildstructor::buildstructor]
impl TlsClient {
    #[builder]
    pub(crate) fn new(
        certificate_authorities: Option<String>,
        client_authentication: Option<Arc<TlsClientAuth>>,
    ) -> Self {
        Self {
            certificate_authorities,
            client_authentication,
        }
    }
}

impl Default for TlsClient {
    fn default() -> Self {
        Self::builder().build()
    }
}

/// TLS client authentication
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct TlsClientAuth {
    /// list of certificates in PEM format
    #[serde(deserialize_with = "deserialize_certificate_chain", skip_serializing)]
    #[schemars(with = "String")]
    pub(crate) certificate_chain: Vec<CertificateDer<'static>>,
    /// key in PEM format
    #[serde(deserialize_with = "deserialize_key", skip_serializing)]
    #[schemars(with = "String")]
    pub(crate) key: PrivateKeyDer<'static>,
}

/// Configuration options pertaining to the sandbox page.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[serde(default)]
pub(crate) struct Sandbox {
    /// Set to true to enable sandbox
    pub(crate) enabled: bool,
}

fn default_sandbox() -> bool {
    false
}

#[buildstructor::buildstructor]
impl Sandbox {
    #[builder]
    pub(crate) fn new(enabled: Option<bool>) -> Self {
        Self {
            enabled: enabled.unwrap_or_else(default_sandbox),
        }
    }
}

#[cfg(test)]
#[buildstructor::buildstructor]
impl Sandbox {
    #[builder]
    pub(crate) fn fake_new(enabled: Option<bool>) -> Self {
        Self {
            enabled: enabled.unwrap_or_else(default_sandbox),
        }
    }
}

impl Default for Sandbox {
    fn default() -> Self {
        Self::builder().build()
    }
}

/// Configuration options pertaining to the home page.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[serde(default)]
pub(crate) struct Homepage {
    /// Set to false to disable the homepage
    pub(crate) enabled: bool,
    /// Graph reference
    /// This will allow you to redirect from the Apollo Router landing page back to Apollo Studio Explorer
    pub(crate) graph_ref: Option<String>,
}

fn default_homepage() -> bool {
    true
}

#[buildstructor::buildstructor]
impl Homepage {
    #[builder]
    pub(crate) fn new(enabled: Option<bool>) -> Self {
        Self {
            enabled: enabled.unwrap_or_else(default_homepage),
            graph_ref: None,
        }
    }
}

#[cfg(test)]
#[buildstructor::buildstructor]
impl Homepage {
    #[builder]
    pub(crate) fn fake_new(enabled: Option<bool>) -> Self {
        Self {
            enabled: enabled.unwrap_or_else(default_homepage),
            graph_ref: None,
        }
    }
}

impl Default for Homepage {
    fn default() -> Self {
        Self::builder().enabled(default_homepage()).build()
    }
}

/// Configuration for chaos testing, trying to reproduce bugs that require uncommon conditions.
/// You probably don’t want this in production!
#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[serde(default)]
pub(crate) struct Chaos {
    /// Force a hot reload of the Router (as if the schema or configuration had changed)
    /// at a regular time interval.
    #[serde(with = "humantime_serde")]
    #[schemars(with = "Option<String>")]
    pub(crate) force_reload: Option<std::time::Duration>,
}

/// Listening address.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize, JsonSchema)]
#[serde(untagged)]
pub enum ListenAddr {
    /// Socket address.
    SocketAddr(SocketAddr),
    /// Unix socket.
    #[cfg(unix)]
    UnixSocket(std::path::PathBuf),
}

impl ListenAddr {
    pub(crate) fn ip_and_port(&self) -> Option<(IpAddr, u16)> {
        #[cfg_attr(not(unix), allow(irrefutable_let_patterns))]
        if let Self::SocketAddr(addr) = self {
            Some((addr.ip(), addr.port()))
        } else {
            None
        }
    }
}

impl From<SocketAddr> for ListenAddr {
    fn from(addr: SocketAddr) -> Self {
        Self::SocketAddr(addr)
    }
}

#[allow(clippy::from_over_into)]
impl Into<serde_json::Value> for ListenAddr {
    fn into(self) -> serde_json::Value {
        match self {
            // It avoids to prefix with `http://` when serializing and relying on the Display impl.
            // Otherwise, it's converted to a `UnixSocket` in any case.
            Self::SocketAddr(addr) => serde_json::Value::String(addr.to_string()),
            #[cfg(unix)]
            Self::UnixSocket(path) => serde_json::Value::String(
                path.as_os_str()
                    .to_str()
                    .expect("unsupported non-UTF-8 path")
                    .to_string(),
            ),
        }
    }
}

#[cfg(unix)]
impl From<tokio_util::either::Either<std::net::SocketAddr, tokio::net::unix::SocketAddr>>
    for ListenAddr
{
    fn from(
        addr: tokio_util::either::Either<std::net::SocketAddr, tokio::net::unix::SocketAddr>,
    ) -> Self {
        match addr {
            tokio_util::either::Either::Left(addr) => Self::SocketAddr(addr),
            tokio_util::either::Either::Right(addr) => Self::UnixSocket(
                addr.as_pathname()
                    .map(ToOwned::to_owned)
                    .unwrap_or_default(),
            ),
        }
    }
}

impl fmt::Display for ListenAddr {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::SocketAddr(addr) => write!(f, "http://{addr}"),
            #[cfg(unix)]
            Self::UnixSocket(path) => write!(f, "{}", path.display()),
        }
    }
}

fn default_graphql_path() -> String {
    String::from("/")
}

fn default_graphql_introspection() -> bool {
    false
}

fn default_connection_shutdown_timeout() -> Duration {
    Duration::from_secs(60)
}

#[derive(Clone, Debug, Default, Error, Display, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub(crate) enum BatchingMode {
    /// batch_http_link
    #[default]
    BatchHttpLink,
}

/// Configuration for Batching
#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Batching {
    /// Activates Batching (disabled by default)
    #[serde(default)]
    pub(crate) enabled: bool,

    /// Batching mode
    pub(crate) mode: BatchingMode,

    /// Subgraph options for batching
    pub(crate) subgraph: Option<SubgraphConfiguration<CommonBatchingConfig>>,

    /// Maximum size for a batch
    #[serde(default)]
    pub(crate) maximum_size: Option<usize>,
}

/// Common options for configuring subgraph batching
#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
pub(crate) struct CommonBatchingConfig {
    /// Whether this batching config should be enabled
    pub(crate) enabled: bool,
}

impl Batching {
    // Check if we should enable batching for a particular subgraph (service_name)
    pub(crate) fn batch_include(&self, service_name: &str) -> bool {
        match &self.subgraph {
            Some(subgraph_batching_config) => {
                // Override by checking if all is enabled
                if subgraph_batching_config.all.enabled {
                    // If it is, require:
                    // - no subgraph entry OR
                    // - an enabled subgraph entry
                    subgraph_batching_config
                        .subgraphs
                        .get(service_name)
                        .is_none_or(|x| x.enabled)
                } else {
                    // If it isn't, require:
                    // - an enabled subgraph entry
                    subgraph_batching_config
                        .subgraphs
                        .get(service_name)
                        .is_some_and(|x| x.enabled)
                }
            }
            None => false,
        }
    }

    pub(crate) fn exceeds_batch_size<T>(&self, batch: &[T]) -> bool {
        match self.maximum_size {
            Some(maximum_size) => batch.len() > maximum_size,
            None => false,
        }
    }
}
