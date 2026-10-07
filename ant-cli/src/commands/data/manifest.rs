//! Manifest commands (ADR-0006).
//!
//! A manifest is a torrent-like description of a set of files. It is shared
//! off the network as a `.ant` file or an `ant://manifest/...` link, never
//! stored on the network.
//!
//! Every upload the CLI performs is also recorded as a manifest in the upload
//! history under the data directory, so `ant manifest list` can show it and
//! `ant manifest download <ID>` can fetch it again.

use std::io::{self, BufRead, IsTerminal, Write};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::time::Instant;

use ant_core::data::{
    apply_compaction, default_history_dir, embeddable_data_map, extract_manifest, format_timestamp,
    is_link, list_uploads, load_upload, manifest_filename_for, manifest_link, parse_link,
    plan_compaction, publish_data_maps, read_manifest_file, record_upload, write_manifest_file,
    BuildEvent, BuildOptions, Client, ContentRef, DownloadEvent, EntryStatus, ExtractEvent,
    ExtractOptions, Link, Manifest, ManifestBuilder, PaymentMode, ReferenceMode, TorrentReference,
    UploadEvent, UploadRecord, Visibility, MANIFEST_LINK_RECOMMENDED_MAX_BYTES,
};
use clap::Subcommand;
use serde_json::json;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::progress;

/// Entries downloaded at once unless `--concurrency` says otherwise.
const DEFAULT_CONCURRENCY: usize = 1;
/// Capacity of the progress channels between core and the terminal UI.
const PROGRESS_CHANNEL_CAPACITY: usize = 64;
/// Separator between address and path in `--public-file ADDRESS=PATH`.
const PUBLIC_FILE_SEPARATOR: char = '=';
/// Answers accepted as "yes" at the publish prompt.
const YES_ANSWERS: &[&str] = &["y", "yes"];
/// Exit code for a second Ctrl-C, matching the shell convention for SIGINT.
const FORCE_QUIT_EXIT_CODE: i32 = 130;

/// Manifest subcommands.
#[derive(Subcommand, Debug)]
pub enum ManifestAction {
    /// Upload files and write a manifest describing them.
    ///
    /// One directory: its contents become the entries and its name the
    /// manifest name. Otherwise each path is added under its own name.
    Create {
        /// Files or directories to upload.
        paths: Vec<PathBuf>,
        /// Where to write the manifest. Defaults to `<name>.ant` here.
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Manifest name, used as the default extraction directory.
        #[arg(long)]
        name: Option<String>,
        /// Reference files by public address where their DataMap chunk is
        /// already on the network, to keep the manifest small. Never
        /// publishes anything itself.
        #[arg(long)]
        compact: bool,
        /// Upload the files as public (stores each DataMap chunk). Anyone
        /// with a file's address can then download it. The manifest still
        /// embeds the DataMaps unless --compact is also given.
        #[arg(long)]
        public: bool,
        /// Follow symlinks to regular files instead of skipping them.
        #[arg(long)]
        follow_symlinks: bool,
        /// Add an already-public file by address, optionally under PATH.
        #[arg(long = "public-file", value_name = "ADDRESS[=PATH]")]
        public_files: Vec<String>,
        /// Add an already-public file by address but fetch its DataMap and
        /// embed it, so recipients skip the DataMap fetch. Needs the network,
        /// no wallet.
        #[arg(long = "embed-public", value_name = "ADDRESS[=PATH]")]
        embed_public: Vec<String>,
        /// Record the BitTorrent info hash of the same files: 40 hex
        /// characters for v1, 64 for v2. Repeat to give both.
        #[arg(long = "torrent-hash", value_name = "HEX")]
        torrent_hashes: Vec<String>,
        /// Force merkle batch payment regardless of chunk count.
        #[arg(long, conflicts_with = "no_merkle")]
        merkle: bool,
        /// Disable merkle batch payment, always use per-chunk payments.
        #[arg(long, conflicts_with = "merkle")]
        no_merkle: bool,
        /// Replace an existing manifest file at the output path.
        #[arg(long)]
        overwrite: bool,
        /// Also print the manifest as an `ant://manifest/...` link.
        #[arg(long)]
        link: bool,
    },
    /// List every recorded upload, newest first.
    List,
    /// List the entries of a manifest.
    Show {
        /// A `.ant` file, an `ant://manifest/...` link, or an upload id
        /// from `ant manifest list`.
        source: String,
    },
    /// Print a manifest file as an `ant://manifest/...` link.
    Link {
        /// The `.ant` file.
        file: PathBuf,
    },
    /// Write a manifest out as a `.ant` file, optionally compacted.
    ///
    /// Compacting replaces embedded DataMaps with public addresses, which
    /// is only possible for DataMaps that are on the network. Any that are
    /// still private are listed and you are asked whether to publish them
    /// first; publishing is paid and makes those files public.
    Export {
        /// A `.ant` file, an `ant://manifest/...` link, or an upload id
        /// from `ant manifest list`.
        source: String,
        /// Where to write the manifest. Defaults to `<name>.ant` here.
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Replace embedded DataMaps with public addresses.
        #[arg(long)]
        compact: bool,
        /// Publish still-private DataMaps without asking.
        #[arg(long, short)]
        yes: bool,
        /// Replace an existing file at the output path.
        #[arg(long)]
        overwrite: bool,
        /// Also print the exported manifest as an `ant://manifest/...` link.
        #[arg(long)]
        link: bool,
    },
    /// Download the files a manifest describes.
    Download {
        /// A `.ant` file, an `ant://manifest/...` link, or an upload id
        /// from `ant manifest list`.
        source: String,
        /// Directory to extract into. Defaults to the manifest name, or
        /// the current directory when it has none.
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Only extract these entries or directories. Repeatable.
        #[arg(long = "select", value_name = "PATH")]
        selection: Vec<String>,
        /// Replace existing regular files at target paths.
        #[arg(long)]
        overwrite: bool,
        /// Entries to download at once.
        #[arg(long, default_value_t = DEFAULT_CONCURRENCY)]
        concurrency: usize,
    },
}

impl ManifestAction {
    /// Whether the command talks to the network.
    pub fn needs_network(&self) -> bool {
        match self {
            Self::Create {
                paths,
                embed_public,
                ..
            } => !paths.is_empty() || !embed_public.is_empty(),
            Self::Download { .. } => true,
            Self::Export { compact, .. } => *compact,
            Self::List | Self::Show { .. } | Self::Link { .. } => false,
        }
    }

    /// Whether the command pays for anything.
    pub fn needs_wallet(&self) -> bool {
        match self {
            Self::Create { paths, .. } => !paths.is_empty(),
            // Compacting may have to publish DataMap chunks, which is paid.
            Self::Export { compact, .. } => *compact,
            Self::List | Self::Show { .. } | Self::Link { .. } | Self::Download { .. } => false,
        }
    }

    /// Run a command that needs no network.
    pub fn execute_offline(self, json: bool) -> anyhow::Result<()> {
        match self {
            Self::List => list_history(json),
            Self::Show { source } => show(&load_manifest(&source)?, json),
            Self::Link { file } => link(&read_manifest_file(&file)?, json),
            Self::Create {
                paths,
                output,
                name,
                public_files,
                embed_public,
                torrent_hashes,
                overwrite,
                link,
                ..
            } if paths.is_empty() && embed_public.is_empty() => {
                // Nothing to upload: a manifest over already-public files.
                let mut manifest = Manifest::new(name);
                manifest.torrent = parse_torrent_hashes(&torrent_hashes)?;
                for spec in &public_files {
                    let (address, path) = parse_public_file(spec)?;
                    manifest.entries.push(ant_core::data::ManifestEntry {
                        path,
                        size: None,
                        source: ContentRef::Public { address },
                    });
                }
                if manifest.entries.is_empty() {
                    anyhow::bail!("nothing to add: pass files, directories or --public-file");
                }
                manifest.canonicalize()?;
                let out = resolve_output(manifest.name.as_deref(), output);
                check_output_writable(&out, overwrite)?;
                write_manifest(&manifest, &out, overwrite)?;
                let history_id = record_upload_manifest(&manifest, None);
                report_created(&manifest, &out, None, history_id, link, json)
            }
            Self::Export {
                source,
                output,
                compact: false,
                overwrite,
                link,
                ..
            } => {
                let manifest = load_manifest(&source)?;
                let out = resolve_output(manifest.name.as_deref(), output);
                write_manifest(&manifest, &out, overwrite)?;
                report_exported(&manifest, &out, 0, link, json)
            }
            Self::Create { .. } | Self::Download { .. } | Self::Export { .. } => {
                anyhow::bail!("this command needs a network connection")
            }
        }
    }

    /// Run a command against the network.
    pub async fn execute(self, client: &Client, json: bool) -> anyhow::Result<()> {
        match self {
            Self::Create {
                paths,
                output,
                name,
                compact,
                public,
                follow_symlinks,
                public_files,
                embed_public,
                torrent_hashes,
                merkle,
                no_merkle,
                overwrite,
                link,
            } => {
                let payment_mode = if merkle {
                    PaymentMode::Merkle
                } else if no_merkle {
                    PaymentMode::Single
                } else {
                    PaymentMode::Auto
                };
                create(
                    client,
                    CreateArgs {
                        paths,
                        output,
                        name,
                        compact,
                        public,
                        follow_symlinks,
                        public_files,
                        embed_public,
                        torrent: parse_torrent_hashes(&torrent_hashes)?,
                        payment_mode,
                        overwrite,
                        link,
                    },
                    json,
                )
                .await
            }
            Self::Download {
                source,
                output,
                selection,
                overwrite,
                concurrency,
            } => {
                let concurrency = NonZeroUsize::new(concurrency)
                    .ok_or_else(|| anyhow::anyhow!("--concurrency must be at least 1"))?;
                download(
                    client,
                    &load_manifest(&source)?,
                    output,
                    selection,
                    overwrite,
                    concurrency,
                    json,
                )
                .await
            }
            Self::Export {
                source,
                output,
                compact: true,
                yes,
                overwrite,
                link,
            } => {
                let manifest = load_manifest(&source)?;
                export_compact(client, &manifest, output, yes, overwrite, link, json).await
            }
            Self::List | Self::Show { .. } | Self::Link { .. } | Self::Export { .. } => {
                self.execute_offline(json)
            }
        }
    }
}

async fn export_compact(
    client: &Client,
    manifest: &Manifest,
    output: Option<PathBuf>,
    yes: bool,
    overwrite: bool,
    with_link: bool,
    json: bool,
) -> anyhow::Result<()> {
    // Nothing is published until the output is known to be writable.
    let out = resolve_output(manifest.name.as_deref(), output);
    check_output_writable(&out, overwrite)?;
    let cancel = CancellationToken::new();
    spawn_ctrl_c(cancel.clone());

    let plan = if json {
        plan_compaction(client, manifest, &cancel).await?
    } else {
        let spinner = progress::new_spinner("Checking which DataMaps are on the network...");
        let plan = plan_compaction(client, manifest, &cancel).await;
        spinner.finish_and_clear();
        plan?
    };

    if !plan.is_free() {
        if !json {
            eprintln!(
                "{} embedded DataMap(s) are already public; {} are still private:",
                plan.already_public.len(),
                plan.needs_publish.len()
            );
            for entry in &plan.needs_publish {
                eprintln!("  {}", entry.name);
            }
        }
        if !yes && !confirm_publish(plan.needs_publish.len(), json)? {
            anyhow::bail!("export cancelled: the manifest was not compacted");
        }
        info!("Publishing {} DataMap chunk(s)", plan.needs_publish.len());
        if json {
            publish_data_maps(client, manifest, &plan.needs_publish, &cancel).await?;
        } else {
            let spinner = progress::new_spinner(&format!(
                "Publishing {} DataMap chunk(s)...",
                plan.needs_publish.len()
            ));
            let result = publish_data_maps(client, manifest, &plan.needs_publish, &cancel).await;
            spinner.finish_and_clear();
            result?;
        }
    }

    let compacted = apply_compaction(manifest, &plan.all_indices())?;
    write_manifest(&compacted, &out, overwrite)?;
    report_exported(&compacted, &out, plan.needs_publish.len(), with_link, json)
}

/// Ask on the terminal whether to publish `count` DataMaps. Without a
/// terminal, or in JSON mode, the answer must come from `--yes`.
fn confirm_publish(count: usize, json: bool) -> anyhow::Result<bool> {
    if json || !io::stdin().is_terminal() {
        anyhow::bail!(
            "{count} DataMap(s) must be published to compact this manifest; \
             pass --yes to publish them (this is paid and makes those files public)"
        );
    }
    eprint!("Publish {count} DataMap chunk(s)? This is paid and makes those files public. [y/N] ");
    io::stderr().flush()?;
    let mut answer = String::new();
    io::stdin().lock().read_line(&mut answer)?;
    Ok(YES_ANSWERS.contains(&answer.trim().to_ascii_lowercase().as_str()))
}

fn report_exported(
    manifest: &Manifest,
    out: &Path,
    published: usize,
    with_link: bool,
    json: bool,
) -> anyhow::Result<()> {
    let link_text = if with_link {
        Some(manifest_link(manifest)?)
    } else {
        None
    };
    let embedded = manifest
        .entries
        .iter()
        .filter(|e| matches!(e.source, ContentRef::Embedded { .. }))
        .count();
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "manifest_file": out.display().to_string(),
                "name": manifest.name,
                "torrent": torrent_json(manifest),
                "entries": entries_json(manifest)?,
                "embedded_entries": embedded,
                "public_entries": manifest.entries.len() - embedded,
                "data_maps_published": published,
                "link": link_text,
            }))?
        );
        return Ok(());
    }
    println!(
        "Wrote {} ({} embedded, {} public)",
        out.display(),
        embedded,
        manifest.entries.len() - embedded
    );
    if published > 0 {
        println!("Published {published} DataMap chunk(s)");
    }
    if let Some(link) = link_text {
        warn_if_long_link(manifest)?;
        println!("{link}");
    }
    Ok(())
}

struct CreateArgs {
    paths: Vec<PathBuf>,
    output: Option<PathBuf>,
    name: Option<String>,
    compact: bool,
    public: bool,
    follow_symlinks: bool,
    public_files: Vec<String>,
    embed_public: Vec<String>,
    torrent: Option<TorrentReference>,
    payment_mode: PaymentMode,
    overwrite: bool,
    link: bool,
}

async fn create(client: &Client, args: CreateArgs, json: bool) -> anyhow::Result<()> {
    let single_dir = match args.paths.as_slice() {
        [only] if only.is_dir() => Some(only.clone()),
        _ => None,
    };
    let name = args.name.or_else(|| {
        single_dir
            .as_ref()
            .and_then(|d| d.file_name())
            .and_then(|n| n.to_str())
            .map(str::to_owned)
    });

    let options = BuildOptions {
        name,
        torrent: args.torrent,
        reference_mode: if args.compact {
            ReferenceMode::Compact
        } else {
            ReferenceMode::Embedded
        },
        visibility: if args.public {
            Visibility::Public
        } else {
            Visibility::Private
        },
        payment_mode: args.payment_mode,
        follow_symlinks: args.follow_symlinks,
        cancel: CancellationToken::new(),
    };
    // Fail before any paid upload if the manifest could not be written.
    let out = resolve_output(options.name.as_deref(), args.output.clone());
    check_output_writable(&out, args.overwrite)?;
    spawn_ctrl_c(options.cancel.clone());
    let mut builder = ManifestBuilder::new(client, options);
    if let Some(dir) = &single_dir {
        builder.add_directory(dir, None)?;
    } else {
        for path in &args.paths {
            builder.add_path(path)?;
        }
    }
    for spec in &args.public_files {
        let (address, path) = parse_public_file(spec)?;
        builder.add_public(address, path, None)?;
    }
    for spec in &args.embed_public {
        let (address, path) = parse_public_file(spec)?;
        info!("Fetching public DataMap {}", hex::encode(address));
        let data_map = client.data_map_fetch(&address).await.map_err(|e| {
            anyhow::anyhow!("Failed to fetch DataMap {}: {e}", hex::encode(address))
        })?;
        // Embed the root map when it is small enough: recipients then start
        // on data chunks with no wrapper-record fetches.
        let data_map = embeddable_data_map(client, &data_map).await?;
        builder.add_embedded(data_map, Some(address), path, None)?;
    }
    if builder.pending_count() == 0 && args.public_files.is_empty() && args.embed_public.is_empty()
    {
        anyhow::bail!("nothing to add: the given paths contain no regular files");
    }

    info!("Building manifest from {} file(s)", builder.pending_count());
    let start = Instant::now();
    let result = if json {
        builder.finish(None).await?
    } else {
        let (tx, rx) = mpsc::channel(PROGRESS_CHANNEL_CAPACITY);
        let ui = tokio::spawn(drive_build_progress(rx));
        let result = builder.finish(Some(tx)).await;
        let _ = ui.await;
        result?
    };

    // Record first: the history is the safety net for paid uploads, so it
    // must exist even if writing the output file fails.
    let history_id = record_upload_manifest(&result.manifest, None);
    if result.cancelled {
        let recorded = history_id
            .map(|id| format!("; the partial manifest is recorded as {id}"))
            .unwrap_or_default();
        anyhow::bail!(
            "cancelled after {} of {} file(s){recorded}",
            result.files_uploaded,
            result.files_uploaded
                + result
                    .manifest
                    .entries
                    .len()
                    .saturating_sub(result.files_uploaded)
        );
    }
    write_manifest(&result.manifest, &out, args.overwrite)?;
    if !json {
        eprintln!(
            "Uploaded {} file(s), {} new chunk(s) in {:.1}s",
            result.files_uploaded,
            result.chunks_stored,
            start.elapsed().as_secs_f64()
        );
        for skipped in &result.skipped_symlinks {
            eprintln!("Skipped symlink: {}", skipped.display());
        }
    }
    report_created(
        &result.manifest,
        &out,
        Some(&result),
        history_id,
        args.link,
        json,
    )
}

/// The output path: the one given, or `<name>.ant` in the current directory.
fn resolve_output(name: Option<&str>, output: Option<PathBuf>) -> PathBuf {
    output.unwrap_or_else(|| PathBuf::from(manifest_filename_for(name)))
}

/// Refuse up front when the output exists and may not be replaced, so no
/// paid work happens for a manifest that could not be written.
fn check_output_writable(out: &Path, overwrite: bool) -> anyhow::Result<()> {
    match std::fs::symlink_metadata(out) {
        Ok(meta) if meta.is_dir() => anyhow::bail!("{} is a directory", out.display()),
        Ok(_) if !overwrite => anyhow::bail!(
            "{} already exists; pass --overwrite to replace it",
            out.display()
        ),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(anyhow::anyhow!("cannot check {}: {e}", out.display())),
    }
}

fn write_manifest(manifest: &Manifest, out: &Path, overwrite: bool) -> anyhow::Result<()> {
    write_manifest_file(out, manifest, overwrite)
        .map_err(|e| anyhow::anyhow!("Failed to write {}: {e}", out.display()))
}

/// Turn the first Ctrl-C into a cancellation and a second one into an exit.
fn spawn_ctrl_c(cancel: CancellationToken) {
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            eprintln!("Cancelling... press Ctrl-C again to quit immediately");
            cancel.cancel();
        }
        if tokio::signal::ctrl_c().await.is_ok() {
            std::process::exit(FORCE_QUIT_EXIT_CODE);
        }
    });
}

/// Record an upload in the history, warning instead of failing when the
/// history directory is unusable: the upload itself has already succeeded.
pub fn record_upload_manifest(manifest: &Manifest, label: Option<&str>) -> Option<String> {
    let recorded = default_history_dir().and_then(|dir| record_upload(&dir, manifest, label));
    match recorded {
        Ok(record) => Some(record.id),
        Err(e) => {
            eprintln!(
                "warning: upload succeeded but could not be recorded in the upload history: {e}"
            );
            None
        }
    }
}

fn report_created(
    manifest: &Manifest,
    out: &Path,
    result: Option<&ant_core::data::BuildResult>,
    history_id: Option<String>,
    with_link: bool,
    json: bool,
) -> anyhow::Result<()> {
    let link_text = if with_link {
        Some(manifest_link(manifest)?)
    } else {
        None
    };
    if json {
        let mut value = json!({
            "manifest_file": out.display().to_string(),
            "name": manifest.name,
                "torrent": torrent_json(manifest),
            "entries": entries_json(manifest)?,
            "total_size": manifest.total_size(),
            "link": link_text,
            "history_id": history_id,
        });
        if let Some(r) = result {
            value["files_uploaded"] = json!(r.files_uploaded);
            value["chunks_stored"] = json!(r.chunks_stored);
            value["storage_cost_atto"] = json!(r.storage_cost_atto.to_string());
            value["gas_cost_wei"] = json!(r.gas_cost_wei.to_string());
            value["skipped_symlinks"] = json!(r
                .skipped_symlinks
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>());
        }
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(());
    }
    println!(
        "Wrote {} ({} entr{})",
        out.display(),
        manifest.entries.len(),
        if manifest.entries.len() == 1 {
            "y"
        } else {
            "ies"
        }
    );
    if let Some(id) = history_id {
        println!("Recorded as {id} (ant manifest download {id})");
    }
    if let Some(link) = link_text {
        warn_if_long_link(manifest)?;
        println!("{link}");
    }
    Ok(())
}

fn list_history(json: bool) -> anyhow::Result<()> {
    let dir = default_history_dir()?;
    let listing = list_uploads(&dir)?;
    if json {
        let records: Vec<_> = listing
            .records
            .iter()
            .map(|r| {
                json!({
                    "id": r.id,
                    "recorded_at": r.recorded_at,
                    "recorded_at_utc": format_timestamp(r.recorded_at),
                    "name": r.manifest.name,
                    "entries": r.manifest.entries.len(),
                    "total_size": r.manifest.total_size(),
                    "file": r.path.display().to_string(),
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "history_dir": dir.display().to_string(),
                "records": records,
                "unreadable": listing
                    .unreadable
                    .iter()
                    .map(|(p, e)| json!({"file": p.display().to_string(), "error": e}))
                    .collect::<Vec<_>>(),
            }))?
        );
        return Ok(());
    }
    if listing.records.is_empty() {
        println!("No recorded uploads in {}", dir.display());
    } else {
        println!(
            "{:<23}  {:>7}  {:>12}  ID",
            "RECORDED (UTC)", "ENTRIES", "SIZE"
        );
        for r in &listing.records {
            print_history_row(r);
        }
    }
    for (path, error) in &listing.unreadable {
        eprintln!("warning: unreadable record {}: {error}", path.display());
    }
    Ok(())
}

fn print_history_row(record: &UploadRecord) {
    let size = record
        .manifest
        .total_size()
        .map(|s| s.to_string())
        .unwrap_or_else(|| "?".to_string());
    // Drop the trailing " UTC"; the header already says so.
    let stamp = format_timestamp(record.recorded_at);
    let stamp = stamp.strip_suffix(" UTC").unwrap_or(&stamp);
    println!(
        "{:<23}  {:>7}  {:>12}  {}",
        stamp,
        record.manifest.entries.len(),
        size,
        record.id
    );
}

fn show(manifest: &Manifest, json: bool) -> anyhow::Result<()> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "name": manifest.name,
                "torrent": torrent_json(manifest),
                "total_size": manifest.total_size(),
                "entries": entries_json(manifest)?,
            }))?
        );
        return Ok(());
    }
    println!("Name:    {}", manifest.name.as_deref().unwrap_or("(none)"));
    if let Some(torrent) = &manifest.torrent {
        if let Some(v1) = torrent.info_hash_v1 {
            println!("Torrent: v1 {}", hex::encode(v1));
        }
        if let Some(v2) = torrent.info_hash_v2 {
            println!("Torrent: v2 {}", hex::encode(v2));
        }
    }
    println!("Entries: {}", manifest.entries.len());
    if let Some(total) = manifest.total_size() {
        println!("Size:    {total} bytes");
    }
    println!();
    for entry in &manifest.entries {
        let size = entry
            .size
            .map(|s| s.to_string())
            .unwrap_or_else(|| "?".to_string());
        println!(
            "{:>12}  {:<9} {}",
            size,
            entry.source.kind(),
            entry.effective_name()?
        );
    }
    Ok(())
}

fn link(manifest: &Manifest, json: bool) -> anyhow::Result<()> {
    let bytes = manifest.encode()?;
    let link = manifest_link(manifest)?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "link": link,
                "manifest_bytes": bytes.len(),
                "recommended_max_bytes": MANIFEST_LINK_RECOMMENDED_MAX_BYTES,
            }))?
        );
        return Ok(());
    }
    warn_if_long_link(manifest)?;
    println!("{link}");
    Ok(())
}

fn warn_if_long_link(manifest: &Manifest) -> anyhow::Result<()> {
    let len = manifest.encode()?.len();
    if len > MANIFEST_LINK_RECOMMENDED_MAX_BYTES {
        eprintln!(
            "warning: this manifest is {len} bytes; links above \
             {MANIFEST_LINK_RECOMMENDED_MAX_BYTES} bytes are long for a chat message. \
             Consider sharing the .ant file instead."
        );
    }
    Ok(())
}

async fn download(
    client: &Client,
    manifest: &Manifest,
    output: Option<PathBuf>,
    selection: Vec<String>,
    overwrite: bool,
    concurrency: NonZeroUsize,
    json: bool,
) -> anyhow::Result<()> {
    let output_root = output.unwrap_or_else(|| {
        manifest
            .name
            .clone()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
    });
    let options = ExtractOptions {
        output_root: output_root.clone(),
        selection,
        overwrite,
        concurrency,
        cancel: CancellationToken::new(),
    };
    spawn_ctrl_c(options.cancel.clone());
    info!("Extracting manifest into {}", output_root.display());
    let start = Instant::now();

    let report = if json {
        extract_manifest(client, manifest, &options, None).await?
    } else {
        let (tx, rx) = mpsc::channel(PROGRESS_CHANNEL_CAPACITY);
        let ui = tokio::spawn(drive_extract_progress(rx));
        let report = extract_manifest(client, manifest, &options, Some(tx)).await;
        let _ = ui.await;
        report?
    };

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "output": output_root.display().to_string(),
                "written": report.written(),
                "failed": report.failed(),
                "cancelled": report.cancelled(),
                "entries": report.entries,
            }))?
        );
    } else {
        for e in &report.entries {
            match &e.status {
                EntryStatus::Written { bytes } => println!("written   {} ({bytes} bytes)", e.name),
                EntryStatus::Failed { error } => println!("failed    {}: {error}", e.name),
                EntryStatus::Cancelled => println!("cancelled {}", e.name),
            }
        }
        eprintln!(
            "{} written, {} failed in {:.1}s -> {}",
            report.written(),
            report.failed(),
            start.elapsed().as_secs_f64(),
            output_root.display()
        );
    }
    if report.cancelled() > 0 {
        anyhow::bail!(
            "cancelled with {} entr(y/ies) not downloaded",
            report.cancelled()
        );
    }
    if report.failed() > 0 {
        anyhow::bail!("{} entr(y/ies) failed to download", report.failed());
    }
    Ok(())
}

/// Load a manifest from an `ant://manifest/...` link, a `.ant` path, or an
/// upload id from the history, tried in that order.
fn load_manifest(source: &str) -> anyhow::Result<Manifest> {
    if is_link(source) {
        return match parse_link(source)? {
            Link::Manifest(m) => Ok(m),
            Link::File(_) => anyhow::bail!(
                "this is a file link, not a manifest link; use `ant file download` for it"
            ),
        };
    }
    let path = Path::new(source);
    if path.is_file() {
        return read_manifest_file(path)
            .map_err(|e| anyhow::anyhow!("Failed to read manifest {source}: {e}"));
    }
    let dir = default_history_dir()?;
    match load_upload(&dir, source)? {
        Some(record) => Ok(record.manifest),
        None => anyhow::bail!(
            "{source} is not a manifest file, a manifest link, or a recorded upload id \
             (see `ant manifest list`)"
        ),
    }
}

/// Parse the `--torrent-hash` values into one reference, or `None` if none.
fn parse_torrent_hashes(hashes: &[String]) -> anyhow::Result<Option<TorrentReference>> {
    let mut merged: Option<TorrentReference> = None;
    for hash in hashes {
        let parsed = TorrentReference::parse_hex(hash)?;
        merged = Some(match merged {
            Some(existing) => existing.merge(parsed)?,
            None => parsed,
        });
    }
    Ok(merged)
}

/// Hex form of a torrent reference for JSON output.
fn torrent_json(manifest: &Manifest) -> serde_json::Value {
    match &manifest.torrent {
        Some(t) => json!({
            "info_hash_v1": t.info_hash_v1.map(hex::encode),
            "info_hash_v2": t.info_hash_v2.map(hex::encode),
        }),
        None => serde_json::Value::Null,
    }
}

/// Parse `ADDRESS[=PATH]` for `--public-file`.
fn parse_public_file(spec: &str) -> anyhow::Result<([u8; 32], Option<String>)> {
    let (address_text, path) = match spec.split_once(PUBLIC_FILE_SEPARATOR) {
        Some((a, p)) => (a, Some(p.to_string())),
        None => (spec, None),
    };
    match parse_link(address_text)? {
        Link::File(address) => Ok((address, path)),
        Link::Manifest(_) => anyhow::bail!("--public-file takes a file address, not a manifest"),
    }
}

fn entries_json(manifest: &Manifest) -> anyhow::Result<Vec<serde_json::Value>> {
    manifest
        .entries
        .iter()
        .map(|entry| {
            Ok(json!({
                "name": entry.effective_name()?,
                "path": entry.path,
                "size": entry.size,
                "kind": entry.source.kind(),
                "address": hex::encode(entry.source.content_address()?),
            }))
        })
        .collect()
}

async fn drive_build_progress(mut rx: mpsc::Receiver<BuildEvent>) {
    let spinner = progress::new_spinner("Preparing upload...");
    let mut current = String::new();
    let mut position = String::new();
    while let Some(event) = rx.recv().await {
        match event {
            BuildEvent::FileStarted { path, index, total } => {
                current = path;
                position = format!("[{}/{total}]", index + 1);
                spinner.set_message(format!("{position} {current}: encrypting"));
            }
            BuildEvent::Upload { event, .. } => {
                let phase = match event {
                    UploadEvent::Encrypting { chunks_done } => {
                        format!("encrypting ({chunks_done})")
                    }
                    UploadEvent::Encrypted { total_chunks } => {
                        format!("encrypted {total_chunks} chunks")
                    }
                    UploadEvent::QuotingChunks {
                        wave, total_waves, ..
                    } => {
                        format!("quoting wave {wave}/{total_waves}")
                    }
                    UploadEvent::ChunkQuoted { quoted, total } => {
                        format!("quoted {quoted}/{total}")
                    }
                    UploadEvent::ChunkStored { stored, total } => {
                        format!("stored {stored}/{total}")
                    }
                };
                spinner.set_message(format!("{position} {current}: {phase}"));
            }
            BuildEvent::FileFinished {
                path,
                chunks_stored,
                reference,
            } => {
                spinner.println(format!(
                    "uploaded {path} ({chunks_stored} new chunks, {reference})"
                ));
            }
        }
    }
    spinner.finish_and_clear();
}

async fn drive_extract_progress(mut rx: mpsc::Receiver<ExtractEvent>) {
    let spinner = progress::new_spinner("Preparing download...");
    while let Some(event) = rx.recv().await {
        match event {
            ExtractEvent::EntryStarted { name, index, total } => {
                spinner.set_message(format!("[{}/{total}] {name}: resolving", index + 1));
            }
            ExtractEvent::Download { name, event } => {
                let phase = match event {
                    DownloadEvent::ResolvingDataMap { total_map_chunks } => {
                        format!("resolving data map ({total_map_chunks} chunks)")
                    }
                    DownloadEvent::MapChunkFetched { fetched } => {
                        format!("resolving data map ({fetched} fetched)")
                    }
                    DownloadEvent::DataMapResolved { total_chunks } => {
                        format!("fetching {total_chunks} chunks")
                    }
                    DownloadEvent::ChunksFetched { fetched, total } => {
                        format!("fetched {fetched}/{total}")
                    }
                };
                spinner.set_message(format!("{name}: {phase}"));
            }
            ExtractEvent::EntryFinished { .. } => {}
        }
    }
    spinner.finish_and_clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_file_spec_parses_with_and_without_path() {
        let hex64 = "ab".repeat(32);
        let (addr, path) = parse_public_file(&hex64).unwrap();
        assert_eq!(addr, [0xab; 32]);
        assert_eq!(path, None);
        let (addr, path) = parse_public_file(&format!("ant://{hex64}=docs/a.pdf")).unwrap();
        assert_eq!(addr, [0xab; 32]);
        assert_eq!(path.as_deref(), Some("docs/a.pdf"));
        assert!(parse_public_file("nope").is_err());
    }

    #[test]
    fn network_and_wallet_needs() {
        let create_offline = ManifestAction::Create {
            paths: vec![],
            output: None,
            name: None,
            compact: false,
            public: false,
            follow_symlinks: false,
            public_files: vec!["ab".repeat(32)],
            embed_public: vec![],
            torrent_hashes: vec![],
            merkle: false,
            no_merkle: false,
            overwrite: false,
            link: false,
        };
        assert!(!create_offline.needs_network());
        assert!(!create_offline.needs_wallet());
        let show = ManifestAction::Show {
            source: "x.ant".into(),
        };
        assert!(!show.needs_network());
        let download = ManifestAction::Download {
            source: "x.ant".into(),
            output: None,
            selection: vec![],
            overwrite: false,
            concurrency: DEFAULT_CONCURRENCY,
        };
        assert!(download.needs_network());
        assert!(!download.needs_wallet());
    }
}
