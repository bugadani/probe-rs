use std::{io::Write as _, path::PathBuf};

use crate::cmd::remote::functions::{Context, EmitterFn, RemoteFunctions};
use anyhow::Context as _;
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

#[derive(Serialize, Deserialize)]
pub(in crate::cmd::remote) struct UploadFile {
    pub data: Vec<u8>,
}

impl super::RemoteFunction for UploadFile {
    type Message = super::NoMessage;
    type Result = PathBuf;

    async fn run(self, mut ctx: Context<'_, impl EmitterFn>) -> anyhow::Result<PathBuf> {
        // TODO: avoid temp files?
        let mut file = NamedTempFile::new().context("Failed to write temporary file")?;

        file.as_file_mut()
            .write_all(&self.data)
            .context("Failed to write temporary file")?;

        let path = file.path().to_path_buf();
        tracing::info!("Saved temporary file to {}", path.display());
        _ = ctx.store_object(file);

        Ok(path)
    }
}

impl From<UploadFile> for RemoteFunctions {
    fn from(func: UploadFile) -> Self {
        RemoteFunctions::UploadFile(func)
    }
}
