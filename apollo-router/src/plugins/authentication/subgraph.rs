use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;

use aws_config::provider_config::ProviderConfig;
use aws_credential_types::Credentials;
use aws_credential_types::provider::ProvideCredentials;
use aws_credential_types::provider::error::CredentialsError;
use aws_sigv4::http_request::PayloadChecksumKind;
use aws_sigv4::http_request::SignableBody;
use aws_sigv4::http_request::SignableRequest;
use aws_sigv4::http_request::SigningSettings;
use aws_sigv4::http_request::sign;
use aws_smithy_async::rt::sleep::TokioSleep;
use aws_smithy_async::time::SystemTimeSource;
use aws_smithy_http_client::tls::Provider;
use aws_smithy_http_client::tls::rustls_provider::CryptoMode;
use aws_smithy_runtime_api::client::behavior_version::BehaviorVersion;
use aws_smithy_runtime_api::client::identity::Identity;
use aws_types::SdkConfig;
use aws_types::region::Region;
use aws_types::sdk_config::SharedCredentialsProvider;
use http::HeaderMap;
use http::Request;
use parking_lot::RwLock;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use tokio::sync::mpsc::Sender;
use tokio::task::JoinHandle;
use tower::BoxError;
use tower::ServiceBuilder;
use tower::ServiceExt;

use crate::services::SubgraphRequest;
use crate::services::router;
use crate::services::router::body::RouterBody;

/// Hardcoded Config using access_key and secret.
/// Prefer using DefaultChain instead.
#[derive(Clone, JsonSchema, Deserialize, Serialize, Debug)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) struct AWSSigV4HardcodedConfig {
    /// The ID for this access key.
    access_key_id: String,
    /// The secret key used to sign requests.
    secret_access_key: String,
    /// The AWS region this chain applies to.
    region: String,
    /// The service you're trying to access, eg: "s3", "vpc-lattice-svcs", etc.
    service_name: String,
    /// Specify assumed role configuration.
    assume_role: Option<AssumeRoleProvider>,
}

impl ProvideCredentials for AWSSigV4HardcodedConfig {
    fn provide_credentials<'a>(
        &'a self,
    ) -> aws_credential_types::provider::future::ProvideCredentials<'a>
    where
        Self: 'a,
    {
        aws_credential_types::provider::future::ProvideCredentials::ready(Ok(Credentials::new(
            self.access_key_id.clone(),
            self.secret_access_key.clone(),
            None,
            None,
            "apollo-router",
        )))
    }
}

/// Configuration of the DefaultChainProvider
#[derive(Clone, JsonSchema, Deserialize, Serialize, Debug)]
#[serde(deny_unknown_fields)]
pub(crate) struct DefaultChainConfig {
    /// The AWS region this chain applies to.
    region: String,
    /// The profile name used by this provider
    profile_name: Option<String>,
    /// The service you're trying to access, eg: "s3", "vpc-lattice-svcs", etc.
    service_name: String,
    /// Specify assumed role configuration.
    assume_role: Option<AssumeRoleProvider>,
}

/// Specify assumed role configuration.
#[derive(Clone, JsonSchema, Deserialize, Serialize, Debug)]
#[serde(deny_unknown_fields)]
pub(crate) struct AssumeRoleProvider {
    /// Amazon Resource Name (ARN)
    /// for the role assumed when making requests
    role_arn: String,
    /// Uniquely identify a session when the same role is assumed by different principals or for different reasons.
    session_name: String,
    /// Unique identifier that might be required when you assume a role in another account.
    external_id: Option<String>,
}

/// Configure AWS sigv4 auth.
#[derive(Clone, JsonSchema, Deserialize, Serialize, Debug)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AWSSigV4Config {
    Hardcoded(AWSSigV4HardcodedConfig),
    DefaultChain(DefaultChainConfig),
}

impl AWSSigV4Config {
    async fn get_credentials_provider(&self) -> Arc<dyn ProvideCredentials> {
        let region = self.region();

        let role_provider_builder = self.assume_role().map(|assume_role_provider| {
            let rp =
                aws_config::sts::AssumeRoleProvider::builder(assume_role_provider.role_arn.clone())
                    .configure(
                        &SdkConfig::builder()
                            .http_client(
                                aws_smithy_http_client::Builder::new()
                                    .tls_provider(Provider::Rustls(CryptoMode::Ring))
                                    .build_https(),
                            )
                            .sleep_impl(TokioSleep::new())
                            .time_source(SystemTimeSource::new())
                            .behavior_version(BehaviorVersion::latest())
                            .build(),
                    )
                    .session_name(assume_role_provider.session_name.clone())
                    .region(region.clone());
            if let Some(external_id) = &assume_role_provider.external_id {
                rp.external_id(external_id.as_str())
            } else {
                rp
            }
        });

        match self {
            Self::DefaultChain(config) => {
                let aws_config = credentials_chain_builder().region(region.clone());

                let aws_config = if let Some(profile_name) = &config.profile_name {
                    aws_config.profile_name(profile_name.as_str())
                } else {
                    aws_config
                };

                let chain = aws_config.build().await;
                if let Some(assume_role_provider) = role_provider_builder {
                    Arc::new(assume_role_provider.build_from_provider(chain).await)
                } else {
                    Arc::new(chain)
                }
            }
            Self::Hardcoded(config) => {
                let chain = credentials_chain_builder().build().await;
                if let Some(assume_role_provider) = role_provider_builder {
                    Arc::new(assume_role_provider.build_from_provider(chain).await)
                } else {
                    Arc::new(config.clone())
                }
            }
        }
    }

    fn region(&self) -> Region {
        let region = match self {
            Self::DefaultChain(config) => config.region.clone(),
            Self::Hardcoded(config) => config.region.clone(),
        };
        aws_types::region::Region::new(region)
    }

    fn service_name(&self) -> String {
        match self {
            Self::DefaultChain(config) => config.service_name.clone(),
            Self::Hardcoded(config) => config.service_name.clone(),
        }
    }

    fn assume_role(&self) -> Option<AssumeRoleProvider> {
        match self {
            Self::DefaultChain(config) => config.assume_role.clone(),
            Self::Hardcoded(config) => config.assume_role.clone(),
        }
    }
}

fn credentials_chain_builder() -> aws_config::default_provider::credentials::Builder {
    aws_config::default_provider::credentials::DefaultCredentialsChain::builder().configure(
        ProviderConfig::default()
            .with_http_client(
                aws_smithy_http_client::Builder::new()
                    .tls_provider(Provider::Rustls(CryptoMode::Ring))
                    .build_https(),
            )
            .with_sleep_impl(TokioSleep::new())
            .with_time_source(SystemTimeSource::new()),
    )
}

#[derive(Clone, Debug, JsonSchema, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) enum AuthConfig {
    #[serde(rename = "aws_sig_v4")]
    AWSSigV4(AWSSigV4Config),
}

/// Configure subgraph authentication
#[derive(Clone, Debug, Default, JsonSchema, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) struct Config {
    /// Configuration that will apply to all subgraphs.
    #[serde(default)]
    pub(crate) all: Option<AuthConfig>,
    #[serde(default)]
    /// Create a configuration that will apply only to a specific subgraph.
    pub(crate) subgraphs: HashMap<String, AuthConfig>,
}

#[allow(dead_code)]
#[derive(Clone, Default)]
pub(crate) struct SigningParams {
    pub(crate) all: Option<Arc<SigningParamsConfig>>,
    pub(crate) subgraphs: HashMap<String, Arc<SigningParamsConfig>>,
}

#[derive(Clone)]
pub(crate) struct SigningParamsConfig {
    credentials_provider: CredentialsProvider,
    region: Region,
    service_name: String,
    subgraph_name: String,
}

#[derive(Clone, Debug)]
struct CredentialsProvider {
    credentials: Arc<RwLock<Credentials>>,
    _credentials_updater_handle: Arc<JoinHandle<()>>,
    #[allow(dead_code)]
    refresh_credentials: Sender<()>,
}

// Refresh token if it will expire within the next 5 minutes
const MIN_REMAINING_DURATION: Duration = std::time::Duration::from_secs(60 * 5);
// If the token couldn't be refreshed, try again in 1 minute
const RETRY_DURATION: Duration = std::time::Duration::from_secs(60);

impl CredentialsProvider {
    async fn from_provide_credentials(
        provide_credentials: impl ProvideCredentials + 'static,
    ) -> Result<Self, CredentialsError> {
        let credentials_provider = SharedCredentialsProvider::new(provide_credentials);
        let (sender, mut refresh_credentials_receiver) = tokio::sync::mpsc::channel(1);
        let credentials = credentials_provider.provide_credentials().await?;
        let mut refresh_timer = next_refresh_timer(&credentials);
        let credentials = Arc::new(RwLock::new(credentials));
        let c2 = credentials.clone();
        let crp2 = credentials_provider.clone();
        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(refresh_timer.unwrap_or(Duration::MAX)) => {
                       refresh_timer = refresh_credentials(&crp2, &c2).await;
                    },
                    rcr = refresh_credentials_receiver.recv() => {
                        if rcr.is_some() {
                            refresh_timer = refresh_credentials(&crp2, &c2).await;
                        } else {
                            return;
                        }
                    },
                }
            }
        });
        Ok(Self {
            _credentials_updater_handle: Arc::new(handle),
            refresh_credentials: sender,
            credentials,
        })
    }

    #[allow(dead_code)]
    pub(crate) async fn refresh_credentials(&self) {
        let _ = self.refresh_credentials.send(()).await;
    }
}

async fn refresh_credentials(
    credentials_provider: &(impl ProvideCredentials + 'static),
    credentials: &RwLock<Credentials>,
) -> Option<Duration> {
    match credentials_provider.provide_credentials().await {
        Ok(new_credentials) => {
            let mut credentials = credentials.write();
            *credentials = new_credentials;
            next_refresh_timer(&credentials)
        }
        Err(e) => {
            tracing::warn!("authentication: couldn't refresh credentials {e}");
            Some(RETRY_DURATION)
        }
    }
}

fn next_refresh_timer(credentials: &Credentials) -> Option<Duration> {
    credentials
        .expiry()
        .and_then(|e| e.duration_since(SystemTime::now()).ok())
        .and_then(|d| {
            d.checked_sub(MIN_REMAINING_DURATION)
                .or(Some(Duration::from_secs(0)))
        })
}

impl ProvideCredentials for CredentialsProvider {
    fn provide_credentials<'a>(
        &'a self,
    ) -> aws_credential_types::provider::future::ProvideCredentials<'a>
    where
        Self: 'a,
    {
        aws_credential_types::provider::future::ProvideCredentials::ready(Ok(self
            .credentials
            .read()
            .clone()))
    }
}

impl SigningParamsConfig {
    pub(crate) async fn sign(
        &self,
        mut req: Request<RouterBody>,
        subgraph_name: &str,
    ) -> Result<Request<RouterBody>, BoxError> {
        let credentials = self.credentials().await?;
        let builder = self.signing_params_builder(&credentials).await?;
        let (parts, body) = req.into_parts();
        // Depending on the service, AWS refuses sigv4 payloads that contain specific headers.
        // We'll go with default signed headers
        let headers = HeaderMap::<&'static str>::default();
        // UnsignedPayload only applies to lattice
        let body_bytes = router::body::into_bytes(body).await?.to_vec();
        let signable_request = SignableRequest::new(
            parts.method.as_str(),
            parts.uri.to_string(),
            headers.iter().map(|(name, value)| (name.as_str(), *value)),
            match self.service_name.as_str() {
                "vpc-lattice-svcs" => SignableBody::UnsignedPayload,
                _ => SignableBody::Bytes(body_bytes.as_slice()),
            },
        )?;

        let signing_params = builder.build().expect("all required fields set");

        let (signing_instructions, _signature) = sign(signable_request, &signing_params.into())
            .map_err(|err| {
                increment_failure_counter(subgraph_name);
                let error = format!("failed to sign GraphQL body for AWS SigV4: {}", err);
                tracing::error!("{}", error);
                error
            })?
            .into_parts();
        req = Request::<RouterBody>::from_parts(parts, router::body::from_bytes(body_bytes));
        signing_instructions.apply_to_request_http1x(&mut req);
        increment_success_counter(subgraph_name);
        Ok(req)
    }

    // This function is the same as above, except it's a new one because () doesn't implement HttpBody`
    pub(crate) async fn sign_empty(
        &self,
        mut req: Request<()>,
        subgraph_name: &str,
    ) -> Result<Request<()>, BoxError> {
        let credentials = self.credentials().await?;
        let builder = self.signing_params_builder(&credentials).await?;
        let (parts, _) = req.into_parts();
        // Depending on the service, AWS refuses sigv4 payloads that contain specific headers.
        // We'll go with default signed headers
        let headers = HeaderMap::<&'static str>::default();
        // UnsignedPayload only applies to lattice
        let signable_request = SignableRequest::new(
            parts.method.as_str(),
            parts.uri.to_string(),
            headers.iter().map(|(name, value)| (name.as_str(), *value)),
            match self.service_name.as_str() {
                "vpc-lattice-svcs" => SignableBody::UnsignedPayload,
                _ => SignableBody::Bytes(&[]),
            },
        )?;

        let signing_params = builder.build().expect("all required fields set");

        let (signing_instructions, _signature) = sign(signable_request, &signing_params.into())
            .map_err(|err| {
                increment_failure_counter(subgraph_name);
                let error = format!("failed to sign GraphQL body for AWS SigV4: {}", err);
                tracing::error!("{}", error);
                error
            })?
            .into_parts();
        req = Request::<()>::from_parts(parts, ());
        signing_instructions.apply_to_request_http1x(&mut req);
        increment_success_counter(subgraph_name);
        Ok(req)
    }

    async fn signing_params_builder<'s>(
        &'s self,
        identity: &'s Identity,
    ) -> Result<aws_sigv4::sign::v4::signing_params::Builder<'s, SigningSettings>, BoxError> {
        let settings = get_signing_settings(self);
        let builder = aws_sigv4::sign::v4::SigningParams::builder()
            .identity(identity)
            .region(self.region.as_ref())
            .name(&self.service_name)
            .time(SystemTime::now())
            .settings(settings);
        Ok(builder)
    }

    async fn credentials(&self) -> Result<Identity, BoxError> {
        self.credentials_provider
            .provide_credentials()
            .await
            .map_err(|err| {
                increment_failure_counter(self.subgraph_name.as_str());
                let error = format!("failed to get credentials for AWS SigV4 signing: {}", err);
                tracing::error!("{}", error);
                error.into()
            })
            .map(Into::into)
    }
}

fn increment_success_counter(subgraph_name: &str) {
    u64_counter!(
        "apollo.router.operations.authentication.aws.sigv4",
        "Number of subgraph requests signed with AWS SigV4",
        1,
        authentication.aws.sigv4.failed = false,
        subgraph.service.name = subgraph_name.to_string()
    );
}
fn increment_failure_counter(subgraph_name: &str) {
    u64_counter!(
        "apollo.router.operations.authentication.aws.sigv4",
        "Number of subgraph requests signed with AWS SigV4",
        1,
        authentication.aws.sigv4.failed = true,
        subgraph.service.name = subgraph_name.to_string()
    );
}

pub(super) async fn make_signing_params(
    config: &AuthConfig,
    subgraph_name: &str,
) -> Result<SigningParamsConfig, BoxError> {
    match config {
        AuthConfig::AWSSigV4(config) => {
            let credentials_provider = config.get_credentials_provider().await;
            Ok(SigningParamsConfig {
                region: config.region(),
                service_name: config.service_name(),
                credentials_provider: CredentialsProvider::from_provide_credentials(
                    credentials_provider,
                )
                .await
                .map_err(BoxError::from)?,
                subgraph_name: subgraph_name.to_string(),
            })
        }
    }
}

/// There are three possible cases
/// https://github.com/awslabs/aws-sdk-rust/blob/9c3168dafa4fd8885ce4e1fd41cec55ce982a33c/sdk/aws-sigv4/src/http_request/sign.rs#L264C1-L271C6
fn get_signing_settings(signing_params: &SigningParamsConfig) -> SigningSettings {
    let mut settings = SigningSettings::default();
    settings.payload_checksum_kind = match signing_params.service_name.as_str() {
        "appsync" | "s3" | "vpc-lattice-svcs" => PayloadChecksumKind::XAmzSha256,
        _ => PayloadChecksumKind::NoHeader,
    };
    settings
}

pub(super) struct SubgraphAuth {
    pub(super) signing_params: Arc<SigningParams>,
}

impl SubgraphAuth {
    pub(super) fn subgraph_service(
        &self,
        name: &str,
        service: crate::services::subgraph::BoxService,
    ) -> crate::services::subgraph::BoxService {
        if let Some(signing_params) = self.params_for_service(name) {
            ServiceBuilder::new()
                .map_request(move |req: SubgraphRequest| {
                    let signing_params = signing_params.clone();
                    req.context
                        .extensions()
                        .with_lock(|lock| lock.insert(signing_params));
                    req
                })
                .service(service)
                .boxed()
        } else {
            service
        }
    }
}

impl SubgraphAuth {
    fn params_for_service(&self, service_name: &str) -> Option<Arc<SigningParamsConfig>> {
        self.signing_params
            .subgraphs
            .get(service_name)
            .cloned()
            .or_else(|| self.signing_params.all.clone())
    }
}

#[cfg(test)]
mod test {
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    use http::header::CONTENT_LENGTH;
    use http::header::CONTENT_TYPE;
    use http::header::HOST;
    use regex::Regex;
    use tower::Service;

    use super::*;
    use crate::Context;
    use crate::graphql::Request;
    use crate::plugin::test::MockSubgraphService;
    use crate::query_planner::fetch::OperationKind;
    use crate::services::SubgraphRequest;
    use crate::services::SubgraphResponse;
    use crate::services::subgraph::SubgraphRequestId;

    async fn test_signing_settings(service_name: &str) -> SigningSettings {
        let params: SigningParamsConfig = make_signing_params(
            &AuthConfig::AWSSigV4(AWSSigV4Config::Hardcoded(AWSSigV4HardcodedConfig {
                access_key_id: "id".to_string(),
                secret_access_key: "secret".to_string(),
                region: "us-east-1".to_string(),
                service_name: service_name.to_string(),
                assume_role: None,
            })),
            "all",
        )
        .await
        .unwrap();
        get_signing_settings(&params)
    }

    #[tokio::test]
    async fn test_get_signing_settings() {
        assert_eq!(
            PayloadChecksumKind::XAmzSha256,
            test_signing_settings("s3").await.payload_checksum_kind
        );
        assert_eq!(
            PayloadChecksumKind::XAmzSha256,
            test_signing_settings("vpc-lattice-svcs")
                .await
                .payload_checksum_kind
        );
        assert_eq!(
            PayloadChecksumKind::XAmzSha256,
            test_signing_settings("appsync").await.payload_checksum_kind
        );
        assert_eq!(
            PayloadChecksumKind::NoHeader,
            test_signing_settings("something-else")
                .await
                .payload_checksum_kind
        );
    }

    #[test]
    fn test_all_aws_sig_v4_hardcoded_config() {
        serde_yaml::from_str::<Config>(
            r#"
        all:
          aws_sig_v4:
            hardcoded:
              access_key_id: "test"
              secret_access_key: "test"
              region: "us-east-1"
              service_name: "lambda"
        "#,
        )
        .unwrap();
    }

    #[test]
    fn test_subgraph_aws_sig_v4_hardcoded_config() {
        serde_yaml::from_str::<Config>(
            r#"
        subgraphs:
          products:
            aws_sig_v4:
              hardcoded:
                access_key_id: "test"
                secret_access_key: "test"
                region: "us-east-1"
                service_name: "test_service"
        "#,
        )
        .unwrap();
    }

    #[test]
    fn test_aws_sig_v4_default_chain_assume_role_config() {
        serde_yaml::from_str::<Config>(
            r#"
        all:
            aws_sig_v4:
                default_chain:
                    profile_name: "my-test-profile"
                    region: "us-east-1"
                    service_name: "lambda"
                    assume_role:
                        role_arn: "test-arn"
                        session_name: "test-session"
                        external_id: "test-id"
        "#,
        )
        .unwrap();
    }

    #[tokio::test]
    async fn test_lattice_body_payload_should_be_unsigned() -> Result<(), BoxError> {
        let subgraph_request = example_request();

        let mut mock = MockSubgraphService::new();
        mock.expect_call()
            .times(1)
            .withf(|request| {
                let http_request = get_signed_request(request, "products".to_string());
                assert_eq!(
                    "UNSIGNED-PAYLOAD",
                    http_request
                        .headers()
                        .get("x-amz-content-sha256")
                        .unwrap()
                        .to_str()
                        .unwrap()
                );
                true
            })
            .returning(example_response);

        let mut service = SubgraphAuth {
            signing_params: Arc::new(SigningParams {
                all: make_signing_params(
                    &AuthConfig::AWSSigV4(AWSSigV4Config::Hardcoded(AWSSigV4HardcodedConfig {
                        access_key_id: "id".to_string(),
                        secret_access_key: "secret".to_string(),
                        region: "us-east-1".to_string(),
                        service_name: "vpc-lattice-svcs".to_string(),
                        assume_role: None,
                    })),
                    "all",
                )
                .await
                .ok()
                .map(Arc::new),
                subgraphs: Default::default(),
            }),
        }
        .subgraph_service("test_subgraph", mock.boxed());

        service.ready().await?.call(subgraph_request).await?;
        Ok(())
    }

    #[tokio::test]
    async fn test_aws_sig_v4_headers() -> Result<(), BoxError> {
        let subgraph_request = example_request();

        let mut mock = MockSubgraphService::new();
        mock.expect_call()
            .times(1)
            .withf(|request| {
                let http_request = get_signed_request(request, "products".to_string());
                let authorization_regex = Regex::new(r"AWS4-HMAC-SHA256 Credential=id/\d{8}/us-east-1/s3/aws4_request, SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature=[a-f0-9]{64}").unwrap();
                let authorization_header_str = http_request.headers().get("authorization").unwrap().to_str().unwrap();
                assert_eq!(match authorization_regex.find(authorization_header_str) {
                    Some(m) => m.as_str(),
                    None => "no match"
                }, authorization_header_str);

                let x_amz_date_regex = Regex::new(r"\d{8}T\d{6}Z").unwrap();
                let x_amz_date_header_str = http_request.headers().get("x-amz-date").unwrap().to_str().unwrap();
                assert_eq!(match x_amz_date_regex.find(x_amz_date_header_str) {
                    Some(m) => m.as_str(),
                    None => "no match"
                }, x_amz_date_header_str);

                assert_eq!(http_request.headers().get("x-amz-content-sha256").unwrap(), "255959b4c6e11c1080f61ce0d75eb1b565c1772173335a7828ba9c13c25c0d8c");

                true
            })
            .returning(example_response);

        let mut service = SubgraphAuth {
            signing_params: Arc::new(SigningParams {
                all: make_signing_params(
                    &AuthConfig::AWSSigV4(AWSSigV4Config::Hardcoded(AWSSigV4HardcodedConfig {
                        access_key_id: "id".to_string(),
                        secret_access_key: "secret".to_string(),
                        region: "us-east-1".to_string(),
                        service_name: "s3".to_string(),
                        assume_role: None,
                    })),
                    "all",
                )
                .await
                .ok()
                .map(Arc::new),
                subgraphs: Default::default(),
            }),
        }
        .subgraph_service("test_subgraph", mock.boxed());

        service.ready().await?.call(subgraph_request).await?;
        Ok(())
    }

    #[tokio::test]
    async fn test_credentials_provider_keeps_credentials_in_cache() -> Result<(), BoxError> {
        #[derive(Debug, Default, Clone)]
        struct TestCredentialsProvider {
            times_called: Arc<AtomicUsize>,
        }

        impl ProvideCredentials for TestCredentialsProvider {
            fn provide_credentials<'a>(
                &'a self,
            ) -> aws_credential_types::provider::future::ProvideCredentials<'a>
            where
                Self: 'a,
            {
                self.times_called.fetch_add(1, Ordering::SeqCst);
                aws_credential_types::provider::future::ProvideCredentials::ready(Ok(
                    Credentials::new("test_key", "test_secret", None, None, "test_provider"),
                ))
            }
        }

        let tcp = TestCredentialsProvider::default();

        let cp = CredentialsProvider::from_provide_credentials(tcp.clone())
            .await
            .unwrap();

        let _ = cp.provide_credentials().await.unwrap();
        let _ = cp.provide_credentials().await.unwrap();

        assert_eq!(1, tcp.times_called.load(Ordering::SeqCst));

        cp.refresh_credentials().await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        let _ = cp.provide_credentials().await.unwrap();
        let _ = cp.provide_credentials().await.unwrap();

        assert_eq!(2, tcp.times_called.load(Ordering::SeqCst));

        Ok(())
    }

    #[tokio::test]
    async fn test_credentials_provider_refresh_on_stale() -> Result<(), BoxError> {
        #[derive(Debug, Default, Clone)]
        struct TestCredentialsProvider {
            times_called: Arc<AtomicUsize>,
        }

        impl ProvideCredentials for TestCredentialsProvider {
            fn provide_credentials<'a>(
                &'a self,
            ) -> aws_credential_types::provider::future::ProvideCredentials<'a>
            where
                Self: 'a,
            {
                self.times_called.fetch_add(1, Ordering::SeqCst);
                aws_credential_types::provider::future::ProvideCredentials::ready(Ok(
                    // The token will expire immediately, it should be refreshed fairly fast
                    Credentials::new(
                        "test_key",
                        "test_secret",
                        None,
                        // 5 minutes + 1 second
                        SystemTime::now().checked_add(Duration::from_secs(60 * 5 + 1)),
                        "test_provider",
                    ),
                ))
            }
        }

        let tcp = TestCredentialsProvider::default();

        let cp = CredentialsProvider::from_provide_credentials(tcp.clone())
            .await
            .unwrap();

        let _ = cp.provide_credentials().await.unwrap();
        let _ = cp.provide_credentials().await.unwrap();

        assert_eq!(1, tcp.times_called.load(Ordering::SeqCst));

        cp.refresh_credentials().await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        let _ = cp.provide_credentials().await.unwrap();
        let _ = cp.provide_credentials().await.unwrap();

        assert_eq!(2, tcp.times_called.load(Ordering::SeqCst));

        tokio::time::sleep(Duration::from_secs(1)).await;

        assert_eq!(3, tcp.times_called.load(Ordering::SeqCst));

        Ok(())
    }

    fn example_response(req: SubgraphRequest) -> Result<SubgraphResponse, BoxError> {
        Ok(SubgraphResponse::new_from_response(
            http::Response::default(),
            Context::new(),
            req.subgraph_name,
            SubgraphRequestId(String::new()),
        ))
    }

    fn example_request() -> SubgraphRequest {
        SubgraphRequest::builder()
            .supergraph_request(Arc::new(
                http::Request::builder()
                    .header(HOST, "host")
                    .header(CONTENT_LENGTH, "2")
                    .header(CONTENT_TYPE, "graphql")
                    .body(
                        Request::builder()
                            .query("query")
                            .operation_name("my_operation_name")
                            .build(),
                    )
                    .expect("expecting valid request"),
            ))
            .subgraph_request(
                http::Request::builder()
                    .header(HOST, "rhost")
                    .header(CONTENT_LENGTH, "22")
                    .header(CONTENT_TYPE, "graphql")
                    .uri("https://test-endpoint.com")
                    .body(Request::builder().query("query").build())
                    .expect("expecting valid request"),
            )
            .operation_kind(OperationKind::Query)
            .context(Context::new())
            .subgraph_name(String::default())
            .build()
    }

    fn get_signed_request(
        request: &SubgraphRequest,
        service_name: String,
    ) -> http::Request<RouterBody> {
        let signing_params = request
            .context
            .extensions()
            .with_lock(|lock| lock.get::<Arc<SigningParamsConfig>>().cloned())
            .unwrap();

        let http_request = request
            .clone()
            .subgraph_request
            .map(|body| router::body::from_bytes(serde_json::to_string(&body).unwrap()));

        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                signing_params
                    .sign(http_request, service_name.as_str())
                    .await
                    .unwrap()
            })
        })
        .join()
        .unwrap()
    }
}
