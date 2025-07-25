//! Tower fetcher for subgraphs.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::Relaxed;
use std::task::Poll;

use bytes::Bytes;
use futures::StreamExt;
use futures::TryFutureExt;
use futures::future::BoxFuture;
use http::HeaderValue;
use http::Request;
use http::StatusCode;
use http::header::ACCEPT;
use http::header::CONTENT_TYPE;
use http::header::{self};
use http::response::Parts;
use hyper_rustls::ConfigBuilderExt;
use itertools::Itertools;
use mediatype::MediaType;
use mediatype::names::APPLICATION;
use mediatype::names::JSON;
use mime::APPLICATION_JSON;
use opentelemetry::Key;
use opentelemetry::KeyValue;
use rustls::RootCertStore;
use serde::Serialize;
use tokio::sync::oneshot;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::connect_async_tls_with_config;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tower::BoxError;
use tower::Service;
use tower::ServiceExt;
use tower::buffer::Buffer;
use tracing::Instrument;
use tracing::instrument;
use uuid::Uuid;

use super::Plugins;
use super::http::HttpClientServiceFactory;
use super::http::HttpRequest;
use super::layers::content_negotiation::GRAPHQL_JSON_RESPONSE_HEADER_VALUE;
use super::router::body::RouterBody;
use super::subgraph::SubgraphRequestId;
use crate::Configuration;
use crate::Context;
use crate::Notify;
use crate::batching::BatchQuery;
use crate::batching::BatchQueryInfo;
use crate::batching::assemble_batch;
use crate::configuration::Batching;
use crate::configuration::BatchingMode;
use crate::configuration::TlsClientAuth;
use crate::context::OPERATION_NAME;
use crate::error::FetchError;
use crate::error::SubgraphBatchingError;
use crate::graphql;
use crate::json_ext::Object;
use crate::layers::DEFAULT_BUFFER_SIZE;
use crate::plugins::authentication::subgraph::SigningParamsConfig;
use crate::plugins::file_uploads;
use crate::plugins::subscription::CallbackMode;
use crate::plugins::subscription::SUBSCRIPTION_WS_CUSTOM_CONNECTION_PARAMS;
use crate::plugins::subscription::SubscriptionConfig;
use crate::plugins::subscription::SubscriptionMode;
use crate::plugins::subscription::WebSocketConfiguration;
use crate::plugins::subscription::create_verifier;
use crate::plugins::telemetry::config_new::events::log_event;
use crate::plugins::telemetry::config_new::subgraph::events::SubgraphEventRequest;
use crate::plugins::telemetry::config_new::subgraph::events::SubgraphEventResponse;
use crate::plugins::telemetry::consts::SUBGRAPH_REQUEST_SPAN_NAME;
use crate::protocols::websocket::GraphqlWebSocket;
use crate::protocols::websocket::convert_websocket_stream;
use crate::query_planner::OperationKind;
use crate::services::SubgraphRequest;
use crate::services::SubgraphResponse;
use crate::services::layers::apq;
use crate::services::router;
use crate::services::subgraph;

const PERSISTED_QUERY_NOT_FOUND_EXTENSION_CODE: &str = "PERSISTED_QUERY_NOT_FOUND";
const PERSISTED_QUERY_NOT_SUPPORTED_EXTENSION_CODE: &str = "PERSISTED_QUERY_NOT_SUPPORTED";
const PERSISTED_QUERY_NOT_FOUND_MESSAGE: &str = "PersistedQueryNotFound";
const PERSISTED_QUERY_NOT_SUPPORTED_MESSAGE: &str = "PersistedQueryNotSupported";
const CODE_STRING: &str = "code";
const PERSISTED_QUERY_KEY: &str = "persistedQuery";
const HASH_VERSION_KEY: &str = "version";
const HASH_VERSION_VALUE: i32 = 1;
const HASH_KEY: &str = "sha256Hash";
const GRAPHQL_RESPONSE: mediatype::Name = mediatype::Name::new_unchecked("graphql-response");

#[allow(clippy::declare_interior_mutable_const)]
static CALLBACK_PROTOCOL_ACCEPT: HeaderValue =
    HeaderValue::from_static("application/json;callbackSpec=1.0");
pub(crate) static APPLICATION_JSON_HEADER_VALUE: HeaderValue =
    HeaderValue::from_static("application/json");
static ACCEPT_GRAPHQL_JSON: HeaderValue =
    HeaderValue::from_static("application/json, application/graphql-response+json");

enum APQError {
    PersistedQueryNotSupported,
    PersistedQueryNotFound,
    Other,
}

#[cfg_attr(test, derive(serde::Deserialize))]
#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
struct SubscriptionExtension {
    subscription_id: String,
    callback_url: url::Url,
    verifier: String,
    heartbeat_interval_ms: u64,
}

/// Client for interacting with subgraphs.
#[derive(Clone)]
pub(crate) struct SubgraphService {
    // we hold a HTTP client service factory here because a service with plugins applied
    // cannot be cloned
    pub(crate) client_factory: HttpClientServiceFactory,
    service: Arc<String>,

    /// Whether apq is enabled in the router for subgraph calls
    /// This is enabled by default can be configured as
    /// subgraph:
    ///      apq: <bool>
    /// If a subgraph sends the error message PERSISTED_QUERY_NOT_SUPPORTED,
    /// apq is set to false
    apq: Arc<AtomicBool>,
    /// Subscription config if enabled
    subscription_config: Option<SubscriptionConfig>,
    notify: Notify<String, graphql::Response>,
}

impl SubgraphService {
    pub(crate) fn from_config(
        service: impl Into<String>,
        configuration: &Configuration,
        subscription_config: Option<SubscriptionConfig>,
        client_factory: HttpClientServiceFactory,
    ) -> Result<Self, BoxError> {
        let name: String = service.into();

        let enable_apq = configuration
            .apq
            .subgraph
            .subgraphs
            .get(&name)
            .map(|apq| apq.enabled)
            .unwrap_or(configuration.apq.subgraph.all.enabled);

        SubgraphService::new(
            name,
            enable_apq,
            subscription_config,
            configuration.notify.clone(),
            client_factory,
        )
    }

    pub(crate) fn new(
        service: impl Into<String>,
        enable_apq: bool,
        subscription_config: Option<SubscriptionConfig>,
        notify: Notify<String, graphql::Response>,
        client_factory: crate::services::http::HttpClientServiceFactory,
    ) -> Result<Self, BoxError> {
        Ok(Self {
            client_factory,
            service: Arc::new(service.into()),
            apq: Arc::new(<AtomicBool>::new(enable_apq)),
            subscription_config,
            notify,
        })
    }
}

pub(crate) fn generate_tls_client_config(
    tls_cert_store: Option<RootCertStore>,
    client_cert_config: Option<&TlsClientAuth>,
) -> Result<rustls::ClientConfig, BoxError> {
    let tls_builder = rustls::ClientConfig::builder();
    Ok(match (tls_cert_store, client_cert_config) {
        (None, None) => tls_builder.with_native_roots()?.with_no_client_auth(),
        (Some(store), None) => tls_builder
            .with_root_certificates(store)
            .with_no_client_auth(),
        (None, Some(client_auth_config)) => {
            tls_builder.with_native_roots()?.with_client_auth_cert(
                client_auth_config.certificate_chain.clone(),
                client_auth_config.key.clone_key(),
            )?
        }
        (Some(store), Some(client_auth_config)) => tls_builder
            .with_root_certificates(store)
            .with_client_auth_cert(
                client_auth_config.certificate_chain.clone(),
                client_auth_config.key.clone_key(),
            )?,
    })
}

impl tower::Service<SubgraphRequest> for SubgraphService {
    type Response = SubgraphResponse;
    type Error = BoxError;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _cx: &mut std::task::Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, mut request: SubgraphRequest) -> Self::Future {
        let subscription_config = (request.operation_kind == OperationKind::Subscription)
            .then(|| self.subscription_config.clone())
            .flatten();
        let service_name = (*self.service).to_owned();

        // Do it only for subscription to dedup them
        let hashed_request = if request.operation_kind == OperationKind::Subscription {
            let subscription_config = match &subscription_config {
                Some(sub_cfg) => sub_cfg,
                None => {
                    return Box::pin(async move {
                        Err(BoxError::from(FetchError::SubrequestWsError {
                            service: service_name,
                            reason: "subscription is not enabled".to_string(),
                        }))
                    });
                }
            };
            if subscription_config.deduplication.enabled {
                request.to_sha256(&subscription_config.deduplication.ignored_headers)
            } else {
                Uuid::new_v4().to_string()
            }
        } else {
            String::new()
        };

        let SubgraphRequest {
            subgraph_request,
            context,
            ..
        } = request.clone();

        let (_, mut body) = subgraph_request.into_parts();

        let client_factory = self.client_factory.clone();

        let arc_apq_enabled = self.apq.clone();

        let mut notify = self.notify.clone();

        let make_calls = async move {
            // Subscription handling
            if request.operation_kind == OperationKind::Subscription
                && request.subscription_stream.is_some()
            {
                let subscription_config =
                    subscription_config.ok_or_else(|| FetchError::SubrequestHttpError {
                        service: service_name.clone(),
                        reason: "subscription is not enabled".to_string(),
                        status_code: None,
                    })?;
                let mode = subscription_config.mode.get_subgraph_config(&service_name);

                match &mode {
                    Some(SubscriptionMode::Passthrough(ws_conf)) => {
                        // call_websocket for passthrough mode
                        return call_websocket(
                            notify,
                            request,
                            context,
                            service_name,
                            ws_conf,
                            hashed_request,
                        )
                        .await;
                    }
                    Some(SubscriptionMode::Callback(CallbackMode {
                        public_url,
                        heartbeat_interval,
                        ..
                    })) => {
                        // Hash the subgraph_request
                        let subscription_id = hashed_request;

                        let operation_name =
                            context.get::<_, String>(OPERATION_NAME).ok().flatten();
                        // Call create_or_subscribe on notify
                        let (handle, created) = notify
                            .create_or_subscribe(subscription_id.clone(), true, operation_name)
                            .await?;

                        // If it existed before just send the right stream (handle) and early return
                        let stream_tx = request.subscription_stream.clone().ok_or_else(|| {
                            FetchError::SubrequestWsError {
                                service: service_name.clone(),
                                reason: "cannot get the callback stream".to_string(),
                            }
                        })?;
                        stream_tx.send(Box::pin(handle.into_stream())).await?;

                        u64_counter!(
                            "apollo.router.operations.subscriptions",
                            "Total requests with subscription operations",
                            1,
                            subscriptions.mode = "callback",
                            subscriptions.deduplicated = !created,
                            subgraph.service.name = service_name.clone()
                        );
                        if !created {
                            // Dedup happens here
                            return Ok(SubgraphResponse::builder()
                                .subgraph_name(service_name.clone())
                                .context(context)
                                .extensions(Object::default())
                                .build());
                        }

                        // If not then put the subscription_id in the extensions for callback mode and continue
                        // Do this if the topic doesn't already exist
                        let mut callback_url = public_url.clone();
                        if callback_url.path_segments_mut().is_err() {
                            callback_url = callback_url.join(&subscription_id)?;
                        } else {
                            callback_url
                                .path_segments_mut()
                                .expect("can't happen because we checked before")
                                .push(&subscription_id);
                        }

                        // Generate verifier
                        let verifier = create_verifier(&subscription_id).map_err(|err| {
                            FetchError::SubrequestHttpError {
                                service: service_name.clone(),
                                reason: format!("{err:?}"),
                                status_code: None,
                            }
                        })?;
                        request
                            .subgraph_request
                            .headers_mut()
                            .append(ACCEPT, CALLBACK_PROTOCOL_ACCEPT.clone());

                        let subscription_extension = SubscriptionExtension {
                            subscription_id,
                            callback_url,
                            verifier,
                            heartbeat_interval_ms: heartbeat_interval
                                .into_option()
                                .map(|duration| duration.as_millis() as u64)
                                .unwrap_or(0),
                        };
                        body.extensions.insert(
                            "subscription",
                            serde_json_bytes::to_value(subscription_extension).map_err(|err| {
                                FetchError::SubrequestHttpError {
                                    service: service_name.clone(),
                                    reason: format!(
                                        "cannot serialize the subscription extension: {err:?}",
                                    ),
                                    status_code: None,
                                }
                            })?,
                        );
                    }
                    _ => {
                        return Err(Box::new(FetchError::SubrequestWsError {
                            service: service_name.clone(),
                            reason: "subscription mode is not enabled".to_string(),
                        }));
                    }
                }
            }

            // If APQ is not enabled, simply make the graphql call
            // with the same request body.
            let apq_enabled = arc_apq_enabled.as_ref();
            if !apq_enabled.load(Relaxed) {
                return call_http(
                    request,
                    body,
                    context,
                    client_factory.clone(),
                    &service_name,
                )
                .await;
            }

            // Else, if APQ is enabled,
            // Calculate the query hash and try the request with
            // a persistedQuery instead of the whole query.
            let graphql::Request {
                query,
                operation_name,
                variables,
                extensions,
            } = body.clone();

            let hash_value = apq::calculate_hash_for_query(query.as_deref().unwrap_or_default());

            let persisted_query = serde_json_bytes::json!({
                HASH_VERSION_KEY: HASH_VERSION_VALUE,
                HASH_KEY: hash_value
            });

            let mut extensions_with_apq = extensions.clone();
            extensions_with_apq.insert(PERSISTED_QUERY_KEY, persisted_query);

            let mut apq_body = graphql::Request {
                query: None,
                operation_name,
                variables,
                extensions: extensions_with_apq,
            };

            let response = call_http(
                request.clone(),
                apq_body.clone(),
                context.clone(),
                client_factory.clone(),
                &service_name,
            )
            .await?;

            // Check the error for the request with only persistedQuery.
            // If PersistedQueryNotSupported, disable APQ for this subgraph
            // If PersistedQueryNotFound, add the original query to the request and retry.
            // Else, return the response like before.
            let gql_response = response.response.body();
            match get_apq_error(gql_response) {
                APQError::PersistedQueryNotSupported => {
                    apq_enabled.store(false, Relaxed);
                    call_http(
                        request,
                        body,
                        context,
                        client_factory.clone(),
                        &service_name,
                    )
                    .await
                }
                APQError::PersistedQueryNotFound => {
                    apq_body.query = query;
                    call_http(
                        request,
                        apq_body,
                        context,
                        client_factory.clone(),
                        &service_name,
                    )
                    .await
                }
                _ => Ok(response),
            }
        };

        Box::pin(make_calls)
    }
}

/// call websocket makes websocket calls with modified graphql::Request (body)
async fn call_websocket(
    mut notify: Notify<String, graphql::Response>,
    request: SubgraphRequest,
    context: Context,
    service_name: String,
    subgraph_cfg: &WebSocketConfiguration,
    subscription_hash: String,
) -> Result<SubgraphResponse, BoxError> {
    let operation_name = request
        .subgraph_request
        .body()
        .operation_name
        .clone()
        .unwrap_or_default();

    let subgraph_request_event = context
        .extensions()
        .with_lock(|lock| lock.get::<SubgraphEventRequest>().cloned());
    let log_request_level = subgraph_request_event.and_then(|s| {
        if s.condition.lock().evaluate_request(&request) == Some(true) {
            Some(s.level)
        } else {
            None
        }
    });

    let SubgraphRequest {
        subgraph_request,
        subscription_stream,
        id: subgraph_request_id,
        ..
    } = request;
    let subscription_stream_tx =
        subscription_stream.ok_or_else(|| FetchError::SubrequestWsError {
            service: service_name.clone(),
            reason: "cannot get the websocket stream".to_string(),
        })?;
    let supergraph_operation_name = context.get::<_, String>(OPERATION_NAME).ok().flatten();
    let (handle, created) = notify
        .create_or_subscribe(subscription_hash.clone(), false, supergraph_operation_name)
        .await?;
    u64_counter!(
        "apollo.router.operations.subscriptions",
        "Total requests with subscription operations",
        1,
        subscriptions.mode = "passthrough",
        subscriptions.deduplicated = !created,
        subgraph.service.name = service_name.clone()
    );
    if !created {
        subscription_stream_tx
            .send(Box::pin(handle.into_stream()))
            .await?;

        // Dedup happens here
        return Ok(SubgraphResponse::builder()
            .context(context)
            .subgraph_name(service_name.clone())
            .extensions(Object::default())
            .build());
    }

    let (parts, body) = subgraph_request.into_parts();

    // Check context key and Authorization header (context key takes precedence) to set connection params if needed
    let connection_params = match (
        context.get_json_value(SUBSCRIPTION_WS_CUSTOM_CONNECTION_PARAMS),
        parts
            .headers
            .get(http::header::AUTHORIZATION)
            .and_then(|auth| auth.to_str().ok()),
    ) {
        (Some(connection_params), _) => Some(connection_params),
        (None, Some(authorization)) => Some(serde_json_bytes::json!({ "token": authorization })),
        _ => None,
    };

    let request = get_websocket_request(service_name.clone(), parts, subgraph_cfg)?;

    let signing_params = context
        .extensions()
        .with_lock(|lock| lock.get::<Arc<SigningParamsConfig>>().cloned());

    let request = if let Some(signing_params) = signing_params {
        signing_params
            .sign_empty(request, service_name.as_str())
            .await?
    } else {
        request
    };

    if let Some(level) = log_request_level {
        let mut attrs = Vec::with_capacity(5);
        attrs.push(KeyValue::new(
            Key::from_static_str("http.request.headers"),
            opentelemetry::Value::String(format!("{:?}", request.headers()).into()),
        ));
        attrs.push(KeyValue::new(
            Key::from_static_str("http.request.method"),
            opentelemetry::Value::String(format!("{}", request.method()).into()),
        ));
        attrs.push(KeyValue::new(
            Key::from_static_str("http.request.version"),
            opentelemetry::Value::String(format!("{:?}", request.version()).into()),
        ));
        attrs.push(KeyValue::new(
            Key::from_static_str("http.request.body"),
            opentelemetry::Value::String(
                serde_json::to_string(request.body())
                    .unwrap_or_default()
                    .into(),
            ),
        ));
        attrs.push(KeyValue::new(
            Key::from_static_str("subgraph.name"),
            opentelemetry::Value::String(service_name.clone().into()),
        ));
        log_event(
            level,
            "subgraph.request",
            attrs,
            &format!("Websocket request body to subgraph {service_name:?}"),
        );
    }

    let uri = request.uri();
    let path = uri.path();
    let host = uri.host().unwrap_or_default();
    let port = uri.port_u16().unwrap_or_else(|| {
        let scheme = uri.scheme_str();
        if scheme == Some("wss") {
            443
        } else if scheme == Some("ws") {
            80
        } else {
            0
        }
    });

    let subgraph_req_span = tracing::info_span!(SUBGRAPH_REQUEST_SPAN_NAME,
        "otel.kind" = "CLIENT",
        "net.peer.name" = %host,
        "net.peer.port" = %port,
        "http.route" = %path,
        "http.url" = %uri,
        "net.transport" = "ip_tcp",
        "apollo.subgraph.name" = %service_name,
        "graphql.operation.name" = %operation_name,
    );

    let (ws_stream, resp) = match request.uri().scheme_str() {
        Some("wss") => {
            connect_async_tls_with_config(request, None, false, None)
                .instrument(subgraph_req_span)
                .await
        }
        _ => connect_async(request).instrument(subgraph_req_span).await,
    }
    .map_err(|err| {
        let error_details = match &err {
            tokio_tungstenite::tungstenite::Error::Utf8(details) => {
                format!("invalid UTF-8 in WebSocket handshake: {details}")
            }

            tokio_tungstenite::tungstenite::Error::Http(response) => {
                let status = response.status();
                let headers = response
                    .headers()
                    .iter()
                    .map(|(k, v)| {
                        let header_value = v.to_str().unwrap_or("HTTP Error");
                        format!("{k:?}: {header_value:?}")
                    })
                    .collect::<Vec<String>>()
                    .join("; ");

                format!("WebSocket upgrade failed. Status: {status}; Headers: [{headers}]")
            }

            tokio_tungstenite::tungstenite::Error::Protocol(proto_err) => {
                format!("WebSocket protocol error: {proto_err}")
            }

            other_error => other_error.to_string(),
        };

        tracing::debug!(
            error.type   = "websocket_connection_failed",
            error.details= %error_details,
            error.source = %std::any::type_name_of_val(&err),
            "WebSocket connection failed"
        );

        FetchError::SubrequestWsError {
            service: service_name.clone(),
            reason: format!("cannot connect websocket to subgraph: {error_details}"),
        }
    })?;

    let gql_socket = GraphqlWebSocket::new(
        convert_websocket_stream(ws_stream, subscription_hash.clone()),
        subscription_hash,
        subgraph_cfg.protocol,
        connection_params,
    )
    .await
    .map_err(|err| FetchError::SubrequestWsError {
        service: service_name.clone(),
        reason: format!("cannot get the GraphQL websocket stream: {}", err.message),
    })?;

    let gql_stream = gql_socket
        .into_subscription(body, subgraph_cfg.heartbeat_interval.into_option())
        .await
        .map_err(|err| FetchError::SubrequestWsError {
            service: service_name.clone(),
            reason: format!("cannot send the subgraph request to websocket stream: {err:?}"),
        })?;

    let (handle_sink, handle_stream) = handle.split();

    // Forward GraphQL subscription stream to WebSocket handle
    // Connection lifecycle is managed by the WebSocket infrastructure,
    // so we don't need to handle connection_closed_signal here
    tokio::task::spawn(async move {
        if let Err(e) = gql_stream
            .map(Ok::<_, graphql::Error>)
            .forward(handle_sink)
            .await
        {
            tracing::debug!("WebSocket subscription stream ended: {}", e);
        }
    });

    subscription_stream_tx.send(Box::pin(handle_stream)).await?;

    Ok(SubgraphResponse::new_from_response(
        resp.map(|_| graphql::Response::default()),
        context,
        service_name,
        subgraph_request_id,
    ))
}

// Utility function to extract uri details.
fn get_uri_details(uri: &hyper::Uri) -> (&str, u16, &str) {
    let port = uri.port_u16().unwrap_or_else(|| {
        let scheme = uri.scheme_str();
        if scheme == Some("https") {
            443
        } else if scheme == Some("http") {
            80
        } else {
            0
        }
    });

    (uri.host().unwrap_or_default(), port, uri.path())
}

// Utility function to create a graphql response from HTTP response components
fn http_response_to_graphql_response(
    service_name: &str,
    content_type: Result<ContentType, FetchError>,
    body: Option<Result<Bytes, FetchError>>,
    parts: &Parts,
) -> graphql::Response {
    let mut graphql_response = match (content_type, body, parts.status.is_success()) {
        (Ok(ContentType::ApplicationGraphqlResponseJson), Some(Ok(body)), _)
        | (Ok(ContentType::ApplicationJson), Some(Ok(body)), true) => {
            // Application graphql json expects valid graphql response
            // Application json expects valid graphql response if 2xx
            tracing::debug_span!("parse_subgraph_response").in_scope(|| {
                // Application graphql json expects valid graphql response
                graphql::Response::from_bytes(body).unwrap_or_else(|error| {
                    let error = FetchError::SubrequestMalformedResponse {
                        service: service_name.to_owned(),
                        reason: error.reason,
                    };
                    graphql::Response::builder()
                        .error(error.to_graphql_error(None))
                        .build()
                })
            })
        }
        (Ok(ContentType::ApplicationJson), Some(Ok(body)), false) => {
            // Application json does not expect a valid graphql response if not 2xx.
            // If parse fails then attach the entire payload as an error
            tracing::debug_span!("parse_subgraph_response").in_scope(|| {
                // Application graphql json expects valid graphql response
                let mut original_response = String::from_utf8_lossy(&body).to_string();
                if original_response.is_empty() {
                    original_response = "<empty response body>".into()
                }
                graphql::Response::from_bytes(body).unwrap_or_else(|_error| {
                    graphql::Response::builder()
                        .error(
                            FetchError::SubrequestMalformedResponse {
                                service: service_name.to_string(),
                                reason: original_response,
                            }
                            .to_graphql_error(None),
                        )
                        .build()
                })
            })
        }
        (content_type, body, _) => {
            // Something went wrong, compose a response with errors if they are present
            let mut graphql_response = graphql::Response::builder().build();
            if let Err(err) = content_type {
                graphql_response.errors.push(err.to_graphql_error(None));
            }
            if let Some(Err(err)) = body {
                graphql_response.errors.push(err.to_graphql_error(None));
            }
            graphql_response
        }
    };

    // Add an error for response codes that are not 2xx
    if !parts.status.is_success() {
        let status = parts.status;
        graphql_response.errors.insert(
            0,
            FetchError::SubrequestHttpError {
                service: service_name.to_string(),
                status_code: Some(status.as_u16()),
                reason: format!(
                    "{}: {}",
                    status.as_str(),
                    status.canonical_reason().unwrap_or("Unknown")
                ),
            }
            .to_graphql_error(None),
        )
    }
    graphql_response
}

/// Process a single subgraph batch request
#[instrument(skip(client_factory, contexts, request))]
pub(crate) async fn process_batch(
    client_factory: HttpClientServiceFactory,
    service: String,
    mut contexts: Vec<(Context, SubgraphRequestId)>,
    mut request: http::Request<RouterBody>,
    listener_count: usize,
) -> Result<Vec<SubgraphResponse>, FetchError> {
    // Now we need to "batch up" our data and send it to our subgraphs
    request
        .headers_mut()
        .insert(CONTENT_TYPE, APPLICATION_JSON_HEADER_VALUE.clone());
    request
        .headers_mut()
        .append(ACCEPT, ACCEPT_GRAPHQL_JSON.clone());

    let schema_uri = request.uri();
    let (host, port, path) = get_uri_details(schema_uri);

    // We can't provide a single operation name in the span (since we may be processing multiple
    // operations). Product decision, use the hard coded value "batch".
    let subgraph_req_span = tracing::info_span!(SUBGRAPH_REQUEST_SPAN_NAME,
        "otel.kind" = "CLIENT",
        "net.peer.name" = %host,
        "net.peer.port" = %port,
        "http.route" = %path,
        "http.url" = %schema_uri,
        "net.transport" = "ip_tcp",
        "apollo.subgraph.name" = %&service,
        "graphql.operation.name" = "batch"
    );

    // The graphql spec is lax about what strategy to use for processing responses: https://github.com/graphql/graphql-over-http/blob/main/spec/GraphQLOverHTTP.md#processing-the-response
    //
    // "If the response uses a non-200 status code and the media type of the response payload is application/json
    // then the client MUST NOT rely on the body to be a well-formed GraphQL response since the source of the response
    // may not be the server but instead some intermediary such as API gateways, proxies, firewalls, etc."
    //
    // The TLDR of this is that it's really asking us to do the best we can with whatever information we have with some modifications depending on content type.
    // Our goal is to give the user the most relevant information possible in the response errors
    //
    // Rules:
    // 1. If the content type of the response is not `application/json` or `application/graphql-response+json` then we won't try to parse.
    // 2. If an HTTP status is not 2xx it will always be attached as a graphql error.
    // 3. If the response type is `application/json` and status is not 2xx and the body the entire body will be output if the response is not valid graphql.

    // We need a "representative context" for a batch. We use the first context in our list of
    // contexts
    let batch_context = contexts
        .first()
        .expect("we have at least one context in the batch")
        .0
        .clone();
    let client = client_factory.create(&service);

    // Update our batching metrics (just before we fetch)
    u64_histogram!(
        "apollo.router.operations.batching.size",
        "Number of queries contained within each query batch",
        listener_count as u64,
        mode = BatchingMode::BatchHttpLink.to_string(), // Only supported mode right now
        subgraph = service.clone()
    );

    u64_counter!(
        "apollo.router.operations.batching",
        "Total requests with batched operations",
        1,
        // XXX(@goto-bus-stop): Should these be `batching.mode`, `batching.subgraph`?
        // Also, other metrics use a different convention to report the subgraph name
        mode = BatchingMode::BatchHttpLink.to_string(), // Only supported mode right now
        subgraph = service.clone()
    );

    // Perform the actual fetch. If this fails then we didn't manage to make the call at all, so we can't do anything with it.
    tracing::debug!("fetching from subgraph: {service}");
    let (parts, content_type, body) = match do_fetch(client, &batch_context, &service, request)
        .instrument(subgraph_req_span)
        .await
    {
        Ok(res) => res,
        Err(err) => {
            let resp = http::Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(err.to_graphql_error(None))
                .map_err(|err| FetchError::SubrequestHttpError {
                    status_code: None,
                    service: service.clone(),
                    reason: format!("cannot create the http response from error: {err:?}"),
                })?;
            let (parts, body) = resp.into_parts();
            let body =
                serde_json::to_vec(&body).map_err(|err| FetchError::SubrequestHttpError {
                    status_code: None,
                    service: service.clone(),
                    reason: format!("cannot serialize the error: {err:?}"),
                })?;
            (
                parts,
                Ok(ContentType::ApplicationJson),
                Some(Ok(body.into())),
            )
        }
    };

    let subgraph_response_event = batch_context
        .extensions()
        .with_lock(|lock| lock.get::<SubgraphEventResponse>().cloned());
    if let Some(event) = subgraph_response_event {
        let mut attrs = Vec::with_capacity(5);
        attrs.push(KeyValue::new(
            Key::from_static_str("http.response.headers"),
            opentelemetry::Value::String(format!("{:?}", parts.headers).into()),
        ));
        attrs.push(KeyValue::new(
            Key::from_static_str("http.response.status"),
            opentelemetry::Value::String(format!("{}", parts.status).into()),
        ));
        attrs.push(KeyValue::new(
            Key::from_static_str("http.response.version"),
            opentelemetry::Value::String(format!("{:?}", parts.version).into()),
        ));
        if let Some(Ok(b)) = &body {
            attrs.push(KeyValue::new(
                Key::from_static_str("http.response.body"),
                opentelemetry::Value::String(String::from_utf8_lossy(b).to_string().into()),
            ));
        }
        attrs.push(KeyValue::new(
            Key::from_static_str("subgraph.name"),
            opentelemetry::Value::String(service.clone().into()),
        ));
        log_event(
            event.level,
            "subgraph.response",
            attrs,
            &format!("Raw response from subgraph {service:?} received"),
        );
    }

    tracing::debug!("parts: {parts:?}, content_type: {content_type:?}, body: {body:?}");
    let value =
        serde_json::from_slice(&body.ok_or(FetchError::SubrequestMalformedResponse {
            service: service.to_string(),
            reason: "no body in response".to_string(),
        })??)
        .map_err(|error| FetchError::SubrequestMalformedResponse {
            service: service.to_string(),
            reason: error.to_string(),
        })?;

    tracing::debug!("json value from body is: {value:?}");

    let array = ensure_array!(value).map_err(|error| FetchError::SubrequestMalformedResponse {
        service: service.to_string(),
        reason: error.to_string(),
    })?;
    let mut graphql_responses = Vec::with_capacity(array.len());
    for value in array {
        let object =
            ensure_object!(value).map_err(|error| FetchError::SubrequestMalformedResponse {
                service: service.to_string(),
                reason: error.to_string(),
            })?;

        // Map our Vec<u8> into Bytes
        // Map our serde conversion error to a FetchError
        let body = Some(
            serde_json::to_vec(&object)
                .map(|v| v.into())
                .map_err(|error| FetchError::SubrequestMalformedResponse {
                    service: service.to_string(),
                    reason: error.to_string(),
                }),
        );

        let graphql_response =
            http_response_to_graphql_response(&service, content_type.clone(), body, &parts);
        graphql_responses.push(graphql_response);
    }

    tracing::debug!("we have a vec of graphql_responses: {graphql_responses:?}");
    // Before we process our graphql responses, ensure that we have a context for each
    // response
    if graphql_responses.len() != contexts.len() {
        return Err(FetchError::SubrequestBatchingError {
            service,
            reason: format!(
                "number of contexts ({}) is not equal to number of graphql responses ({})",
                contexts.len(),
                graphql_responses.len()
            ),
        });
    }

    // We are going to pop contexts from the back, so let's reverse our contexts
    contexts.reverse();
    let subgraph_name = service.clone();
    // Build an http Response for each graphql response
    let subgraph_responses: Result<Vec<_>, _> = graphql_responses
        .into_iter()
        .map(|res| {
            let subgraph_name = subgraph_name.clone();
            http::Response::builder()
                .status(parts.status)
                .version(parts.version)
                .body(res)
                .map(|mut http_res| {
                    *http_res.headers_mut() = parts.headers.clone();
                    // Use the original context for the request to create the response
                    let (context, id) =
                        contexts.pop().expect("we have a context for each response");
                    let resp =
                        SubgraphResponse::new_from_response(http_res, context, subgraph_name, id);

                    tracing::debug!("we have a resp: {resp:?}");
                    resp
                })
                .map_err(|e| FetchError::MalformedResponse {
                    reason: e.to_string(),
                })
        })
        .collect();

    tracing::debug!("we have a vec of subgraph_responses: {subgraph_responses:?}");
    subgraph_responses
}

/// Notify all listeners of a batch query of the results
pub(crate) async fn notify_batch_query(
    service: String,
    senders: Vec<oneshot::Sender<Result<SubgraphResponse, BoxError>>>,
    responses: Result<Vec<SubgraphResponse>, FetchError>,
) -> Result<(), BoxError> {
    tracing::debug!(
        "handling response for service '{service}' with {} listeners: {responses:#?}",
        senders.len()
    );

    match responses {
        // If we had an error processing the batch, then pipe that error to all of the listeners
        Err(e) => {
            for tx in senders {
                // Try to notify all waiters. If we can't notify an individual sender, then log an error
                // which, unlike failing to notify on success (see below), contains the the entire error
                // response.
                if let Err(log_error) = tx.send(Err(Box::new(e.clone()))).map_err(|error| {
                    FetchError::SubrequestBatchingError {
                        service: service.clone(),
                        reason: format!("tx send failed: {error:?}"),
                    }
                }) {
                    tracing::error!(service, error=%log_error, "failed to notify sender that batch processing failed");
                }
            }
        }

        Ok(rs) => {
            // Before we process our graphql responses, ensure that we have a tx for each
            // response
            if senders.len() != rs.len() {
                return Err(Box::new(FetchError::SubrequestBatchingError {
                    service,
                    reason: format!(
                        "number of txs ({}) is not equal to number of graphql responses ({})",
                        senders.len(),
                        rs.len()
                    ),
                }));
            }

            // We have checked before we started looping that we had a tx for every
            // graphql_response, so zip_eq shouldn't panic.
            // Use the tx to send a graphql_response message to each waiter.
            for (response, sender) in rs.into_iter().zip_eq(senders) {
                if let Err(log_error) = sender
                    .send(Ok(response))
                    // If we fail to notify the waiter that our request succeeded, do not log
                    // out the entire response since this may be substantial and/or contain
                    // PII data. Simply log that the send failed.
                    .map_err(|_error| FetchError::SubrequestBatchingError {
                        service: service.to_string(),
                        reason: "tx send failed".to_string(),
                    })
                {
                    tracing::error!(service, error=%log_error, "failed to notify sender that batch processing succeeded");
                }
            }
        }
    }

    Ok(())
}

type BatchInfo = (
    (
        String,
        http::Request<RouterBody>,
        Vec<(Context, SubgraphRequestId)>,
        usize,
    ),
    Vec<oneshot::Sender<Result<SubgraphResponse, BoxError>>>,
);

/// Collect all batch requests and process them concurrently
#[instrument(skip_all)]
pub(crate) async fn process_batches(
    client_factory: HttpClientServiceFactory,
    svc_map: HashMap<String, Vec<BatchQueryInfo>>,
) -> Result<(), BoxError> {
    // We need to strip out the senders so that we can work with them separately.
    let mut errors = vec![];
    let (info, txs): (Vec<_>, Vec<_>) =
        futures::future::join_all(svc_map.into_iter().map(|(service, requests)| async {
            let (_op_name, contexts, request, txs) = assemble_batch(requests).await?;

            Ok(((service, request, contexts, txs.len()), txs))
        }))
        .await
        .into_iter()
        .filter_map(|x: Result<BatchInfo, BoxError>| x.map_err(|e| errors.push(e)).ok())
        .unzip();

    // If errors isn't empty, then process_batches cannot proceed. Let's log out the errors and
    // return
    if !errors.is_empty() {
        for error in errors {
            tracing::error!("assembling batch failed: {error}");
        }
        return Err(SubgraphBatchingError::ProcessingFailed(
            "assembling batches failed".to_string(),
        )
        .into());
    }
    // Collect all of the processing logic and run them concurrently, collecting all errors
    let cf = &client_factory;
    // It is not ok to panic if the length of the txs and info do not match. Let's make sure they
    // do
    if txs.len() != info.len() {
        return Err(SubgraphBatchingError::ProcessingFailed(
            "length of txs and info are not equal".to_string(),
        )
        .into());
    }
    let batch_futures = info.into_iter().zip_eq(txs).map(
        |((service, request, contexts, listener_count), senders)| async move {
            let batch_result = process_batch(
                cf.clone(),
                service.clone(),
                contexts,
                request,
                listener_count,
            )
            .await;

            notify_batch_query(service, senders, batch_result).await
        },
    );

    futures::future::try_join_all(batch_futures).await?;

    Ok(())
}

async fn call_http(
    request: SubgraphRequest,
    body: graphql::Request,
    context: Context,
    client_factory: HttpClientServiceFactory,
    service_name: &str,
) -> Result<SubgraphResponse, BoxError> {
    // We use configuration to determine if calls may be batched. If we have Batching
    // configuration, then we check (batch_include()) if the current subgraph has batching enabled
    // in configuration. If it does, we then start to process a potential batch.
    //
    // If we are processing a batch, then we'd like to park tasks here, but we can't park them whilst
    // we have the context extensions lock held. That would be very bad...
    // We grab the (potential) BatchQuery and then operate on it later
    let opt_batch_query = context.extensions().with_lock(|lock| {
        lock.get::<Batching>()
            .and_then(|batching_config| batching_config.batch_include(service_name).then_some(()))
            .and_then(|_| lock.get::<BatchQuery>().cloned())
            .and_then(|bq| (!bq.finished()).then_some(bq))
    });

    // If we have a batch query, then it's time for batching
    if let Some(query) = opt_batch_query {
        // Let the owning batch know that this query is ready to process, getting back the channel
        // from which we'll eventually receive our response.
        let response_rx = query.signal_progress(client_factory, request, body).await?;

        // Park this query until we have our response and pass it back up
        response_rx
            .await
            .map_err(|err| FetchError::SubrequestBatchingError {
                service: service_name.to_string(),
                reason: format!("tx receive failed: {err}"),
            })?
    } else {
        tracing::debug!("we called http");
        let client = client_factory.create(service_name);
        call_single_http(request, body, context, client, service_name).await
    }
}

/// call_single_http makes http calls with modified graphql::Request (body)
pub(crate) async fn call_single_http(
    request: SubgraphRequest,
    body: graphql::Request,
    context: Context,
    client: crate::services::http::BoxService,
    service_name: &str,
) -> Result<SubgraphResponse, BoxError> {
    let subgraph_request_event = context
        .extensions()
        .with_lock(|lock| lock.get::<SubgraphEventRequest>().cloned());
    let log_request_level = subgraph_request_event.and_then(|s| {
        if s.condition.lock().evaluate_request(&request) == Some(true) {
            Some(s.level)
        } else {
            None
        }
    });

    let SubgraphRequest {
        subgraph_request,
        id: subgraph_request_id,
        ..
    } = request;

    let operation_name = subgraph_request
        .body()
        .operation_name
        .clone()
        .unwrap_or_default();

    let (parts, _) = subgraph_request.into_parts();
    let body = serde_json::to_string(&body)?;
    tracing::debug!("our JSON body: {body:?}");
    let mut request = http::Request::from_parts(parts, router::body::from_bytes(body));

    request
        .headers_mut()
        .insert(CONTENT_TYPE, APPLICATION_JSON_HEADER_VALUE.clone());
    request
        .headers_mut()
        .append(ACCEPT, ACCEPT_GRAPHQL_JSON.clone());

    let schema_uri = request.uri();
    let (host, port, path) = get_uri_details(schema_uri);

    let subgraph_req_span = tracing::info_span!(SUBGRAPH_REQUEST_SPAN_NAME,
        "otel.kind" = "CLIENT",
        "net.peer.name" = %host,
        "net.peer.port" = %port,
        "http.route" = %path,
        "http.url" = %schema_uri,
        "net.transport" = "ip_tcp",
        "apollo.subgraph.name" = %service_name,
        "graphql.operation.name" = %operation_name,
    );

    // The graphql spec is lax about what strategy to use for processing responses: https://github.com/graphql/graphql-over-http/blob/main/spec/GraphQLOverHTTP.md#processing-the-response
    //
    // "If the response uses a non-200 status code and the media type of the response payload is application/json
    // then the client MUST NOT rely on the body to be a well-formed GraphQL response since the source of the response
    // may not be the server but instead some intermediary such as API gateways, proxies, firewalls, etc."
    //
    // The TLDR of this is that it's really asking us to do the best we can with whatever information we have with some modifications depending on content type.
    // Our goal is to give the user the most relevant information possible in the response errors
    //
    // Rules:
    // 1. If the content type of the response is not `application/json` or `application/graphql-response+json` then we won't try to parse.
    // 2. If an HTTP status is not 2xx it will always be attached as a graphql error.
    // 3. If the response type is `application/json` and status is not 2xx and the body the entire body will be output if the response is not valid graphql.

    // TODO: Temporary solution to plug FileUploads plugin until 'http_client' will be fixed https://github.com/apollographql/router/pull/4666
    let request = file_uploads::http_request_wrapper(request).await;

    if let Some(level) = log_request_level {
        let mut attrs = Vec::with_capacity(5);
        attrs.push(KeyValue::new(
            Key::from_static_str("http.request.headers"),
            opentelemetry::Value::String(format!("{:?}", request.headers()).into()),
        ));
        attrs.push(KeyValue::new(
            Key::from_static_str("http.request.method"),
            opentelemetry::Value::String(format!("{}", request.method()).into()),
        ));
        attrs.push(KeyValue::new(
            Key::from_static_str("http.request.version"),
            opentelemetry::Value::String(format!("{:?}", request.version()).into()),
        ));
        attrs.push(KeyValue::new(
            Key::from_static_str("http.request.body"),
            opentelemetry::Value::String(format!("{:?}", request.body()).into()),
        ));
        attrs.push(KeyValue::new(
            Key::from_static_str("subgraph.name"),
            opentelemetry::Value::String(service_name.to_string().into()),
        ));

        log_event(
            level,
            "subgraph.request",
            attrs,
            &format!("Request to subgraph {service_name:?}"),
        );
    }

    // Perform the actual fetch. If this fails then we didn't manage to make the call at all, so we can't do anything with it.
    let (parts, content_type, body) = match do_fetch(client, &context, service_name, request)
        .instrument(subgraph_req_span)
        .await
    {
        Ok(resp) => resp,
        Err(err) => {
            return Ok(SubgraphResponse::builder()
                .subgraph_name(service_name.to_string())
                .error(err.to_graphql_error(None))
                .status_code(StatusCode::INTERNAL_SERVER_ERROR)
                .context(context)
                .extensions(Object::default())
                .build());
        }
    };

    let subgraph_response_event = context
        .extensions()
        .with_lock(|lock| lock.get::<SubgraphEventResponse>().cloned());

    if let Some(subgraph_response_event) = subgraph_response_event {
        // We have to do this in order to use selectors
        let mut resp_builder = http::Response::builder()
            .status(parts.status)
            .version(parts.version);
        if let Some(headers) = resp_builder.headers_mut() {
            *headers = parts.headers.clone();
        }
        let subgraph_response = SubgraphResponse::new_from_response(
            resp_builder
                .body(graphql::Response::default())
                .expect("it won't fail everything is coming from an existing response"),
            context.clone(),
            service_name.to_owned(),
            subgraph_request_id.clone(),
        );

        let should_log = subgraph_response_event
            .condition
            .evaluate_response(&subgraph_response);
        if should_log {
            let mut attrs = Vec::with_capacity(5);
            attrs.push(KeyValue::new(
                Key::from_static_str("http.response.headers"),
                opentelemetry::Value::String(format!("{:?}", parts.headers).into()),
            ));
            attrs.push(KeyValue::new(
                Key::from_static_str("http.response.status"),
                opentelemetry::Value::String(format!("{}", parts.status).into()),
            ));
            attrs.push(KeyValue::new(
                Key::from_static_str("http.response.version"),
                opentelemetry::Value::String(format!("{:?}", parts.version).into()),
            ));
            if let Some(Ok(b)) = &body {
                attrs.push(KeyValue::new(
                    Key::from_static_str("http.response.body"),
                    opentelemetry::Value::String(String::from_utf8_lossy(b).to_string().into()),
                ));
            }
            attrs.push(KeyValue::new(
                Key::from_static_str("subgraph.name"),
                opentelemetry::Value::String(service_name.to_string().into()),
            ));
            log_event(
                subgraph_response_event.level,
                "subgraph.response",
                attrs,
                &format!("Raw response from subgraph {service_name:?} received"),
            );
        }
    }

    let graphql_response =
        http_response_to_graphql_response(service_name, content_type, body, &parts);

    let resp = http::Response::from_parts(parts, graphql_response);
    Ok(SubgraphResponse::new_from_response(
        resp,
        context,
        service_name.to_owned(),
        subgraph_request_id,
    ))
}

#[derive(Clone, Debug)]
enum ContentType {
    ApplicationJson,
    ApplicationGraphqlResponseJson,
}

fn get_graphql_content_type(service_name: &str, parts: &Parts) -> Result<ContentType, FetchError> {
    if let Some(raw_content_type) = parts.headers.get(header::CONTENT_TYPE) {
        let content_type = raw_content_type
            .to_str()
            .ok()
            .and_then(|str| MediaType::parse(str).ok());

        match content_type {
            Some(mime) if mime.ty == APPLICATION && mime.subty == JSON => {
                Ok(ContentType::ApplicationJson)
            }
            Some(mime)
                if mime.ty == APPLICATION
                    && mime.subty == GRAPHQL_RESPONSE
                    && mime.suffix == Some(JSON) =>
            {
                Ok(ContentType::ApplicationGraphqlResponseJson)
            }
            Some(mime) => Err(format!(
                "subgraph response contains unsupported content-type: {}",
                mime,
            )),
            None => Err(format!(
                "subgraph response contains invalid 'content-type' header value {:?}",
                raw_content_type,
            )),
        }
    } else {
        Err("subgraph response does not contain 'content-type' header".to_owned())
    }
    .map_err(|reason| FetchError::SubrequestHttpError {
        status_code: Some(parts.status.as_u16()),
        service: service_name.to_string(),
        reason: format!(
            "{}; expected content-type: {} or content-type: {}",
            reason,
            APPLICATION_JSON.essence_str(),
            GRAPHQL_JSON_RESPONSE_HEADER_VALUE
        ),
    })
}

async fn do_fetch(
    mut client: crate::services::http::BoxService,
    context: &Context,
    service_name: &str,
    request: Request<RouterBody>,
) -> Result<
    (
        Parts,
        Result<ContentType, FetchError>,
        Option<Result<Bytes, FetchError>>,
    ),
    FetchError,
> {
    let response = client
        .call(HttpRequest {
            http_request: request,
            context: context.clone(),
        })
        .map_err(|err| {
            tracing::error!(fetch_error = ?err);
            FetchError::SubrequestHttpError {
                status_code: None,
                service: service_name.to_string(),
                reason: err.to_string(),
            }
        })
        .await?;

    let (parts, body) = response.http_response.into_parts();

    let content_type = get_graphql_content_type(service_name, &parts);

    let body = if content_type.is_ok() {
        let body = router::body::into_bytes(body)
            .instrument(tracing::debug_span!("aggregate_response_data"))
            .await
            .map_err(|err| {
                tracing::error!(fetch_error = ?err);
                FetchError::SubrequestHttpError {
                    status_code: Some(parts.status.as_u16()),
                    service: service_name.to_string(),
                    reason: err.to_string(),
                }
            });
        Some(body)
    } else {
        None
    };
    Ok((parts, content_type, body))
}

fn get_websocket_request(
    service_name: String,
    mut parts: http::request::Parts,
    subgraph_ws_cfg: &WebSocketConfiguration,
) -> Result<http::Request<()>, FetchError> {
    let mut subgraph_url = url::Url::parse(&parts.uri.to_string()).map_err(|err| {
        tracing::error!("cannot parse subgraph url {}: {err:?}", parts.uri);
        FetchError::SubrequestWsError {
            service: service_name.clone(),
            reason: "cannot parse subgraph url".to_string(),
        }
    })?;
    let new_scheme = match subgraph_url.scheme() {
        "http" => "ws",
        "https" => "wss",
        _ => "ws",
    };
    subgraph_url.set_scheme(new_scheme).map_err(|err| {
        tracing::error!("cannot set a scheme '{new_scheme}' on subgraph url: {err:?}");

        FetchError::SubrequestWsError {
            service: service_name.clone(),
            reason: "cannot set a scheme on websocket url".to_string(),
        }
    })?;

    let subgraph_url = match &subgraph_ws_cfg.path {
        Some(path) => subgraph_url
            .join(path)
            .map_err(|_| FetchError::SubrequestWsError {
                service: service_name.clone(),
                reason: "cannot parse subgraph url with the specific websocket path".to_string(),
            })?,
        None => subgraph_url,
    };
    // XXX During hyper upgrade, observed that we had lost the implementation for Url
    // so I made the expedient decision to get a string representation (as_str())
    // for the creation of the client request. This works fine, but I'm not sure
    // why we need to do it, because into_client_request **should** be implemented
    // for Url...
    let mut request = subgraph_url.as_str().into_client_request().map_err(|err| {
        tracing::error!("cannot create websocket client request: {err:?}");

        FetchError::SubrequestWsError {
            service: service_name.clone(),
            reason: "cannot create websocket client request".to_string(),
        }
    })?;
    request.headers_mut().insert(
        http::header::SEC_WEBSOCKET_PROTOCOL,
        subgraph_ws_cfg.protocol.into(),
    );
    parts.headers.extend(request.headers_mut().drain());
    *request.headers_mut() = parts.headers;

    Ok(request)
}

fn get_apq_error(gql_response: &graphql::Response) -> APQError {
    for error in &gql_response.errors {
        // Check if error message is an APQ error
        match error.message.as_str() {
            PERSISTED_QUERY_NOT_FOUND_MESSAGE => {
                return APQError::PersistedQueryNotFound;
            }
            PERSISTED_QUERY_NOT_SUPPORTED_MESSAGE => {
                return APQError::PersistedQueryNotSupported;
            }
            _ => {}
        }
        // Check if extensions contains the APQ error in "code"
        if let Some(value) = error.extensions.get(CODE_STRING) {
            if value == PERSISTED_QUERY_NOT_FOUND_EXTENSION_CODE {
                return APQError::PersistedQueryNotFound;
            } else if value == PERSISTED_QUERY_NOT_SUPPORTED_EXTENSION_CODE {
                return APQError::PersistedQueryNotSupported;
            }
        }
    }
    APQError::Other
}

#[derive(Clone)]
pub(crate) struct SubgraphServiceFactory {
    pub(crate) services: Arc<
        HashMap<String, Buffer<subgraph::Request, BoxFuture<'static, subgraph::ServiceResult>>>,
    >,
}

impl SubgraphServiceFactory {
    pub(crate) fn new(
        services: Vec<(String, Arc<dyn MakeSubgraphService>)>,
        plugins: Arc<Plugins>,
    ) -> Self {
        let mut map = HashMap::with_capacity(services.len());
        for (name, maker) in services.into_iter() {
            let service = Buffer::new(
                plugins
                    .iter()
                    .rev()
                    .fold(maker.make(), |acc, (_, e)| e.subgraph_service(&name, acc))
                    .boxed(),
                DEFAULT_BUFFER_SIZE,
            );
            map.insert(name, service);
        }

        SubgraphServiceFactory {
            services: Arc::new(map),
        }
    }

    pub(crate) fn create(&self, name: &str) -> Option<subgraph::BoxService> {
        // Note: We have to box our cloned service to erase the type of the Buffer.
        self.services.get(name).map(|svc| svc.clone().boxed())
    }
}

/// make new instances of the subgraph service
///
/// there can be multiple instances of that service executing at any given time
pub(crate) trait MakeSubgraphService: Send + Sync + 'static {
    fn make(&self) -> subgraph::BoxService;
}

impl<S> MakeSubgraphService for S
where
    S: Service<SubgraphRequest, Response = SubgraphResponse, Error = BoxError>
        + Clone
        + Send
        + Sync
        + 'static,
    <S as Service<SubgraphRequest>>::Future: Send,
{
    fn make(&self) -> subgraph::BoxService {
        self.clone().boxed()
    }
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::net::SocketAddr;
    use std::str::FromStr;

    use SubgraphRequest;
    use axum::Router;
    use axum::body::Body;
    use axum::extract::ConnectInfo;
    use axum::extract::WebSocketUpgrade;
    use axum::extract::ws::Message;
    use axum::response::IntoResponse;
    use axum::routing::get;
    use bytes::Buf;
    use futures::StreamExt;
    use http::StatusCode;
    use http::Uri;
    use http::header::HOST;
    use serde_json_bytes::ByteString;
    use serde_json_bytes::Value;
    use tokio::net::TcpListener;
    use tokio::sync::mpsc;
    use tokio_stream::wrappers::ReceiverStream;
    use tower::ServiceExt;
    use url::Url;

    use super::*;
    use crate::Context;
    use crate::assert_response_eq_ignoring_error_id;
    use crate::graphql::Error;
    use crate::graphql::Request;
    use crate::graphql::Response;
    use crate::plugins::subscription::DeduplicationConfig;
    use crate::plugins::subscription::HeartbeatInterval;
    use crate::plugins::subscription::SUBSCRIPTION_CALLBACK_HMAC_KEY;
    use crate::plugins::subscription::SubgraphPassthroughMode;
    use crate::plugins::subscription::SubscriptionModeConfig;
    use crate::protocols::websocket::ClientMessage;
    use crate::protocols::websocket::ServerMessage;
    use crate::protocols::websocket::WebSocketProtocol;
    use crate::query_planner::fetch::OperationKind;
    use crate::services::router;

    async fn serve<Handler, Fut>(listener: TcpListener, handle: Handler) -> std::io::Result<()>
    where
        Handler: (Fn(http::Request<Body>) -> Fut) + Clone + Sync + Send + 'static,
        Fut:
            std::future::Future<Output = Result<http::Response<Body>, Infallible>> + Send + 'static,
    {
        use hyper::body::Incoming;
        use hyper_util::rt::TokioExecutor;
        use hyper_util::rt::TokioIo;

        // Not sure this is the *right* place to do it, because it's actually clients that
        // use crypto, not the server.

        loop {
            let (stream, _) = listener.accept().await?;
            let io = TokioIo::new(stream);
            let handle = handle.clone();
            tokio::spawn(async move {
                // N.B. should use hyper service_fn here, since it's required to be implemented hyper Service trait!
                let svc = hyper::service::service_fn(|request: http::Request<Incoming>| {
                    handle(request.map(Body::new))
                });
                if let Err(err) = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new())
                    .serve_connection_with_upgrades(io, svc)
                    .await
                {
                    eprintln!("server error: {}", err);
                }
            });
        }
    }

    // starts a local server emulating a subgraph returning status code 400
    async fn emulate_subgraph_bad_request(listener: TcpListener) {
        async fn handle(_request: http::Request<Body>) -> Result<http::Response<Body>, Infallible> {
            Ok(http::Response::builder()
                .header(CONTENT_TYPE, APPLICATION_JSON.essence_str())
                .status(StatusCode::BAD_REQUEST)
                .body(
                    serde_json::to_string(&Response {
                        errors: vec![
                            Error::builder()
                                .message("This went wrong")
                                .extension_code("FETCH_ERROR")
                                .build(),
                        ],
                        ..Response::default()
                    })
                    .expect("always valid")
                    .into(),
                )
                .unwrap())
        }

        serve(listener, handle).await.unwrap();
    }

    // starts a local server emulating a subgraph returning status code 401
    async fn emulate_subgraph_unauthorized(listener: TcpListener) {
        async fn handle(_request: http::Request<Body>) -> Result<http::Response<Body>, Infallible> {
            Ok(http::Response::builder()
                .header(CONTENT_TYPE, APPLICATION_JSON.essence_str())
                .status(StatusCode::UNAUTHORIZED)
                .body(r#""#.into())
                .unwrap())
        }

        serve(listener, handle).await.unwrap();
    }

    // starts a local server emulating a subgraph returning connection closed
    #[cfg(not(target_os = "macos"))]
    async fn emulate_subgraph_panic(listener: TcpListener) {
        async fn handle(_request: http::Request<Body>) -> Result<http::Response<Body>, Infallible> {
            panic!("test")
        }

        let server = axum::serve(
            listener,
            Router::new().route("/", axum::routing::any_service(tower::service_fn(handle))),
        );
        server.await.unwrap();
    }

    // starts a local server emulating a subgraph returning bad response format
    async fn emulate_subgraph_ok_status_invalid_response(listener: TcpListener) {
        async fn handle(_request: http::Request<Body>) -> Result<http::Response<Body>, Infallible> {
            Ok(http::Response::builder()
                .header(CONTENT_TYPE, APPLICATION_JSON.essence_str())
                .status(StatusCode::OK)
                .body(r#"invalid"#.into())
                .unwrap())
        }

        serve(listener, handle).await.unwrap();
    }

    // starts a local server emulating a subgraph returning bad response format
    async fn emulate_subgraph_invalid_response_invalid_status_application_json(
        listener: TcpListener,
    ) {
        async fn handle(_request: http::Request<Body>) -> Result<http::Response<Body>, Infallible> {
            Ok(http::Response::builder()
                .header(CONTENT_TYPE, APPLICATION_JSON.essence_str())
                .status(StatusCode::UNAUTHORIZED)
                .body(r#"invalid"#.into())
                .unwrap())
        }

        serve(listener, handle).await.unwrap();
    }

    // starts a local server emulating a subgraph returning bad response format
    async fn emulate_subgraph_invalid_response_invalid_status_application_graphql(
        listener: TcpListener,
    ) {
        async fn handle(_request: http::Request<Body>) -> Result<http::Response<Body>, Infallible> {
            Ok(http::Response::builder()
                .header(CONTENT_TYPE, GRAPHQL_JSON_RESPONSE_HEADER_VALUE)
                .status(StatusCode::UNAUTHORIZED)
                .body(r#"invalid"#.into())
                .unwrap())
        }

        serve(listener, handle).await.unwrap();
    }

    // starts a local server emulating a subgraph returning bad response format
    async fn emulate_subgraph_application_json_response(listener: TcpListener) {
        async fn handle(_request: http::Request<Body>) -> Result<http::Response<Body>, Infallible> {
            Ok(http::Response::builder()
                .header(CONTENT_TYPE, APPLICATION_JSON.essence_str())
                .status(StatusCode::OK)
                .body(r#"{"data": null}"#.into())
                .unwrap())
        }

        serve(listener, handle).await.unwrap();
    }

    // starts a local server emulating a subgraph returning bad response format
    async fn emulate_subgraph_application_graphql_response(listener: TcpListener) {
        async fn handle(_request: http::Request<Body>) -> Result<http::Response<Body>, Infallible> {
            Ok(http::Response::builder()
                .header(CONTENT_TYPE, GRAPHQL_JSON_RESPONSE_HEADER_VALUE)
                .status(StatusCode::OK)
                .body(r#"{"data": null}"#.into())
                .unwrap())
        }

        serve(listener, handle).await.unwrap();
    }

    // starts a local server emulating a subgraph returning response with missing content_type
    async fn emulate_subgraph_missing_content_type(listener: TcpListener) {
        async fn handle(_request: http::Request<Body>) -> Result<http::Response<Body>, Infallible> {
            Ok(http::Response::builder()
                .status(StatusCode::OK)
                .body(r#"TEST"#.into())
                .unwrap())
        }

        serve(listener, handle).await.unwrap();
    }

    // starts a local server emulating a subgraph returning response with invalid content_type
    async fn emulate_subgraph_invalid_content_type(listener: TcpListener) {
        async fn handle(_request: http::Request<Body>) -> Result<http::Response<Body>, Infallible> {
            Ok(http::Response::builder()
                .header(CONTENT_TYPE, "application/json,application/json")
                .status(StatusCode::OK)
                .body(r#"TEST"#.into())
                .unwrap())
        }

        serve(listener, handle).await.unwrap();
    }

    // starts a local server emulating a subgraph returning unsupported content_type
    async fn emulate_subgraph_unsupported_content_type(listener: TcpListener) {
        async fn handle(_request: http::Request<Body>) -> Result<http::Response<Body>, Infallible> {
            Ok(http::Response::builder()
                .header(CONTENT_TYPE, "text/html")
                .status(StatusCode::OK)
                .body(r#"TEST"#.into())
                .unwrap())
        }

        serve(listener, handle).await.unwrap();
    }

    // starts a local server emulating a subgraph returning response with
    // "errors" : {["message": "PersistedQueryNotSupported",...],...}
    async fn emulate_persisted_query_not_supported_message(listener: TcpListener) {
        async fn handle(request: http::Request<Body>) -> Result<http::Response<Body>, Infallible> {
            let (_, body) = request.into_parts();
            let graphql_request: Result<graphql::Request, &str> = router::body::into_bytes(body)
                .await
                .map_err(|_| ())
                .and_then(|bytes| serde_json::from_reader(bytes.reader()).map_err(|_| ()))
                .map_err(|_| "failed to parse the request body as JSON");
            match graphql_request {
                Ok(request) => {
                    if request.extensions.contains_key(PERSISTED_QUERY_KEY) {
                        return Ok(http::Response::builder()
                            .header(CONTENT_TYPE, APPLICATION_JSON.essence_str())
                            .status(StatusCode::OK)
                            .body(
                                serde_json::to_string(&Response {
                                    data: Some(Value::String(ByteString::from("test"))),
                                    errors: vec![
                                        Error::builder()
                                            .message(PERSISTED_QUERY_NOT_SUPPORTED_MESSAGE)
                                            .extension_code("Random code")
                                            .build(),
                                    ],
                                    ..Response::default()
                                })
                                .expect("always valid")
                                .into(),
                            )
                            .unwrap());
                    }

                    Ok(http::Response::builder()
                        .header(CONTENT_TYPE, APPLICATION_JSON.essence_str())
                        .status(StatusCode::OK)
                        .body(
                            serde_json::to_string(&Response {
                                data: Some(Value::String(ByteString::from("test"))),
                                ..Response::default()
                            })
                            .expect("always valid")
                            .into(),
                        )
                        .unwrap())
                }
                Err(_) => {
                    panic!("invalid graphql request recieved")
                }
            }
        }

        serve(listener, handle).await.unwrap();
    }

    // starts a local server emulating a subgraph returning response with
    // "errors" : {[..., "extensions": {"code": "PERSISTED_QUERY_NOT_SUPPORTED"}],...}
    async fn emulate_persisted_query_not_supported_extension_code(listener: TcpListener) {
        async fn handle(request: http::Request<Body>) -> Result<http::Response<Body>, Infallible> {
            let (_, body) = request.into_parts();
            let graphql_request: Result<graphql::Request, &str> = router::body::into_bytes(body)
                .await
                .map_err(|_| ())
                .and_then(|bytes| serde_json::from_reader(bytes.reader()).map_err(|_| ()))
                .map_err(|_| "failed to parse the request body as JSON");
            match graphql_request {
                Ok(request) => {
                    if request.extensions.contains_key(PERSISTED_QUERY_KEY) {
                        return Ok(http::Response::builder()
                            .header(CONTENT_TYPE, APPLICATION_JSON.essence_str())
                            .status(StatusCode::OK)
                            .body(
                                serde_json::to_string(&Response {
                                    data: Some(Value::String(ByteString::from("test"))),
                                    errors: vec![
                                        Error::builder()
                                            .message("Random message")
                                            .extension_code(
                                                PERSISTED_QUERY_NOT_SUPPORTED_EXTENSION_CODE,
                                            )
                                            .build(),
                                    ],
                                    ..Response::default()
                                })
                                .expect("always valid")
                                .into(),
                            )
                            .unwrap());
                    }

                    Ok(http::Response::builder()
                        .header(CONTENT_TYPE, APPLICATION_JSON.essence_str())
                        .status(StatusCode::OK)
                        .body(
                            serde_json::to_string(&Response {
                                data: Some(Value::String(ByteString::from("test"))),
                                ..Response::default()
                            })
                            .expect("always valid")
                            .into(),
                        )
                        .unwrap())
                }
                Err(_) => {
                    panic!("invalid graphql request recieved")
                }
            }
        }

        serve(listener, handle).await.unwrap();
    }

    // starts a local server emulating a subgraph returning response with
    // "errors" : {["message": "PersistedQueryNotFound",...],...}
    async fn emulate_persisted_query_not_found_message(listener: TcpListener) {
        async fn handle(request: http::Request<Body>) -> Result<http::Response<Body>, Infallible> {
            let (_, body) = request.into_parts();
            let graphql_request: Result<graphql::Request, &str> = router::body::into_bytes(body)
                .await
                .map_err(|_| ())
                .and_then(|bytes| serde_json::from_reader(bytes.reader()).map_err(|_| ()))
                .map_err(|_| "failed to parse the request body as JSON");

            match graphql_request {
                Ok(request) => {
                    if !request.extensions.contains_key(PERSISTED_QUERY_KEY) {
                        panic!(
                            "Recieved request without persisted query in persisted_query_not_found test."
                        )
                    }

                    if request.query.is_none() {
                        Ok(http::Response::builder()
                            .header(CONTENT_TYPE, APPLICATION_JSON.essence_str())
                            .status(StatusCode::OK)
                            .body(
                                serde_json::to_string(&Response {
                                    data: Some(Value::String(ByteString::from("test"))),
                                    errors: vec![
                                        Error::builder()
                                            .message(PERSISTED_QUERY_NOT_FOUND_MESSAGE)
                                            .extension_code("Random Code")
                                            .build(),
                                    ],
                                    ..Response::default()
                                })
                                .expect("always valid")
                                .into(),
                            )
                            .unwrap())
                    } else {
                        Ok(http::Response::builder()
                            .header(CONTENT_TYPE, APPLICATION_JSON.essence_str())
                            .status(StatusCode::OK)
                            .body(
                                serde_json::to_string(&Response {
                                    data: Some(Value::String(ByteString::from("test"))),
                                    ..Response::default()
                                })
                                .expect("always valid")
                                .into(),
                            )
                            .unwrap())
                    }
                }
                Err(_) => {
                    panic!("invalid graphql request recieved")
                }
            }
        }

        serve(listener, handle).await.unwrap();
    }

    // starts a local server emulating a subgraph returning response with
    // "errors" : {[..., "extensions": {"code": "PERSISTED_QUERY_NOT_FOUND"}],...}
    async fn emulate_persisted_query_not_found_extension_code(listener: TcpListener) {
        async fn handle(request: http::Request<Body>) -> Result<http::Response<Body>, Infallible> {
            let (_, body) = request.into_parts();
            let graphql_request: Result<graphql::Request, &str> = router::body::into_bytes(body)
                .await
                .map_err(|_| ())
                .and_then(|bytes| serde_json::from_reader(bytes.reader()).map_err(|_| ()))
                .map_err(|_| "failed to parse the request body as JSON");

            match graphql_request {
                Ok(request) => {
                    if !request.extensions.contains_key(PERSISTED_QUERY_KEY) {
                        panic!(
                            "Recieved request without persisted query in persisted_query_not_found test."
                        )
                    }

                    if request.query.is_none() {
                        Ok(http::Response::builder()
                            .header(CONTENT_TYPE, APPLICATION_JSON.essence_str())
                            .status(StatusCode::OK)
                            .body(
                                serde_json::to_string(&Response {
                                    data: Some(Value::String(ByteString::from("test"))),
                                    errors: vec![
                                        Error::builder()
                                            .message("Random message")
                                            .extension_code(
                                                PERSISTED_QUERY_NOT_FOUND_EXTENSION_CODE,
                                            )
                                            .build(),
                                    ],
                                    ..Response::default()
                                })
                                .expect("always valid")
                                .into(),
                            )
                            .unwrap())
                    } else {
                        Ok(http::Response::builder()
                            .header(CONTENT_TYPE, APPLICATION_JSON.essence_str())
                            .status(StatusCode::OK)
                            .body(
                                serde_json::to_string(&Response {
                                    data: Some(Value::String(ByteString::from("test"))),
                                    ..Response::default()
                                })
                                .expect("always valid")
                                .into(),
                            )
                            .unwrap())
                    }
                }
                Err(_) => {
                    panic!("invalid graphql request recieved")
                }
            }
        }

        serve(listener, handle).await.unwrap();
    }

    // starts a local server emulating a subgraph returning a response to request with apq
    // and panics if it does not find a persistedQuery.
    async fn emulate_expected_apq_enabled_configuration(listener: TcpListener) {
        async fn handle(request: http::Request<Body>) -> Result<http::Response<Body>, Infallible> {
            let (_, body) = request.into_parts();
            let graphql_request: Result<graphql::Request, &str> = router::body::into_bytes(body)
                .await
                .map_err(|_| ())
                .and_then(|bytes| serde_json::from_reader(bytes.reader()).map_err(|_| ()))
                .map_err(|_| "failed to parse the request body as JSON");

            match graphql_request {
                Ok(request) => {
                    if !request.extensions.contains_key(PERSISTED_QUERY_KEY) {
                        panic!("persistedQuery expected when configuration has apq_enabled=true")
                    }

                    Ok(http::Response::builder()
                        .header(CONTENT_TYPE, APPLICATION_JSON.essence_str())
                        .status(StatusCode::OK)
                        .body(
                            serde_json::to_string(&Response {
                                data: Some(Value::String(ByteString::from("test"))),
                                ..Response::default()
                            })
                            .expect("always valid")
                            .into(),
                        )
                        .unwrap())
                }
                Err(_) => {
                    panic!("invalid graphql request recieved")
                }
            }
        }

        serve(listener, handle).await.unwrap();
    }

    // starts a local server emulating a subgraph returning a response to request without apq
    // and panics if it finds a persistedQuery.
    async fn emulate_expected_apq_disabled_configuration(listener: TcpListener) {
        async fn handle(request: http::Request<Body>) -> Result<http::Response<Body>, Infallible> {
            let (_, body) = request.into_parts();
            let graphql_request: Result<graphql::Request, &str> = router::body::into_bytes(body)
                .await
                .map_err(|_| ())
                .and_then(|bytes| serde_json::from_reader(bytes.reader()).map_err(|_| ()))
                .map_err(|_| "failed to parse the request body as JSON");

            match graphql_request {
                Ok(request) => {
                    if request.extensions.contains_key(PERSISTED_QUERY_KEY) {
                        panic!(
                            "persistedQuery not expected when configuration has apq_enabled=false"
                        )
                    }

                    Ok(http::Response::builder()
                        .header(CONTENT_TYPE, APPLICATION_JSON.essence_str())
                        .status(StatusCode::OK)
                        .body(
                            serde_json::to_string(&Response {
                                data: Some(Value::String(ByteString::from("test"))),
                                ..Response::default()
                            })
                            .expect("always valid")
                            .into(),
                        )
                        .unwrap())
                }
                Err(_) => {
                    panic!("invalid graphql request recieved")
                }
            }
        }

        serve(listener, handle).await.unwrap();
    }

    async fn emulate_correct_websocket_server(listener: TcpListener) {
        async fn ws_handler(
            ws: WebSocketUpgrade,
            ConnectInfo(_addr): ConnectInfo<SocketAddr>,
        ) -> Result<impl IntoResponse, Infallible> {
            // finalize the upgrade process by returning upgrade callback.
            // we can customize the callback by sending additional info such as address.
            let res = ws.protocols(["graphql-transport-ws"]).on_upgrade(move |mut socket| async move {
                let connection_ack = socket.recv().await.unwrap().unwrap().into_text().unwrap();
                let ack_msg: ClientMessage = serde_json::from_str(&connection_ack).unwrap();
                assert!(matches!(ack_msg, ClientMessage::ConnectionInit { .. }));

                socket
                    .send(Message::text(
                        serde_json::to_string(&ServerMessage::ConnectionAck).unwrap(),
                    ))
                    .await
                    .unwrap();
                let new_message = socket.recv().await.unwrap().unwrap().into_text().unwrap();
                let subscribe_msg: ClientMessage = serde_json::from_str(&new_message).unwrap();
                assert!(matches!(subscribe_msg, ClientMessage::Subscribe { .. }));
                let client_id = if let ClientMessage::Subscribe { payload, id } = subscribe_msg {
                    assert_eq!(
                        payload,
                        Request::builder()
                            .query("subscription {\n  userWasCreated {\n    username\n  }\n}")
                            .build()
                    );

                    id
                } else {
                    panic!("subscribe message should be sent");
                };

                socket
                    .send(Message::text(
                        serde_json::to_string(&ServerMessage::Next { id: client_id, payload: graphql::Response::builder().data(serde_json_bytes::json!({"userWasCreated": {"username": "ada_lovelace"}})).build() }).unwrap(),
                    ))
                    .await
                    .unwrap();
            });

            Ok(res)
        }

        let app = Router::new().route("/ws", get(ws_handler));
        let server = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        );
        server.await.unwrap();
    }

    async fn emulate_incorrect_websocket_server(listener: TcpListener) {
        async fn ws_handler(
            _ws: WebSocketUpgrade,
            ConnectInfo(_addr): ConnectInfo<SocketAddr>,
        ) -> Result<impl IntoResponse, Infallible> {
            Ok((http::StatusCode::BAD_REQUEST, "bad request"))
        }

        let app = Router::new().route("/ws", get(ws_handler));
        let server = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        );
        server.await.unwrap();
    }

    async fn emulate_subgraph_with_callback_data(listener: TcpListener) {
        async fn handle(request: http::Request<Body>) -> Result<http::Response<Body>, Infallible> {
            let (parts, body) = request.into_parts();
            assert!(
                parts
                    .headers
                    .get_all(ACCEPT)
                    .iter()
                    .any(|header_value| header_value == CALLBACK_PROTOCOL_ACCEPT)
            );
            let graphql_request: Result<graphql::Request, &str> = router::body::into_bytes(body)
                .await
                .map_err(|_| ())
                .and_then(|bytes| serde_json::from_reader(bytes.reader()).map_err(|_| ()))
                .map_err(|_| "failed to parse the request body as JSON");
            let graphql_request = graphql_request.unwrap();
            assert!(graphql_request.extensions.contains_key("subscription"));
            let subscription_extension: SubscriptionExtension = serde_json_bytes::from_value(
                graphql_request
                    .extensions
                    .get("subscription")
                    .unwrap()
                    .clone(),
            )
            .unwrap();
            assert_eq!(
                subscription_extension.callback_url.to_string(),
                format!(
                    "http://localhost:4000/testcallback/{}",
                    subscription_extension.subscription_id
                )
            );
            assert_eq!(subscription_extension.heartbeat_interval_ms, 0);

            Ok(http::Response::builder()
                .header(CONTENT_TYPE, APPLICATION_JSON.essence_str())
                .status(StatusCode::OK)
                .body(
                    serde_json::to_string(&Response::builder().data(Value::Null).build())
                        .expect("always valid")
                        .into(),
                )
                .unwrap())
        }

        serve(listener, handle).await.unwrap();
    }

    fn subscription_config() -> SubscriptionConfig {
        SubscriptionConfig {
            enabled: true,
            mode: SubscriptionModeConfig {
                callback: Some(CallbackMode {
                    public_url: Url::parse("http://localhost:4000/testcallback").unwrap(),
                    listen: None,
                    path: Some("/testcallback".to_string()),
                    subgraphs: vec![String::from("testbis")].into_iter().collect(),
                    heartbeat_interval: HeartbeatInterval::new_disabled(),
                }),
                passthrough: Some(SubgraphPassthroughMode {
                    all: None,
                    subgraphs: [(
                        "test".to_string(),
                        WebSocketConfiguration {
                            path: Some(String::from("/ws")),
                            protocol: WebSocketProtocol::default(),
                            heartbeat_interval: HeartbeatInterval::new_disabled(),
                        },
                    )]
                    .into(),
                }),
            },
            deduplication: DeduplicationConfig::default(),
            max_opened_subscriptions: None,
            queue_capacity: None,
        }
    }

    fn supergraph_request(query: &str) -> Arc<http::Request<Request>> {
        Arc::new(
            http::Request::builder()
                .header(HOST, "host")
                .header(CONTENT_TYPE, APPLICATION_JSON.essence_str())
                .body(Request::builder().query(query).build())
                .expect("expecting valid request"),
        )
    }

    fn subgraph_http_request(uri: Uri, query: &str) -> http::Request<Request> {
        http::Request::builder()
            .header(HOST, "rhost")
            .header(CONTENT_TYPE, APPLICATION_JSON.essence_str())
            .uri(uri)
            .body(Request::builder().query(query).build())
            .expect("expecting valid request")
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_subgraph_service_callback() {
        let _ = SUBSCRIPTION_CALLBACK_HMAC_KEY.set(String::from("TESTEST"));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let socket_addr = listener.local_addr().unwrap();
        let spawned_task = tokio::task::spawn(emulate_subgraph_with_callback_data(listener));
        let subgraph_service = SubgraphService::new(
            "testbis",
            true,
            subscription_config().into(),
            Notify::builder().build(),
            HttpClientServiceFactory::from_config(
                "testbis",
                &Configuration::default(),
                crate::configuration::shared::Client::default(),
            ),
        )
        .expect("can create a SubgraphService");
        let (tx, _rx) = mpsc::channel(2);
        let url = Uri::from_str(&format!("http://{socket_addr}")).unwrap();
        let response = subgraph_service
            .oneshot(
                SubgraphRequest::builder()
                    .supergraph_request(supergraph_request(
                        "subscription {\n  userWasCreated {\n    username\n  }\n}",
                    ))
                    .subgraph_request(subgraph_http_request(
                        url,
                        "subscription {\n  userWasCreated {\n    username\n  }\n}",
                    ))
                    .operation_kind(OperationKind::Subscription)
                    .subscription_stream(tx)
                    .subgraph_name(String::from("test"))
                    .context(Context::new())
                    .build(),
            )
            .await
            .unwrap();
        response.response.body().errors.iter().for_each(|e| {
            println!("error: {}", e.message);
        });
        assert!(response.response.body().errors.is_empty());
        spawned_task.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_subgraph_service_content_type_application_graphql() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let socket_addr = listener.local_addr().unwrap();
        tokio::task::spawn(emulate_subgraph_application_graphql_response(listener));
        let subgraph_service = SubgraphService::new(
            "test",
            true,
            None,
            Notify::default(),
            HttpClientServiceFactory::from_config(
                "test",
                &Configuration::default(),
                crate::configuration::shared::Client::default(),
            ),
        )
        .expect("can create a SubgraphService");

        let url = Uri::from_str(&format!("http://{socket_addr}")).unwrap();
        let response = subgraph_service
            .oneshot(
                SubgraphRequest::builder()
                    .supergraph_request(supergraph_request("query"))
                    .subgraph_request(subgraph_http_request(url, "query"))
                    .operation_kind(OperationKind::Query)
                    .subgraph_name(String::from("test"))
                    .context(Context::new())
                    .build(),
            )
            .await
            .unwrap();
        assert!(response.response.body().errors.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_subgraph_service_content_type_application_json() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let socket_addr = listener.local_addr().unwrap();
        tokio::task::spawn(emulate_subgraph_application_json_response(listener));
        let subgraph_service = SubgraphService::new(
            "test",
            true,
            None,
            Notify::default(),
            HttpClientServiceFactory::from_config(
                "test",
                &Configuration::default(),
                crate::configuration::shared::Client::default(),
            ),
        )
        .expect("can create a SubgraphService");

        let url = Uri::from_str(&format!("http://{socket_addr}")).unwrap();
        let response = subgraph_service
            .oneshot(
                SubgraphRequest::builder()
                    .supergraph_request(supergraph_request("query"))
                    .subgraph_request(subgraph_http_request(url, "query"))
                    .operation_kind(OperationKind::Query)
                    .subgraph_name(String::from("test"))
                    .context(Context::new())
                    .build(),
            )
            .await
            .unwrap();
        assert!(response.response.body().errors.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    #[cfg(not(target_os = "macos"))]
    async fn test_subgraph_service_panic() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let socket_addr = listener.local_addr().unwrap();
        tokio::task::spawn(emulate_subgraph_panic(listener));
        let subgraph_service = SubgraphService::new(
            "test",
            true,
            None,
            Notify::default(),
            HttpClientServiceFactory::from_config(
                "test",
                &Configuration::default(),
                crate::configuration::shared::Client::default(),
            ),
        )
        .expect("can create a SubgraphService");

        let url = Uri::from_str(&format!("http://{socket_addr}")).unwrap();
        let response = subgraph_service
            .oneshot(
                SubgraphRequest::builder()
                    .supergraph_request(supergraph_request("query"))
                    .subgraph_request(subgraph_http_request(url, "query"))
                    .operation_kind(OperationKind::Query)
                    .subgraph_name(String::from("test"))
                    .context(Context::new())
                    .build(),
            )
            .await
            .unwrap();
        assert!(!response.response.body().errors.is_empty());
        assert_eq!(
            response.response.body().errors[0].message,
            "HTTP fetch failed from 'test': HTTP fetch failed from 'test': connection closed before message completed"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_subgraph_service_invalid_response() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let socket_addr = listener.local_addr().unwrap();
        tokio::task::spawn(emulate_subgraph_ok_status_invalid_response(listener));
        let subgraph_service = SubgraphService::new(
            "test",
            true,
            None,
            Notify::default(),
            HttpClientServiceFactory::from_config(
                "test",
                &Configuration::default(),
                crate::configuration::shared::Client::default(),
            ),
        )
        .expect("can create a SubgraphService");

        let url = Uri::from_str(&format!("http://{socket_addr}")).unwrap();
        let response = subgraph_service
            .oneshot(
                SubgraphRequest::builder()
                    .supergraph_request(supergraph_request("query"))
                    .subgraph_request(subgraph_http_request(url, "query"))
                    .operation_kind(OperationKind::Query)
                    .subgraph_name(String::from("test"))
                    .context(Context::new())
                    .build(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.response.body().errors[0].message,
            "service 'test' response was malformed: expected value at line 1 column 1"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_subgraph_invalid_status_invalid_response_application_json() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let socket_addr = listener.local_addr().unwrap();
        tokio::task::spawn(
            emulate_subgraph_invalid_response_invalid_status_application_json(listener),
        );
        let subgraph_service = SubgraphService::new(
            "test",
            true,
            None,
            Notify::default(),
            HttpClientServiceFactory::from_config(
                "test",
                &Configuration::default(),
                crate::configuration::shared::Client::default(),
            ),
        )
        .expect("can create a SubgraphService");

        let url = Uri::from_str(&format!("http://{socket_addr}")).unwrap();
        let response = subgraph_service
            .oneshot(
                SubgraphRequest::builder()
                    .supergraph_request(supergraph_request("query"))
                    .subgraph_request(subgraph_http_request(url, "query"))
                    .operation_kind(OperationKind::Query)
                    .subgraph_name(String::from("test"))
                    .context(Context::new())
                    .build(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.response.body().errors[0].message,
            "HTTP fetch failed from 'test': 401: Unauthorized"
        );
        assert_eq!(
            response.response.body().errors[1].message,
            "service 'test' response was malformed: invalid"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_subgraph_invalid_status_invalid_response_application_graphql() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let socket_addr = listener.local_addr().unwrap();
        tokio::task::spawn(
            emulate_subgraph_invalid_response_invalid_status_application_graphql(listener),
        );
        let subgraph_service = SubgraphService::new(
            "test",
            true,
            None,
            Notify::default(),
            HttpClientServiceFactory::from_config(
                "test",
                &Configuration::default(),
                crate::configuration::shared::Client::default(),
            ),
        )
        .expect("can create a SubgraphService");

        let url = Uri::from_str(&format!("http://{socket_addr}")).unwrap();
        let response = subgraph_service
            .oneshot(
                SubgraphRequest::builder()
                    .supergraph_request(supergraph_request("query"))
                    .subgraph_request(subgraph_http_request(url, "query"))
                    .operation_kind(OperationKind::Query)
                    .subgraph_name(String::from("test"))
                    .context(Context::new())
                    .build(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.response.body().errors[0].message,
            "HTTP fetch failed from 'test': 401: Unauthorized"
        );
        assert_eq!(
            response.response.body().errors[1].message,
            "service 'test' response was malformed: expected value at line 1 column 1"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_subgraph_service_websocket() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let socket_addr = listener.local_addr().unwrap();
        let spawned_task = tokio::task::spawn(emulate_correct_websocket_server(listener));
        let subgraph_service = SubgraphService::new(
            "test",
            true,
            subscription_config().into(),
            Notify::builder().build(),
            HttpClientServiceFactory::from_config(
                "test",
                &Configuration::default(),
                crate::configuration::shared::Client::default(),
            ),
        )
        .expect("can create a SubgraphService");
        let (tx, rx) = mpsc::channel(2);
        let mut rx_stream = ReceiverStream::new(rx);

        let url = Uri::from_str(&format!("ws://{socket_addr}")).unwrap();
        let response = subgraph_service
            .oneshot(
                SubgraphRequest::builder()
                    .supergraph_request(supergraph_request(
                        "subscription {\n  userWasCreated {\n    username\n  }\n}",
                    ))
                    .subgraph_request(subgraph_http_request(
                        url,
                        "subscription {\n  userWasCreated {\n    username\n  }\n}",
                    ))
                    .operation_kind(OperationKind::Subscription)
                    .subscription_stream(tx)
                    .subgraph_name(String::from("test"))
                    .context(Context::new())
                    .build(),
            )
            .await
            .unwrap();
        assert!(response.response.body().errors.is_empty());

        let mut gql_stream = rx_stream.next().await.unwrap();
        let message = gql_stream.next().await.unwrap();
        assert_eq!(
            message,
            graphql::Response::builder()
                .subscribed(true)
                .data(serde_json_bytes::json!({"userWasCreated": {"username": "ada_lovelace"}}))
                .build()
        );
        spawned_task.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_subgraph_service_websocket_with_error() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let socket_addr = listener.local_addr().unwrap();
        tokio::task::spawn(emulate_incorrect_websocket_server(listener));
        let subgraph_service = SubgraphService::new(
            "test",
            true,
            subscription_config().into(),
            Notify::builder().build(),
            HttpClientServiceFactory::from_config(
                "test",
                &Configuration::default(),
                crate::configuration::shared::Client::default(),
            ),
        )
        .expect("can create a SubgraphService");
        let (tx, _rx) = mpsc::channel(2);

        let url = Uri::from_str(&format!("ws://{socket_addr}")).unwrap();
        let err = subgraph_service
            .oneshot(
                SubgraphRequest::builder()
                    .supergraph_request(supergraph_request(
                        "subscription {\n  userWasCreated {\n    username\n  }\n}",
                    ))
                    .subgraph_request(subgraph_http_request(
                        url,
                        "subscription {\n  userWasCreated {\n    username\n  }\n}",
                    ))
                    .operation_kind(OperationKind::Subscription)
                    .subscription_stream(tx)
                    .subgraph_name(String::from("test"))
                    .context(Context::new())
                    .build(),
            )
            .await
            .unwrap_err();

        let err_str = err.to_string();
        assert!(err_str.starts_with("Websocket fetch failed from 'test': cannot connect websocket to subgraph: WebSocket upgrade failed. Status: 400 Bad Request; Headers: [\"content-type\": \"text/plain; charset=utf-8\"; \"content-length\": \"11\";"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_bad_status_code_should_not_fail() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let socket_addr = listener.local_addr().unwrap();
        tokio::task::spawn(emulate_subgraph_bad_request(listener));
        let subgraph_service = SubgraphService::new(
            "test",
            true,
            None,
            Notify::default(),
            HttpClientServiceFactory::from_config(
                "test",
                &Configuration::default(),
                crate::configuration::shared::Client::default(),
            ),
        )
        .expect("can create a SubgraphService");

        let url = Uri::from_str(&format!("http://{socket_addr}")).unwrap();
        let response = subgraph_service
            .oneshot(
                SubgraphRequest::builder()
                    .supergraph_request(supergraph_request("query"))
                    .subgraph_request(subgraph_http_request(url, "query"))
                    .operation_kind(OperationKind::Query)
                    .subgraph_name(String::from("test"))
                    .context(Context::new())
                    .build(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.response.body().errors[0].message,
            "HTTP fetch failed from 'test': 400: Bad Request"
        );
        assert_eq!(
            response.response.body().errors[1].message,
            "This went wrong"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_missing_content_type() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let socket_addr = listener.local_addr().unwrap();
        tokio::task::spawn(emulate_subgraph_missing_content_type(listener));

        let subgraph_service = SubgraphService::new(
            "test",
            true,
            None,
            Notify::default(),
            HttpClientServiceFactory::from_config(
                "test",
                &Configuration::default(),
                crate::configuration::shared::Client::default(),
            ),
        )
        .expect("can create a SubgraphService");

        let url = Uri::from_str(&format!("http://{socket_addr}")).unwrap();
        let response = subgraph_service
            .oneshot(
                SubgraphRequest::builder()
                    .supergraph_request(supergraph_request("query"))
                    .subgraph_request(subgraph_http_request(url, "query"))
                    .operation_kind(OperationKind::Query)
                    .subgraph_name(String::from("test"))
                    .context(Context::new())
                    .build(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.response.body().errors[0].message,
            "HTTP fetch failed from 'test': subgraph response does not contain 'content-type' header; expected content-type: application/json or content-type: application/graphql-response+json"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_invalid_content_type() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let socket_addr = listener.local_addr().unwrap();
        tokio::task::spawn(emulate_subgraph_invalid_content_type(listener));

        let subgraph_service = SubgraphService::new(
            "test",
            true,
            None,
            Notify::default(),
            HttpClientServiceFactory::from_config(
                "test",
                &Configuration::default(),
                crate::configuration::shared::Client::default(),
            ),
        )
        .expect("can create a SubgraphService");

        let url = Uri::from_str(&format!("http://{socket_addr}")).unwrap();
        let response = subgraph_service
            .oneshot(
                SubgraphRequest::builder()
                    .supergraph_request(supergraph_request("query"))
                    .subgraph_request(subgraph_http_request(url, "query"))
                    .operation_kind(OperationKind::Query)
                    .subgraph_name(String::from("test"))
                    .context(Context::new())
                    .build(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.response.body().errors[0].message,
            "HTTP fetch failed from 'test': subgraph response contains invalid 'content-type' header value \"application/json,application/json\"; expected content-type: application/json or content-type: application/graphql-response+json"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_unsupported_content_type() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let socket_addr = listener.local_addr().unwrap();
        tokio::task::spawn(emulate_subgraph_unsupported_content_type(listener));

        let subgraph_service = SubgraphService::new(
            "test",
            true,
            None,
            Notify::default(),
            HttpClientServiceFactory::from_config(
                "test",
                &Configuration::default(),
                crate::configuration::shared::Client::default(),
            ),
        )
        .expect("can create a SubgraphService");

        let url = Uri::from_str(&format!("http://{socket_addr}")).unwrap();
        let response = subgraph_service
            .oneshot(
                SubgraphRequest::builder()
                    .supergraph_request(supergraph_request("query"))
                    .subgraph_request(subgraph_http_request(url, "query"))
                    .operation_kind(OperationKind::Query)
                    .subgraph_name(String::from("test"))
                    .context(Context::new())
                    .build(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.response.body().errors[0].message,
            "HTTP fetch failed from 'test': subgraph response contains unsupported content-type: text/html; expected content-type: application/json or content-type: application/graphql-response+json"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_unauthorized() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let socket_addr = listener.local_addr().unwrap();
        tokio::task::spawn(emulate_subgraph_unauthorized(listener));
        let subgraph_service = SubgraphService::new(
            "test",
            true,
            None,
            Notify::default(),
            HttpClientServiceFactory::from_config(
                "test",
                &Configuration::default(),
                crate::configuration::shared::Client::default(),
            ),
        )
        .expect("can create a SubgraphService");

        let url = Uri::from_str(&format!("http://{socket_addr}")).unwrap();
        let response = subgraph_service
            .oneshot(
                SubgraphRequest::builder()
                    .supergraph_request(supergraph_request("query"))
                    .subgraph_request(subgraph_http_request(url, "query"))
                    .operation_kind(OperationKind::Query)
                    .subgraph_name(String::from("test"))
                    .context(Context::new())
                    .build(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.response.body().errors[0].message,
            "HTTP fetch failed from 'test': 401: Unauthorized"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_persisted_query_not_supported_message() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let socket_addr = listener.local_addr().unwrap();
        tokio::task::spawn(emulate_persisted_query_not_supported_message(listener));
        let subgraph_service = SubgraphService::new(
            "test",
            true,
            None,
            Notify::default(),
            HttpClientServiceFactory::from_config(
                "test",
                &Configuration::default(),
                crate::configuration::shared::Client::default(),
            ),
        )
        .expect("can create a SubgraphService");

        assert!(subgraph_service.clone().apq.as_ref().load(Relaxed));

        let url = Uri::from_str(&format!("http://{socket_addr}")).unwrap();
        let resp = subgraph_service
            .clone()
            .oneshot(
                SubgraphRequest::builder()
                    .supergraph_request(supergraph_request("query"))
                    .subgraph_request(subgraph_http_request(url, "query"))
                    .operation_kind(OperationKind::Query)
                    .subgraph_name(String::from("test"))
                    .context(Context::new())
                    .build(),
            )
            .await
            .unwrap();

        let expected_resp = Response {
            data: Some(Value::String(ByteString::from("test"))),
            ..Response::default()
        };

        assert_eq!(resp.response.body(), &expected_resp);
        assert!(!subgraph_service.apq.as_ref().load(Relaxed));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_persisted_query_not_supported_extension_code() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let socket_addr = listener.local_addr().unwrap();
        tokio::task::spawn(emulate_persisted_query_not_supported_extension_code(
            listener,
        ));
        let subgraph_service = SubgraphService::new(
            "test",
            true,
            None,
            Notify::default(),
            HttpClientServiceFactory::from_config(
                "test",
                &Configuration::default(),
                crate::configuration::shared::Client::default(),
            ),
        )
        .expect("can create a SubgraphService");

        assert!(subgraph_service.clone().apq.as_ref().load(Relaxed));

        let url = Uri::from_str(&format!("http://{socket_addr}")).unwrap();
        let resp = subgraph_service
            .clone()
            .oneshot(
                SubgraphRequest::builder()
                    .supergraph_request(supergraph_request("query"))
                    .subgraph_request(subgraph_http_request(url, "query"))
                    .operation_kind(OperationKind::Query)
                    .subgraph_name(String::from("test"))
                    .context(Context::new())
                    .build(),
            )
            .await
            .unwrap();

        let expected_resp = Response {
            data: Some(Value::String(ByteString::from("test"))),
            ..Response::default()
        };

        assert_eq!(resp.response.body(), &expected_resp);
        assert!(!subgraph_service.apq.as_ref().load(Relaxed));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_persisted_query_not_found_message() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let socket_addr = listener.local_addr().unwrap();
        tokio::task::spawn(emulate_persisted_query_not_found_message(listener));
        let subgraph_service = SubgraphService::new(
            "test",
            true,
            None,
            Notify::default(),
            HttpClientServiceFactory::from_config(
                "test",
                &Configuration::default(),
                crate::configuration::shared::Client::default(),
            ),
        )
        .expect("can create a SubgraphService");

        let url = Uri::from_str(&format!("http://{socket_addr}")).unwrap();
        let resp = subgraph_service
            .clone()
            .oneshot(
                SubgraphRequest::builder()
                    .supergraph_request(supergraph_request("query"))
                    .subgraph_request(subgraph_http_request(url, "query"))
                    .operation_kind(OperationKind::Query)
                    .subgraph_name(String::from("test"))
                    .context(Context::new())
                    .build(),
            )
            .await
            .unwrap();

        let expected_resp = Response {
            data: Some(Value::String(ByteString::from("test"))),
            ..Response::default()
        };

        assert_eq!(resp.response.body(), &expected_resp);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_persisted_query_not_found_extension_code() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let socket_addr = listener.local_addr().unwrap();
        tokio::task::spawn(emulate_persisted_query_not_found_extension_code(listener));
        let subgraph_service = SubgraphService::new(
            "test",
            true,
            None,
            Notify::default(),
            HttpClientServiceFactory::from_config(
                "test",
                &Configuration::default(),
                crate::configuration::shared::Client::default(),
            ),
        )
        .expect("can create a SubgraphService");

        let url = Uri::from_str(&format!("http://{socket_addr}")).unwrap();
        let resp = subgraph_service
            .clone()
            .oneshot(
                SubgraphRequest::builder()
                    .supergraph_request(supergraph_request("query"))
                    .subgraph_request(subgraph_http_request(url, "query"))
                    .operation_kind(OperationKind::Query)
                    .subgraph_name(String::from("test"))
                    .context(Context::new())
                    .build(),
            )
            .await
            .unwrap();

        let expected_resp = Response {
            data: Some(Value::String(ByteString::from("test"))),
            ..Response::default()
        };

        assert_eq!(resp.response.body(), &expected_resp);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_apq_enabled_subgraph_configuration() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let socket_addr = listener.local_addr().unwrap();
        tokio::task::spawn(emulate_expected_apq_enabled_configuration(listener));
        let subgraph_service = SubgraphService::new(
            "test",
            true,
            None,
            Notify::default(),
            HttpClientServiceFactory::from_config(
                "test",
                &Configuration::default(),
                crate::configuration::shared::Client::default(),
            ),
        )
        .expect("can create a SubgraphService");

        let url = Uri::from_str(&format!("http://{socket_addr}")).unwrap();
        let resp = subgraph_service
            .clone()
            .oneshot(
                SubgraphRequest::builder()
                    .supergraph_request(supergraph_request("query"))
                    .subgraph_request(subgraph_http_request(url, "query"))
                    .operation_kind(OperationKind::Query)
                    .subgraph_name(String::from("test"))
                    .context(Context::new())
                    .build(),
            )
            .await
            .unwrap();

        let expected_resp = Response {
            data: Some(Value::String(ByteString::from("test"))),
            ..Response::default()
        };

        assert_eq!(resp.response.body(), &expected_resp);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_apq_disabled_subgraph_configuration() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let socket_addr = listener.local_addr().unwrap();
        tokio::task::spawn(emulate_expected_apq_disabled_configuration(listener));
        let subgraph_service = SubgraphService::new(
            "test",
            false,
            None,
            Notify::default(),
            HttpClientServiceFactory::from_config(
                "test",
                &Configuration::default(),
                crate::configuration::shared::Client::default(),
            ),
        )
        .expect("can create a SubgraphService");

        let url = Uri::from_str(&format!("http://{socket_addr}")).unwrap();
        let resp = subgraph_service
            .clone()
            .oneshot(
                SubgraphRequest::builder()
                    .supergraph_request(supergraph_request("query"))
                    .subgraph_request(subgraph_http_request(url, "query"))
                    .operation_kind(OperationKind::Query)
                    .subgraph_name(String::from("test"))
                    .context(Context::new())
                    .build(),
            )
            .await
            .unwrap();

        let expected_resp = Response {
            data: Some(Value::String(ByteString::from("test"))),
            ..Response::default()
        };

        assert_eq!(resp.response.body(), &expected_resp);
    }

    #[test]
    fn it_gets_uri_details() {
        let path = "https://example.com/path".parse().unwrap();
        let (host, port, path) = super::get_uri_details(&path);

        assert_eq!(host, "example.com");
        assert_eq!(port, 443);
        assert_eq!(path, "/path");
    }

    #[test]
    fn it_converts_ok_http_to_graphql() {
        let (parts, body) = http::Response::builder()
            .status(StatusCode::OK)
            .body(None)
            .unwrap()
            .into_parts();
        let actual = super::http_response_to_graphql_response(
            "test_service",
            Ok(ContentType::ApplicationGraphqlResponseJson),
            body,
            &parts,
        );

        let expected = graphql::Response::builder().build();
        assert_eq!(actual, expected);
    }

    #[test]
    fn it_converts_error_http_to_graphql() {
        let (parts, body) = http::Response::builder()
            .status(StatusCode::IM_A_TEAPOT)
            .body(None)
            .unwrap()
            .into_parts();
        let actual = super::http_response_to_graphql_response(
            "test_service",
            Ok(ContentType::ApplicationGraphqlResponseJson),
            body,
            &parts,
        );

        let expected = graphql::Response::builder()
            .error(
                super::FetchError::SubrequestHttpError {
                    status_code: Some(418),
                    service: "test_service".into(),
                    reason: "418: I'm a teapot".into(),
                }
                .to_graphql_error(None),
            )
            .build();
        assert_response_eq_ignoring_error_id!(actual, expected);
    }

    #[test]
    fn it_converts_http_with_body_to_graphql() {
        let mut json = serde_json::json!({
            "data": {
                "some_field": "some_value"
            }
        });

        let (parts, body) = http::Response::builder()
            .status(StatusCode::OK)
            .body(Some(Ok(Bytes::from(json.to_string()))))
            .unwrap()
            .into_parts();

        let actual = super::http_response_to_graphql_response(
            "test_service",
            Ok(ContentType::ApplicationGraphqlResponseJson),
            body,
            &parts,
        );

        let expected = graphql::Response::builder()
            .data(json["data"].take())
            .build();
        assert_eq!(actual, expected);
    }

    #[test]
    fn it_converts_http_with_graphql_errors_to_graphql() {
        let error = graphql::Error::builder()
            .message("error was encountered for test")
            .extension_code("SOME_EXTENSION")
            .build();
        let mut json = serde_json::json!({
            "data": {
                "some_field": "some_value",
                "error_field": null,
            },
            "errors": [error],
        });

        let (parts, body) = http::Response::builder()
            .status(StatusCode::OK)
            .body(Some(Ok(Bytes::from(json.to_string()))))
            .unwrap()
            .into_parts();

        let actual = super::http_response_to_graphql_response(
            "test_service",
            Ok(ContentType::ApplicationGraphqlResponseJson),
            body,
            &parts,
        );

        let expected = graphql::Response::builder()
            .data(json["data"].take())
            .error(error)
            .build();
        assert_response_eq_ignoring_error_id!(actual, expected);
    }

    #[test]
    fn it_converts_error_http_with_graphql_errors_to_graphql() {
        let error = graphql::Error::builder()
            .message("error was encountered for test")
            .extension_code("SOME_EXTENSION")
            .build();
        let mut json = serde_json::json!({
            "data": {
                "some_field": "some_value",
                "error_field": null,
            },
            "errors": [error],
        });

        let (parts, body) = http::Response::builder()
            .status(StatusCode::IM_A_TEAPOT)
            .body(Some(Ok(Bytes::from(json.to_string()))))
            .unwrap()
            .into_parts();

        let actual = super::http_response_to_graphql_response(
            "test_service",
            Ok(ContentType::ApplicationGraphqlResponseJson),
            body,
            &parts,
        );

        let expected = graphql::Response::builder()
            .data(json["data"].take())
            .error(
                super::FetchError::SubrequestHttpError {
                    status_code: Some(418),
                    service: "test_service".into(),
                    reason: "418: I'm a teapot".into(),
                }
                .to_graphql_error(None),
            )
            .error(error)
            .build();
        assert_response_eq_ignoring_error_id!(expected, actual);
    }
}
