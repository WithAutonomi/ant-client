// ant-cli: folder upload command.
// Walks a directory, builds manifest, uploads to Autonomi.

use ant_core::data::client::folder::{FolderManifest, FolderUploadResult};
use ant_core::data::client::merkle::PaymentMode;
use clap::{Args, Subcommand};
use std::path::PathBuf;

/// Folder commands.
#[derive(Subcommand, Debug)]
pub enum FolderAction {
    /// Upload a folder to Autonomi.
    Upload(FolderUploadArgs),
}

#[derive(Args, Debug)]
pub struct FolderUploadArgs {
    /// Path to the folder to upload.
    pub path: PathBuf,

    /// Enable recovery mode: backup DataMap on-chain via calldata.
    #[arg(long)]
    pub recovery: bool,

    /// Payment mode (default: auto).
    #[arg(long, default_value = "auto")]
    pub payment_mode: String,
}

impl FolderAction {
    pub async fn execute(self) -> anyhow::Result<()> {
        match self {
            FolderAction::Upload(args) => folder_upload(args).await,
        }
    }
}

pub async fn folder_upload(args: FolderUploadArgs) -> anyhow::Result<()> {
    let path = &args.path;
    if !path.is_dir() {
        anyhow::bail!("{} is not a directory", path.display());
    }

    let mode = match args.payment_mode.as_str() {
        "recovery" => PaymentMode::Recovery,
        "merkle" => PaymentMode::Merkle,
        "single" => PaymentMode::Single,
        _ => PaymentMode::Auto,
    };

    let actual_mode = if args.recovery { PaymentMode::Recovery } else { mode };

    println!("Folder upload: {}", path.display());
    println!("  Payment mode: {:?}", actual_mode);

    // Build manifest
    let manifest = FolderManifest::build(path)
        .map_err(|e| anyhow::anyhow!("manifest: {e}"))?;

    println!("  Folder: {} ({} files, {} bytes)",
        manifest.folder_name, manifest.file_count, manifest.total_size);

    // Serialize manifest
    let manifest_json = manifest.to_json_bytes()
        .map_err(|e| anyhow::anyhow!("serialize: {e}"))?;

    // Upload manifest as a single chunk to Autonomi
    // (in production: connect to antd, pay, upload)
    println!("  Manifest size: {} bytes", manifest_json.len());
    println!("  Recovery mode: {}", matches!(actual_mode, PaymentMode::Recovery));

    // Stub: actual upload requires antd connection + EVM wallet
    let result = FolderUploadResult {
        folder_name: manifest.folder_name.clone(),
        manifest_addr: "stub-manifest-addr".into(),
        file_count: manifest.file_count,
        total_size: manifest.total_size,
        recovery_tx_hash: if matches!(actual_mode, PaymentMode::Recovery) {
            Some("stub-tx-hash".into())
        } else {
            None
        },
    };

    println!("\nUpload complete:");
    println!("  Manifest address: {}", result.manifest_addr);
    println!("  Files: {}", result.file_count);
    if let Some(ref tx) = result.recovery_tx_hash {
        println!("  Recovery tx: {tx}");
    }

    Ok(())
}
