//! Various utility functions and core structures used to implement batching support within
//! the router.

use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use opentelemetry::Context as otelContext;
use opentelemetry::trace::TraceContextExt;
use parking_lot::Mutex as PMutex;
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tower::BoxError;
use tracing::Instrument;
use tracing::Span;

use crate::Context;
use crate::error::FetchError;
use crate::error::SubgraphBatchingError;
use crate::graphql;
use crate::plugins::telemetry::otel::span_ext::OpenTelemetrySpanExt;
use crate::services::SubgraphRequest;
use crate::services::SubgraphResponse;
use crate::services::http::HttpClientServiceFactory;
use crate::services::process_batches;
use crate::services::router;
use crate::services::router::body::RouterBody;
use crate::services::subgraph::SubgraphRequestId;
use crate::spec::QueryHash;

/// A query that is part of a batch.
/// Note: It's ok to make transient clones of this struct, but *do not* store clones anywhere apart
/// from the single copy in the extensions. The batching co-ordinator relies on the fact that all
/// senders are dropped to know when to finish processing.
#[derive(Clone, Debug)]
pub(crate) struct BatchQuery {
    /// The index of this query relative to the entire batch
    index: usize,

    /// A channel sender for sending updates to the entire batch
    sender: Arc<Mutex<Option<mpsc::Sender<BatchHandlerMessage>>>>,

    /// How many more progress updates are we expecting to send?
    remaining: Arc<AtomicUsize>,

    /// Batch to which this BatchQuery belongs
    batch: Arc<Batch>,
}

impl fmt::Display for BatchQuery {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "index: {}, ", self.index)?;
        write!(f, "remaining: {}, ", self.remaining.load(Ordering::Acquire))?;
        write!(f, "sender: {:?}, ", self.sender)?;
        write!(f, "batch: {:?}, ", self.batch)?;
        Ok(())
    }
}

impl BatchQuery {
    /// Is this BatchQuery finished?
    pub(crate) fn finished(&self) -> bool {
        self.remaining.load(Ordering::Acquire) == 0
    }

    /// Inform the batch of query hashes representing fetches needed by this element of the batch query
    pub(crate) async fn set_query_hashes(
        &self,
        query_hashes: Vec<Arc<QueryHash>>,
    ) -> Result<(), BoxError> {
        self.remaining.store(query_hashes.len(), Ordering::Release);

        self.sender
            .lock()
            .await
            .as_ref()
            .ok_or(SubgraphBatchingError::SenderUnavailable)?
            .send(BatchHandlerMessage::Begin {
                index: self.index,
                query_hashes,
            })
            .await?;
        Ok(())
    }

    /// Signal to the batch handler that this specific batch query has made some progress.
    ///
    /// The returned channel can be awaited to receive the GraphQL response, when ready.
    pub(crate) async fn signal_progress(
        &self,
        client_factory: HttpClientServiceFactory,
        request: SubgraphRequest,
        gql_request: graphql::Request,
    ) -> Result<oneshot::Receiver<Result<SubgraphResponse, BoxError>>, BoxError> {
        // Create a receiver for this query so that it can eventually get the request meant for it
        let (tx, rx) = oneshot::channel();

        tracing::debug!(
            "index: {}, REMAINING: {}",
            self.index,
            self.remaining.load(Ordering::Acquire)
        );
        self.sender
            .lock()
            .await
            .as_ref()
            .ok_or(SubgraphBatchingError::SenderUnavailable)?
            .send(BatchHandlerMessage::Progress(Box::new(
                BatchHandlerMessageProgress {
                    index: self.index,
                    client_factory,
                    request,
                    gql_request,
                    response_sender: tx,
                    span_context: Span::current().context(),
                },
            )))
            .await?;

        if !self.finished() {
            self.remaining.fetch_sub(1, Ordering::AcqRel);
        }

        // May now be finished
        if self.finished() {
            let mut sender = self.sender.lock().await;
            *sender = None;
        }

        Ok(rx)
    }

    /// Signal to the batch handler that this specific batch query is cancelled
    pub(crate) async fn signal_cancelled(&self, reason: String) -> Result<(), BoxError> {
        self.sender
            .lock()
            .await
            .as_ref()
            .ok_or(SubgraphBatchingError::SenderUnavailable)?
            .send(BatchHandlerMessage::Cancel {
                index: self.index,
                reason,
            })
            .await?;

        if !self.finished() {
            self.remaining.fetch_sub(1, Ordering::AcqRel);
        }

        // May now be finished
        if self.finished() {
            let mut sender = self.sender.lock().await;
            *sender = None;
        }

        Ok(())
    }
}

// #[derive(Debug)]
enum BatchHandlerMessage {
    /// Cancel one of the batch items
    Cancel {
        index: usize,
        reason: String,
    },

    Progress(Box<BatchHandlerMessageProgress>),

    /// A query has passed query planning and knows how many fetches are needed
    /// to complete.
    Begin {
        index: usize,
        query_hashes: Vec<Arc<QueryHash>>,
    },
}

/// A query has reached the subgraph service and we should update its state
struct BatchHandlerMessageProgress {
    index: usize,
    client_factory: HttpClientServiceFactory,
    request: SubgraphRequest,
    gql_request: graphql::Request,
    response_sender: oneshot::Sender<Result<SubgraphResponse, BoxError>>,
    span_context: otelContext,
}

/// Collection of info needed to resolve a batch query
pub(crate) struct BatchQueryInfo {
    /// The owning subgraph request
    request: SubgraphRequest,

    /// The GraphQL request tied to this subgraph request
    gql_request: graphql::Request,

    /// Notifier for the subgraph service handler
    ///
    /// Note: This must be used or else the subgraph request will time out
    sender: oneshot::Sender<Result<SubgraphResponse, BoxError>>,
}

// TODO: Do we want to generate a UUID for a batch for observability reasons?
// TODO: Do we want to track the size of a batch?
#[derive(Debug)]
pub(crate) struct Batch {
    /// A sender channel to communicate with the batching handler
    senders: PMutex<Vec<Option<mpsc::Sender<BatchHandlerMessage>>>>,

    /// The spawned batching handler task handle
    ///
    /// Note: We keep this as a failsafe. If the task doesn't terminate _before_ the batch is
    /// dropped, then we will abort() the task on drop.
    spawn_handle: JoinHandle<Result<(), BoxError>>,

    /// What is the size (number of input operations) of the batch?
    #[allow(dead_code)]
    size: usize,
}

impl Batch {
    /// Creates a new batch, spawning an async task for handling updates to the
    /// batch lifecycle.
    pub(crate) fn spawn_handler(size: usize) -> Self {
        tracing::debug!("New batch created with size {size}");

        // Create the message channel pair for sending update events to the spawned task
        let (spawn_tx, mut rx) = mpsc::channel(size);

        // Populate Senders
        let mut senders = vec![];

        for _ in 0..size {
            senders.push(Some(spawn_tx.clone()));
        }

        let spawn_handle = tokio::spawn(async move {
            /// Helper struct for keeping track of the state of each individual BatchQuery
            ///
            #[derive(Debug)]
            struct BatchQueryState {
                registered: HashSet<Arc<QueryHash>>,
                committed: HashSet<Arc<QueryHash>>,
                cancelled: HashSet<Arc<QueryHash>>,
            }

            impl BatchQueryState {
                // We are ready when everything we registered is in either cancelled or
                // committed.
                fn is_ready(&self) -> bool {
                    self.registered.difference(&self.committed.union(&self.cancelled).cloned().collect()).collect::<Vec<_>>().is_empty()
                }
            }

            // Progressively track the state of the various batch fetches that we expect to see. Keys are batch
            // indices.
            let mut batch_state: HashMap<usize, BatchQueryState> = HashMap::with_capacity(size);

            // We also need to keep track of all requests we need to make and their send handles
            let mut requests: Vec<Vec<BatchQueryInfo>> =
                Vec::from_iter((0..size).map(|_| Vec::new()));

            let mut master_client_factory = None;
            tracing::debug!("Batch about to await messages...");
            // Start handling messages from various portions of the request lifecycle
            // When recv() returns None, we want to stop processing messages
            while let Some(msg) = rx.recv().await {
                match msg {
                    BatchHandlerMessage::Cancel { index, reason } => {
                        // Log the reason for cancelling, update the state
                        tracing::debug!("Cancelling index: {index}, {reason}");

                        if let Some(state) = batch_state.get_mut(&index) {
                            // Short-circuit any requests that are waiting for this cancelled request to complete.
                            let cancelled_requests = std::mem::take(&mut requests[index]);
                            for BatchQueryInfo {
                                request, sender, ..
                            } in cancelled_requests
                            {
                                let subgraph_name = request.subgraph_name;
                                if let Err(log_error) = sender.send(Err(Box::new(FetchError::SubrequestBatchingError {
                                        service: subgraph_name.clone(),
                                        reason: format!("request cancelled: {reason}"),
                                    }))) {
                                    tracing::error!(service=subgraph_name, error=?log_error, "failed to notify waiter that request is cancelled");
                                }
                            }

                            // Clear out everything that has committed, now that they are cancelled, and
                            // mark everything as having been cancelled.
                            state.committed.clear();
                            state.cancelled = state.registered.clone();
                        }
                    }

                    BatchHandlerMessage::Begin {
                        index,
                        query_hashes,
                    } => {
                        tracing::debug!("Beginning batch for index {index} with {query_hashes:?}");

                        batch_state.insert(
                            index,
                            BatchQueryState {
                                cancelled: HashSet::with_capacity(query_hashes.len()),
                                committed: HashSet::with_capacity(query_hashes.len()),
                                registered: HashSet::from_iter(query_hashes),
                            },
                        );
                    }

                    BatchHandlerMessage::Progress(progress) => {
                        // Progress the index
                        let BatchHandlerMessageProgress {
                            index,
                            client_factory,
                            request,
                            gql_request,
                            response_sender,
                            span_context,
                        } = *progress;

                        tracing::debug!("Progress index: {index}");

                        if let Some(state) = batch_state.get_mut(&index) {
                            state.committed.insert(request.query_hash.clone());
                        }

                        if master_client_factory.is_none() {
                            master_client_factory = Some(client_factory);
                        }
                        Span::current().add_link(span_context.span().span_context().clone());
                        requests[index].push(BatchQueryInfo {
                            request,
                            gql_request,
                            sender: response_sender,
                        })
                    }
                }
            }

            // Make sure that we are actually ready and haven't forgotten to update something somewhere
            if batch_state.values().any(|f| !f.is_ready()) {
                tracing::error!("All senders for the batch have dropped before reaching the ready state: {batch_state:#?}");
                // There's not much else we can do, so perform an early return
                return Err(SubgraphBatchingError::ProcessingFailed("batch senders not ready when required".to_string()).into());
            }

            tracing::debug!("Assembling {size} requests into batches");

            // We now have a bunch of requests which are organised by index and we would like to
            // convert them into a bunch of requests organised by service...

            let all_in_one: Vec<_> = requests.into_iter().flatten().collect();

            // Now build up a Service oriented view to use in constructing our batches
            let mut svc_map: HashMap<String, Vec<BatchQueryInfo>> = HashMap::new();
            for BatchQueryInfo {
                request: sg_request,
                gql_request,
                sender: tx,
            } in all_in_one
            {
                let subgraph_name = sg_request.subgraph_name.clone();
                let value = svc_map
                    .entry(
                        subgraph_name,
                    )
                    .or_default();
                value.push(BatchQueryInfo {
                    request: sg_request,
                    gql_request,
                    sender: tx,
                });
            }

            // If we don't have a master_client_factory, we can't do anything.
            if let Some(client_factory) = master_client_factory {
                process_batches(client_factory, svc_map).await?;
            }
            Ok(())
        }.instrument(tracing::info_span!("batch_request", size)));

        Self {
            senders: PMutex::new(senders),
            spawn_handle,
            size,
        }
    }

    /// Create a batch query for a specific index in this batch
    ///
    /// This function may fail if the index doesn't exist or has already been taken
    pub(crate) fn query_for_index(
        batch: Arc<Batch>,
        index: usize,
    ) -> Result<BatchQuery, SubgraphBatchingError> {
        let mut guard = batch.senders.lock();
        // It's a serious error if we try to get a query at an index which doesn't exist or which has already been taken
        if index >= guard.len() {
            return Err(SubgraphBatchingError::ProcessingFailed(format!(
                "tried to retriever sender for index: {index} which does not exist"
            )));
        }
        let opt_sender = std::mem::take(&mut guard[index]);
        if opt_sender.is_none() {
            return Err(SubgraphBatchingError::ProcessingFailed(format!(
                "tried to retriever sender for index: {index} which has already been taken"
            )));
        }
        drop(guard);
        Ok(BatchQuery {
            index,
            sender: Arc::new(Mutex::new(opt_sender)),
            remaining: Arc::new(AtomicUsize::new(0)),
            batch,
        })
    }
}

impl Drop for Batch {
    fn drop(&mut self) {
        // Failsafe: make sure that we kill the background task if the batch itself is dropped
        self.spawn_handle.abort();
    }
}

// Assemble a single batch request to a subgraph
pub(crate) async fn assemble_batch(
    requests: Vec<BatchQueryInfo>,
) -> Result<
    (
        String,
        Vec<(Context, SubgraphRequestId)>,
        http::Request<RouterBody>,
        Vec<oneshot::Sender<Result<SubgraphResponse, BoxError>>>,
    ),
    BoxError,
> {
    // Extract the collection of parts from the requests
    let (txs, request_pairs): (Vec<_>, Vec<_>) = requests
        .into_iter()
        .map(|r| (r.sender, (r.request, r.gql_request)))
        .unzip();
    let (requests, gql_requests): (Vec<_>, Vec<_>) = request_pairs.into_iter().unzip();

    // Construct the actual byte body of the batched request
    let bytes = router::body::into_bytes(serde_json::to_string(&gql_requests)?).await?;

    // Retain the various contexts for later use
    let contexts = requests
        .iter()
        .map(|request| (request.context.clone(), request.id.clone()))
        .collect::<Vec<(Context, SubgraphRequestId)>>();
    // Grab the common info from the first request
    let first_request = requests
        .into_iter()
        .next()
        .ok_or(SubgraphBatchingError::RequestsIsEmpty)?
        .subgraph_request;
    let operation_name = first_request
        .body()
        .operation_name
        .clone()
        .unwrap_or_default();
    let (parts, _) = first_request.into_parts();

    // Generate the final request and pass it up
    let request = http::Request::from_parts(parts, router::body::from_bytes(bytes));
    Ok((operation_name, contexts, request, txs))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use http::header::ACCEPT;
    use http::header::CONTENT_TYPE;
    use tokio::sync::oneshot;
    use tower::ServiceExt;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;
    use wiremock::matchers;

    use super::Batch;
    use super::BatchQueryInfo;
    use super::assemble_batch;
    use crate::Configuration;
    use crate::Context;
    use crate::TestHarness;
    use crate::graphql;
    use crate::graphql::Request;
    use crate::layers::ServiceExt as LayerExt;
    use crate::services::SubgraphRequest;
    use crate::services::SubgraphResponse;
    use crate::services::http::HttpClientServiceFactory;
    use crate::services::router;
    use crate::services::router::body;
    use crate::services::subgraph;
    use crate::services::subgraph::SubgraphRequestId;
    use crate::spec::QueryHash;

    #[tokio::test(flavor = "multi_thread")]
    async fn it_assembles_batch() {
        // Assemble a list of requests for testing
        let (receivers, requests): (Vec<_>, Vec<_>) = (0..2)
            .map(|index| {
                let (tx, rx) = oneshot::channel();
                let gql_request = graphql::Request::fake_builder()
                    .operation_name(format!("batch_test_{index}"))
                    .query(format!("query batch_test {{ slot{index} }}"))
                    .build();

                (
                    rx,
                    BatchQueryInfo {
                        request: SubgraphRequest::fake_builder()
                            .subgraph_request(
                                http::Request::builder().body(gql_request.clone()).unwrap(),
                            )
                            .subgraph_name(format!("slot{index}"))
                            .build(),
                        gql_request,
                        sender: tx,
                    },
                )
            })
            .unzip();

        // Create a vector of the input request context IDs for comparison
        let input_context_ids = requests
            .iter()
            .map(|r| r.request.context.id.clone())
            .collect::<Vec<String>>();
        // Assemble them
        let (op_name, contexts, request, txs) = assemble_batch(requests)
            .await
            .expect("it can assemble a batch");

        let output_context_ids = contexts
            .iter()
            .map(|r| r.0.id.clone())
            .collect::<Vec<String>>();
        // Make sure all of our contexts are preserved during assembly
        assert_eq!(input_context_ids, output_context_ids);

        // Make sure that the name of the entire batch is that of the first
        assert_eq!(op_name, "batch_test_0");

        // We should see the aggregation of all of the requests
        let actual: Vec<graphql::Request> = serde_json::from_str(
            std::str::from_utf8(&router::body::into_bytes(request.into_body()).await.unwrap())
                .unwrap(),
        )
        .unwrap();

        let expected: Vec<_> = (0..2)
            .map(|index| {
                graphql::Request::fake_builder()
                    .operation_name(format!("batch_test_{index}"))
                    .query(format!("query batch_test {{ slot{index} }}"))
                    .build()
            })
            .collect();
        assert_eq!(actual, expected);

        // We should also have all of the correct senders and they should be linked to the correct waiter
        // Note: We reverse the senders since they should be in reverse order when assembled
        assert_eq!(txs.len(), receivers.len());
        for (index, (tx, rx)) in Iterator::zip(txs.into_iter(), receivers).enumerate() {
            let data = serde_json_bytes::json!({
                "data": {
                    format!("slot{index}"): "valid"
                }
            });
            let response = SubgraphResponse {
                response: http::Response::builder()
                    .body(graphql::Response::builder().data(data.clone()).build())
                    .unwrap(),
                context: Context::new(),
                subgraph_name: String::default(),
                id: SubgraphRequestId(String::new()),
            };

            tx.send(Ok(response)).unwrap();

            // We want to make sure that we don't hang the test if we don't get the correct message
            let received = tokio::time::timeout(Duration::from_millis(10), rx)
                .await
                .unwrap()
                .unwrap()
                .unwrap();

            assert_eq!(received.response.into_body().data, Some(data));
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn it_rejects_index_out_of_bounds() {
        let batch = Arc::new(Batch::spawn_handler(2));

        assert!(Batch::query_for_index(batch.clone(), 2).is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn it_rejects_duplicated_index_get() {
        let batch = Arc::new(Batch::spawn_handler(2));

        assert!(Batch::query_for_index(batch.clone(), 0).is_ok());
        assert!(Batch::query_for_index(batch.clone(), 0).is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn it_limits_the_number_of_cancelled_sends() {
        let batch = Arc::new(Batch::spawn_handler(2));

        let bq = Batch::query_for_index(batch.clone(), 0).expect("its a valid index");

        assert!(
            bq.set_query_hashes(vec![Arc::new(QueryHash::default())])
                .await
                .is_ok()
        );
        assert!(!bq.finished());
        assert!(bq.signal_cancelled("why not?".to_string()).await.is_ok());
        assert!(bq.finished());
        assert!(
            bq.signal_cancelled("only once though".to_string())
                .await
                .is_err()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn it_limits_the_number_of_progressed_sends() {
        let batch = Arc::new(Batch::spawn_handler(2));

        let bq = Batch::query_for_index(batch.clone(), 0).expect("its a valid index");

        let factory = HttpClientServiceFactory::from_config(
            "testbatch",
            &Configuration::default(),
            crate::configuration::shared::Client::default(),
        );
        let request = SubgraphRequest::fake_builder()
            .subgraph_request(
                http::Request::builder()
                    .body(graphql::Request::default())
                    .unwrap(),
            )
            .subgraph_name("whatever".to_string())
            .build();
        assert!(
            bq.set_query_hashes(vec![Arc::new(QueryHash::default())])
                .await
                .is_ok()
        );
        assert!(!bq.finished());
        assert!(
            bq.signal_progress(
                factory.clone(),
                request.clone(),
                graphql::Request::default()
            )
            .await
            .is_ok()
        );
        assert!(bq.finished());
        assert!(
            bq.signal_progress(factory, request, graphql::Request::default())
                .await
                .is_err()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn it_limits_the_number_of_mixed_sends() {
        let batch = Arc::new(Batch::spawn_handler(2));

        let bq = Batch::query_for_index(batch.clone(), 0).expect("its a valid index");

        let factory = HttpClientServiceFactory::from_config(
            "testbatch",
            &Configuration::default(),
            crate::configuration::shared::Client::default(),
        );
        let request = SubgraphRequest::fake_builder()
            .subgraph_request(
                http::Request::builder()
                    .body(graphql::Request::default())
                    .unwrap(),
            )
            .subgraph_name("whatever".to_string())
            .build();
        assert!(
            bq.set_query_hashes(vec![Arc::new(QueryHash::default())])
                .await
                .is_ok()
        );
        assert!(!bq.finished());
        assert!(
            bq.signal_progress(factory, request, graphql::Request::default())
                .await
                .is_ok()
        );
        assert!(bq.finished());
        assert!(
            bq.signal_cancelled("only once though".to_string())
                .await
                .is_err()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn it_limits_the_number_of_mixed_sends_two_query_hashes() {
        let batch = Arc::new(Batch::spawn_handler(2));

        let bq = Batch::query_for_index(batch.clone(), 0).expect("its a valid index");

        let factory = HttpClientServiceFactory::from_config(
            "testbatch",
            &Configuration::default(),
            crate::configuration::shared::Client::default(),
        );
        let request = SubgraphRequest::fake_builder()
            .subgraph_request(
                http::Request::builder()
                    .body(graphql::Request::default())
                    .unwrap(),
            )
            .subgraph_name("whatever".to_string())
            .build();
        let qh = Arc::new(QueryHash::default());
        assert!(bq.set_query_hashes(vec![qh.clone(), qh]).await.is_ok());
        assert!(!bq.finished());
        assert!(
            bq.signal_progress(factory, request, graphql::Request::default())
                .await
                .is_ok()
        );
        assert!(!bq.finished());
        assert!(
            bq.signal_cancelled("only twice though".to_string())
                .await
                .is_ok()
        );
        assert!(bq.finished());
        assert!(
            bq.signal_cancelled("only twice though".to_string())
                .await
                .is_err()
        );
    }

    fn expect_batch(request: &wiremock::Request) -> ResponseTemplate {
        let requests: Vec<Request> = request.body_json().unwrap();

        // Extract info about this operation
        let (subgraph, count): (String, usize) = {
            let re = regex::Regex::new(r"entry([AB])\(count: ?([0-9]+)\)").unwrap();
            let captures = re.captures(requests[0].query.as_ref().unwrap()).unwrap();

            (captures[1].to_string(), captures[2].parse().unwrap())
        };

        // We should have gotten `count` elements
        assert_eq!(requests.len(), count);

        // Each element should have be for the specified subgraph and should have a field selection
        // of index.
        // Note: The router appends info to the query, so we append it at this check
        for (index, request) in requests.into_iter().enumerate() {
            assert_eq!(
                request.query,
                Some(format!(
                    "query op{index}__{}__0 {{ entry{}(count: {count}) {{ index }} }}",
                    subgraph.to_lowercase(),
                    subgraph
                ))
            );
        }

        ResponseTemplate::new(200).set_body_json(
            (0..count)
                .map(|index| {
                    serde_json::json!({
                        "data": {
                            format!("entry{subgraph}"): {
                                "index": index
                            }
                        }
                    })
                })
                .collect::<Vec<_>>(),
        )
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn it_matches_subgraph_request_ids_to_responses() {
        // Create a wiremock server for each handler
        let mock_server = MockServer::start().await;
        mock_server
            .register(
                wiremock::Mock::given(matchers::method("POST"))
                    .and(matchers::path("/a"))
                    .respond_with(expect_batch)
                    .expect(1),
            )
            .await;

        let schema = include_str!("../tests/fixtures/batching/schema.graphql");
        let service = TestHarness::builder()
            .configuration_json(serde_json::json!({
            "include_subgraph_errors": {
                "all": true
            },
            "include_subgraph_errors": {
                "all": true
            },
            "batching": {
                "enabled": true,
                "mode": "batch_http_link",
                "subgraph": {
                    "all": {
                        "enabled": true
                    }
                }
            },
            "override_subgraph_url": {
                "a": format!("{}/a", mock_server.uri())
            }}))
            .unwrap()
            .schema(schema)
            .subgraph_hook(move |_subgraph_name, service| {
                service
                    .map_future_with_request_data(
                        |r: &subgraph::Request| r.id.clone(),
                        |id, f| async move {
                            let r: subgraph::ServiceResult = f.await;
                            assert_eq!(id, r.as_ref().map(|r| r.id.clone()).unwrap());
                            r
                        },
                    )
                    .boxed()
            })
            .with_subgraph_network_requests()
            .build_router()
            .await
            .unwrap();

        let requests: Vec<_> = (0..3)
            .map(|index| {
                Request::fake_builder()
                    .query(format!("query op{index}{{ entryA(count: 3) {{ index }} }}"))
                    .build()
            })
            .collect();
        let request = serde_json::to_value(requests).unwrap();

        let context = Context::new();
        let request = router::Request {
            context,
            router_request: http::Request::builder()
                .method("POST")
                .header(CONTENT_TYPE, "application/json")
                .header(ACCEPT, "application/json")
                .body(body::from_bytes(serde_json::to_vec(&request).unwrap()))
                .unwrap(),
        };

        let response = service
            .oneshot(request)
            .await
            .unwrap()
            .next_response()
            .await
            .unwrap()
            .unwrap();

        let response: serde_json::Value = serde_json::from_slice(&response).unwrap();
        insta::assert_json_snapshot!(response);
    }
}
