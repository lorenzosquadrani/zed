use std::{io::Read as _, path::Path, sync::Arc};

use anyhow::{Context as _, Result, anyhow, ensure};
use fs::Fs;
use gpui::{App, AppContext as _, Task};
use rpc::proto::{self, REMOTE_SERVER_PROJECT_ID};

use crate::{Project, ProjectPath};

pub const MAX_BINARY_FILE_SIZE: u64 = 128 * 1024 * 1024;

impl Project {
    /// Read a bounded binary file from the project's filesystem, including SSH remotes.
    pub fn read_binary_file(
        &self,
        path: &ProjectPath,
        max_bytes: u64,
        cx: &App,
    ) -> Task<Result<Vec<u8>>> {
        if max_bytes > MAX_BINARY_FILE_SIZE {
            return Task::ready(Err(anyhow!("Binary preview supports files up to 128 MiB")));
        }
        if self.is_local() {
            let Some(abs_path) = self.absolute_path(path, cx) else {
                return Task::ready(Err(anyhow!("File worktree is no longer available")));
            };
            let fs = self.fs().clone();
            return cx.background_spawn(async move {
                read_bounded_file(&fs, &abs_path, max_bytes).await
            });
        }
        let Some(remote_client) = self.remote_client() else {
            return Task::ready(Err(anyhow!(
                "Binary preview is not supported in collaborative projects"
            )));
        };
        if self.is_disconnected(cx) {
            return Task::ready(Err(anyhow!("Remote project is disconnected")));
        }
        let request = remote_client
            .read(cx)
            .proto_client()
            .request(proto::ReadProjectFile {
                project_id: REMOTE_SERVER_PROJECT_ID,
                worktree_id: path.worktree_id.to_proto(),
                path: path.path.as_unix_str().into(),
                max_bytes,
            });
        cx.background_spawn(async move {
            let response = request
                .await
                .context("Cannot read file from remote server")?;
            ensure!(
                response.data.len() as u64 <= max_bytes,
                "Remote file exceeds preview size limit"
            );
            Ok(response.data)
        })
    }
}

pub async fn read_bounded_file(fs: &Arc<dyn Fs>, path: &Path, max_bytes: u64) -> Result<Vec<u8>> {
    ensure!(
        max_bytes <= MAX_BINARY_FILE_SIZE,
        "Binary preview supports files up to 128 MiB"
    );
    let metadata = fs.metadata(path).await?.context("File was deleted")?;
    ensure!(!metadata.is_dir && !metadata.is_fifo, "Not a regular file");
    ensure!(
        metadata.len <= max_bytes,
        "File exceeds preview size limit ({max_bytes} bytes)"
    );
    // The file can grow after the metadata check, so also bound the actual read.
    let mut bytes = Vec::new();
    fs.open_sync(path)
        .await?
        .take(max_bytes + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= max_bytes,
        "File exceeds preview size limit ({max_bytes} bytes)"
    );
    Ok(bytes)
}
