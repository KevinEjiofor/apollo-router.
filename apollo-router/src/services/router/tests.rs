use std::sync::Arc;

use futures::stream::StreamExt;
use http::HeaderMap;
use http::HeaderValue;
use http::Method;
use http::Request;
use http::Uri;
use http::header::CONTENT_TYPE;
use http::header::VARY;
use mime::APPLICATION_JSON;
use opentelemetry::KeyValue;
use parking_lot::Mutex;
use serde_json_bytes::json;
use tower::ServiceExt;
use tower_service::Service;

use crate::Configuration;
use crate::Context;
use crate::context::OPERATION_KIND;
use crate::context::OPERATION_NAME;
use crate::graphql;
use crate::json_ext::Path;
use crate::metrics::FutureMetricsExt;
use crate::plugins::telemetry::CLIENT_NAME;
use crate::plugins::telemetry::CLIENT_VERSION;
use crate::query_planner::APOLLO_OPERATION_ID;
use crate::services::MULTIPART_DEFER_CONTENT_TYPE;
use crate::services::SupergraphRequest;
use crate::services::SupergraphResponse;
use crate::services::router;
use crate::services::router::body::RouterBody;
use crate::services::router::service::from_supergraph_mock_callback;
use crate::services::router::service::from_supergraph_mock_callback_and_configuration;
use crate::services::router::service::process_vary_header;
use crate::services::subgraph;
use crate::services::supergraph;
use crate::spec::query::EXTENSIONS_VALUE_COMPLETION_KEY;
use crate::test_harness::make_fake_batch;

// Test Vary processing

#[test]
fn it_adds_default_with_value_origin_if_no_vary_header() {
    let mut default_headers = HeaderMap::new();
    process_vary_header(&mut default_headers);
    let vary_opt = default_headers.get(VARY);
    assert!(vary_opt.is_some());
    let vary = vary_opt.expect("has a value");
    assert_eq!(vary, "origin");
}

#[test]
fn it_leaves_vary_alone_if_set() {
    let mut default_headers = HeaderMap::new();
    default_headers.insert(VARY, HeaderValue::from_static("*"));
    process_vary_header(&mut default_headers);
    let vary_opt = default_headers.get(VARY);
    assert!(vary_opt.is_some());
    let vary = vary_opt.expect("has a value");
    assert_eq!(vary, "*");
}

#[test]
fn it_leaves_varys_alone_if_there_are_more_than_one() {
    let mut default_headers = HeaderMap::new();
    default_headers.insert(VARY, HeaderValue::from_static("one"));
    default_headers.append(VARY, HeaderValue::from_static("two"));
    process_vary_header(&mut default_headers);
    let vary = default_headers.get_all(VARY);
    assert_eq!(vary.iter().count(), 2);
    for value in vary {
        assert!(value == "one" || value == "two");
    }
}

#[tokio::test]
async fn it_extracts_query_and_operation_name() {
    let query = "query";
    let expected_query = query;
    let operation_name = "operationName";
    let expected_operation_name = operation_name;

    let expected_response = graphql::Response::builder()
        .data(json!({"response": "yay"}))
        .build();

    let mut router_service = from_supergraph_mock_callback(move |req| {
        let example_response = expected_response.clone();

        assert_eq!(
            req.supergraph_request.body().query.as_deref().unwrap(),
            expected_query
        );
        assert_eq!(
            req.supergraph_request
                .body()
                .operation_name
                .as_deref()
                .unwrap(),
            expected_operation_name
        );

        Ok(SupergraphResponse::new_from_graphql_response(
            example_response,
            req.context,
        ))
    })
    .await;

    // get request
    let get_request = supergraph::Request::builder()
        .query(query)
        .operation_name(operation_name)
        .header(CONTENT_TYPE, APPLICATION_JSON.essence_str())
        .uri(Uri::from_static("/"))
        .method(Method::GET)
        .context(Context::new())
        .build()
        .unwrap()
        .try_into()
        .unwrap();

    router_service
        .ready()
        .await
        .expect("readied")
        .call(get_request)
        .await
        .unwrap();

    // post request
    let post_request = supergraph::Request::builder()
        .query(query)
        .operation_name(operation_name)
        .header(CONTENT_TYPE, APPLICATION_JSON.essence_str())
        .uri(Uri::from_static("/"))
        .method(Method::POST)
        .context(Context::new())
        .build()
        .unwrap();

    router_service
        .ready()
        .await
        .expect("readied")
        .call(post_request.try_into().unwrap())
        .await
        .unwrap();
}

#[tokio::test]
async fn it_fails_on_empty_query() {
    let expected_error = "Must provide query string.";

    let router_service = from_supergraph_mock_callback(move |_req| unreachable!()).await;

    let request = SupergraphRequest::fake_builder()
        .query("".to_string())
        .build()
        .expect("expecting valid request")
        .try_into()
        .unwrap();

    let response = router_service
        .oneshot(request)
        .await
        .unwrap()
        .into_graphql_response_stream()
        .await
        .next()
        .await
        .unwrap()
        .unwrap();
    let actual_error = response.errors[0].message.clone();

    assert_eq!(expected_error, actual_error);
    assert!(response.errors[0].extensions.contains_key("code"));
}

#[tokio::test]
async fn it_fails_on_no_query() {
    let expected_error = "Must provide query string.";

    let router_service = from_supergraph_mock_callback(move |_req| unreachable!()).await;

    let request = SupergraphRequest::fake_builder()
        .build()
        .expect("expecting valid request")
        .try_into()
        .unwrap();

    let response = router_service
        .oneshot(request)
        .await
        .unwrap()
        .into_graphql_response_stream()
        .await
        .next()
        .await
        .unwrap()
        .unwrap();
    let actual_error = response.errors[0].message.clone();
    assert_eq!(expected_error, actual_error);
    assert!(response.errors[0].extensions.contains_key("code"));
}

#[tokio::test]
async fn test_http_max_request_bytes() {
    /// Size of the JSON serialization of the request created by `fn canned_new`
    /// in `apollo-router/src/services/supergraph.rs`
    const CANNED_REQUEST_LEN: usize = 391;

    async fn with_config(http_max_request_bytes: usize) -> router::Response {
        let http_request = supergraph::Request::canned_builder()
            .build()
            .unwrap()
            .supergraph_request
            .map(|body| {
                let json_bytes = serde_json::to_vec(&body).unwrap();
                assert_eq!(
                    json_bytes.len(),
                    CANNED_REQUEST_LEN,
                    "The request generated by `fn canned_new` \
                     in `apollo-router/src/services/supergraph.rs` has changed. \
                     Please update `CANNED_REQUEST_LEN` accordingly."
                );
                router::body::from_bytes(json_bytes)
            });
        let config = serde_json::json!({
            "limits": {
                "http_max_request_bytes": http_max_request_bytes
            }
        });
        crate::TestHarness::builder()
            .configuration_json(config)
            .unwrap()
            .build_router()
            .await
            .unwrap()
            .oneshot(router::Request::from(http_request))
            .await
            .unwrap()
    }
    // Send a request just at (under) the limit
    let response = with_config(CANNED_REQUEST_LEN).await.response;
    assert_eq!(response.status(), http::StatusCode::OK);

    // Send a request just over the limit
    let response = with_config(CANNED_REQUEST_LEN - 1).await.response;
    assert_eq!(response.status(), http::StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn it_only_accepts_batch_http_link_mode_for_query_batch() {
    let expected_response: serde_json::Value = serde_json::from_str(include_str!(
        "../query_batching/testdata/batching_not_enabled_response.json"
    ))
    .unwrap();

    async fn with_config() -> router::Response {
        let http_request = make_fake_batch(
            supergraph::Request::canned_builder()
                .build()
                .unwrap()
                .supergraph_request,
            None,
        );
        let config = serde_json::json!({});
        crate::TestHarness::builder()
            .configuration_json(config)
            .unwrap()
            .build_router()
            .await
            .unwrap()
            .oneshot(router::Request::from(http_request))
            .await
            .unwrap()
    }
    // Send a request
    let response = with_config().await.response;
    assert_eq!(response.status(), http::StatusCode::BAD_REQUEST);
    let data: serde_json::Value = serde_json::from_slice(
        &router::body::into_bytes(response.into_body())
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(expected_response, data);
}

#[tokio::test]
async fn it_processes_a_valid_query_batch() {
    let expected_response: serde_json::Value = serde_json::from_str(include_str!(
        "../query_batching/testdata/expected_good_response.json"
    ))
    .unwrap();

    async fn with_config() -> router::Response {
        let http_request = batch_with_three_unique_queries();
        let config = serde_json::json!({
            "batching": {
                "enabled": true,
                "mode" : "batch_http_link"
            }
        });
        crate::TestHarness::builder()
            .configuration_json(config)
            .unwrap()
            .build_router()
            .await
            .unwrap()
            .oneshot(router::Request::from(http_request))
            .await
            .unwrap()
    }
    async move {
        // Send a request
        let response = with_config().await.response;
        assert_eq!(response.status(), http::StatusCode::OK);
        let data: serde_json::Value = serde_json::from_slice(
            &router::body::into_bytes(response.into_body())
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(expected_response, data);

        assert_histogram_sum!(
            "apollo.router.operations.batching.size",
            3,
            "mode" = "batch_http_link"
        );
    }
    .with_metrics()
    .await;
}

#[tokio::test]
async fn it_will_not_process_a_query_batch_without_enablement() {
    let expected_response: serde_json::Value = serde_json::from_str(include_str!(
        "../query_batching/testdata/batching_not_enabled_response.json"
    ))
    .unwrap();

    async fn with_config() -> router::Response {
        let http_request = make_fake_batch(
            supergraph::Request::canned_builder()
                .build()
                .unwrap()
                .supergraph_request,
            None,
        );
        let config = serde_json::json!({});
        crate::TestHarness::builder()
            .configuration_json(config)
            .unwrap()
            .build_router()
            .await
            .unwrap()
            .oneshot(router::Request::from(http_request))
            .await
            .unwrap()
    }
    // Send a request
    let response = with_config().await.response;
    assert_eq!(response.status(), http::StatusCode::BAD_REQUEST);
    let data: serde_json::Value = serde_json::from_slice(
        &router::body::into_bytes(response.into_body())
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(expected_response, data);
}

#[tokio::test]
async fn it_will_not_process_a_poorly_formatted_query_batch() {
    let expected_response: serde_json::Value = serde_json::from_str(include_str!(
        "../query_batching/testdata/badly_formatted_batch_response.json"
    ))
    .unwrap();

    async fn with_config() -> router::Response {
        let http_request = supergraph::Request::canned_builder()
            .build()
            .unwrap()
            .supergraph_request
            .map(|req: graphql::Request| {
                // Modify the request so that it is an invalid array of requests.
                let mut json_bytes = serde_json::to_vec(&req).unwrap();
                let mut result = vec![b'['];
                result.append(&mut json_bytes.clone());
                result.push(b',');
                result.append(&mut json_bytes);
                // Deliberately omit the required trailing ]
                router::body::from_bytes(result)
            });
        let config = serde_json::json!({
            "batching": {
                "enabled": true,
                "mode" : "batch_http_link"
            }
        });
        crate::TestHarness::builder()
            .configuration_json(config)
            .unwrap()
            .build_router()
            .await
            .unwrap()
            .oneshot(router::Request::from(http_request))
            .await
            .unwrap()
    }
    // Send a request
    let response = with_config().await.response;
    assert_eq!(response.status(), http::StatusCode::BAD_REQUEST);
    let data: serde_json::Value = serde_json::from_slice(
        &router::body::into_bytes(response.into_body())
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(expected_response, data);
}

#[tokio::test]
async fn it_will_process_a_non_batched_defered_query() {
    let expected_response = "\r\n--graphql\r\ncontent-type: application/json\r\n\r\n{\"data\":{\"topProducts\":[{\"upc\":\"1\",\"name\":\"Table\",\"reviews\":[{\"product\":{\"name\":\"Table\"},\"author\":{\"id\":\"1\",\"name\":\"Ada Lovelace\"}},{\"product\":{\"name\":\"Table\"},\"author\":{\"id\":\"2\",\"name\":\"Alan Turing\"}}]},{\"upc\":\"2\",\"name\":\"Couch\",\"reviews\":[{\"product\":{\"name\":\"Couch\"},\"author\":{\"id\":\"1\",\"name\":\"Ada Lovelace\"}}]}]},\"hasNext\":true}\r\n--graphql\r\ncontent-type: application/json\r\n\r\n{\"hasNext\":false,\"incremental\":[{\"data\":{\"id\":\"1\"},\"path\":[\"topProducts\",0,\"reviews\",0]},{\"data\":{\"id\":\"4\"},\"path\":[\"topProducts\",0,\"reviews\",1]},{\"data\":{\"id\":\"2\"},\"path\":[\"topProducts\",1,\"reviews\",0]}]}\r\n--graphql--\r\n";
    async fn with_config() -> router::Response {
        let query = "
            query TopProducts($first: Int) {
                topProducts(first: $first) {
                    upc
                    name
                    reviews {
                        ... @defer {
                        id
                        }
                        product { name }
                        author { id name }
                    }
                }
            }
        ";
        let http_request = supergraph::Request::canned_builder()
            .header(http::header::ACCEPT, MULTIPART_DEFER_CONTENT_TYPE)
            .query(query)
            .build()
            .unwrap()
            .supergraph_request
            .map(|req: graphql::Request| {
                let bytes = serde_json::to_vec(&req).unwrap();
                router::body::from_bytes(bytes)
            });
        let config = serde_json::json!({
            "batching": {
                "enabled": true,
                "mode" : "batch_http_link"
            }
        });
        crate::TestHarness::builder()
            .configuration_json(config)
            .unwrap()
            .build_router()
            .await
            .unwrap()
            .oneshot(router::Request::from(http_request))
            .await
            .unwrap()
    }
    // Send a request
    let response = with_config().await.response;
    assert_eq!(response.status(), http::StatusCode::OK);
    let bytes = router::body::into_bytes(response.into_body())
        .await
        .unwrap();
    let data = String::from_utf8_lossy(&bytes);
    assert_eq!(expected_response, data);
}

#[tokio::test]
async fn it_will_not_process_a_batched_deferred_query() {
    let expected_response = "[\r\n--graphql\r\ncontent-type: application/json\r\n\r\n{\"errors\":[{\"message\":\"Deferred responses and subscriptions aren't supported in batches\",\"extensions\":{\"code\":\"BATCHING_DEFER_UNSUPPORTED\"}}]}\r\n--graphql--\r\n, \r\n--graphql\r\ncontent-type: application/json\r\n\r\n{\"errors\":[{\"message\":\"Deferred responses and subscriptions aren't supported in batches\",\"extensions\":{\"code\":\"BATCHING_DEFER_UNSUPPORTED\"}}]}\r\n--graphql--\r\n]";

    async fn with_config() -> router::Response {
        let query = "
            query TopProducts($first: Int) {
                topProducts(first: $first) {
                    upc
                    name
                    reviews {
                        ... @defer {
                        id
                        }
                        product { name }
                        author { id name }
                    }
                }
            }
        ";
        let http_request = make_fake_batch(
            supergraph::Request::canned_builder()
                .header(http::header::ACCEPT, MULTIPART_DEFER_CONTENT_TYPE)
                .query(query)
                .build()
                .unwrap()
                .supergraph_request,
            None,
        );
        let config = serde_json::json!({
            "batching": {
                "enabled": true,
                "mode" : "batch_http_link"
            }
        });
        crate::TestHarness::builder()
            .configuration_json(config)
            .unwrap()
            .build_router()
            .await
            .unwrap()
            .oneshot(router::Request::from(http_request))
            .await
            .unwrap()
    }
    // Send a request
    let response = with_config().await.response;
    assert_eq!(response.status(), http::StatusCode::NOT_ACCEPTABLE);
    let bytes = router::body::into_bytes(response.into_body())
        .await
        .unwrap();
    let data = String::from_utf8_lossy(&bytes);
    assert_eq!(expected_response, data);
}

/// <https://github.com/apollographql/router/issues/3541>
#[tokio::test]
async fn escaped_quotes_in_string_literal() {
    let query = r#"
        query TopProducts($first: Int) {
            topProducts(first: $first) {
                name
                reviewsForAuthor(authorID: "\"1\"") {
                    body
                }
            }
        }
    "#;
    let request = supergraph::Request::fake_builder()
        .query(query)
        .variable("first", 2)
        .build()
        .unwrap();
    let config = serde_json::json!({
        "include_subgraph_errors": {"all": true},
    });
    let subgraph_query_log = Arc::new(Mutex::new(Vec::new()));
    let subgraph_query_log_2 = subgraph_query_log.clone();
    let mut response = crate::TestHarness::builder()
        .configuration_json(config)
        .unwrap()
        .subgraph_hook(move |subgraph_name, service| {
            let is_reviews = subgraph_name == "reviews";
            let subgraph_name = subgraph_name.to_owned();
            let subgraph_query_log_3 = subgraph_query_log_2.clone();
            service
                .map_request(move |request: subgraph::Request| {
                    subgraph_query_log_3.lock().push((
                        subgraph_name.clone(),
                        request.subgraph_request.body().query.clone(),
                    ));
                    request
                })
                .map_response(move |mut response| {
                    if is_reviews {
                        // Replace "couldn't find mock for query" error with empty data
                        let graphql_response = response.response.body_mut();
                        graphql_response.errors.clear();
                        graphql_response.data = Some(serde_json_bytes::json!({
                            "_entities": {"reviews": []},
                        }));
                    }
                    response
                })
                .boxed()
        })
        .build_supergraph()
        .await
        .unwrap()
        .oneshot(request)
        .await
        .unwrap();
    let graphql_response = response.next_response().await.unwrap();
    let subgraph_query_log = subgraph_query_log.lock();
    insta::assert_debug_snapshot!((graphql_response, &subgraph_query_log));
    let subgraph_query = subgraph_query_log[1].1.as_ref().unwrap();

    // The string literal made it through unchanged:
    assert!(subgraph_query.contains(r#"reviewsForAuthor(authorID: "\"1\"")"#));
}

#[tokio::test]
async fn it_stores_operation_error_when_config_is_enabled() {
    async {
        let query = "query operationName { __typename }";
        let operation_name = "operationName";
        let operation_type = "query";
        let operation_id = "opId";
        let client_name = "client";
        let client_version = "version";

        let mut config = Configuration::default();
        config.apollo_plugins.plugins.insert(
            "telemetry".to_string(),
            serde_json::json!({
                "apollo": {
                    "errors": {
                        "preview_extended_error_metrics": "enabled",
                        "subgraph": {
                            "subgraphs": {
                                "myIgnoredSubgraph": {
                                    "send": false,
                                }
                            }
                        }
                    }
                }
            }),
        );

        let mut router_service = from_supergraph_mock_callback_and_configuration(
            move |req| {
                let example_response = graphql::Response::builder()
                    .data(json!({"data": null}))
                    .extension(EXTENSIONS_VALUE_COMPLETION_KEY, json!([{
                        "message": "Cannot return null for non-nullable field SomeType.someField",
                        "path": Path::from("someType/someField")
                    }]))
                    .errors(vec![
                        graphql::Error::builder()
                            .message("some error")
                            .extension_code("SOME_ERROR_CODE")
                            .extension("service", "mySubgraph")
                            .path(Path::from("obj/field"))
                            .build(),
                        graphql::Error::builder()
                            .message("some other error")
                            .extension_code("SOME_OTHER_ERROR_CODE")
                            .extension("service", "myOtherSubgraph")
                            .path(Path::from("obj/arr/@/firstElementField"))
                            .build(),
                        graphql::Error::builder()
                            .message("some ignored error")
                            .extension_code("SOME_IGNORED_ERROR_CODE")
                            .extension("service", "myIgnoredSubgraph")
                            .path(Path::from("obj/arr/@/firstElementField"))
                            .build(),
                    ])
                    .build();
                Ok(SupergraphResponse::new_from_graphql_response(
                    example_response,
                    req.context,
                ))
            },
            Arc::new(config),
        )
        .await;

        let context = Context::new();
        context.insert_json_value(APOLLO_OPERATION_ID, operation_id.into());
        context.insert_json_value(OPERATION_NAME, operation_name.into());
        context.insert_json_value(OPERATION_KIND, query.into());
        context.insert_json_value(CLIENT_NAME, client_name.into());
        context.insert_json_value(CLIENT_VERSION, client_version.into());

        let post_request = supergraph::Request::builder()
            .query(query)
            .operation_name(operation_name)
            .header(CONTENT_TYPE, APPLICATION_JSON.essence_str())
            .uri(Uri::from_static("/"))
            .method(Method::POST)
            .context(context)
            .build()
            .unwrap();

        router_service
            .ready()
            .await
            .unwrap()
            .call(post_request.try_into().unwrap())
            .await
            .unwrap();

        assert_counter!(
            "apollo.router.operations.error",
            1,
            &[
                KeyValue::new("apollo.operation.id", operation_id),
                KeyValue::new("graphql.operation.name", operation_name),
                KeyValue::new("graphql.operation.type", operation_type),
                KeyValue::new("apollo.client.name", client_name),
                KeyValue::new("apollo.client.version", client_version),
                KeyValue::new("graphql.error.extensions.code", "SOME_ERROR_CODE"),
                KeyValue::new("graphql.error.extensions.severity", "ERROR"),
                KeyValue::new("graphql.error.path", "/obj/field"),
                KeyValue::new("apollo.router.error.service", "mySubgraph"),
            ]
        );
        assert_counter!(
            "apollo.router.operations.error",
            1,
            &[
                KeyValue::new("apollo.operation.id", operation_id),
                KeyValue::new("graphql.operation.name", operation_name),
                KeyValue::new("graphql.operation.type", operation_type),
                KeyValue::new("apollo.client.name", client_name),
                KeyValue::new("apollo.client.version", client_version),
                KeyValue::new("graphql.error.extensions.code", "SOME_OTHER_ERROR_CODE"),
                KeyValue::new("graphql.error.extensions.severity", "ERROR"),
                KeyValue::new("graphql.error.path", "/obj/arr/@/firstElementField"),
                KeyValue::new("apollo.router.error.service", "myOtherSubgraph"),
            ]
        );
        assert_counter!(
            "apollo.router.operations.error",
            1,
            &[
                KeyValue::new("apollo.operation.id", operation_id),
                KeyValue::new("graphql.operation.name", operation_name),
                KeyValue::new("graphql.operation.type", operation_type),
                KeyValue::new("apollo.client.name", client_name),
                KeyValue::new("apollo.client.version", client_version),
                KeyValue::new(
                    "graphql.error.extensions.code",
                    "RESPONSE_VALIDATION_FAILED"
                ),
                KeyValue::new("graphql.error.extensions.severity", "WARN"),
                KeyValue::new("graphql.error.path", "/someType/someField"),
                KeyValue::new("apollo.router.error.service", ""),
            ]
        );
        assert_counter_not_exists!(
            "apollo.router.operations.error",
            u64,
            &[
                KeyValue::new("apollo.operation.id", operation_id),
                KeyValue::new("graphql.operation.name", operation_name),
                KeyValue::new("graphql.operation.type", operation_type),
                KeyValue::new("apollo.client.name", client_name),
                KeyValue::new("apollo.client.version", client_version),
                KeyValue::new("graphql.error.extensions.code", "SOME_IGNORED_ERROR_CODE"),
                KeyValue::new("graphql.error.extensions.severity", "ERROR"),
                KeyValue::new("graphql.error.path", "/obj/arr/@/firstElementField"),
                KeyValue::new("apollo.router.error.service", "myIgnoredSubgraph"),
            ]
        );
    }
    .with_metrics()
    .await;
}

#[tokio::test]
async fn it_processes_a_valid_query_batch_with_maximum_size() {
    let expected_response: serde_json::Value = serde_json::from_str(include_str!(
        "../query_batching/testdata/expected_good_response.json"
    ))
    .unwrap();

    let http_request = batch_with_three_unique_queries();
    let config = serde_json::json!({
        "batching": {
            "enabled": true,
            "mode" : "batch_http_link",
            "maximum_size": 3
        }
    });

    // Send a request
    let response = oneshot_request(http_request, config).await.response;
    assert_eq!(response.status(), http::StatusCode::OK);

    let data: serde_json::Value = serde_json::from_slice(
        &router::body::into_bytes(response.into_body())
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(expected_response, data);
}

#[tokio::test]
async fn it_will_not_process_a_batch_that_exceeds_the_maximum_size() {
    let expected_response: serde_json::Value = serde_json::from_str(include_str!(
        "../query_batching/testdata/batch_exceeds_maximum_size_response.json"
    ))
    .unwrap();

    // NB: make_fake_batch creates a request with a batch size of 2
    let http_request = make_fake_batch(
        supergraph::Request::canned_builder()
            .build()
            .unwrap()
            .supergraph_request,
        None,
    );
    let config = serde_json::json!({
        "batching": {
            "enabled": true,
            "mode" : "batch_http_link",
            "maximum_size": 1
        }
    });

    // Send a request
    let response = oneshot_request(http_request, config).await.response;
    assert_eq!(response.status(), http::StatusCode::UNPROCESSABLE_ENTITY);

    let data: serde_json::Value = serde_json::from_slice(
        &router::body::into_bytes(response.into_body())
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(expected_response, data);
}

async fn oneshot_request(
    http_request: Request<RouterBody>,
    config: serde_json::Value,
) -> router::Response {
    crate::TestHarness::builder()
        .configuration_json(config)
        .unwrap()
        .build_router()
        .await
        .unwrap()
        .oneshot(router::Request::from(http_request))
        .await
        .unwrap()
}

fn batch_with_three_unique_queries() -> Request<RouterBody> {
    supergraph::Request::canned_builder()
        .build()
        .unwrap()
        .supergraph_request
        .map(|req_2: graphql::Request| {
            // Create clones of our standard query and update it to have 3 unique queries
            let mut req_1 = req_2.clone();
            let mut req_3 = req_2.clone();
            req_1.query = req_2.query.clone().map(|x| x.replace("upc\n", ""));
            req_3.query = req_2.query.clone().map(|x| x.replace("id name", "name"));

            // Modify the request so that it is a valid array of 3 requests.
            let mut json_bytes_1 = serde_json::to_vec(&req_1).unwrap();
            let mut json_bytes_2 = serde_json::to_vec(&req_2).unwrap();
            let mut json_bytes_3 = serde_json::to_vec(&req_3).unwrap();
            let mut result = vec![b'['];
            result.append(&mut json_bytes_1);
            result.push(b',');
            result.append(&mut json_bytes_2);
            result.push(b',');
            result.append(&mut json_bytes_3);
            result.push(b']');
            router::body::from_bytes(result)
        })
}

const ENUM_SCHEMA: &str = r#"schema
    @link(url: "https://specs.apollo.dev/link/v1.0")
    @link(url: "https://specs.apollo.dev/join/v0.3", for: EXECUTION) {
      query: Query
   }
   directive @link(url: String, as: String, for: link__Purpose, import: [link__Import]) repeatable on SCHEMA
   directive @join__enumValue(graph: join__Graph!) repeatable on ENUM_VALUE
   directive @join__field(graph: join__Graph, requires: join__FieldSet, provides: join__FieldSet, type: String, external: Boolean, override: String, usedOverridden: Boolean) repeatable on FIELD_DEFINITION | INPUT_FIELD_DEFINITION
   directive @join__graph(name: String!, url: String!) on ENUM_VALUE
   directive @join__implements(graph: join__Graph!, interface: String!) repeatable on OBJECT | INTERFACE
   directive @join__type(graph: join__Graph!, key: join__FieldSet, extension: Boolean! = false, resolvable: Boolean! = true, isInterfaceObject: Boolean! = false) repeatable on OBJECT | INTERFACE | UNION | ENUM | INPUT_OBJECT | SCALAR
   directive @join__unionMember(graph: join__Graph!, member: String!) repeatable on UNION

   scalar link__Import

   enum link__Purpose {
     SECURITY
     EXECUTION
   }

   scalar join__FieldSet

   enum join__Graph {
       USER @join__graph(name: "user", url: "http://localhost:4001/graphql")
       ORGA @join__graph(name: "orga", url: "http://localhost:4002/graphql")
   }
   type Query @join__type(graph: USER) @join__type(graph: ORGA){
      test(input: InputEnum): String @join__field(graph: USER)
   }

   enum InputEnum @join__type(graph: USER) @join__type(graph: ORGA) {
    A
    B
  }"#;

// Companion test: services::supergraph::tests::invalid_input_enum
#[tokio::test]
async fn invalid_input_enum() {
    let service = crate::TestHarness::builder()
        .configuration_json(serde_json::json!({
            "include_subgraph_errors": {
                "all": true,
            },
        }))
        .unwrap()
        .schema(ENUM_SCHEMA)
        .build_router()
        .await
        .unwrap();

    let request = supergraph::Request::fake_builder()
        .query("query { test(input: C) }")
        .build()
        .unwrap()
        .try_into()
        .unwrap();
    let response = service
        .clone()
        .oneshot(request)
        .await
        .unwrap()
        .next_response()
        .await
        .expect("should have one response")
        .unwrap();

    let response: serde_json::Value = serde_json::from_slice(&response).unwrap();

    insta::assert_json_snapshot!(response);
}
