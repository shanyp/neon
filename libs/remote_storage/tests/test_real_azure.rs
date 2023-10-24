use std::collections::HashSet;
use std::env;
use std::num::{NonZeroU32, NonZeroUsize};
use std::ops::ControlFlow;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use anyhow::Context;
use camino::Utf8Path;
use once_cell::sync::OnceCell;
use remote_storage::{
    AzureConfig, Download, GenericRemoteStorage, RemotePath, RemoteStorageConfig, RemoteStorageKind,
};
use test_context::{test_context, AsyncTestContext};
use tokio::task::JoinSet;
use tracing::{debug, error, info};

static LOGGING_DONE: OnceCell<()> = OnceCell::new();

const ENABLE_REAL_AZURE_REMOTE_STORAGE_ENV_VAR_NAME: &str = "ENABLE_REAL_AZURE_REMOTE_STORAGE";

const BASE_PREFIX: &str = "test";

/// Tests that the Azure client can list all prefixes, even if the response comes paginated and requires multiple HTTP queries.
/// Uses real Azure and requires [`ENABLE_REAL_AZURE_REMOTE_STORAGE_ENV_VAR_NAME`] and related Azure cred env vars specified.
/// See the client creation in [`create_azure_client`] for details on the required env vars.
/// If real Azure tests are disabled, the test passes, skipping any real test run: currently, there's no way to mark the test ignored in runtime with the
/// deafult test framework, see https://github.com/rust-lang/rust/issues/68007 for details.
///
/// First, the test creates a set of Azure blobs with keys `/${random_prefix_part}/${base_prefix_str}/sub_prefix_${i}/blob_${i}` in [`upload_azure_data`]
/// where
/// * `random_prefix_part` is set for the entire Azure client during the Azure client creation in [`create_azure_client`], to avoid multiple test runs interference
/// * `base_prefix_str` is a common prefix to use in the client requests: we would want to ensure that the client is able to list nested prefixes inside the bucket
///
/// Then, verifies that the client does return correct prefixes when queried:
/// * with no prefix, it lists everything after its `${random_prefix_part}/` — that should be `${base_prefix_str}` value only
/// * with `${base_prefix_str}/` prefix, it lists every `sub_prefix_${i}`
///
/// With the real Azure enabled and `#[cfg(test)]` Rust configuration used, the Azure client test adds a `max-keys` param to limit the response keys.
/// This way, we are able to test the pagination implicitly, by ensuring all results are returned from the remote storage and avoid uploading too many blobs to Azure.
///
/// Lastly, the test attempts to clean up and remove all uploaded Azure files.
/// If any errors appear during the clean up, they get logged, but the test is not failed or stopped until clean up is finished.
#[test_context(MaybeEnabledAzureWithTestBlobs)]
#[tokio::test]
async fn azure_pagination_should_work(
    ctx: &mut MaybeEnabledAzureWithTestBlobs,
) -> anyhow::Result<()> {
    let ctx = match ctx {
        MaybeEnabledAzureWithTestBlobs::Enabled(ctx) => ctx,
        MaybeEnabledAzureWithTestBlobs::Disabled => return Ok(()),
        MaybeEnabledAzureWithTestBlobs::UploadsFailed(e, _) => {
            anyhow::bail!("Azure init failed: {e:?}")
        }
    };

    let test_client = Arc::clone(&ctx.enabled.client);
    let expected_remote_prefixes = ctx.remote_prefixes.clone();

    let base_prefix = RemotePath::new(Utf8Path::new(ctx.enabled.base_prefix))
        .context("common_prefix construction")?;
    let root_remote_prefixes = test_client
        .list_prefixes(None)
        .await
        .context("client list root prefixes failure")?
        .into_iter()
        .collect::<HashSet<_>>();
    assert_eq!(
        root_remote_prefixes, HashSet::from([base_prefix.clone()]),
        "remote storage root prefixes list mismatches with the uploads. Returned prefixes: {root_remote_prefixes:?}"
    );

    let nested_remote_prefixes = test_client
        .list_prefixes(Some(&base_prefix))
        .await
        .context("client list nested prefixes failure")?
        .into_iter()
        .collect::<HashSet<_>>();
    let remote_only_prefixes = nested_remote_prefixes
        .difference(&expected_remote_prefixes)
        .collect::<HashSet<_>>();
    let missing_uploaded_prefixes = expected_remote_prefixes
        .difference(&nested_remote_prefixes)
        .collect::<HashSet<_>>();
    assert_eq!(
        remote_only_prefixes.len() + missing_uploaded_prefixes.len(), 0,
        "remote storage nested prefixes list mismatches with the uploads. Remote only prefixes: {remote_only_prefixes:?}, missing uploaded prefixes: {missing_uploaded_prefixes:?}",
    );

    Ok(())
}

/// Tests that Azure client can list all files in a folder, even if the response comes paginated and requirees multiple Azure queries.
/// Uses real Azure and requires [`ENABLE_REAL_AZURE_REMOTE_STORAGE_ENV_VAR_NAME`] and related Azure cred env vars specified. Test will skip real code and pass if env vars not set.
/// See `Azure_pagination_should_work` for more information.
///
/// First, create a set of Azure objects with keys `random_prefix/folder{j}/blob_{i}.txt` in [`upload_azure_data`]
/// Then performs the following queries:
///    1. `list_files(None)`. This should return all files `random_prefix/folder{j}/blob_{i}.txt`
///    2. `list_files("folder1")`.  This  should return all files `random_prefix/folder1/blob_{i}.txt`
#[test_context(MaybeEnabledAzureWithSimpleTestBlobs)]
#[tokio::test]
async fn azure_list_files_works(
    ctx: &mut MaybeEnabledAzureWithSimpleTestBlobs,
) -> anyhow::Result<()> {
    let ctx = match ctx {
        MaybeEnabledAzureWithSimpleTestBlobs::Enabled(ctx) => ctx,
        MaybeEnabledAzureWithSimpleTestBlobs::Disabled => return Ok(()),
        MaybeEnabledAzureWithSimpleTestBlobs::UploadsFailed(e, _) => {
            anyhow::bail!("Azure init failed: {e:?}")
        }
    };
    let test_client = Arc::clone(&ctx.enabled.client);
    let base_prefix =
        RemotePath::new(Utf8Path::new("folder1")).context("common_prefix construction")?;
    let root_files = test_client
        .list_files(None)
        .await
        .context("client list root files failure")?
        .into_iter()
        .collect::<HashSet<_>>();
    assert_eq!(
        root_files,
        ctx.remote_blobs.clone(),
        "remote storage list_files on root mismatches with the uploads."
    );
    let nested_remote_files = test_client
        .list_files(Some(&base_prefix))
        .await
        .context("client list nested files failure")?
        .into_iter()
        .collect::<HashSet<_>>();
    let trim_remote_blobs: HashSet<_> = ctx
        .remote_blobs
        .iter()
        .map(|x| x.get_path())
        .filter(|x| x.starts_with("folder1"))
        .map(|x| RemotePath::new(x).expect("must be valid path"))
        .collect();
    assert_eq!(
        nested_remote_files, trim_remote_blobs,
        "remote storage list_files on subdirrectory mismatches with the uploads."
    );
    Ok(())
}

#[test_context(MaybeEnabledAzure)]
#[tokio::test]
async fn azure_delete_non_exising_works(ctx: &mut MaybeEnabledAzure) -> anyhow::Result<()> {
    let ctx = match ctx {
        MaybeEnabledAzure::Enabled(ctx) => ctx,
        MaybeEnabledAzure::Disabled => return Ok(()),
    };

    let path = RemotePath::new(Utf8Path::new(
        format!("{}/for_sure_there_is_nothing_there_really", ctx.base_prefix).as_str(),
    ))
    .with_context(|| "RemotePath conversion")?;

    ctx.client.delete(&path).await.expect("should succeed");

    Ok(())
}

#[test_context(MaybeEnabledAzure)]
#[tokio::test]
async fn azure_delete_objects_works(ctx: &mut MaybeEnabledAzure) -> anyhow::Result<()> {
    let ctx = match ctx {
        MaybeEnabledAzure::Enabled(ctx) => ctx,
        MaybeEnabledAzure::Disabled => return Ok(()),
    };

    let path1 = RemotePath::new(Utf8Path::new(format!("{}/path1", ctx.base_prefix).as_str()))
        .with_context(|| "RemotePath conversion")?;

    let path2 = RemotePath::new(Utf8Path::new(format!("{}/path2", ctx.base_prefix).as_str()))
        .with_context(|| "RemotePath conversion")?;

    let path3 = RemotePath::new(Utf8Path::new(format!("{}/path3", ctx.base_prefix).as_str()))
        .with_context(|| "RemotePath conversion")?;

    let data1 = "remote blob data1".as_bytes();
    let data1_len = data1.len();
    let data2 = "remote blob data2".as_bytes();
    let data2_len = data2.len();
    let data3 = "remote blob data3".as_bytes();
    let data3_len = data3.len();
    ctx.client
        .upload(std::io::Cursor::new(data1), data1_len, &path1, None)
        .await?;

    ctx.client
        .upload(std::io::Cursor::new(data2), data2_len, &path2, None)
        .await?;

    ctx.client
        .upload(std::io::Cursor::new(data3), data3_len, &path3, None)
        .await?;

    ctx.client.delete_objects(&[path1, path2]).await?;

    let prefixes = ctx.client.list_prefixes(None).await?;

    assert_eq!(prefixes.len(), 1);

    ctx.client.delete_objects(&[path3]).await?;

    Ok(())
}

#[test_context(MaybeEnabledAzure)]
#[tokio::test]
async fn azure_upload_download_works(ctx: &mut MaybeEnabledAzure) -> anyhow::Result<()> {
    let MaybeEnabledAzure::Enabled(ctx) = ctx else {
        return Ok(());
    };

    let path = RemotePath::new(Utf8Path::new(format!("{}/file", ctx.base_prefix).as_str()))
        .with_context(|| "RemotePath conversion")?;

    let data = "remote blob data here".as_bytes();
    let data_len = data.len() as u64;

    ctx.client
        .upload(std::io::Cursor::new(data), data.len(), &path, None)
        .await?;

    async fn download_and_compare(mut dl: Download) -> anyhow::Result<Vec<u8>> {
        let mut buf = Vec::new();
        tokio::io::copy(&mut dl.download_stream, &mut buf).await?;
        Ok(buf)
    }
    // Normal download request
    let dl = ctx.client.download(&path).await?;
    let buf = download_and_compare(dl).await?;
    assert_eq!(buf, data);

    // Full range (end specified)
    let dl = ctx
        .client
        .download_byte_range(&path, 0, Some(data_len))
        .await?;
    let buf = download_and_compare(dl).await?;
    assert_eq!(buf, data);

    // partial range (end specified)
    let dl = ctx.client.download_byte_range(&path, 4, Some(10)).await?;
    let buf = download_and_compare(dl).await?;
    assert_eq!(buf, data[4..10]);

    // partial range (end beyond real end)
    let dl = ctx
        .client
        .download_byte_range(&path, 8, Some(data_len * 100))
        .await?;
    let buf = download_and_compare(dl).await?;
    assert_eq!(buf, data[8..]);

    // Partial range (end unspecified)
    let dl = ctx.client.download_byte_range(&path, 4, None).await?;
    let buf = download_and_compare(dl).await?;
    assert_eq!(buf, data[4..]);

    // Full range (end unspecified)
    let dl = ctx.client.download_byte_range(&path, 0, None).await?;
    let buf = download_and_compare(dl).await?;
    assert_eq!(buf, data);

    debug!("Cleanup: deleting file at path {path:?}");
    ctx.client
        .delete(&path)
        .await
        .with_context(|| format!("{path:?} removal"))?;

    Ok(())
}

fn ensure_logging_ready() {
    LOGGING_DONE.get_or_init(|| {
        utils::logging::init(
            utils::logging::LogFormat::Test,
            utils::logging::TracingErrorLayerEnablement::Disabled,
        )
        .expect("logging init failed");
    });
}

struct EnabledAzure {
    client: Arc<GenericRemoteStorage>,
    base_prefix: &'static str,
}

impl EnabledAzure {
    async fn setup(max_keys_in_list_response: Option<i32>) -> Self {
        let client = create_azure_client(max_keys_in_list_response)
            .context("Azure client creation")
            .expect("Azure client creation failed");

        EnabledAzure {
            client,
            base_prefix: BASE_PREFIX,
        }
    }
}

enum MaybeEnabledAzure {
    Enabled(EnabledAzure),
    Disabled,
}

#[async_trait::async_trait]
impl AsyncTestContext for MaybeEnabledAzure {
    async fn setup() -> Self {
        ensure_logging_ready();

        if env::var(ENABLE_REAL_AZURE_REMOTE_STORAGE_ENV_VAR_NAME).is_err() {
            info!(
                "`{}` env variable is not set, skipping the test",
                ENABLE_REAL_AZURE_REMOTE_STORAGE_ENV_VAR_NAME
            );
            return Self::Disabled;
        }

        Self::Enabled(EnabledAzure::setup(None).await)
    }
}

enum MaybeEnabledAzureWithTestBlobs {
    Enabled(AzureWithTestBlobs),
    Disabled,
    UploadsFailed(anyhow::Error, AzureWithTestBlobs),
}

struct AzureWithTestBlobs {
    enabled: EnabledAzure,
    remote_prefixes: HashSet<RemotePath>,
    remote_blobs: HashSet<RemotePath>,
}

#[async_trait::async_trait]
impl AsyncTestContext for MaybeEnabledAzureWithTestBlobs {
    async fn setup() -> Self {
        ensure_logging_ready();
        if env::var(ENABLE_REAL_AZURE_REMOTE_STORAGE_ENV_VAR_NAME).is_err() {
            info!(
                "`{}` env variable is not set, skipping the test",
                ENABLE_REAL_AZURE_REMOTE_STORAGE_ENV_VAR_NAME
            );
            return Self::Disabled;
        }

        let max_keys_in_list_response = 10;
        let upload_tasks_count = 1 + (2 * usize::try_from(max_keys_in_list_response).unwrap());

        let enabled = EnabledAzure::setup(Some(max_keys_in_list_response)).await;

        match upload_azure_data(&enabled.client, enabled.base_prefix, upload_tasks_count).await {
            ControlFlow::Continue(uploads) => {
                info!("Remote objects created successfully");

                Self::Enabled(AzureWithTestBlobs {
                    enabled,
                    remote_prefixes: uploads.prefixes,
                    remote_blobs: uploads.blobs,
                })
            }
            ControlFlow::Break(uploads) => Self::UploadsFailed(
                anyhow::anyhow!("One or multiple blobs failed to upload to Azure"),
                AzureWithTestBlobs {
                    enabled,
                    remote_prefixes: uploads.prefixes,
                    remote_blobs: uploads.blobs,
                },
            ),
        }
    }

    async fn teardown(self) {
        match self {
            Self::Disabled => {}
            Self::Enabled(ctx) | Self::UploadsFailed(_, ctx) => {
                cleanup(&ctx.enabled.client, ctx.remote_blobs).await;
            }
        }
    }
}

// NOTE: the setups for the list_prefixes test and the list_files test are very similar
// However, they are not idential. The list_prefixes function is concerned with listing prefixes,
// whereas the list_files function is concerned with listing files.
// See `RemoteStorage::list_files` documentation for more details
enum MaybeEnabledAzureWithSimpleTestBlobs {
    Enabled(AzureWithSimpleTestBlobs),
    Disabled,
    UploadsFailed(anyhow::Error, AzureWithSimpleTestBlobs),
}
struct AzureWithSimpleTestBlobs {
    enabled: EnabledAzure,
    remote_blobs: HashSet<RemotePath>,
}

#[async_trait::async_trait]
impl AsyncTestContext for MaybeEnabledAzureWithSimpleTestBlobs {
    async fn setup() -> Self {
        ensure_logging_ready();
        if env::var(ENABLE_REAL_AZURE_REMOTE_STORAGE_ENV_VAR_NAME).is_err() {
            info!(
                "`{}` env variable is not set, skipping the test",
                ENABLE_REAL_AZURE_REMOTE_STORAGE_ENV_VAR_NAME
            );
            return Self::Disabled;
        }

        let max_keys_in_list_response = 10;
        let upload_tasks_count = 1 + (2 * usize::try_from(max_keys_in_list_response).unwrap());

        let enabled = EnabledAzure::setup(Some(max_keys_in_list_response)).await;

        match upload_simple_azure_data(&enabled.client, upload_tasks_count).await {
            ControlFlow::Continue(uploads) => {
                info!("Remote objects created successfully");

                Self::Enabled(AzureWithSimpleTestBlobs {
                    enabled,
                    remote_blobs: uploads,
                })
            }
            ControlFlow::Break(uploads) => Self::UploadsFailed(
                anyhow::anyhow!("One or multiple blobs failed to upload to Azure"),
                AzureWithSimpleTestBlobs {
                    enabled,
                    remote_blobs: uploads,
                },
            ),
        }
    }

    async fn teardown(self) {
        match self {
            Self::Disabled => {}
            Self::Enabled(ctx) | Self::UploadsFailed(_, ctx) => {
                cleanup(&ctx.enabled.client, ctx.remote_blobs).await;
            }
        }
    }
}

fn create_azure_client(
    max_keys_per_list_response: Option<i32>,
) -> anyhow::Result<Arc<GenericRemoteStorage>> {
    use rand::Rng;

    let remote_storage_azure_container = env::var("REMOTE_STORAGE_AZURE_CONTAINER").context(
        "`REMOTE_STORAGE_AZURE_CONTAINER` env var is not set, but real Azure tests are enabled",
    )?;
    let remote_storage_azure_region = env::var("REMOTE_STORAGE_AZURE_REGION").context(
        "`REMOTE_STORAGE_AZURE_REGION` env var is not set, but real Azure tests are enabled",
    )?;

    // due to how time works, we've had test runners use the same nanos as bucket prefixes.
    // millis is just a debugging aid for easier finding the prefix later.
    let millis = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("random Azure test prefix part calculation")?
        .as_millis();

    // because nanos can be the same for two threads so can millis, add randomness
    let random = rand::thread_rng().gen::<u32>();

    let remote_storage_config = RemoteStorageConfig {
        max_concurrent_syncs: NonZeroUsize::new(100).unwrap(),
        max_sync_errors: NonZeroU32::new(5).unwrap(),
        storage: RemoteStorageKind::AzureContainer(AzureConfig {
            container_name: remote_storage_azure_container,
            container_region: remote_storage_azure_region,
            prefix_in_container: Some(format!("test_{millis}_{random:08x}/")),
            concurrency_limit: NonZeroUsize::new(100).unwrap(),
            max_keys_per_list_response,
        }),
    };
    Ok(Arc::new(
        GenericRemoteStorage::from_config(&remote_storage_config).context("remote storage init")?,
    ))
}

struct Uploads {
    prefixes: HashSet<RemotePath>,
    blobs: HashSet<RemotePath>,
}

async fn upload_azure_data(
    client: &Arc<GenericRemoteStorage>,
    base_prefix_str: &'static str,
    upload_tasks_count: usize,
) -> ControlFlow<Uploads, Uploads> {
    info!("Creating {upload_tasks_count} Azure files");
    let mut upload_tasks = JoinSet::new();
    for i in 1..upload_tasks_count + 1 {
        let task_client = Arc::clone(client);
        upload_tasks.spawn(async move {
            let prefix = format!("{base_prefix_str}/sub_prefix_{i}/");
            let blob_prefix = RemotePath::new(Utf8Path::new(&prefix))
                .with_context(|| format!("{prefix:?} to RemotePath conversion"))?;
            let blob_path = blob_prefix.join(Utf8Path::new(&format!("blob_{i}")));
            debug!("Creating remote item {i} at path {blob_path:?}");

            let data = format!("remote blob data {i}").into_bytes();
            let data_len = data.len();
            task_client
                .upload(std::io::Cursor::new(data), data_len, &blob_path, None)
                .await?;

            Ok::<_, anyhow::Error>((blob_prefix, blob_path))
        });
    }

    let mut upload_tasks_failed = false;
    let mut uploaded_prefixes = HashSet::with_capacity(upload_tasks_count);
    let mut uploaded_blobs = HashSet::with_capacity(upload_tasks_count);
    while let Some(task_run_result) = upload_tasks.join_next().await {
        match task_run_result
            .context("task join failed")
            .and_then(|task_result| task_result.context("upload task failed"))
        {
            Ok((upload_prefix, upload_path)) => {
                uploaded_prefixes.insert(upload_prefix);
                uploaded_blobs.insert(upload_path);
            }
            Err(e) => {
                error!("Upload task failed: {e:?}");
                upload_tasks_failed = true;
            }
        }
    }

    let uploads = Uploads {
        prefixes: uploaded_prefixes,
        blobs: uploaded_blobs,
    };
    if upload_tasks_failed {
        ControlFlow::Break(uploads)
    } else {
        ControlFlow::Continue(uploads)
    }
}

async fn cleanup(client: &Arc<GenericRemoteStorage>, objects_to_delete: HashSet<RemotePath>) {
    info!(
        "Removing {} objects from the remote storage during cleanup",
        objects_to_delete.len()
    );
    let mut delete_tasks = JoinSet::new();
    for object_to_delete in objects_to_delete {
        let task_client = Arc::clone(client);
        delete_tasks.spawn(async move {
            debug!("Deleting remote item at path {object_to_delete:?}");
            task_client
                .delete(&object_to_delete)
                .await
                .with_context(|| format!("{object_to_delete:?} removal"))
        });
    }

    while let Some(task_run_result) = delete_tasks.join_next().await {
        match task_run_result {
            Ok(task_result) => match task_result {
                Ok(()) => {}
                Err(e) => error!("Delete task failed: {e:?}"),
            },
            Err(join_err) => error!("Delete task did not finish correctly: {join_err}"),
        }
    }
}

// Uploads files `folder{j}/blob{i}.txt`. See test description for more details.
async fn upload_simple_azure_data(
    client: &Arc<GenericRemoteStorage>,
    upload_tasks_count: usize,
) -> ControlFlow<HashSet<RemotePath>, HashSet<RemotePath>> {
    info!("Creating {upload_tasks_count} Azure files");
    let mut upload_tasks = JoinSet::new();
    for i in 1..upload_tasks_count + 1 {
        let task_client = Arc::clone(client);
        upload_tasks.spawn(async move {
            let blob_path = PathBuf::from(format!("folder{}/blob_{}.txt", i / 7, i));
            let blob_path = RemotePath::new(
                Utf8Path::from_path(blob_path.as_path()).expect("must be valid blob path"),
            )
            .with_context(|| format!("{blob_path:?} to RemotePath conversion"))?;
            debug!("Creating remote item {i} at path {blob_path:?}");

            let data = format!("remote blob data {i}").into_bytes();
            let data_len = data.len();
            task_client
                .upload(std::io::Cursor::new(data), data_len, &blob_path, None)
                .await?;

            Ok::<_, anyhow::Error>(blob_path)
        });
    }

    let mut upload_tasks_failed = false;
    let mut uploaded_blobs = HashSet::with_capacity(upload_tasks_count);
    while let Some(task_run_result) = upload_tasks.join_next().await {
        match task_run_result
            .context("task join failed")
            .and_then(|task_result| task_result.context("upload task failed"))
        {
            Ok(upload_path) => {
                uploaded_blobs.insert(upload_path);
            }
            Err(e) => {
                error!("Upload task failed: {e:?}");
                upload_tasks_failed = true;
            }
        }
    }

    if upload_tasks_failed {
        ControlFlow::Break(uploaded_blobs)
    } else {
        ControlFlow::Continue(uploaded_blobs)
    }
}
