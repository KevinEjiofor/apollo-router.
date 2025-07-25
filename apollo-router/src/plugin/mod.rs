//! Plugin system for the router.
//!
//! Provides a customization mechanism for the router.
//!
//! Requests received by the router make their way through a processing pipeline. Each request is
//! processed at:
//!  - router
//!  - execution
//!  - subgraph (multiple in parallel if multiple subgraphs are accessed)
//!
//! stages.
//!
//! A plugin can choose to interact with the flow of requests at any or all of these stages of
//! processing. At each stage a [`Service`] is provided which provides an appropriate
//! mechanism for interacting with the request and response.

pub mod serde;
#[macro_use]
pub mod test;

use std::any::TypeId;
use std::collections::HashMap;
use std::fmt;
#[cfg(test)]
use std::path::PathBuf;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;

use ::serde::Deserialize;
use ::serde::de::DeserializeOwned;
use apollo_compiler::Schema;
use apollo_compiler::validation::Valid;
use async_trait::async_trait;
use futures::future::BoxFuture;
use multimap::MultiMap;
use once_cell::sync::Lazy;
use schemars::JsonSchema;
use schemars::r#gen::SchemaGenerator;
use serde_json::Value;
use tower::BoxError;
use tower::Service;
use tower::ServiceBuilder;
use tower::buffer::Buffer;
use tower::buffer::future::ResponseFuture;

use crate::ListenAddr;
use crate::graphql;
use crate::layers::ServiceBuilderExt;
use crate::notification::Notify;
use crate::router_factory::Endpoint;
use crate::services::execution;
use crate::services::router;
use crate::services::subgraph;
use crate::services::supergraph;
use crate::uplink::license_enforcement::LicenseState;

type InstanceFactory =
    fn(PluginInit<serde_json::Value>) -> BoxFuture<'static, Result<Box<dyn DynPlugin>, BoxError>>;

type SchemaFactory = fn(&mut SchemaGenerator) -> schemars::schema::Schema;

/// Global list of plugins.
#[linkme::distributed_slice]
pub static PLUGINS: [Lazy<PluginFactory>] = [..];

/// Initialise details for a plugin
#[non_exhaustive]
pub struct PluginInit<T> {
    /// Configuration
    pub config: T,
    /// Router Supergraph Schema (schema definition language)
    pub supergraph_sdl: Arc<String>,
    /// Router Supergraph Schema ID (SHA256 of the SDL))
    pub(crate) supergraph_schema_id: Arc<String>,
    /// The supergraph schema (parsed)
    pub(crate) supergraph_schema: Arc<Valid<Schema>>,

    /// The parsed subgraph schemas from the query planner, keyed by subgraph name
    pub(crate) subgraph_schemas: Arc<HashMap<String, Arc<Valid<Schema>>>>,

    /// Launch ID
    pub(crate) launch_id: Option<Arc<String>>,

    pub(crate) notify: Notify<String, graphql::Response>,

    /// User's license's state, including any limits of use
    pub(crate) license: LicenseState,

    /// The full router configuration json for use by the telemetry plugin ONLY.
    /// NEVER use this in any other plugin. Plugins should only ever access their pre-defined
    /// configuration subset.
    pub(crate) full_config: Option<Value>,
}

impl<T> PluginInit<T>
where
    T: for<'de> Deserialize<'de>,
{
    #[cfg(test)]
    pub(crate) fn fake_new(config: T, supergraph_sdl: Arc<String>) -> Self {
        let supergraph_schema = Arc::new(if !supergraph_sdl.is_empty() {
            Schema::parse_and_validate(supergraph_sdl.to_string(), PathBuf::from("synthetic"))
                .expect("failed to parse supergraph schema")
        } else {
            Valid::assume_valid(Schema::new())
        });

        PluginInit::fake_builder()
            .config(config)
            .supergraph_schema_id(crate::spec::Schema::schema_id(&supergraph_sdl).into_inner())
            .supergraph_sdl(supergraph_sdl)
            .supergraph_schema(supergraph_schema)
            .launch_id(Arc::new("launch_id".to_string()))
            .notify(Notify::for_tests())
            .license(LicenseState::default())
            .build()
    }
}

#[buildstructor::buildstructor]
impl<T> PluginInit<T>
where
    T: for<'de> Deserialize<'de>,
{
    /// Create a new PluginInit builder
    #[builder(entry = "builder", exit = "build", visibility = "pub")]
    /// Build a new PluginInit for the supplied configuration and SDL.
    ///
    /// You can reuse a notify instance, or Build your own.
    pub(crate) fn new_builder(
        config: T,
        supergraph_sdl: Arc<String>,
        supergraph_schema_id: Arc<String>,
        supergraph_schema: Arc<Valid<Schema>>,
        subgraph_schemas: Option<Arc<HashMap<String, Arc<Valid<Schema>>>>>,
        launch_id: Option<Option<Arc<String>>>,
        notify: Notify<String, graphql::Response>,
        license: LicenseState,
        full_config: Option<Value>,
    ) -> Self {
        PluginInit {
            config,
            supergraph_sdl,
            supergraph_schema_id,
            supergraph_schema,
            subgraph_schemas: subgraph_schemas.unwrap_or_default(),
            launch_id: launch_id.flatten(),
            notify,
            license,
            full_config,
        }
    }

    #[builder(entry = "try_builder", exit = "build", visibility = "pub")]
    /// Try to build a new PluginInit for the supplied json configuration and SDL.
    ///
    /// You can reuse a notify instance, or Build your own.
    /// invoking build() will fail if the JSON doesn't comply with the configuration format.
    pub(crate) fn try_new_builder(
        config: serde_json::Value,
        supergraph_sdl: Arc<String>,
        supergraph_schema_id: Arc<String>,
        supergraph_schema: Arc<Valid<Schema>>,
        subgraph_schemas: Option<Arc<HashMap<String, Arc<Valid<Schema>>>>>,
        launch_id: Option<Arc<String>>,
        notify: Notify<String, graphql::Response>,
        license: LicenseState,
        full_config: Option<Value>,
    ) -> Result<Self, BoxError> {
        let config: T = serde_json::from_value(config)?;
        Ok(PluginInit {
            config,
            supergraph_sdl,
            supergraph_schema,
            supergraph_schema_id,
            subgraph_schemas: subgraph_schemas.unwrap_or_default(),
            launch_id,
            notify,
            license,
            full_config,
        })
    }

    /// Create a new PluginInit builder
    #[builder(entry = "fake_builder", exit = "build", visibility = "pub")]
    fn fake_new_builder(
        config: T,
        supergraph_sdl: Option<Arc<String>>,
        supergraph_schema_id: Option<Arc<String>>,
        supergraph_schema: Option<Arc<Valid<Schema>>>,
        subgraph_schemas: Option<Arc<HashMap<String, Arc<Valid<Schema>>>>>,
        launch_id: Option<Arc<String>>,
        notify: Option<Notify<String, graphql::Response>>,
        license: Option<LicenseState>,
        full_config: Option<Value>,
    ) -> Self {
        PluginInit {
            config,
            supergraph_sdl: supergraph_sdl.unwrap_or_default(),
            supergraph_schema_id: supergraph_schema_id.unwrap_or_default(),
            supergraph_schema: supergraph_schema
                .unwrap_or_else(|| Arc::new(Valid::assume_valid(Schema::new()))),
            subgraph_schemas: subgraph_schemas.unwrap_or_default(),
            launch_id,
            notify: notify.unwrap_or_else(Notify::for_tests),
            license: license.unwrap_or_default(),
            full_config,
        }
    }
}

impl PluginInit<serde_json::Value> {
    /// Attempts to convert the plugin configuration from `serde_json::Value` to the desired type `T`
    pub fn with_deserialized_config<T>(self) -> Result<PluginInit<T>, BoxError>
    where
        T: for<'de> Deserialize<'de>,
    {
        PluginInit::try_builder()
            .config(self.config)
            .supergraph_schema(self.supergraph_schema)
            .supergraph_schema_id(self.supergraph_schema_id)
            .supergraph_sdl(self.supergraph_sdl)
            .subgraph_schemas(self.subgraph_schemas)
            .notify(self.notify.clone())
            .license(self.license)
            .and_full_config(self.full_config)
            .build()
    }
}

/// Factories for plugin schema and configuration.
#[derive(Clone)]
pub struct PluginFactory {
    pub(crate) name: String,
    pub(crate) hidden_from_config_json_schema: bool,
    instance_factory: InstanceFactory,
    schema_factory: SchemaFactory,
    pub(crate) type_id: TypeId,
}

impl fmt::Debug for PluginFactory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PluginFactory")
            .field("name", &self.name)
            .field("type_id", &self.type_id)
            .finish()
    }
}

impl PluginFactory {
    pub(crate) fn is_apollo(&self) -> bool {
        self.name.starts_with("apollo.") || self.name.starts_with("experimental.")
    }

    /// Create a plugin factory.
    pub fn new<P: PluginUnstable>(group: &str, name: &str) -> PluginFactory {
        let plugin_factory_name = if group.is_empty() {
            name.to_string()
        } else {
            format!("{group}.{name}")
        };
        tracing::debug!(%plugin_factory_name, "creating plugin factory");
        PluginFactory {
            name: plugin_factory_name,
            hidden_from_config_json_schema: false,
            instance_factory: |init| {
                Box::pin(async move {
                    let init = init.with_deserialized_config()?;
                    let plugin = P::new(init).await?;
                    Ok(Box::new(plugin) as Box<dyn DynPlugin>)
                })
            },
            schema_factory: |generator| generator.subschema_for::<<P as PluginUnstable>::Config>(),
            type_id: TypeId::of::<P>(),
        }
    }

    /// Create a plugin factory.
    pub(crate) fn new_private<P: PluginPrivate>(group: &str, name: &str) -> PluginFactory {
        let plugin_factory_name = if group.is_empty() {
            name.to_string()
        } else {
            format!("{group}.{name}")
        };
        tracing::debug!(%plugin_factory_name, "creating plugin factory");
        PluginFactory {
            name: plugin_factory_name,
            hidden_from_config_json_schema: P::HIDDEN_FROM_CONFIG_JSON_SCHEMA,
            instance_factory: |init| {
                Box::pin(async move {
                    let init = init.with_deserialized_config()?;
                    let plugin = P::new(init).await?;
                    Ok(Box::new(plugin) as Box<dyn DynPlugin>)
                })
            },
            schema_factory: |generator| generator.subschema_for::<<P as PluginPrivate>::Config>(),
            type_id: TypeId::of::<P>(),
        }
    }

    pub(crate) async fn create_instance(
        &self,
        init: PluginInit<serde_json::Value>,
    ) -> Result<Box<dyn DynPlugin>, BoxError> {
        (self.instance_factory)(init).await
    }

    #[cfg(test)]
    pub(crate) async fn create_instance_without_schema(
        &self,
        configuration: &serde_json::Value,
    ) -> Result<Box<dyn DynPlugin>, BoxError> {
        (self.instance_factory)(
            PluginInit::fake_builder()
                .config(configuration.clone())
                .build(),
        )
        .await
    }

    pub(crate) fn create_schema(
        &self,
        generator: &mut SchemaGenerator,
    ) -> schemars::schema::Schema {
        (self.schema_factory)(generator)
    }
}

// If we wanted to create a custom subset of plugins, this is where we would do it
/// Get a copy of the registered plugin factories.
pub(crate) fn plugins() -> impl Iterator<Item = &'static Lazy<PluginFactory>> {
    PLUGINS.iter()
}

/// All router plugins must implement the Plugin trait.
///
/// This trait defines lifecycle hooks that enable hooking into Apollo Router services.
/// The trait also provides a default implementations for each hook, which returns the associated service unmodified.
/// For more information about the plugin lifecycle please check this documentation <https://www.apollographql.com/docs/router/customizations/native/#plugin-lifecycle>
#[async_trait]
pub trait Plugin: Send + Sync + 'static {
    /// The configuration for this plugin.
    /// Typically a `struct` with `#[derive(serde::Deserialize)]`.
    ///
    /// If a plugin is [registered][register_plugin()!],
    /// it can be enabled through the `plugins` section of Router YAML configuration
    /// by having a sub-section named after the plugin.
    /// The contents of this section are deserialized into this `Config` type
    /// and passed to [`Plugin::new`] as part of [`PluginInit`].
    type Config: JsonSchema + DeserializeOwned + Send;

    /// This is invoked once after the router starts and compiled-in
    /// plugins are registered.
    async fn new(init: PluginInit<Self::Config>) -> Result<Self, BoxError>
    where
        Self: Sized;

    /// This function is EXPERIMENTAL and its signature is subject to change.
    ///
    /// This service runs at the very beginning and very end of the request lifecycle.
    /// It's the entrypoint of every requests and also the last hook before sending the response.
    /// Define supergraph_service if your customization needs to interact at the earliest or latest point possible.
    /// For example, this is a good opportunity to perform JWT verification before allowing a request to proceed further.
    fn router_service(&self, service: router::BoxService) -> router::BoxService {
        service
    }

    /// This service runs after the HTTP request payload has been deserialized into a GraphQL request,
    /// and before the GraphQL response payload is serialized into a raw HTTP response.
    /// Define supergraph_service if your customization needs to interact at the earliest or latest point possible, yet operates on GraphQL payloads.
    fn supergraph_service(&self, service: supergraph::BoxService) -> supergraph::BoxService {
        service
    }

    /// This service handles initiating the execution of a query plan after it's been generated.
    /// Define `execution_service` if your customization includes logic to govern execution (for example, if you want to block a particular query based on a policy decision).
    fn execution_service(&self, service: execution::BoxService) -> execution::BoxService {
        service
    }

    /// This service handles communication between the Apollo Router and your subgraphs.
    /// Define `subgraph_service` to configure this communication (for example, to dynamically add headers to pass to a subgraph).
    /// The `_subgraph_name` parameter is useful if you need to apply a customization only specific subgraphs.
    fn subgraph_service(
        &self,
        _subgraph_name: &str,
        service: subgraph::BoxService,
    ) -> subgraph::BoxService {
        service
    }

    /// Return the name of the plugin.
    fn name(&self) -> &'static str
    where
        Self: Sized,
    {
        get_type_of(self)
    }

    /// Return one or several `Endpoint`s and `ListenAddr` and the router will serve your custom web Endpoint(s).
    ///
    /// This method is experimental and subject to change post 1.0
    fn web_endpoints(&self) -> MultiMap<ListenAddr, Endpoint> {
        MultiMap::new()
    }
}

/// Plugin trait for unstable features
///
/// This trait defines lifecycle hooks that enable hooking into Apollo Router services. The hooks that are not already defined
/// in the [Plugin] trait are not considered stable and may change at any moment.
/// The trait also provides a default implementations for each hook, which returns the associated service unmodified.
/// For more information about the plugin lifecycle please check this documentation <https://www.apollographql.com/docs/router/customizations/native/#plugin-lifecycle>
#[async_trait]
pub trait PluginUnstable: Send + Sync + 'static {
    /// The configuration for this plugin.
    /// Typically a `struct` with `#[derive(serde::Deserialize)]`.
    ///
    /// If a plugin is [registered][register_plugin()!],
    /// it can be enabled through the `plugins` section of Router YAML configuration
    /// by having a sub-section named after the plugin.
    /// The contents of this section are deserialized into this `Config` type
    /// and passed to [`Plugin::new`] as part of [`PluginInit`].
    type Config: JsonSchema + DeserializeOwned + Send;

    /// This is invoked once after the router starts and compiled-in
    /// plugins are registered.
    async fn new(init: PluginInit<Self::Config>) -> Result<Self, BoxError>
    where
        Self: Sized;

    /// This function is EXPERIMENTAL and its signature is subject to change.
    ///
    /// This service runs at the very beginning and very end of the request lifecycle.
    /// It's the entrypoint of every requests and also the last hook before sending the response.
    /// Define supergraph_service if your customization needs to interact at the earliest or latest point possible.
    /// For example, this is a good opportunity to perform JWT verification before allowing a request to proceed further.
    fn router_service(&self, service: router::BoxService) -> router::BoxService {
        service
    }

    /// This service runs after the HTTP request payload has been deserialized into a GraphQL request,
    /// and before the GraphQL response payload is serialized into a raw HTTP response.
    /// Define supergraph_service if your customization needs to interact at the earliest or latest point possible, yet operates on GraphQL payloads.
    fn supergraph_service(&self, service: supergraph::BoxService) -> supergraph::BoxService {
        service
    }

    /// This service handles initiating the execution of a query plan after it's been generated.
    /// Define `execution_service` if your customization includes logic to govern execution (for example, if you want to block a particular query based on a policy decision).
    fn execution_service(&self, service: execution::BoxService) -> execution::BoxService {
        service
    }

    /// This service handles communication between the Apollo Router and your subgraphs.
    /// Define `subgraph_service` to configure this communication (for example, to dynamically add headers to pass to a subgraph).
    /// The `_subgraph_name` parameter is useful if you need to apply a customization only specific subgraphs.
    fn subgraph_service(
        &self,
        _subgraph_name: &str,
        service: subgraph::BoxService,
    ) -> subgraph::BoxService {
        service
    }

    /// Return the name of the plugin.
    fn name(&self) -> &'static str
    where
        Self: Sized,
    {
        get_type_of(self)
    }

    /// Return one or several `Endpoint`s and `ListenAddr` and the router will serve your custom web Endpoint(s).
    ///
    /// This method is experimental and subject to change post 1.0
    fn web_endpoints(&self) -> MultiMap<ListenAddr, Endpoint> {
        MultiMap::new()
    }

    /// test
    fn unstable_method(&self);
}

#[async_trait]
impl<P> PluginUnstable for P
where
    P: Plugin,
{
    type Config = <P as Plugin>::Config;

    async fn new(init: PluginInit<Self::Config>) -> Result<Self, BoxError>
    where
        Self: Sized,
    {
        Plugin::new(init).await
    }

    fn router_service(&self, service: router::BoxService) -> router::BoxService {
        Plugin::router_service(self, service)
    }

    fn supergraph_service(&self, service: supergraph::BoxService) -> supergraph::BoxService {
        Plugin::supergraph_service(self, service)
    }

    fn execution_service(&self, service: execution::BoxService) -> execution::BoxService {
        Plugin::execution_service(self, service)
    }

    fn subgraph_service(
        &self,
        subgraph_name: &str,
        service: subgraph::BoxService,
    ) -> subgraph::BoxService {
        Plugin::subgraph_service(self, subgraph_name, service)
    }

    /// Return the name of the plugin.
    fn name(&self) -> &'static str
    where
        Self: Sized,
    {
        Plugin::name(self)
    }

    fn web_endpoints(&self) -> MultiMap<ListenAddr, Endpoint> {
        Plugin::web_endpoints(self)
    }

    fn unstable_method(&self) {
        todo!()
    }
}

/// Internal Plugin trait
///
/// This trait defines lifecycle hooks that enable hooking into Apollo Router services. The hooks that are not already defined
/// in the [Plugin] or [PluginUnstable] traits are internal hooks not yet open to public usage. This allows testing of new plugin
/// hooks without committing to their API right away.
/// The trait also provides a default implementations for each hook, which returns the associated service unmodified.
/// For more information about the plugin lifecycle please check this documentation <https://www.apollographql.com/docs/router/customizations/native/#plugin-lifecycle>
#[async_trait]
pub(crate) trait PluginPrivate: Send + Sync + 'static {
    /// The configuration for this plugin.
    /// Typically a `struct` with `#[derive(serde::Deserialize)]`.
    ///
    /// If a plugin is [registered][register_plugin()!],
    /// it can be enabled through the `plugins` section of Router YAML configuration
    /// by having a sub-section named after the plugin.
    /// The contents of this section are deserialized into this `Config` type
    /// and passed to [`Plugin::new`] as part of [`PluginInit`].
    type Config: JsonSchema + DeserializeOwned + Send;

    const HIDDEN_FROM_CONFIG_JSON_SCHEMA: bool = false;

    /// This is invoked once after the router starts and compiled-in
    /// plugins are registered.
    async fn new(init: PluginInit<Self::Config>) -> Result<Self, BoxError>
    where
        Self: Sized;

    /// This function is EXPERIMENTAL and its signature is subject to change.
    ///
    /// This service runs at the very beginning and very end of the request lifecycle.
    /// It's the entrypoint of every requests and also the last hook before sending the response.
    /// Define supergraph_service if your customization needs to interact at the earliest or latest point possible.
    /// For example, this is a good opportunity to perform JWT verification before allowing a request to proceed further.
    fn router_service(&self, service: router::BoxService) -> router::BoxService {
        service
    }

    /// This service runs after the HTTP request payload has been deserialized into a GraphQL request,
    /// and before the GraphQL response payload is serialized into a raw HTTP response.
    /// Define supergraph_service if your customization needs to interact at the earliest or latest point possible, yet operates on GraphQL payloads.
    fn supergraph_service(&self, service: supergraph::BoxService) -> supergraph::BoxService {
        service
    }

    /// This service handles initiating the execution of a query plan after it's been generated.
    /// Define `execution_service` if your customization includes logic to govern execution (for example, if you want to block a particular query based on a policy decision).
    fn execution_service(&self, service: execution::BoxService) -> execution::BoxService {
        service
    }

    /// This service handles communication between the Apollo Router and your subgraphs.
    /// Define `subgraph_service` to configure this communication (for example, to dynamically add headers to pass to a subgraph).
    /// The `_subgraph_name` parameter is useful if you need to apply a customization only specific subgraphs.
    fn subgraph_service(
        &self,
        _subgraph_name: &str,
        service: subgraph::BoxService,
    ) -> subgraph::BoxService {
        service
    }

    /// This service handles HTTP communication
    fn http_client_service(
        &self,
        _subgraph_name: &str,
        service: crate::services::http::BoxService,
    ) -> crate::services::http::BoxService {
        service
    }

    /// This service handles individual requests to Apollo Connectors
    fn connector_request_service(
        &self,
        service: crate::services::connector::request_service::BoxService,
        _source_name: String,
    ) -> crate::services::connector::request_service::BoxService {
        service
    }

    /// Return the name of the plugin.
    fn name(&self) -> &'static str
    where
        Self: Sized,
    {
        get_type_of(self)
    }

    /// Return one or several `Endpoint`s and `ListenAddr` and the router will serve your custom web Endpoint(s).
    ///
    /// This method is experimental and subject to change post 1.0
    fn web_endpoints(&self) -> MultiMap<ListenAddr, Endpoint> {
        MultiMap::new()
    }

    /// The point of no return this plugin is about to go live
    fn activate(&self) {}
}

#[async_trait]
impl<P> PluginPrivate for P
where
    P: PluginUnstable,
{
    type Config = <P as PluginUnstable>::Config;

    async fn new(init: PluginInit<Self::Config>) -> Result<Self, BoxError>
    where
        Self: Sized,
    {
        PluginUnstable::new(init).await
    }

    fn router_service(&self, service: router::BoxService) -> router::BoxService {
        PluginUnstable::router_service(self, service)
    }

    fn supergraph_service(&self, service: supergraph::BoxService) -> supergraph::BoxService {
        PluginUnstable::supergraph_service(self, service)
    }

    fn execution_service(&self, service: execution::BoxService) -> execution::BoxService {
        PluginUnstable::execution_service(self, service)
    }

    fn subgraph_service(
        &self,
        subgraph_name: &str,
        service: subgraph::BoxService,
    ) -> subgraph::BoxService {
        PluginUnstable::subgraph_service(self, subgraph_name, service)
    }

    /// Return the name of the plugin.
    fn name(&self) -> &'static str
    where
        Self: Sized,
    {
        PluginUnstable::name(self)
    }

    fn web_endpoints(&self) -> MultiMap<ListenAddr, Endpoint> {
        PluginUnstable::web_endpoints(self)
    }

    fn activate(&self) {}
}

fn get_type_of<T>(_: &T) -> &'static str {
    std::any::type_name::<T>()
}

/// All router plugins must implement the DynPlugin trait.
///
/// This trait defines lifecycle hooks that enable hooking into Apollo Router services.
/// The trait also provides a default implementations for each hook, which returns the associated service unmodified.
/// For more information about the plugin lifecycle please check this documentation <https://www.apollographql.com/docs/router/customizations/native/#plugin-lifecycle>
#[async_trait]
pub(crate) trait DynPlugin: Send + Sync + 'static {
    /// This service runs at the very beginning and very end of the request lifecycle.
    /// It's the entrypoint of every requests and also the last hook before sending the response.
    /// Define supergraph_service if your customization needs to interact at the earliest or latest point possible.
    /// For example, this is a good opportunity to perform JWT verification before allowing a request to proceed further.
    fn router_service(&self, service: router::BoxService) -> router::BoxService;

    /// This service runs after the HTTP request payload has been deserialized into a GraphQL request,
    /// and before the GraphQL response payload is serialized into a raw HTTP response.
    /// Define supergraph_service if your customization needs to interact at the earliest or latest point possible, yet operates on GraphQL payloads.
    fn supergraph_service(&self, service: supergraph::BoxService) -> supergraph::BoxService;

    /// This service handles initiating the execution of a query plan after it's been generated.
    /// Define `execution_service` if your customization includes logic to govern execution (for example, if you want to block a particular query based on a policy decision).
    fn execution_service(&self, service: execution::BoxService) -> execution::BoxService;

    /// This service handles communication between the Apollo Router and your subgraphs.
    /// Define `subgraph_service` to configure this communication (for example, to dynamically add headers to pass to a subgraph).
    /// The `_subgraph_name` parameter is useful if you need to apply a customization only on specific subgraphs.
    fn subgraph_service(
        &self,
        _subgraph_name: &str,
        service: subgraph::BoxService,
    ) -> subgraph::BoxService;

    /// This service handles HTTP communication
    fn http_client_service(
        &self,
        _subgraph_name: &str,
        service: crate::services::http::BoxService,
    ) -> crate::services::http::BoxService;

    /// This service handles individual requests to Apollo Connectors
    fn connector_request_service(
        &self,
        service: crate::services::connector::request_service::BoxService,
        source_name: String,
    ) -> crate::services::connector::request_service::BoxService;

    /// Return the name of the plugin.
    fn name(&self) -> &'static str;

    /// Return one or several `Endpoint`s and `ListenAddr` and the router will serve your custom web Endpoint(s).
    fn web_endpoints(&self) -> MultiMap<ListenAddr, Endpoint>;

    /// Support downcasting
    fn as_any(&self) -> &dyn std::any::Any;

    /// Support downcasting
    #[cfg(test)]
    #[allow(dead_code)]
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any;

    /// The point of no return, this plugin is about to go live
    fn activate(&self) {}
}

#[async_trait]
impl<T> DynPlugin for T
where
    T: PluginPrivate,
    for<'de> <T as PluginPrivate>::Config: Deserialize<'de>,
{
    fn router_service(&self, service: router::BoxService) -> router::BoxService {
        self.router_service(service)
    }

    fn supergraph_service(&self, service: supergraph::BoxService) -> supergraph::BoxService {
        self.supergraph_service(service)
    }

    fn execution_service(&self, service: execution::BoxService) -> execution::BoxService {
        self.execution_service(service)
    }

    fn subgraph_service(&self, name: &str, service: subgraph::BoxService) -> subgraph::BoxService {
        self.subgraph_service(name, service)
    }

    /// This service handles HTTP communication
    fn http_client_service(
        &self,
        name: &str,
        service: crate::services::http::BoxService,
    ) -> crate::services::http::BoxService {
        self.http_client_service(name, service)
    }

    fn connector_request_service(
        &self,
        service: crate::services::connector::request_service::BoxService,
        source_name: String,
    ) -> crate::services::connector::request_service::BoxService {
        self.connector_request_service(service, source_name)
    }

    fn name(&self) -> &'static str {
        self.name()
    }

    /// Return one or several `Endpoint`s and `ListenAddr` and the router will serve your custom web Endpoint(s).
    fn web_endpoints(&self) -> MultiMap<ListenAddr, Endpoint> {
        self.web_endpoints()
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    #[cfg(test)]
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn activate(&self) {
        self.activate()
    }
}

impl<T> From<T> for Box<dyn DynPlugin>
where
    T: PluginPrivate,
{
    fn from(value: T) -> Self {
        Box::new(value)
    }
}

/// Register a plugin with a group and a name
/// Grouping prevent name clashes for plugins, so choose something unique, like your domain name.
/// Plugins will appear in the configuration as a layer property called: {group}.{name}
#[macro_export]
macro_rules! register_plugin {
    ($group: literal, $name: literal, $plugin_type: ident <  $generic: ident >) => {
        //  Artificial scope to avoid naming collisions
        const _: () = {
            use $crate::_private::PLUGINS;
            use $crate::_private::PluginFactory;
            use $crate::_private::once_cell::sync::Lazy;

            #[$crate::_private::linkme::distributed_slice(PLUGINS)]
            #[linkme(crate = $crate::_private::linkme)]
            static REGISTER_PLUGIN: Lazy<PluginFactory> = Lazy::new(|| {
                $crate::plugin::PluginFactory::new::<$plugin_type<$generic>>($group, $name)
            });
        };
    };

    ($group: literal, $name: expr, $plugin_type: ident) => {
        //  Artificial scope to avoid naming collisions
        const _: () = {
            use $crate::_private::PLUGINS;
            use $crate::_private::PluginFactory;
            use $crate::_private::once_cell::sync::Lazy;

            #[$crate::_private::linkme::distributed_slice(PLUGINS)]
            #[linkme(crate = $crate::_private::linkme)]
            static REGISTER_PLUGIN: Lazy<PluginFactory> =
                Lazy::new(|| $crate::plugin::PluginFactory::new::<$plugin_type>($group, $name));
        };
    };
}

/// Register a private plugin with a group and a name
/// Grouping prevent name clashes for plugins, so choose something unique, like your domain name.
/// Plugins will appear in the configuration as a layer property called: {group}.{name}
#[macro_export]
macro_rules! register_private_plugin {
    ($group: literal, $name: literal, $plugin_type: ident <  $generic: ident >) => {
        //  Artificial scope to avoid naming collisions
        const _: () = {
            use $crate::_private::PLUGINS;
            use $crate::_private::PluginFactory;
            use $crate::_private::once_cell::sync::Lazy;

            #[$crate::_private::linkme::distributed_slice(PLUGINS)]
            #[linkme(crate = $crate::_private::linkme)]
            static REGISTER_PLUGIN: Lazy<PluginFactory> = Lazy::new(|| {
                $crate::plugin::PluginFactory::new_private::<$plugin_type<$generic>>($group, $name)
            });
        };
    };

    ($group: literal, $name: literal, $plugin_type: ident) => {
        //  Artificial scope to avoid naming collisions
        const _: () = {
            use $crate::_private::PLUGINS;
            use $crate::_private::PluginFactory;
            use $crate::_private::once_cell::sync::Lazy;

            #[$crate::_private::linkme::distributed_slice(PLUGINS)]
            #[linkme(crate = $crate::_private::linkme)]
            static REGISTER_PLUGIN: Lazy<PluginFactory> = Lazy::new(|| {
                $crate::plugin::PluginFactory::new_private::<$plugin_type>($group, $name)
            });
        };
    };
}

/// Handler represents a [`Plugin`] endpoint.
#[derive(Clone)]
pub(crate) struct Handler {
    service: Buffer<router::Request, <router::BoxService as Service<router::Request>>::Future>,
}

impl Handler {
    pub(crate) fn new(service: router::BoxService) -> Self {
        Self {
            service: ServiceBuilder::new().buffered().service(service),
        }
    }
}

impl Service<router::Request> for Handler {
    type Response = router::Response;
    type Error = BoxError;
    type Future = ResponseFuture<BoxFuture<'static, Result<Self::Response, Self::Error>>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(cx)
    }

    fn call(&mut self, req: router::Request) -> Self::Future {
        self.service.call(req)
    }
}

impl From<router::BoxService> for Handler {
    fn from(original: router::BoxService) -> Self {
        Self::new(original)
    }
}
