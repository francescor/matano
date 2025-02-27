use arrow2::datatypes::Schema;
use async_once::AsyncOnce;
use aws_config::SdkConfig;
use futures::future::join_all;
use futures::AsyncReadExt;
use serde::{Deserialize, Serialize};
use serde_json::json;
use shared::*;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::{time::Instant, vec};

use aws_lambda_events::event::sqs::SqsEvent;
use lambda_runtime::{run, service_fn, Context, Error as LambdaError, LambdaEvent};

use lazy_static::lazy_static;
use log::{error, info};
use threadpool::ThreadPool;

use anyhow::{anyhow, Result};
use futures::StreamExt;

use arrow2::io::avro::avro_schema::file::Block;
use arrow2::io::avro::avro_schema::read_async::{block_stream, decompress_block, read_metadata};
use arrow2::io::avro::read::deserialize;
use tokio_util::compat::TokioAsyncReadCompatExt;

mod common;
mod matano_alerts;
use common::{load_table_arrow_schema, write_arrow_to_s3_parquet};

use crate::common::struct_wrap_arrow2_for_ffi;

#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

const ALERTS_TABLE_NAME: &str = "matano_alerts";

lazy_static! {
    static ref AWS_CONFIG: AsyncOnce<SdkConfig> =
        AsyncOnce::new(async { aws_config::load_from_env().await });
    static ref S3_CLIENT: AsyncOnce<aws_sdk_s3::Client> =
        AsyncOnce::new(async { aws_sdk_s3::Client::new(AWS_CONFIG.get().await) });
}

lazy_static! {
    static ref TABLE_SCHEMA_MAP: Arc<Mutex<HashMap<String, Schema>>> =
        Arc::new(Mutex::new(HashMap::new()));
}

#[tokio::main]
async fn main() -> Result<(), LambdaError> {
    setup_logging();

    let func = service_fn(my_handler);
    run(func).await?;

    Ok(())
}

pub(crate) async fn my_handler(event: LambdaEvent<SqsEvent>) -> Result<()> {
    let msg_logs = event.payload.records.iter().map(|r| {
        json!({ "message_id": r.message_id, "body": r.body, "message_attributes": r.message_attributes })
    }).collect::<Vec<_>>();
    info!(
        "Received messages: {}",
        serde_json::to_string(&msg_logs).unwrap_or_default()
    );

    let downloads = event
        .payload
        .records
        .iter()
        .flat_map(|record| {
            let body = record.body.as_ref().ok_or("SQS message body is required")?;
            let items = serde_json::from_str::<S3SQSMessage>(&body).map_err(|e| e.to_string());
            items
        })
        .collect::<Vec<_>>();

    if downloads.len() == 0 {
        info!("Empty event, returning...");
        return Ok(());
    }

    let resolved_table_name = downloads
        .first()
        .map(|m| m.resolved_table_name.clone())
        .unwrap();
    info!("Processing for table: {}", resolved_table_name);

    let mut table_schema_map = TABLE_SCHEMA_MAP
        .lock()
        .map_err(|e| anyhow!(e.to_string()))?;
    let table_schema = table_schema_map
        .entry(resolved_table_name.clone())
        .or_insert_with_key(|k| load_table_arrow_schema(k).unwrap());

    info!("Starting {} downloads from S3", downloads.len());

    let s3 = S3_CLIENT.get().await.clone();

    let pool = ThreadPool::new(4);
    let pool_ref = Arc::new(pool);
    let blocks = vec![];
    let blocks_ref = Arc::new(Mutex::new(blocks));

    let alert_blocks = vec![];
    let alert_blocks_ref = Arc::new(Mutex::new(alert_blocks));

    let tasks = downloads
        .into_iter()
        .map(|r| {
            let s3 = &s3;
            let ret = (async move {
                let obj_res = s3
                    .get_object()
                    .bucket(r.bucket)
                    .key(r.key.clone())
                    .send()
                    .await
                    .map_err(|e| {
                        error!("Error downloading {} from S3: {}", r.key, e);
                        e
                    });
                let obj = obj_res.unwrap();

                let stream = obj.body;
                let reader = TokioAsyncReadCompatExt::compat(stream.into_async_read());
                reader
            });
            ret
        })
        .collect::<Vec<_>>();

    let mut result = join_all(tasks).await;

    let work_futures = result.iter_mut().map(|reader| {
        let blocks_ref = blocks_ref.clone();
        let alert_blocks_ref = alert_blocks_ref.clone();
        let pool_ref = pool_ref.clone();
        let resolved_table_name = resolved_table_name.clone();

        async move {
            if resolved_table_name == ALERTS_TABLE_NAME {
                let mut buf = vec![];
                reader.read_to_end(&mut buf).await.unwrap();

                let mut alert_blocks = alert_blocks_ref.lock().unwrap();
                alert_blocks.push(buf);

                return None;
            };

            let metadata = read_metadata(reader).await.unwrap();

            let blocks = block_stream(reader, metadata.marker).await;

            let fut = blocks.for_each_concurrent(1000000, move |block| {
                let mut block = block.unwrap();

                let blocks_ref = blocks_ref.clone();
                let pool = pool_ref.clone();

                // the content here is CPU-bounded. It should run on a dedicated thread pool
                pool.execute(move || {
                    let mut decompressed = Block::new(0, vec![]);

                    decompress_block(&mut block, &mut decompressed, metadata.compression).unwrap();

                    let mut blocks = blocks_ref.lock().unwrap();
                    blocks.push(decompressed);
                    ()
                });
                async {}
            });

            fut.await;
            Some(metadata)
        }
    });

    let results = join_all(work_futures).await;

    let pool = pool_ref.clone();
    pool.join();

    if resolved_table_name == ALERTS_TABLE_NAME {
        info!("Processing alerts...");
        let alert_blocks_ref =
            Arc::try_unwrap(alert_blocks_ref).map_err(|e| anyhow!("fail get rowgroups"))?;
        let alert_blocks = Mutex::into_inner(alert_blocks_ref)?;
        matano_alerts::process_alerts(s3, alert_blocks).await?;
        return Ok(());
    }

    let blocks_ref = Arc::try_unwrap(blocks_ref).map_err(|e| anyhow!("fail get rowgroups"))?;
    let blocks = Mutex::into_inner(blocks_ref)?;

    if blocks.len() == 0 {
        return Ok(());
    }

    let metadata = results.first().unwrap().as_ref().unwrap();
    let block = concat_blocks(blocks);
    let projection = table_schema.fields.iter().map(|_| true).collect::<Vec<_>>();

    // There's an edge case bug here when schema is updated. Using static schema
    // on old data before schema will throw since field length unequal.
    // TODO: Group block's by schema and then deserialize.
    let chunk = deserialize(
        &block,
        &table_schema.fields,
        &metadata.record.fields,
        &projection,
    )?;
    let chunks = vec![chunk];

    let (field, arrays) = struct_wrap_arrow2_for_ffi(&table_schema, chunks);
    // TODO: fix to use correct partition (@shaeq)
    let partition_hour = chrono::offset::Utc::now().format("%Y-%m-%d-%H").to_string();
    write_arrow_to_s3_parquet(s3, resolved_table_name, partition_hour, field, arrays).await?;

    Ok(())
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub(crate) struct S3SQSMessage {
    pub resolved_table_name: String,
    pub bucket: String,
    pub key: String,
}

fn concat_blocks(mut blocks: Vec<Block>) -> Block {
    let mut ret = Block::new(0, vec![]);
    for block in blocks.iter_mut() {
        ret.number_of_rows += block.number_of_rows;
        ret.data.extend(block.data.as_slice());
    }
    ret
}
