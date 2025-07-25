use std::fs;
use std::io;
use std::num::NonZeroU32;
use std::path::Path;
use std::path::PathBuf;
use std::process::ExitCode;

use apollo_compiler::ExecutableDocument;
use apollo_federation::ApiSchemaOptions;
use apollo_federation::Supergraph;
use apollo_federation::connectors::expand::ExpansionResult;
use apollo_federation::connectors::expand::expand_connectors;
use apollo_federation::correctness::CorrectnessError;
use apollo_federation::error::FederationError;
use apollo_federation::error::SingleFederationError;
use apollo_federation::internal_error;
use apollo_federation::query_graph;
use apollo_federation::query_plan::query_planner::QueryPlanner;
use apollo_federation::query_plan::query_planner::QueryPlannerConfig;
use apollo_federation::subgraph;
use apollo_federation::subgraph::typestate;
use clap::Parser;
use tracing_subscriber::prelude::*;

mod bench;
use bench::BenchOutput;
use bench::run_bench;

#[derive(Parser)]
struct QueryPlannerArgs {
    /// Enable @defer support.
    #[arg(long, default_value_t = false)]
    enable_defer: bool,
    /// Generate fragments to compress subgraph queries.
    #[arg(long, default_value_t = false)]
    generate_fragments: bool,
    /// Enable type conditioned fetching.
    #[arg(long, default_value_t = false)]
    type_conditioned_fetching: bool,
    /// Run GraphQL validation check on generated subgraph queries. (default: true)
    #[arg(long, default_missing_value = "true", require_equals = true, num_args = 0..=1)]
    subgraph_validation: Option<bool>,
    /// Set the `debug.max_evaluated_plans` option.
    #[arg(long)]
    max_evaluated_plans: Option<NonZeroU32>,
    /// Set the `debug.paths_limit` option.
    #[arg(long)]
    paths_limit: Option<u32>,
}

/// CLI arguments. See <https://docs.rs/clap/latest/clap/_derive/index.html>
#[derive(Parser)]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Subcommand)]
enum Command {
    /// Converts a supergraph schema to the corresponding API schema
    Api {
        /// Path(s) to one supergraph schema file, `-` for stdin or multiple subgraph schemas.
        schemas: Vec<PathBuf>,
        /// Enable @defer support.
        #[arg(long, default_value_t = false)]
        enable_defer: bool,
    },
    /// Outputs the query graph from a supergraph schema or subgraph schemas
    QueryGraph {
        /// Path(s) to one supergraph schema file, `-` for stdin or multiple subgraph schemas.
        schemas: Vec<PathBuf>,
    },
    /// Outputs the federated query graph from a supergraph schema or subgraph schemas
    FederatedGraph {
        /// Path(s) to one supergraph schema file, `-` for stdin or multiple subgraph schemas.
        schemas: Vec<PathBuf>,
    },
    /// Outputs the formatted query plan for the given query and schema
    Plan {
        #[arg(long)]
        json: bool,
        query: PathBuf,
        /// Path(s) to one supergraph schema file, `-` for stdin or multiple subgraph schemas.
        schemas: Vec<PathBuf>,
        #[command(flatten)]
        planner: QueryPlannerArgs,
    },
    /// Validate one supergraph schema file or multiple subgraph schemas
    Validate {
        /// Path(s) to one supergraph schema file, `-` for stdin or multiple subgraph schemas.
        schemas: Vec<PathBuf>,
    },
    /// Compose a supergraph schema from multiple subgraph schemas
    Compose {
        /// Path(s) to subgraph schemas.
        schemas: Vec<PathBuf>,
    },
    /// Expand and validate a subgraph schema and print the result
    Subgraph {
        /// The path to the subgraph schema file, or `-` for stdin
        subgraph_schema: PathBuf,
    },
    /// Extract subgraph schemas from a supergraph schema to stdout (or in a directory if specified)
    Extract {
        /// The path to the supergraph schema file, or `-` for stdin
        supergraph_schema: PathBuf,
        /// The output directory for the extracted subgraph schemas
        destination_dir: Option<PathBuf>,
    },
    Bench {
        /// The path to the supergraph schema file
        supergraph_schema: PathBuf,
        /// The path to the directory that contains all operations to run against
        operations_dir: PathBuf,
        #[command(flatten)]
        planner: QueryPlannerArgs,
    },

    /// Expand connector-enabled supergraphs
    Expand {
        /// The path to the supergraph schema file, or `-` for stdin
        supergraph_schema: PathBuf,

        /// The output directory for the extracted subgraph schemas
        destination_dir: Option<PathBuf>,

        /// An optional prefix to match against expanded subgraph names
        #[arg(long)]
        filter_prefix: Option<String>,
    },
}

impl QueryPlannerArgs {
    fn apply(&self, config: &mut QueryPlannerConfig) {
        config.incremental_delivery.enable_defer = self.enable_defer;
        config.generate_query_fragments = self.generate_fragments;
        config.type_conditioned_fetching = self.type_conditioned_fetching;
        config.subgraph_graphql_validation = self.subgraph_validation.unwrap_or(true);
        if let Some(max_evaluated_plans) = self.max_evaluated_plans {
            config.debug.max_evaluated_plans = max_evaluated_plans;
        }
        config.debug.paths_limit = self.paths_limit;
    }
}

impl From<QueryPlannerArgs> for QueryPlannerConfig {
    fn from(value: QueryPlannerArgs) -> Self {
        let mut config = QueryPlannerConfig::default();
        value.apply(&mut config);
        config
    }
}

/// Set up the tracing subscriber
fn init_tracing() {
    let fmt_layer = tracing_subscriber::fmt::layer()
        .without_time()
        .with_target(false);
    let filter_layer = tracing_subscriber::EnvFilter::from_default_env();
    tracing_subscriber::registry()
        .with(fmt_layer)
        .with(filter_layer)
        .init();
}

fn main() -> ExitCode {
    init_tracing();
    let args = Args::parse();
    let result = match args.command {
        Command::Api {
            schemas,
            enable_defer,
        } => cmd_api_schema(&schemas, enable_defer),
        Command::QueryGraph { schemas } => cmd_query_graph(&schemas),
        Command::FederatedGraph { schemas } => cmd_federated_graph(&schemas),
        Command::Plan {
            json,
            query,
            schemas,
            planner,
        } => cmd_plan(json, &query, &schemas, planner),
        Command::Validate { schemas } => cmd_validate(&schemas),
        Command::Subgraph { subgraph_schema } => cmd_subgraph(&subgraph_schema),
        Command::Compose { schemas } => cmd_compose(&schemas),
        Command::Extract {
            supergraph_schema,
            destination_dir,
        } => cmd_extract(&supergraph_schema, destination_dir.as_ref()),
        Command::Bench {
            supergraph_schema,
            operations_dir,
            planner,
        } => cmd_bench(&supergraph_schema, &operations_dir, planner),
        Command::Expand {
            supergraph_schema,
            destination_dir,
            filter_prefix,
        } => cmd_expand(
            &supergraph_schema,
            destination_dir.as_ref(),
            filter_prefix.as_deref(),
        ),
    };
    match result {
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }

        Ok(_) => ExitCode::SUCCESS,
    }
}

fn read_input(input_path: &Path) -> String {
    if input_path == std::path::Path::new("-") {
        io::read_to_string(io::stdin()).unwrap()
    } else {
        fs::read_to_string(input_path).unwrap()
    }
}

fn cmd_api_schema(file_paths: &[PathBuf], enable_defer: bool) -> Result<(), FederationError> {
    let supergraph = load_supergraph(file_paths)?;
    let api_schema = supergraph.to_api_schema(apollo_federation::ApiSchemaOptions {
        include_defer: enable_defer,
        include_stream: false,
    })?;
    println!("{}", api_schema.schema());
    Ok(())
}

/// Compose a supergraph from multiple subgraph files.
fn compose_files(file_paths: &[PathBuf]) -> Result<apollo_federation::Supergraph, FederationError> {
    let schemas: Vec<_> = file_paths
        .iter()
        .map(|pathname| {
            let doc_str = std::fs::read_to_string(pathname).unwrap();
            let url = format!("file://{}", pathname.to_str().unwrap());
            let basename = pathname.file_stem().unwrap().to_str().unwrap();
            subgraph::Subgraph::parse_and_expand(basename, &url, &doc_str).unwrap()
        })
        .collect();
    let supergraph = apollo_federation::Supergraph::compose(schemas.iter().collect()).unwrap();
    Ok(supergraph)
}

fn load_supergraph_file(
    file_path: &Path,
) -> Result<apollo_federation::Supergraph, FederationError> {
    let doc_str = read_input(file_path);
    apollo_federation::Supergraph::new_with_router_specs(&doc_str)
}

/// Load either single supergraph schema file or compose one from multiple subgraph files.
/// If the single file is "-", read from stdin.
fn load_supergraph(
    file_paths: &[PathBuf],
) -> Result<apollo_federation::Supergraph, FederationError> {
    if file_paths.is_empty() {
        panic!("Error: missing command arguments");
    } else if file_paths.len() == 1 {
        load_supergraph_file(&file_paths[0])
    } else {
        compose_files(file_paths)
    }
}

fn cmd_query_graph(file_paths: &[PathBuf]) -> Result<(), FederationError> {
    let supergraph = load_supergraph(file_paths)?;
    let name: &str = if file_paths.len() == 1 {
        file_paths[0].file_stem().unwrap().to_str().unwrap()
    } else {
        "supergraph"
    };
    let query_graph =
        query_graph::build_query_graph::build_query_graph(name.into(), supergraph.schema)?;
    println!("{}", query_graph::output::to_dot(&query_graph));
    Ok(())
}

fn cmd_federated_graph(file_paths: &[PathBuf]) -> Result<(), FederationError> {
    let supergraph = load_supergraph(file_paths)?;
    let api_schema = supergraph.to_api_schema(Default::default())?;
    let query_graph =
        query_graph::build_federated_query_graph(supergraph.schema, api_schema, None, None)?;
    println!("{}", query_graph::output::to_dot(&query_graph));
    Ok(())
}

fn cmd_plan(
    use_json: bool,
    query_path: &Path,
    schema_paths: &[PathBuf],
    planner: QueryPlannerArgs,
) -> Result<(), FederationError> {
    let query = read_input(query_path);
    let supergraph = load_supergraph(schema_paths)?;

    let config = QueryPlannerConfig::from(planner);
    let planner = QueryPlanner::new(&supergraph, config)?;

    let query_doc =
        ExecutableDocument::parse_and_validate(planner.api_schema().schema(), query, query_path)?;
    let query_plan = planner.build_query_plan(&query_doc, None, Default::default())?;
    if use_json {
        println!("{}", serde_json::to_string_pretty(&query_plan).unwrap());
    } else {
        println!("{query_plan}");
    }

    // Check the query plan
    let subgraphs_by_name = supergraph
        .extract_subgraphs()
        .unwrap()
        .into_iter()
        .map(|(name, subgraph)| (name, subgraph.schema))
        .collect();
    let result = apollo_federation::correctness::check_plan(
        planner.api_schema(),
        &supergraph.schema,
        &subgraphs_by_name,
        &query_doc,
        &query_plan,
    );
    match result {
        Ok(_) => Ok(()),
        Err(CorrectnessError::FederationError(e)) => Err(e),
        Err(CorrectnessError::ComparisonError(e)) => Err(internal_error!("{}", e.description())),
    }
}

fn cmd_validate(file_paths: &[PathBuf]) -> Result<(), FederationError> {
    load_supergraph(file_paths)?;
    println!("[SUCCESS]");
    Ok(())
}

fn cmd_subgraph(file_path: &Path) -> Result<(), FederationError> {
    let doc_str = read_input(file_path);
    let name = file_path
        .file_name()
        .and_then(|name| name.to_str().map(|x| x.to_string()));
    let name = name.unwrap_or("subgraph".to_string());
    let subgraph = typestate::Subgraph::parse(&name, &format!("http://{name}"), &doc_str)
        .map_err(|e| e.into_inner())?
        .expand_links()
        .map_err(|e| e.into_inner())?
        .assume_upgraded()
        .validate()
        .map_err(|e| e.into_inner())?;
    println!("{}", subgraph.schema_string());
    Ok(())
}

fn cmd_compose(file_paths: &[PathBuf]) -> Result<(), FederationError> {
    let supergraph = compose_files(file_paths)?;
    println!("{}", supergraph.schema.schema());
    Ok(())
}

fn cmd_extract(file_path: &Path, dest: Option<&PathBuf>) -> Result<(), FederationError> {
    let supergraph = load_supergraph_file(file_path)?;
    let subgraphs = supergraph.extract_subgraphs()?;
    if let Some(dest) = dest {
        fs::create_dir_all(dest).map_err(|_| SingleFederationError::Internal {
            message: "Error: directory creation failed".into(),
        })?;
        for (name, subgraph) in subgraphs {
            let subgraph_path = dest.join(format!("{}.graphql", name));
            fs::write(subgraph_path, subgraph.schema.schema().to_string()).map_err(|_| {
                SingleFederationError::Internal {
                    message: "Error: file output failed".into(),
                }
            })?;
        }
    } else {
        for (name, subgraph) in subgraphs {
            println!("[Subgraph `{}`]", name);
            println!("{}", subgraph.schema.schema());
            println!(); // newline
        }
    }
    Ok(())
}

fn cmd_expand(
    file_path: &Path,
    dest: Option<&PathBuf>,
    filter_prefix: Option<&str>,
) -> Result<(), FederationError> {
    let original_supergraph = load_supergraph_file(file_path)?;
    let ExpansionResult::Expanded { raw_sdl, .. } = expand_connectors(
        &original_supergraph.schema.schema().serialize().to_string(),
        &ApiSchemaOptions::default(),
    )?
    else {
        return Err(FederationError::internal(
            "supplied supergraph has no connectors to expand",
        ));
    };

    // Validate the schema
    // TODO: If expansion errors here due to bugs, it can be very hard to trace
    // what specific portion of the expansion process failed. Work will need to be
    // done to expansion to allow for returning an error type that carries the error
    // and the expanded subgraph as seen until the error.
    let expanded = Supergraph::new_with_router_specs(&raw_sdl)?;

    let subgraphs = expanded.extract_subgraphs()?;
    if let Some(dest) = dest {
        fs::create_dir_all(dest).map_err(|_| SingleFederationError::Internal {
            message: "Error: directory creation failed".into(),
        })?;
        for (name, subgraph) in subgraphs {
            // Skip any files not matching the prefix, if specified
            if let Some(prefix) = filter_prefix {
                if !name.starts_with(prefix) {
                    continue;
                }
            }

            let subgraph_path = dest.join(format!("{}.graphql", name));
            fs::write(subgraph_path, subgraph.schema.schema().to_string()).map_err(|_| {
                SingleFederationError::Internal {
                    message: "Error: file output failed".into(),
                }
            })?;
        }
    } else {
        // Print out the schemas as YAML so that it can be piped into rover
        // TODO: It would be nice to use rover's supergraph type here instead of manually printing
        println!("federation_version: 2");
        println!("subgraphs:");
        for (name, subgraph) in subgraphs {
            // Skip any files not matching the prefix, if specified
            if let Some(prefix) = filter_prefix {
                if !name.starts_with(prefix) {
                    continue;
                }
            }

            let schema_str = subgraph.schema.schema().serialize().initial_indent_level(4);
            println!("  {name}:");
            println!("    routing_url: none");
            println!("    schema:");
            println!("      sdl: |");
            println!("{schema_str}");
            println!(); // newline
        }
    }

    Ok(())
}

fn _cmd_bench(
    file_path: &Path,
    operations_dir: &PathBuf,
    config: QueryPlannerConfig,
) -> Result<Vec<BenchOutput>, FederationError> {
    let supergraph = load_supergraph_file(file_path)?;
    run_bench(supergraph, operations_dir, config)
}

fn cmd_bench(
    file_path: &Path,
    operations_dir: &PathBuf,
    planner: QueryPlannerArgs,
) -> Result<(), FederationError> {
    let results = _cmd_bench(file_path, operations_dir, planner.into())?;
    println!("| operation_name | time (ms) | evaluated_plans (max 10000) | error |");
    println!("|----------------|----------------|-----------|-----------------------------|");
    for r in results {
        println!("{}", r);
    }
    Ok(())
}

#[test]
fn test_bench() {
    insta::assert_json_snapshot!(
        _cmd_bench(
            Path::new("./fixtures/starstuff.graphql"),
            &PathBuf::from("./fixtures/queries"),
            Default::default(),
        ).unwrap(),
        { "[].timing" => 1.234 },
    );
}
