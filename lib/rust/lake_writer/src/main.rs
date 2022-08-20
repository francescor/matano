use async_compat::CompatExt;
use aws_sdk_s3::types::ByteStream;
use concurrent_queue::ConcurrentQueue;
use futures::future::join_all;
use serde::{Deserialize, Serialize};
use shared::*;
use tokio::runtime::Handle;
use tokio_util::codec::{FramedRead, LinesCodec};
use tokio_util::io::{ReaderStream, StreamReader};
use uuid::Uuid;

use bytes::Bytes;
use std::fs::File;
use std::rc::Rc;
use std::sync::{Arc, MutexGuard};
use std::sync::{Condvar, Mutex};
use std::thread::{self, JoinHandle, ScopedJoinHandle};
use std::{time::Instant, vec};

use aws_lambda_events::event::sqs::SqsEvent;
use lambda_runtime::{handler_fn, Context as LambdaContext, Error as LambdaError};
use log::{debug, error, info};
use threadpool::ThreadPool;

use std::io::Write;

use std::io::{BufRead, BufReader, Seek};
use std::io::{Cursor, Read};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt};

use lazy_static::{__Deref, lazy_static};

use anyhow::{anyhow, Error, Result};

use aws_sdk_s3::Region;
use futures::{stream, Future, TryFutureExt, TryStreamExt};
// use tokio_stream::StreamExt;
use futures::StreamExt;

use tokio_stream::{wrappers::LinesStream, StreamMap};

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use arrow2::io::avro::avro_schema::file::{Block, CompressedBlock, FileMetadata};
use arrow2::io::avro::avro_schema::read_async::{block_stream, decompress_block, read_metadata};
use arrow2::io::avro::read::{deserialize, infer_schema};
use arrow2::io::parquet::read as parquet_read;
use arrow2::{
    chunk::Chunk,
    datatypes::{Field, Schema},
    error::Error as ArrowError,
    error::Result as ArrowResult,
    io::parquet::write::{
        transverse, CompressionOptions, Encoding, FileWriter, RowGroupIterator, Version,
        WriteOptions,
    },
};
use futures::pin_mut;
use tokio_util::compat::TokioAsyncReadCompatExt;

// const ECS_PARQUET: &[u8] = include_bytes!("../../../../data/ecs_parquet_metadata.parquet");

// lazy_static! {
//     static ref ECS_SCHEMA: Schema = {
//         let rr1 = SerializedFileReader::new(Bytes::from(ECS_PARQUET)).unwrap();
//         let rr1_ref = Arc::new(rr1);
//         let mut rr2 = ParquetFileArrowReader::new(rr1_ref);
//         rr2.get_schema().unwrap()
//     };
// }

#[tokio::main]
async fn main() -> Result<(), LambdaError> {
    setup_logging();
    let start = Instant::now();

    // let ev11: SqsEvent = serde_json::from_reader(File::open(
    //     "/home/samrose/workplace/matano/lib/rust/garbage2.json",
    // )?)?;

    // my_handler(ev11).await.unwrap();

    let func = handler_fn(my_handler);
    lambda_runtime::run(func).await?;

    info!("Call lambda took {:.2?}", start.elapsed());

    Ok(())
}

// fn getit<T>(v: Arc<Mutex<T>>) -> MutexGuard<'static, T> {
//     let ret = v.clone();
//     let ret = v.lock().unwrap();
//     ret
// }

#[derive(Debug, Clone)]
pub(crate) struct MyConfig {
    schema: Schema,
    metadata: Arc<FileMetadata>,
}

impl MyConfig {
    fn new(schema: Schema, metadata: FileMetadata) -> MyConfig {
        MyConfig {
            schema,
            metadata: Arc::new(metadata),
        }
    }
}

pub(crate) async fn my_handler(event: SqsEvent, _ctx: LambdaContext) -> Result<()> {
    info!("Request: {:?}", event);

    let downloads = event
        .records
        .iter()
        .flat_map(|record| {
            let body = record.body.as_ref().ok_or("SQS message body is required")?;
            let items = serde_json::from_str::<S3SQSMessage>(&body).map_err(|e| e.to_string());
            items
        })
        .collect::<Vec<_>>();

    if downloads.len() == 0 {
        println!("Empty event, returning...");
        return Ok(());
    }

    info!("Starting {} downloads from S3", downloads.len());

    let start = Instant::now();

    let config = aws_config::load_from_env().await;
    let s3 = aws_sdk_s3::Client::new(&config);

    println!("GOT HERE???");

    let start = Instant::now();

    let options = WriteOptions {
        write_statistics: true,
        compression: CompressionOptions::Uncompressed,
        version: Version::V2,
    };

    let pool = ThreadPool::new(4);
    let pool_ref = Arc::new(pool);
    let mut all_row_groups: Vec<
        RowGroupIterator<
            Box<dyn arrow2::array::Array>,
            std::vec::IntoIter<Result<Chunk<Box<dyn arrow2::array::Array>>, ArrowError>>,
        >,
    > = vec![];
    let mut all_row_groups_ref = Arc::new(Mutex::new(all_row_groups));

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
                println!("Got object...");

                let stream = obj.body;
                let reader = TokioAsyncReadCompatExt::compat(stream.into_async_read());
                reader
            });
            ret
        })
        .collect::<Vec<_>>();

    let mut result = join_all(tasks).await;

    let mut schema_holder: Option<Schema> = None;
    let mut schema_holder_ref = Arc::new(Mutex::new(schema_holder));

    let work_futures = result.iter_mut().map(|mut reader| {
        let all_row_groups_ref = all_row_groups_ref.clone();
        let pool_ref = pool_ref.clone();
        // let schema_holder_ref = schema_holder_ref.clone();
        async move {
            // let mut schema_holder = schema_holder_ref.lock().unwrap();
            let metadata = read_metadata(reader).await.unwrap();
            let schema = infer_schema(&metadata.record).unwrap();
            let schema_copy = schema.clone();
            // let schema = match &mut *schema_holder {
            //     Some(x) => x.clone(),
            //     None => {
            //         let ret = infer_schema(&metadata.record).unwrap();
            //         *schema_holder = Some(ret.clone());
            //         ret
            //     },
            // };

            // // TODO move out
            let projection = Arc::new(schema.fields.iter().map(|_| true).collect::<Vec<_>>());
            let encodings: Vec<Vec<Encoding>> = schema
                .clone()
                .fields
                .iter()
                .map(|f| transverse(&f.data_type, |_| Encoding::Plain))
                .collect();

            let blocks = block_stream(reader, metadata.marker).await;

            let fut = blocks.for_each_concurrent(1000000, move |block| {
                println!("Getting block....");
                let mut block = block.unwrap();
                println!("GOT block");
                let schema = schema.clone();
                let metadata = metadata.clone();
                let projection = projection.clone();
                let encodings = encodings.clone();
                dbg!(block.number_of_rows);
                let all_row_groups_ref = all_row_groups_ref.clone();
                let pool = pool_ref.clone();

                async move {
                    // the content here is CPU-bounded. It should run on a dedicated thread pool
                    pool.execute(move || {
                        let st1 = Instant::now();
                        let mut decompressed = Block::new(0, vec![]);

                        decompress_block(&mut block, &mut decompressed, metadata.compression)
                            .unwrap();

                        let chunk = deserialize(
                            &decompressed,
                            &schema.fields,
                            &metadata.record.fields,
                            &projection,
                        )
                        .unwrap();
                        let iter = vec![Ok(chunk)];
                        let row_groups = RowGroupIterator::try_new(
                            iter.into_iter(),
                            &schema,
                            options,
                            encodings,
                        )
                        .unwrap();
                        let mut all_row_groups = all_row_groups_ref.lock().unwrap();
                        all_row_groups.push(row_groups);
                        println!(
                            "$$$$$$$$$$$$$$$$$$$$$$$$$$$$  THREAD Call took {:.2?}",
                            st1.elapsed()
                        );
                        ()
                    });
                }
            });

            fut.await;
            schema_copy.clone()
        }
    });

    let res = join_all(work_futures).await;

    let pool = pool_ref.clone();
    pool.join();

    let mut all_row_groups_ref =
        Arc::try_unwrap(all_row_groups_ref).map_err(|e| anyhow!("fail get rowgroups"))?;
    let mut all_row_groups = Mutex::into_inner(all_row_groups_ref)?;
    dbg!(all_row_groups.len());

    let schema = res.first().unwrap().clone();

    println!("Writing...");
    let mut buf = vec![];
    let mut writer = FileWriter::try_new(buf, schema.clone(), options)?;

    for groups in all_row_groups {
        for group in groups {
            writer.write(group?).unwrap();
        }
    }

    let filesize = writer.end(None)?;
    println!("Parquet file size: {}", filesize);

    let bytestream = ByteStream::from(writer.into_inner());

    let bucket = std::env::var("OUT_BUCKET_NAME")?;
    let key_prefix = std::env::var("OUT_KEY_PREFIX")?;
    let key = format!("{}/{}.parquet", key_prefix, Uuid::new_v4());
    println!("Writing to: {}", key);

    println!("Starting upload...");
    let ws1 = Instant::now();
    let _upload_res = &s3
        .put_object()
        .bucket(bucket)
        .key(key)
        .body(bytestream)
        .send()
        .await?;
    println!("Upload took: {:.2?}", ws1.elapsed());

    println!("----------------  Call took {:.2?}", start.elapsed());

    Ok(())
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub(crate) struct S3SQSMessage {
    pub bucket: String,
    pub key: String,
}