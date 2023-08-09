use crate::{apis::coredb_types::CoreDB, defaults, secret::PrometheusExporterSecretData, Context, Error};
use kube::Client;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, sync::Arc};
use tracing::{debug, error};

pub const QUERIES_YAML: &str = "queries.yaml";
pub const EXPORTER_VOLUME: &str = "postgres-exporter";
pub const EXPORTER_CONFIGMAP_PREFIX: &str = "metrics-";

#[derive(Deserialize, Serialize, Clone, Debug, JsonSchema, Default)]
#[allow(non_snake_case)]
pub struct PostgresMetrics {
    #[serde(default = "defaults::default_postgres_exporter_image")]
    pub image: String,
    #[serde(default = "defaults::default_postgres_exporter_enabled")]
    pub enabled: bool,

    #[schemars(schema_with = "preserve_arbitrary")]
    pub queries: Option<QueryConfig>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
pub struct Metric {
    pub usage: Usage,
    pub description: String,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
pub struct Metrics {
    #[serde(flatten)]
    pub metrics: BTreeMap<String, Metric>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
pub struct QueryItem {
    pub query: String,
    pub master: bool,
    pub metrics: Vec<Metrics>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
pub struct QueryConfig {
    #[serde(flatten)]
    pub queries: BTreeMap<String, QueryItem>,
}

// source: https://github.com/kube-rs/kube/issues/844
fn preserve_arbitrary(_gen: &mut schemars::gen::SchemaGenerator) -> schemars::schema::Schema {
    let mut obj = schemars::schema::SchemaObject::default();
    obj.extensions
        .insert("x-kubernetes-preserve-unknown-fields".into(), true.into());
    schemars::schema::Schema::Object(obj)
}

use crate::configmap::apply_configmap;
use kube::runtime::controller::Action;
use std::str::FromStr;
use tokio::time::Duration;

#[derive(Clone, Debug, JsonSchema, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "UPPERCASE")]
pub enum Usage {
    Counter,
    Gauge,
    Histogram,
    Label,
}

impl FromStr for Usage {
    type Err = ();

    fn from_str(input: &str) -> Result<Usage, Self::Err> {
        match input {
            "COUNTER" => Ok(Usage::Counter),
            "GAUGE" => Ok(Usage::Gauge),
            "HISTOGRAM" => Ok(Usage::Histogram),
            "LABEL" => Ok(Usage::Label),
            _ => Err(()),
        }
    }
}

pub async fn create_postgres_exporter_role(
    cdb: &CoreDB,
    ctx: Arc<Context>,
    secret: Option<PrometheusExporterSecretData>,
) -> Result<(), Action> {
    if !(cdb.spec.postgresExporterEnabled) {
        return Ok(());
    }

    debug!(
        "Creating postgres_exporter role for database {} in namespace {}",
        cdb.metadata
            .name
            .clone()
            .expect("instance should always have a name"),
        cdb.metadata
            .namespace
            .clone()
            .expect("instance should always have a namespace")
    );

    // Check if secret data is available
    let password = match &secret {
        Some(data) => data.password.clone(),
        None => {
            error!(
                "No secret data available for postgres_exporter in instance {}",
                cdb.metadata
                    .name
                    .clone()
                    .expect("instance should always have a name")
            );
            return Err(Action::requeue(Duration::from_secs(300)));
        }
    };
    // https://github.com/prometheus-community/postgres_exporter#running-as-non-superuser
    let query = format!(
        "
        CREATE OR REPLACE FUNCTION __tmp_create_user() returns void as $$
        BEGIN
          IF NOT EXISTS (
                  SELECT
                  FROM   pg_catalog.pg_user
                  WHERE  usename = 'postgres_exporter') THEN
            CREATE USER postgres_exporter;
          END IF;
        END;
        $$ language plpgsql;

        SELECT __tmp_create_user();
        DROP FUNCTION __tmp_create_user();

        ALTER USER postgres_exporter SET SEARCH_PATH TO postgres_exporter,pg_catalog;
        ALTER USER postgres_exporter WITH PASSWORD '{}';
        GRANT CONNECT ON DATABASE postgres TO postgres_exporter;
        GRANT pg_monitor to postgres_exporter;
        GRANT pg_read_all_stats to postgres_exporter;
        ",
        password
    );
    let query_result = cdb.psql(query, "postgres".to_owned(), ctx.clone()).await;
    match query_result {
        Ok(_) => Ok(()),
        Err(e) => Err(e),
    }
}

pub async fn reconcile_prom_configmap(cdb: &CoreDB, client: Client, ns: &str) -> Result<(), Error> {
    // set custom pg-prom metrics in configmap values if they are specified
    let coredb_name = cdb
        .metadata
        .name
        .clone()
        .expect("instance should always have a name");
    match cdb.spec.metrics.clone().and_then(|m| m.queries) {
        Some(queries) => {
            let qdata = serde_yaml::to_string(&queries).unwrap();
            let d: BTreeMap<String, String> = BTreeMap::from([(QUERIES_YAML.to_string(), qdata)]);
            apply_configmap(
                client.clone(),
                ns,
                &format!("{}{}", EXPORTER_CONFIGMAP_PREFIX, coredb_name),
                d,
            )
            .await?
        }
        None => {
            debug!("No queries specified in CoreDB spec");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_yaml;

    #[test]
    fn query_deserialize_serialize() {
        // query data received as json. map to struct.
        // serialize struct to yaml
        let incoming_data = serde_json::json!(
            {
                "pg_postmaster": {
                  "query": "SELECT pg_postmaster_start_time as start_time_seconds from pg_postmaster_start_time()",
                  "master": true,
                  "metrics": [
                    {
                      "start_time_seconds": {
                        "usage": "GAUGE",
                        "description": "Time at which postmaster started"
                      }
                    }
                  ]
                },
                "extensions": {
                  "query": "select count(*) as num_ext from pg_available_extensions",
                  "master": true,
                  "metrics": [
                    {
                      "num_ext": {
                        "usage": "GAUGE",
                        "description": "Num extensions"
                      }
                    }
                  ]
                }
              }
        );

        let query_config: QueryConfig = serde_json::from_value(incoming_data).expect("failed to deserialize");

        assert!(query_config.queries.contains_key("pg_postmaster"));
        assert!(query_config.queries.contains_key("extensions"));

        let pg_postmaster = query_config.queries.get("pg_postmaster").unwrap();
        assert_eq!(
            pg_postmaster.query,
            "SELECT pg_postmaster_start_time as start_time_seconds from pg_postmaster_start_time()"
        );
        assert!(pg_postmaster.master);
        assert!(pg_postmaster.metrics[0]
            .metrics
            .contains_key("start_time_seconds"));

        let start_time_seconds_metric = pg_postmaster.metrics[0]
            .metrics
            .get("start_time_seconds")
            .unwrap();
        assert_eq!(
            start_time_seconds_metric.description,
            "Time at which postmaster started"
        );

        let extensions = query_config
            .queries
            .get("extensions")
            .expect("extensions not found");
        assert_eq!(
            extensions.query,
            "select count(*) as num_ext from pg_available_extensions"
        );
        assert!(extensions.master);
        assert!(extensions.metrics[0].metrics.contains_key("num_ext"));

        // yaml to yaml

        let yaml = serde_yaml::to_string(&query_config).expect("failed to serialize to yaml");

        let data = r#"extensions:
  query: select count(*) as num_ext from pg_available_extensions
  master: true
  metrics:
  - num_ext:
      usage: GAUGE
      description: Num extensions
pg_postmaster:
  query: SELECT pg_postmaster_start_time as start_time_seconds from pg_postmaster_start_time()
  master: true
  metrics:
  - start_time_seconds:
      usage: GAUGE
      description: Time at which postmaster started
"#;
        // formmatted correctly as yaml (for configmap)
        assert_eq!(yaml, data);
    }
}
