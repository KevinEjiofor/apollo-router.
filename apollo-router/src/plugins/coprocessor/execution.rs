use std::ops::ControlFlow;
use std::sync::Arc;

use futures::future;
use futures::stream;
use schemars::JsonSchema;
use serde::Deserialize;
use tower::BoxError;
use tower::ServiceBuilder;
use tower_service::Service;

use super::*;
use crate::graphql;
use crate::json_ext::Value;
use crate::layers::ServiceBuilderExt;
use crate::layers::async_checkpoint::AsyncCheckpointLayer;
use crate::plugins::coprocessor::EXTERNAL_SPAN_NAME;
use crate::services::execution;

/// What information is passed to a router request/response stage
#[derive(Clone, Debug, Default, Deserialize, PartialEq, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub(super) struct ExecutionRequestConf {
    /// Send the headers
    pub(super) headers: bool,
    /// Send the context
    pub(super) context: ContextConf,
    /// Send the body
    pub(super) body: bool,
    /// Send the SDL
    pub(super) sdl: bool,
    /// Send the method
    pub(super) method: bool,
    /// Send the query plan
    pub(super) query_plan: bool,
}

/// What information is passed to a router request/response stage
#[derive(Clone, Debug, Default, Deserialize, PartialEq, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub(super) struct ExecutionResponseConf {
    /// Send the headers
    pub(super) headers: bool,
    /// Send the context
    pub(super) context: ContextConf,
    /// Send the body
    pub(super) body: bool,
    /// Send the SDL
    pub(super) sdl: bool,
    /// Send the HTTP status
    pub(super) status_code: bool,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, JsonSchema)]
#[serde(default)]
pub(super) struct ExecutionStage {
    /// The request configuration
    pub(super) request: ExecutionRequestConf,
    // /// The response configuration
    pub(super) response: ExecutionResponseConf,
}

impl ExecutionStage {
    pub(crate) fn as_service<C>(
        &self,
        http_client: C,
        service: execution::BoxService,
        coprocessor_url: String,
        sdl: Arc<String>,
        response_validation: bool,
    ) -> execution::BoxService
    where
        C: Service<
                http::Request<RouterBody>,
                Response = http::Response<RouterBody>,
                Error = BoxError,
            > + Clone
            + Send
            + Sync
            + 'static,
        <C as tower::Service<http::Request<RouterBody>>>::Future: Send + 'static,
    {
        let request_layer = (self.request != Default::default()).then_some({
            let request_config = self.request.clone();
            let coprocessor_url = coprocessor_url.clone();
            let http_client = http_client.clone();
            let sdl = sdl.clone();

            AsyncCheckpointLayer::new(move |request: execution::Request| {
                let request_config = request_config.clone();
                let coprocessor_url = coprocessor_url.clone();
                let http_client = http_client.clone();
                let sdl = sdl.clone();

                async move {
                    let mut succeeded = true;
                    let result = process_execution_request_stage(
                        http_client,
                        coprocessor_url,
                        sdl,
                        request,
                        request_config,
                        response_validation,
                    )
                    .await
                    .map_err(|error| {
                        succeeded = false;
                        tracing::error!("coprocessor: execution request stage error: {error}");
                        error
                    });

                    u64_counter!(
                        "apollo.router.operations.coprocessor",
                        "Total operations with co-processors enabled",
                        1,
                        "coprocessor.stage" = PipelineStep::ExecutionRequest,
                        "coprocessor.succeeded" = succeeded
                    );
                    result
                }
            })
        });

        let response_layer = (self.response != Default::default()).then_some({
            let response_config = self.response.clone();

            MapFutureLayer::new(move |fut| {
                let coprocessor_url = coprocessor_url.clone();
                let sdl: Arc<String> = sdl.clone();
                let http_client = http_client.clone();
                let response_config = response_config.clone();

                async move {
                    let response: execution::Response = fut.await?;

                    let mut succeeded = true;
                    let result = process_execution_response_stage(
                        http_client,
                        coprocessor_url,
                        sdl,
                        response,
                        response_config,
                        response_validation,
                    )
                    .await
                    .map_err(|error| {
                        succeeded = false;
                        tracing::error!("coprocessor: execution response stage error: {error}");
                        error
                    });

                    u64_counter!(
                        "apollo.router.operations.coprocessor",
                        "Total operations with co-processors enabled",
                        1,
                        "coprocessor.stage" = PipelineStep::ExecutionResponse,
                        "coprocessor.succeeded" = succeeded
                    );
                    result
                }
            })
        });

        fn external_service_span() -> impl Fn(&execution::Request) -> tracing::Span + Clone {
            move |_request: &execution::Request| {
                tracing::info_span!(
                    EXTERNAL_SPAN_NAME,
                    "external service" = stringify!(execution::Request),
                    "otel.kind" = "INTERNAL"
                )
            }
        }

        ServiceBuilder::new()
            .instrument(external_service_span())
            .option_layer(request_layer)
            .option_layer(response_layer)
            .buffered() // XXX: Added during backpressure fixing
            .service(service)
            .boxed()
    }
}

async fn process_execution_request_stage<C>(
    http_client: C,
    coprocessor_url: String,
    sdl: Arc<String>,
    mut request: execution::Request,
    request_config: ExecutionRequestConf,
    response_validation: bool,
) -> Result<ControlFlow<execution::Response, execution::Request>, BoxError>
where
    C: Service<http::Request<RouterBody>, Response = http::Response<RouterBody>, Error = BoxError>
        + Clone
        + Send
        + Sync
        + 'static,
    <C as tower::Service<http::Request<RouterBody>>>::Future: Send + 'static,
{
    // Call into our out of process processor with a body of our body
    // First, extract the data we need from our request and prepare our
    // external call. Use our configuration to figure out which data to send.
    let (parts, body) = request.supergraph_request.into_parts();
    let bytes = Bytes::from(serde_json::to_vec(&body)?);

    let headers_to_send = request_config
        .headers
        .then(|| externalize_header_map(&parts.headers))
        .transpose()?;

    let body_to_send = request_config
        .body
        .then(|| serde_json::from_slice::<Value>(&bytes))
        .transpose()?;
    let context_to_send = request_config.context.get_context(&request.context);
    let sdl_to_send = request_config.sdl.then(|| sdl.clone().to_string());
    let method = request_config.method.then(|| parts.method.to_string());
    let query_plan = request_config
        .query_plan
        .then(|| request.query_plan.clone());

    let payload = Externalizable::execution_builder()
        .stage(PipelineStep::ExecutionRequest)
        .control(Control::default())
        .id(request.context.id.clone())
        .and_headers(headers_to_send)
        .and_body(body_to_send)
        .and_context(context_to_send)
        .and_method(method)
        .and_sdl(sdl_to_send)
        .and_query_plan(query_plan)
        .build();

    tracing::debug!(?payload, "externalized output");
    let start = Instant::now();
    let co_processor_result = payload.call(http_client, &coprocessor_url).await;
    let duration = start.elapsed();
    record_coprocessor_duration(PipelineStep::ExecutionRequest, duration);

    tracing::debug!(?co_processor_result, "co-processor returned");
    let co_processor_output = co_processor_result?;
    validate_coprocessor_output(&co_processor_output, PipelineStep::ExecutionRequest)?;
    // unwrap is safe here because validate_coprocessor_output made sure control is available
    let control = co_processor_output.control.expect("validated above; qed");

    // Thirdly, we need to interpret the control flow which may have been
    // updated by our co-processor and decide if we should proceed or stop.

    if matches!(control, Control::Break(_)) {
        // Ensure the code is a valid http status code
        let code = control.get_http_status()?;

        let res = {
            let graphql_response = {
                let body_value = co_processor_output.body.unwrap_or(Value::Null);
                deserialize_coprocessor_response(body_value, response_validation)
            };

            let mut http_response = http::Response::builder()
                .status(code)
                .body(stream::once(future::ready(graphql_response)).boxed())?;
            if let Some(headers) = co_processor_output.headers {
                *http_response.headers_mut() = internalize_header_map(headers)?;
            }

            let execution_response = execution::Response {
                response: http_response,
                context: request.context,
            };

            if let Some(context) = co_processor_output.context {
                for (mut key, value) in context.try_into_iter()? {
                    if let ContextConf::NewContextConf(NewContextConf::Deprecated) =
                        &request_config.context
                    {
                        key = context_key_from_deprecated(key);
                    }
                    execution_response
                        .context
                        .upsert_json_value(key, move |_current| value);
                }
            }

            execution_response
        };
        return Ok(ControlFlow::Break(res));
    }

    // Finally, process our reply and act on the contents. Our processing logic is
    // that we replace "bits" of our incoming request with the updated bits if they
    // are present in our co_processor_output.
    let new_body: graphql::Request = match co_processor_output.body {
        Some(value) => serde_json_bytes::from_value(value)?,
        None => body,
    };

    request.supergraph_request = http::Request::from_parts(parts, new_body);

    if let Some(context) = co_processor_output.context {
        for (mut key, value) in context.try_into_iter()? {
            if let ContextConf::NewContextConf(NewContextConf::Deprecated) = &request_config.context
            {
                key = context_key_from_deprecated(key);
            }
            request
                .context
                .upsert_json_value(key, move |_current| value);
        }
    }

    if let Some(headers) = co_processor_output.headers {
        *request.supergraph_request.headers_mut() = internalize_header_map(headers)?;
    }

    if let Some(uri) = co_processor_output.uri {
        *request.supergraph_request.uri_mut() = uri.parse()?;
    }

    Ok(ControlFlow::Continue(request))
}

async fn process_execution_response_stage<C>(
    http_client: C,
    coprocessor_url: String,
    sdl: Arc<String>,
    response: execution::Response,
    response_config: ExecutionResponseConf,
    response_validation: bool,
) -> Result<execution::Response, BoxError>
where
    C: Service<http::Request<RouterBody>, Response = http::Response<RouterBody>, Error = BoxError>
        + Clone
        + Send
        + Sync
        + 'static,
    <C as tower::Service<http::Request<RouterBody>>>::Future: Send + 'static,
{
    // split the response into parts + body
    let (mut parts, body) = response.response.into_parts();

    // we split the body (which is a stream) into first response + rest of responses,
    // for which we will implement mapping later
    let (first, rest): (Option<graphql::Response>, graphql::ResponseStream) =
        StreamExt::into_future(body).await;

    // If first is None, we return an error
    let first = first.ok_or_else(|| {
        BoxError::from("Coprocessor cannot convert body into future due to problem with first part")
    })?;

    // Now we process our first chunk of response
    // Encode headers, body, status, context, sdl to create a payload
    let headers_to_send = response_config
        .headers
        .then(|| externalize_header_map(&parts.headers))
        .transpose()?;
    let body_to_send = response_config
        .body
        .then(|| serde_json_bytes::to_value(&first).expect("serialization will not fail"));
    let status_to_send = response_config.status_code.then(|| parts.status.as_u16());
    let context_to_send = response_config.context.get_context(&response.context);
    let sdl_to_send = response_config.sdl.then(|| sdl.clone().to_string());

    let payload = Externalizable::execution_builder()
        .stage(PipelineStep::ExecutionResponse)
        .id(response.context.id.clone())
        .and_headers(headers_to_send)
        .and_body(body_to_send)
        .and_context(context_to_send)
        .and_status_code(status_to_send)
        .and_sdl(sdl_to_send.clone())
        .and_has_next(first.has_next)
        .build();

    // Second, call our co-processor and get a reply.
    tracing::debug!(?payload, "externalized output");
    let start = Instant::now();
    let co_processor_result = payload.call(http_client.clone(), &coprocessor_url).await;
    let duration = start.elapsed();
    record_coprocessor_duration(PipelineStep::ExecutionResponse, duration);

    tracing::debug!(?co_processor_result, "co-processor returned");
    let co_processor_output = co_processor_result?;

    validate_coprocessor_output(&co_processor_output, PipelineStep::ExecutionResponse)?;

    // Check if the incoming GraphQL response was valid according to GraphQL spec
    let incoming_payload_was_valid =
        crate::plugins::coprocessor::was_incoming_payload_valid(&first, response_config.body);

    // Third, process our reply and act on the contents. Our processing logic is
    // that we replace "bits" of our incoming response with the updated bits if they
    // are present in our co_processor_output. If they aren't present, just use the
    // bits that we sent to the co_processor.
    let new_body = handle_graphql_response(
        first,
        co_processor_output.body,
        response_validation,
        incoming_payload_was_valid,
    )?;

    if let Some(control) = co_processor_output.control {
        parts.status = control.get_http_status()?
    }

    if let Some(context) = co_processor_output.context {
        for (mut key, value) in context.try_into_iter()? {
            if let ContextConf::NewContextConf(NewContextConf::Deprecated) =
                &response_config.context
            {
                key = context_key_from_deprecated(key);
            }
            response
                .context
                .upsert_json_value(key, move |_current| value);
        }
    }

    if let Some(headers) = co_processor_output.headers {
        parts.headers = internalize_header_map(headers)?;
    }

    // Clone all the bits we need
    let context = response.context.clone();
    let map_context = response.context.clone();

    // Map the rest of our body to process subsequent chunks of response
    let mapped_stream = rest
        .then(move |deferred_response| {
            let generator_client = http_client.clone();
            let generator_coprocessor_url = coprocessor_url.clone();
            let generator_map_context = map_context.clone();
            let generator_sdl_to_send = sdl_to_send.clone();
            let generator_id = map_context.id.clone();
            let response_config_context = response_config.context.clone();

            async move {
                let body_to_send = response_config.body.then(|| {
                    serde_json_bytes::to_value(&deferred_response)
                        .expect("serialization will not fail")
                });
                let context_to_send = response_config_context.get_context(&generator_map_context);

                // Note: We deliberately DO NOT send headers or status_code even if the user has
                // requested them. That's because they are meaningless on a deferred response and
                // providing them will be a source of confusion.
                let payload = Externalizable::execution_builder()
                    .stage(PipelineStep::ExecutionResponse)
                    .id(generator_id)
                    .and_body(body_to_send)
                    .and_context(context_to_send)
                    .and_sdl(generator_sdl_to_send)
                    .and_has_next(deferred_response.has_next)
                    .build();

                // Second, call our co-processor and get a reply.
                tracing::debug!(?payload, "externalized output");
                let co_processor_result = payload
                    .call(generator_client, &generator_coprocessor_url)
                    .await;
                tracing::debug!(?co_processor_result, "co-processor returned");
                let co_processor_output = co_processor_result?;

                validate_coprocessor_output(&co_processor_output, PipelineStep::ExecutionResponse)?;

                // Check if the incoming deferred GraphQL response was valid according to GraphQL spec
                let incoming_payload_was_valid =
                    crate::plugins::coprocessor::was_incoming_payload_valid(
                        &deferred_response,
                        response_config.body,
                    );

                // Third, process our reply and act on the contents. Our processing logic is
                // that we replace "bits" of our incoming response with the updated bits if they
                // are present in our co_processor_output. If they aren't present, just use the
                // bits that we sent to the co_processor.
                let new_deferred_response = handle_graphql_response(
                    deferred_response,
                    co_processor_output.body,
                    response_validation,
                    incoming_payload_was_valid,
                )?;

                if let Some(context) = co_processor_output.context {
                    for (mut key, value) in context.try_into_iter()? {
                        if let ContextConf::NewContextConf(NewContextConf::Deprecated) =
                            &response_config_context
                        {
                            key = context_key_from_deprecated(key);
                        }
                        generator_map_context.upsert_json_value(key, move |_current| value);
                    }
                }

                // We return the deferred_response into our stream of response chunks
                Ok::<_, BoxError>(new_deferred_response)
            }
        })
        .map(|res: Result<graphql::Response, BoxError>| match res {
            Ok(response) => response,
            Err(e) => {
                tracing::error!("coprocessor error handling deferred execution response: {e}");
                graphql::Response::builder()
                    .error(
                        Error::builder()
                            .message("Internal error handling deferred response")
                            .extension_code("INTERNAL_ERROR")
                            .build(),
                    )
                    .build()
            }
        });

    // Create our response stream which consists of our first body chained with the
    // rest of the responses in our mapped stream.
    let stream = once(ready(new_body)).chain(mapped_stream).boxed();

    // Finally, return a response which has a Body that wraps our stream of response chunks.
    Ok(execution::Response {
        context,
        response: http::Response::from_parts(parts, stream),
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use futures::future::BoxFuture;
    use http::StatusCode;
    use serde_json_bytes::json;
    use tower::BoxError;
    use tower::ServiceExt;

    use super::super::*;
    use super::*;
    use crate::json_ext::Object;
    use crate::plugin::test::MockExecutionService;
    use crate::plugin::test::MockInternalHttpClientService;
    use crate::services::execution;
    use crate::services::router;
    use crate::services::router::body::RouterBody;

    #[allow(clippy::type_complexity)]
    pub(crate) fn mock_with_callback(
        callback: fn(
            http::Request<RouterBody>,
        ) -> BoxFuture<'static, Result<http::Response<RouterBody>, BoxError>>,
    ) -> MockInternalHttpClientService {
        let mut mock_http_client = MockInternalHttpClientService::new();
        mock_http_client.expect_clone().returning(move || {
            let mut mock_http_client = MockInternalHttpClientService::new();

            mock_http_client.expect_clone().returning(move || {
                let mut mock_http_client = MockInternalHttpClientService::new();
                mock_http_client.expect_call().returning(callback);
                mock_http_client
            });
            mock_http_client
        });

        mock_http_client
    }

    #[allow(clippy::type_complexity)]
    fn mock_with_deferred_callback(
        callback: fn(
            http::Request<RouterBody>,
        ) -> BoxFuture<'static, Result<http::Response<RouterBody>, BoxError>>,
    ) -> MockInternalHttpClientService {
        let mut mock_http_client = MockInternalHttpClientService::new();
        mock_http_client.expect_clone().returning(move || {
            let mut mock_http_client = MockInternalHttpClientService::new();
            mock_http_client.expect_clone().returning(move || {
                let mut mock_http_client = MockInternalHttpClientService::new();
                mock_http_client.expect_clone().returning(move || {
                    let mut mock_http_client = MockInternalHttpClientService::new();
                    mock_http_client.expect_call().returning(callback);
                    mock_http_client
                });
                mock_http_client
            });
            mock_http_client
        });

        mock_http_client
    }

    #[tokio::test]
    async fn external_plugin_execution_request() {
        let execution_stage = ExecutionStage {
            request: ExecutionRequestConf {
                headers: false,
                context: ContextConf::Deprecated(false),
                body: true,
                sdl: false,
                method: false,
                query_plan: false,
            },
            response: Default::default(),
        };

        // This will never be called because we will fail at the coprocessor.
        let mut mock_execution_service = MockExecutionService::new();

        mock_execution_service
            .expect_call()
            .returning(|req: execution::Request| {
                // Let's assert that the subgraph request has been transformed as it should have.
                assert_eq!(
                    req.supergraph_request.headers().get("cookie").unwrap(),
                    "tasty_cookie=strawberry"
                );

                assert_eq!(
                    req.context
                        .get::<&str, u8>("this-is-a-test-context")
                        .unwrap()
                        .unwrap(),
                    42
                );

                // The subgraph uri should have changed
                assert_eq!(
                    Some("MyQuery"),
                    req.supergraph_request.body().operation_name.as_deref()
                );

                // The query should have changed
                assert_eq!(
                    "query Long {\n  me {\n  name\n}\n}",
                    req.supergraph_request.body().query.as_ref().unwrap()
                );

                Ok(execution::Response::builder()
                    .data(json!({ "test": 1234_u32 }))
                    .errors(Vec::new())
                    .extensions(Object::new())
                    .context(req.context)
                    .build()
                    .unwrap())
            });

        let mock_http_client = mock_with_callback(move |_: http::Request<RouterBody>| {
            Box::pin(async {
                Ok(http::Response::builder()
                    .body(router::body::from_bytes(
                        r#"{
                                "version": 1,
                                "stage": "ExecutionRequest",
                                "control": "continue",
                                "headers": {
                                    "cookie": [
                                      "tasty_cookie=strawberry"
                                    ],
                                    "content-type": [
                                      "application/json"
                                    ],
                                    "host": [
                                      "127.0.0.1:4000"
                                    ],
                                    "apollo-federation-include-trace": [
                                      "ftv1"
                                    ],
                                    "apollographql-client-name": [
                                      "manual"
                                    ],
                                    "accept": [
                                      "*/*"
                                    ],
                                    "user-agent": [
                                      "curl/7.79.1"
                                    ],
                                    "content-length": [
                                      "46"
                                    ]
                                  },
                                  "body": {
                                    "query": "query Long {\n  me {\n  name\n}\n}",
                                    "operationName": "MyQuery"
                                  },
                                  "context": {
                                    "entries": {
                                      "accepts-json": false,
                                      "accepts-wildcard": true,
                                      "accepts-multipart": false,
                                      "this-is-a-test-context": 42
                                    }
                                  },
                                  "serviceName": "service name shouldn't change",
                                  "uri": "http://thisurihaschanged"
                            }"#,
                    ))
                    .unwrap())
            })
        });

        let service = execution_stage.as_service(
            mock_http_client,
            mock_execution_service.boxed(),
            "http://test".to_string(),
            Arc::new("".to_string()),
            true,
        );

        let request = execution::Request::fake_builder().build();

        assert_eq!(
            json!({ "test": 1234_u32 }),
            service
                .oneshot(request)
                .await
                .unwrap()
                .response
                .into_body()
                .next()
                .await
                .unwrap()
                .data
                .unwrap()
        );
    }

    #[tokio::test]
    async fn external_plugin_execution_request_controlflow_break() {
        let execution_stage = ExecutionStage {
            request: ExecutionRequestConf {
                headers: false,
                context: ContextConf::Deprecated(false),
                body: true,
                sdl: false,
                method: false,
                query_plan: false,
            },
            response: Default::default(),
        };

        // This will never be called because we will fail at the coprocessor.
        let mock_execution_service = MockExecutionService::new();

        let mock_http_client = mock_with_callback(move |_: http::Request<RouterBody>| {
            Box::pin(async {
                Ok(http::Response::builder()
                    .body(router::body::from_bytes(
                        r#"{
                                "version": 1,
                                "stage": "ExecutionRequest",
                                "control": {
                                    "break": 200
                                },
                                "body": {
                                    "errors": [{ "message": "my error message" }]
                                },
                                "context": {
                                    "entries": {
                                        "testKey": true
                                    }
                                },
                                "headers": {
                                    "aheader": ["a value"]
                                }
                            }"#,
                    ))
                    .unwrap())
            })
        });

        let service = execution_stage.as_service(
            mock_http_client,
            mock_execution_service.boxed(),
            "http://test".to_string(),
            Arc::new("".to_string()),
            true,
        );

        let request = execution::Request::fake_builder().build();

        let crate::services::execution::Response {
            mut response,
            context,
        } = service.oneshot(request).await.unwrap();

        assert!(context.get::<_, bool>("testKey").unwrap().unwrap());

        let value = response.headers().get("aheader").unwrap();

        assert_eq!(value, "a value");

        assert_eq!(
            response.body_mut().next().await.unwrap().errors[0]
                .message
                .as_str(),
            "my error message"
        );
    }

    #[tokio::test]
    async fn external_plugin_execution_response() {
        let execution_stage = ExecutionStage {
            response: ExecutionResponseConf {
                headers: true,
                context: ContextConf::NewContextConf(NewContextConf::All),
                body: true,
                sdl: true,
                status_code: false,
            },
            request: Default::default(),
        };

        let mut mock_execution_service = MockExecutionService::new();

        mock_execution_service
            .expect_call()
            .returning(|req: execution::Request| {
                Ok(execution::Response::builder()
                    .data(json!({ "test": 1234_u32 }))
                    .errors(Vec::new())
                    .extensions(Object::new())
                    .context(req.context)
                    .build()
                    .unwrap())
            });

        let mock_http_client =
            mock_with_deferred_callback(move |res: http::Request<RouterBody>| {
                Box::pin(async {
                    let deserialized_response: Externalizable<Value> = serde_json::from_slice(
                        &router::body::into_bytes(res.into_body()).await.unwrap(),
                    )
                    .unwrap();

                    assert_eq!(EXTERNALIZABLE_VERSION, deserialized_response.version);
                    assert_eq!(
                        PipelineStep::ExecutionResponse.to_string(),
                        deserialized_response.stage
                    );

                    assert_eq!(
                        json! {{"data":{ "test": 1234_u32 }}},
                        deserialized_response.body.unwrap()
                    );

                    let input = json!(
                          {
                      "version": 1,
                      "stage": "ExecutionResponse",
                      "control": {
                          "break": 400
                      },
                      "id": "1b19c05fdafc521016df33148ad63c1b",
                      "headers": {
                        "cookie": [
                          "tasty_cookie=strawberry"
                        ],
                        "content-type": [
                          "application/json"
                        ],
                        "host": [
                          "127.0.0.1:4000"
                        ],
                        "apollo-federation-include-trace": [
                          "ftv1"
                        ],
                        "apollographql-client-name": [
                          "manual"
                        ],
                        "accept": [
                          "*/*"
                        ],
                        "user-agent": [
                          "curl/7.79.1"
                        ],
                        "content-length": [
                          "46"
                        ]
                      },
                      "body": {
                        "data": { "test": 42 }
                      },
                      "context": {
                        "entries": {
                          "accepts-json": false,
                          "accepts-wildcard": true,
                          "accepts-multipart": false,
                          "this-is-a-test-context": 42
                        }
                      },
                      "sdl": "the sdl shouldn't change"
                    });
                    Ok(http::Response::builder()
                        .body(router::body::from_bytes(
                            serde_json::to_string(&input).unwrap(),
                        ))
                        .unwrap())
                })
            });

        let service = execution_stage.as_service(
            mock_http_client,
            mock_execution_service.boxed(),
            "http://test".to_string(),
            Arc::new("".to_string()),
            true,
        );

        let request = execution::Request::fake_builder().build();

        let mut res = service.oneshot(request).await.unwrap();

        // Let's assert that the router request has been transformed as it should have.
        assert_eq!(res.response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            res.response.headers().get("cookie").unwrap(),
            "tasty_cookie=strawberry"
        );

        assert_eq!(
            res.context
                .get::<&str, u8>("this-is-a-test-context")
                .unwrap()
                .unwrap(),
            42
        );

        let body = res.response.body_mut().next().await.unwrap();
        // the body should have changed:
        assert_eq!(
            serde_json_bytes::to_value(&body).unwrap(),
            json!({ "data": { "test": 42_u32 } }),
        );
    }

    #[tokio::test]
    async fn multi_part() {
        let execution_stage = ExecutionStage {
            response: ExecutionResponseConf {
                headers: true,
                context: ContextConf::NewContextConf(NewContextConf::All),
                body: true,
                sdl: true,
                status_code: false,
            },
            request: Default::default(),
        };

        let mut mock_execution_service = MockExecutionService::new();

        mock_execution_service
            .expect_call()
            .returning(|req: execution::Request| {
                Ok(execution::Response::fake_stream_builder()
                    .response(
                        graphql::Response::builder()
                            .data(json!({ "test": 1 }))
                            .has_next(true)
                            .build(),
                    )
                    .response(
                        graphql::Response::builder()
                            .data(json!({ "test": 2 }))
                            .has_next(true)
                            .build(),
                    )
                    .response(
                        graphql::Response::builder()
                            .data(json!({ "test": 3 }))
                            .has_next(false)
                            .build(),
                    )
                    .context(req.context)
                    .build()
                    .unwrap())
            });

        let mock_http_client =
            mock_with_deferred_callback(move |res: http::Request<RouterBody>| {
                Box::pin(async {
                    let mut deserialized_response: Externalizable<Value> = serde_json::from_slice(
                        &router::body::into_bytes(res.into_body()).await.unwrap(),
                    )
                    .unwrap();
                    assert_eq!(EXTERNALIZABLE_VERSION, deserialized_response.version);
                    assert_eq!(
                        PipelineStep::ExecutionResponse.to_string(),
                        deserialized_response.stage
                    );

                    // Copy the has_next from the body into the data for checking later
                    deserialized_response
                        .body
                        .as_mut()
                        .unwrap()
                        .as_object_mut()
                        .unwrap()
                        .get_mut("data")
                        .unwrap()
                        .as_object_mut()
                        .unwrap()
                        .insert(
                            "has_next".to_string(),
                            Value::from(deserialized_response.has_next.unwrap_or_default()),
                        );

                    Ok(http::Response::builder()
                        .body(router::body::from_bytes(
                            serde_json::to_string(&deserialized_response).unwrap_or_default(),
                        ))
                        .unwrap())
                })
            });

        let service = execution_stage.as_service(
            mock_http_client,
            mock_execution_service.boxed(),
            "http://test".to_string(),
            Arc::new("".to_string()),
            true,
        );

        let request = execution::Request::fake_builder()
            //.query("foo")
            .build();

        let mut res = service.oneshot(request).await.unwrap();

        let body = res.response.body_mut().next().await.unwrap();
        assert_eq!(
            serde_json_bytes::to_value(&body).unwrap(),
            json!({ "data": { "test": 1, "has_next": true }, "hasNext": true }),
        );
        let body = res.response.body_mut().next().await.unwrap();
        assert_eq!(
            serde_json_bytes::to_value(&body).unwrap(),
            json!({ "data": { "test": 2, "has_next": true }, "hasNext": true }),
        );
        let body = res.response.body_mut().next().await.unwrap();
        assert_eq!(
            serde_json_bytes::to_value(&body).unwrap(),
            json!({ "data": { "test": 3, "has_next": false }, "hasNext": false }),
        );
    }

    // Helper function to create execution stage for validation tests
    fn create_execution_stage_for_response_validation_test() -> ExecutionStage {
        ExecutionStage {
            request: Default::default(),
            response: ExecutionResponseConf {
                headers: true,
                context: ContextConf::NewContextConf(NewContextConf::All),
                body: true,
                sdl: true,
                status_code: false,
            },
        }
    }

    // Helper function to create mock execution service
    fn create_mock_execution_service() -> MockExecutionService {
        let mut mock_execution_service = MockExecutionService::new();
        mock_execution_service
            .expect_call()
            .returning(|req: execution::Request| {
                Ok(execution::Response::builder()
                    .data(json!({ "test": 1234_u32 }))
                    .errors(Vec::new())
                    .extensions(Object::new())
                    .context(req.context)
                    .build()
                    .unwrap())
            });
        mock_execution_service
    }

    // Helper functions for execution request validation tests
    fn create_execution_stage_for_request_validation_test() -> ExecutionStage {
        ExecutionStage {
            request: ExecutionRequestConf {
                headers: true,
                context: ContextConf::NewContextConf(NewContextConf::All),
                body: true,
                sdl: true,
                method: true,
                query_plan: true,
            },
            response: Default::default(),
        }
    }

    // Helper function to create mock http client that returns valid GraphQL break response
    fn create_mock_http_client_execution_request_valid_response() -> MockInternalHttpClientService {
        mock_with_callback(move |_: http::Request<RouterBody>| {
            Box::pin(async {
                let response = json!({
                    "version": 1,
                    "stage": "ExecutionRequest",
                    "control": {
                        "break": 400
                    },
                    "body": {
                        "data": {"test": "valid_response"}
                    }
                });
                Ok(http::Response::builder()
                    .status(200)
                    .body(router::body::from_bytes(
                        serde_json::to_string(&response).unwrap(),
                    ))
                    .unwrap())
            })
        })
    }

    // Helper function to create mock http client that returns empty GraphQL break response
    fn create_mock_http_client_execution_request_empty_response() -> MockInternalHttpClientService {
        mock_with_callback(move |_: http::Request<RouterBody>| {
            Box::pin(async {
                let response = json!({
                    "version": 1,
                    "stage": "ExecutionRequest",
                    "control": {
                        "break": 400
                    },
                    "body": {}
                });
                Ok(http::Response::builder()
                    .status(200)
                    .body(router::body::from_bytes(
                        serde_json::to_string(&response).unwrap(),
                    ))
                    .unwrap())
            })
        })
    }

    // Helper function to create mock http client that returns invalid GraphQL break response
    fn create_mock_http_client_execution_request_invalid_response() -> MockInternalHttpClientService
    {
        mock_with_callback(move |_: http::Request<RouterBody>| {
            Box::pin(async {
                let response = json!({
                    "version": 1,
                    "stage": "ExecutionRequest",
                    "control": {
                        "break": 400
                    },
                    "body": {
                        "errors": "this should be an array not a string"
                    }
                });
                Ok(http::Response::builder()
                    .status(200)
                    .body(router::body::from_bytes(
                        serde_json::to_string(&response).unwrap(),
                    ))
                    .unwrap())
            })
        })
    }

    // Helper function to create mock http client that returns valid GraphQL response
    fn create_mock_http_client_execution_response_valid_response() -> MockInternalHttpClientService
    {
        mock_with_deferred_callback(move |_: http::Request<RouterBody>| {
            Box::pin(async {
                let input = json!({
                    "version": 1,
                    "stage": "ExecutionResponse",
                    "control": "continue",
                    "body": {
                        "data": {"test": "valid_response"}
                    }
                });
                Ok(http::Response::builder()
                    .body(router::body::from_bytes(
                        serde_json::to_string(&input).unwrap(),
                    ))
                    .unwrap())
            })
        })
    }

    // Helper function to create mock http client that returns invalid GraphQL response
    fn create_mock_http_client_invalid_response() -> MockInternalHttpClientService {
        mock_with_deferred_callback(move |_: http::Request<RouterBody>| {
            Box::pin(async {
                let input = json!({
                    "version": 1,
                    "stage": "ExecutionResponse",
                    "control": "continue",
                    "body": {
                        "errors": "this should be an array not a string"
                    }
                });
                Ok(http::Response::builder()
                    .body(router::body::from_bytes(
                        serde_json::to_string(&input).unwrap(),
                    ))
                    .unwrap())
            })
        })
    }

    // Helper function to create mock http client that returns empty response
    fn create_mock_http_client_empty_response() -> MockInternalHttpClientService {
        mock_with_deferred_callback(move |_: http::Request<RouterBody>| {
            Box::pin(async {
                let input = json!({
                    "version": 1,
                    "stage": "ExecutionResponse",
                    "control": "continue",
                    "body": {}
                });
                Ok(http::Response::builder()
                    .body(router::body::from_bytes(
                        serde_json::to_string(&input).unwrap(),
                    ))
                    .unwrap())
            })
        })
    }

    #[tokio::test]
    async fn external_plugin_execution_response_validation_disabled_invalid() {
        let service = create_execution_stage_for_response_validation_test().as_service(
            create_mock_http_client_invalid_response(),
            create_mock_execution_service().boxed(),
            "http://test".to_string(),
            Arc::new("".to_string()),
            false, // Validation disabled
        );

        let request = execution::Request::fake_builder().build();
        let mut res = service.oneshot(request).await.unwrap();

        // With validation disabled, uses permissive serde deserialization instead of strict GraphQL validation
        // Falls back to original response when serde deserialization fails (string can't deserialize to Vec<Error>)
        let body = res.response.body_mut().next().await.unwrap();
        assert_eq!(json!({ "test": 1234_u32 }), body.data.unwrap());
    }

    #[tokio::test]
    async fn external_plugin_execution_response_validation_disabled_empty() {
        let service = create_execution_stage_for_response_validation_test().as_service(
            create_mock_http_client_empty_response(),
            create_mock_execution_service().boxed(),
            "http://test".to_string(),
            Arc::new("".to_string()),
            false, // Validation disabled
        );

        let request = execution::Request::fake_builder().build();
        let mut res = service.oneshot(request).await.unwrap();

        // With validation disabled, empty response deserializes successfully via serde
        // (all fields are optional with defaults), resulting in a response with no data/errors
        let body = res.response.body_mut().next().await.unwrap();
        assert_eq!(body.data, None);
        assert_eq!(body.errors.len(), 0);
    }

    // ===== EXECUTION REQUEST VALIDATION TESTS =====

    #[tokio::test]
    async fn external_plugin_execution_request_validation_enabled_valid() {
        let service = create_execution_stage_for_request_validation_test().as_service(
            create_mock_http_client_execution_request_valid_response(),
            create_mock_execution_service().boxed(),
            "http://test".to_string(),
            Arc::new("".to_string()),
            true, // Validation enabled
        );

        let request = execution::Request::fake_builder().build();
        let mut res = service.oneshot(request).await.unwrap();

        // Should return 400 due to break with valid GraphQL response
        assert_eq!(res.response.status(), 400);
        let body = res.response.body_mut().next().await.unwrap();
        assert_eq!(body.data.unwrap()["test"], "valid_response");
    }

    #[tokio::test]
    async fn external_plugin_execution_request_validation_enabled_empty() {
        let service = create_execution_stage_for_request_validation_test().as_service(
            create_mock_http_client_execution_request_empty_response(),
            create_mock_execution_service().boxed(),
            "http://test".to_string(),
            Arc::new("".to_string()),
            true, // Validation enabled
        );

        let request = execution::Request::fake_builder().build();
        let mut res = service.oneshot(request).await.unwrap();

        // Should return 400 with validation error since empty response violates GraphQL spec
        assert_eq!(res.response.status(), 400);
        let body = res.response.body_mut().next().await.unwrap();
        assert!(!body.errors.is_empty());
        assert!(
            body.errors[0]
                .message
                .contains("couldn't deserialize coprocessor output body")
        );
    }

    #[tokio::test]
    async fn external_plugin_execution_request_validation_enabled_invalid() {
        let service = create_execution_stage_for_request_validation_test().as_service(
            create_mock_http_client_execution_request_invalid_response(),
            create_mock_execution_service().boxed(),
            "http://test".to_string(),
            Arc::new("".to_string()),
            true, // Validation enabled
        );

        let request = execution::Request::fake_builder().build();
        let mut res = service.oneshot(request).await.unwrap();

        // Should return 400 with validation error since errors should be array not string
        assert_eq!(res.response.status(), 400);
        let body = res.response.body_mut().next().await.unwrap();
        assert!(!body.errors.is_empty());
        assert!(
            body.errors[0]
                .message
                .contains("couldn't deserialize coprocessor output body")
        );
    }

    #[tokio::test]
    async fn external_plugin_execution_request_validation_disabled_valid() {
        let service = create_execution_stage_for_request_validation_test().as_service(
            create_mock_http_client_execution_request_valid_response(),
            create_mock_execution_service().boxed(),
            "http://test".to_string(),
            Arc::new("".to_string()),
            false, // Validation disabled
        );

        let request = execution::Request::fake_builder().build();
        let mut res = service.oneshot(request).await.unwrap();

        // Should return 400 due to break with valid response preserved via permissive deserialization
        assert_eq!(res.response.status(), 400);
        let body = res.response.body_mut().next().await.unwrap();
        assert_eq!(body.data.unwrap()["test"], "valid_response");
    }

    #[tokio::test]
    async fn external_plugin_execution_request_validation_disabled_empty() {
        let service = create_execution_stage_for_request_validation_test().as_service(
            create_mock_http_client_execution_request_empty_response(),
            create_mock_execution_service().boxed(),
            "http://test".to_string(),
            Arc::new("".to_string()),
            false, // Validation disabled
        );

        let request = execution::Request::fake_builder().build();
        let mut res = service.oneshot(request).await.unwrap();

        // Should return 400 with empty response preserved via permissive deserialization
        assert_eq!(res.response.status(), 400);
        let body = res.response.body_mut().next().await.unwrap();
        // Empty object deserializes to GraphQL response with no data/errors
        assert_eq!(body.data, None);
        assert_eq!(body.errors.len(), 0);
    }

    #[tokio::test]
    async fn external_plugin_execution_request_validation_disabled_invalid() {
        let service = create_execution_stage_for_request_validation_test().as_service(
            create_mock_http_client_execution_request_invalid_response(),
            create_mock_execution_service().boxed(),
            "http://test".to_string(),
            Arc::new("".to_string()),
            false, // Validation disabled
        );

        let request = execution::Request::fake_builder().build();
        let mut res = service.oneshot(request).await.unwrap();

        // Should return 400 with fallback to original response since invalid structure can't deserialize
        assert_eq!(res.response.status(), 400);
        let body = res.response.body_mut().next().await.unwrap();
        // Falls back to original response since permissive deserialization fails too
        assert!(body.data.is_some() || !body.errors.is_empty());
    }

    // ===== EXECUTION RESPONSE VALIDATION TESTS =====

    #[tokio::test]
    async fn external_plugin_execution_response_validation_enabled_valid() {
        let service = create_execution_stage_for_response_validation_test().as_service(
            create_mock_http_client_execution_response_valid_response(),
            create_mock_execution_service().boxed(),
            "http://test".to_string(),
            Arc::new("".to_string()),
            true, // Validation enabled
        );

        let request = execution::Request::fake_builder().build();
        let mut res = service.oneshot(request).await.unwrap();

        // With validation enabled, valid GraphQL response should be processed normally
        let body = res.response.body_mut().next().await.unwrap();
        assert_eq!(body.data.unwrap()["test"], "valid_response");
    }

    #[tokio::test]
    async fn external_plugin_execution_response_validation_enabled_empty() {
        let service = create_execution_stage_for_response_validation_test().as_service(
            create_mock_http_client_empty_response(),
            create_mock_execution_service().boxed(),
            "http://test".to_string(),
            Arc::new("".to_string()),
            true, // Validation enabled
        );

        let request = execution::Request::fake_builder().build();

        // With validation enabled, empty response should cause service call to fail due to GraphQL validation
        let result = service.oneshot(request).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn external_plugin_execution_response_validation_enabled_invalid() {
        let service = create_execution_stage_for_response_validation_test().as_service(
            create_mock_http_client_invalid_response(),
            create_mock_execution_service().boxed(),
            "http://test".to_string(),
            Arc::new("".to_string()),
            true, // Validation enabled
        );

        let request = execution::Request::fake_builder().build();

        // With validation enabled, invalid GraphQL response should cause service call to fail
        let result = service.oneshot(request).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn external_plugin_execution_response_validation_disabled_valid() {
        let service = create_execution_stage_for_response_validation_test().as_service(
            create_mock_http_client_execution_response_valid_response(),
            create_mock_execution_service().boxed(),
            "http://test".to_string(),
            Arc::new("".to_string()),
            false, // Validation disabled
        );

        let request = execution::Request::fake_builder().build();
        let mut res = service.oneshot(request).await.unwrap();

        // With validation disabled, valid response processed via permissive deserialization
        let body = res.response.body_mut().next().await.unwrap();
        assert_eq!(body.data.unwrap()["test"], "valid_response");
    }
}
