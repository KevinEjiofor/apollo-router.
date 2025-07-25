use std::path::Path;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use derivative::Derivative;
use derive_more::Display;
use derive_more::From;
use futures::prelude::*;

use crate::Configuration;
use crate::router::Event;
use crate::router::Event::NoMoreConfiguration;
use crate::router::Event::RhaiReload;
use crate::router::Event::UpdateConfiguration;
use crate::uplink::UplinkConfig;

type ConfigurationStream = Pin<Box<dyn Stream<Item = Configuration> + Send>>;

/// The user supplied config. Either a static instance or a stream for hot reloading.
#[derive(From, Display, Derivative)]
#[derivative(Debug)]
#[non_exhaustive]
pub enum ConfigurationSource {
    /// A static configuration.
    ///
    /// Can be created through `serde::Deserialize` from various formats,
    /// or inline in Rust code with `serde_json::json!` and `serde_json::from_value`.
    #[display("Static")]
    #[from(Configuration, Box<Configuration>)]
    Static(Box<Configuration>),

    /// A configuration stream where the server will react to new configuration. If possible
    /// the configuration will be applied without restarting the internal http server.
    #[display("Stream")]
    Stream(#[derivative(Debug = "ignore")] ConfigurationStream),

    /// A yaml file that may be watched for changes
    #[display("File")]
    File {
        /// The path of the configuration file.
        path: PathBuf,

        /// `true` to watch the file for changes and hot apply them.
        watch: bool,
    },
}

impl Default for ConfigurationSource {
    fn default() -> Self {
        ConfigurationSource::Static(Default::default())
    }
}

impl ConfigurationSource {
    /// Convert this config into a stream regardless of if is static or not. Allows for unified handling later.
    pub(crate) fn into_stream(
        self,
        uplink_config: Option<UplinkConfig>,
    ) -> impl Stream<Item = Event> {
        match self {
            ConfigurationSource::Static(mut instance) => {
                instance.uplink = uplink_config;
                stream::iter(vec![UpdateConfiguration(instance.into())]).boxed()
            }
            ConfigurationSource::Stream(stream) => stream
                .map(move |mut c| {
                    c.uplink = uplink_config.clone();
                    UpdateConfiguration(Arc::new(c))
                })
                .boxed(),
            ConfigurationSource::File { path, watch } => {
                // Sanity check, does the config file exists, if it doesn't then bail.
                if !path.exists() {
                    tracing::error!(
                        "configuration file at path '{}' does not exist.",
                        path.to_string_lossy()
                    );
                    stream::empty().boxed()
                } else {
                    match ConfigurationSource::read_config(&path) {
                        Ok(mut configuration) => {
                            if watch {
                                let config_watcher = crate::files::watch(&path)
                                    .filter_map(move |_| {
                                        let path = path.clone();
                                        let uplink_config = uplink_config.clone();
                                        async move {
                                            match ConfigurationSource::read_config_async(&path)
                                                .await
                                            {
                                                Ok(mut configuration) => {
                                                    configuration.uplink = uplink_config.clone();
                                                    Some(UpdateConfiguration(Arc::new(
                                                        configuration,
                                                    )))
                                                }
                                                Err(err) => {
                                                    tracing::error!("{}", err);
                                                    None
                                                }
                                            }
                                        }
                                    })
                                    .boxed();
                                if let Some(rhai_plugin) =
                                    configuration.apollo_plugins.plugins.get("rhai")
                                {
                                    let scripts_path = match rhai_plugin["scripts"].as_str() {
                                        Some(path) => Path::new(path),
                                        None => Path::new("rhai"),
                                    };
                                    // If our path is relative, add it to the current dir
                                    let scripts_watch = if scripts_path.is_relative() {
                                        let current_directory = std::env::current_dir();
                                        if current_directory.is_err() {
                                            tracing::error!("No current directory found",);
                                            return stream::empty().boxed();
                                        }
                                        current_directory.unwrap().join(scripts_path)
                                    } else {
                                        scripts_path.into()
                                    };
                                    let rhai_watcher = crate::files::watch_rhai(&scripts_watch)
                                        .filter_map(move |_| future::ready(Some(RhaiReload)))
                                        .boxed();
                                    // Select across both our streams
                                    futures::stream::select(config_watcher, rhai_watcher).boxed()
                                } else {
                                    config_watcher
                                }
                            } else {
                                configuration.uplink = uplink_config.clone();
                                stream::once(future::ready(UpdateConfiguration(Arc::new(
                                    configuration,
                                ))))
                                .boxed()
                            }
                        }
                        Err(err) => {
                            tracing::error!("Failed to read configuration: {}", err);
                            stream::empty().boxed()
                        }
                    }
                }
            }
        }
        .chain(stream::iter(vec![NoMoreConfiguration]))
        .boxed()
    }

    fn read_config(path: &Path) -> Result<Configuration, ReadConfigError> {
        let config = std::fs::read_to_string(path)?;
        config.parse().map_err(ReadConfigError::Validation)
    }
    async fn read_config_async(path: &Path) -> Result<Configuration, ReadConfigError> {
        let config = tokio::fs::read_to_string(path).await?;
        config.parse().map_err(ReadConfigError::Validation)
    }
}

#[derive(From, Display)]
enum ReadConfigError {
    /// could not read configuration: {0}
    Io(std::io::Error),
    /// {0}
    Validation(crate::configuration::ConfigurationError),
}

#[cfg(test)]
mod tests {
    use std::env::temp_dir;

    use futures::StreamExt;

    use super::*;
    use crate::files::tests::create_temp_file;
    use crate::files::tests::write_and_flush;
    use crate::uplink::UplinkConfig;

    #[tokio::test(flavor = "multi_thread")]
    async fn config_by_file_watching() {
        let (path, mut file) = create_temp_file();
        let contents = include_str!("../../testdata/supergraph_config.router.yaml");
        write_and_flush(&mut file, contents).await;
        let mut stream = ConfigurationSource::File { path, watch: true }
            .into_stream(Some(UplinkConfig::default()))
            .boxed();

        // First update is guaranteed
        assert!(matches!(
            stream.next().await.unwrap(),
            UpdateConfiguration(_)
        ));

        // Need different contents, since we won't get an event if content is the same
        let contents_datadog = include_str!("../../testdata/datadog.router.yaml");
        // Modify the file and try again
        write_and_flush(&mut file, contents_datadog).await;
        assert!(matches!(
            stream.next().await.unwrap(),
            UpdateConfiguration(_)
        ));

        // This time write garbage, there should not be an update.
        write_and_flush(&mut file, ":garbage").await;
        let event = StreamExt::into_future(stream).now_or_never();
        assert!(event.is_none() || matches!(event, Some((Some(NoMoreConfiguration), _))));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn config_by_file_missing() {
        let mut stream = ConfigurationSource::File {
            path: temp_dir().join("does_not_exit"),
            watch: true,
        }
        .into_stream(Some(UplinkConfig::default()));

        // First update fails because the file is invalid.
        assert!(matches!(stream.next().await.unwrap(), NoMoreConfiguration));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn config_by_file_invalid() {
        let (path, mut file) = create_temp_file();
        write_and_flush(&mut file, "Garbage").await;
        let mut stream = ConfigurationSource::File { path, watch: true }
            .into_stream(Some(UplinkConfig::default()));

        // First update fails because the file is invalid.
        assert!(matches!(stream.next().await.unwrap(), NoMoreConfiguration));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn config_by_file_no_watch() {
        let (path, mut file) = create_temp_file();
        let contents = include_str!("../../testdata/supergraph_config.router.yaml");
        write_and_flush(&mut file, contents).await;

        let mut stream = ConfigurationSource::File { path, watch: false }
            .into_stream(Some(UplinkConfig::default()));
        assert!(matches!(
            stream.next().await.unwrap(),
            UpdateConfiguration(_)
        ));
        assert!(matches!(stream.next().await.unwrap(), NoMoreConfiguration));
    }
}
