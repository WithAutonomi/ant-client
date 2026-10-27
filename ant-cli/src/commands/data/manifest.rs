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
    apply_compaction, data_map_address, default_history_dir, embeddable_data_map, extract_manifest,
    format_timestamp, is_link, list_uploads, load_upload, manifest_filename_for,
    manifest_link_bytes, manifest_link_from_bytes, parse_link, plan_compaction, publish_data_maps,
    read_manifest_file, record_upload, select_entries, write_manifest_file, BuildEvent,
    BuildOptions, BuildResult, Client, ContentRef, DownloadEvent, EntryStatus, ExtractEvent,
    ExtractOptions, Link, Manifest, ManifestBuilder, ManifestError, PaymentMode, RecordState,
    ReferenceMode, TorrentReference, UploadEvent, UploadRecord, Visibility,
    MANIFEST_LINK_RECOMMENDED_MAX_BYTES,
};
use clap::Subcommand;
use serde_json::json;
use sha2::{Digest, Sha256};
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
/// Marks a size in `manifest show` that only the manifest's creator vouches for.
const CLAIMED_SIZE_MARK: char = '*';
/// Extension of a stopped build's partial manifest, written beside an
/// output file that already exists rather than over it.
const PARTIAL_MANIFEST_EXTENSION: &str = "partial.ant";
/// Directory under the data directory that records the default download
/// directories whose download has not finished, so a re-run can continue.
const DOWNLOAD_RECORDS_DIR: &str = "downloads";

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
    /// Print a manifest as an `ant://manifest/...` link. The link carries
    /// each embedded DataMap in its short published form.
    Link {
        /// A `.ant` file, an `ant://manifest/...` link, or an upload id
        /// from `ant manifest list`.
        source: String,
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
        /// Directory to extract into, existing or not. Defaults to a new
        /// directory named after the manifest in your downloads directory,
        /// which must not exist yet unless an earlier run of this download
        /// made it and did not finish. A manifest without a name, or with a
        /// hidden one, needs --output. Files already in place with their
        /// recorded SHA-256 are kept, so a download that stopped can be run
        /// again.
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
            Self::Link { source } => link(&load_manifest(&source)?, json),
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
                        sha256: None,
                        source: ContentRef::Public { address },
                    });
                }
                if manifest.entries.is_empty() {
                    anyhow::bail!("nothing to add: pass files, directories or --public-file");
                }
                manifest.canonicalize()?;
                let out = resolve_output(manifest.name.as_deref(), output)?;
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
                let out = resolve_output(manifest.name.as_deref(), output)?;
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
    let out = resolve_output(manifest.name.as_deref(), output)?;
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
        Some(make_link(manifest)?)
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
                "link": link_text.as_ref().map(|(link, _)| link),
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
    if let Some((link, len)) = link_text {
        warn_if_long_link(len);
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
        // Each completed upload is journaled here before the next is paid
        // for; a build that cannot record itself stops before paying.
        history: Some(default_history_dir()?),
    };
    // Fail before any paid upload if the manifest could not be written.
    let out = resolve_output(options.name.as_deref(), args.output.clone())?;
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
        // An entry's identity is derived from its DataMap. A map stored in
        // some other form than an upload publishes would derive a different
        // address, so such a file is referenced by its address instead.
        if data_map_address(&data_map)? == address {
            builder.add_embedded(data_map, path, None)?;
        } else {
            eprintln!(
                "warning: the DataMap at {} is not in published form; adding it by address",
                hex::encode(address)
            );
            builder.add_public(address, path, None)?;
        }
    }
    if builder.pending_count() == 0 && args.public_files.is_empty() && args.embed_public.is_empty()
    {
        anyhow::bail!("nothing to add: the given paths contain no regular files");
    }

    info!("Building manifest from {} file(s)", builder.pending_count());
    let start = Instant::now();
    let built = if json {
        builder.finish(None).await
    } else {
        let (tx, rx) = mpsc::channel(PROGRESS_CHANNEL_CAPACITY);
        let ui = tokio::spawn(drive_build_progress(rx));
        let result = builder.finish(Some(tx)).await;
        let _ = ui.await;
        result
    };

    let (result, history_id) = build_outcome(built, &out)?;
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

/// Turn a build's outcome into its result and the id of the record that
/// holds it, or an error that says where the completed uploads are kept. The
/// builder has already recorded them in the history, complete or partial;
/// when it could not, a stopped build's partial manifest is written to `out`.
fn build_outcome(
    built: Result<BuildResult, ManifestError>,
    out: &Path,
) -> anyhow::Result<(BuildResult, Option<String>)> {
    let result = match built {
        Ok(result) => result,
        Err(ManifestError::BuildFailed {
            path,
            cause,
            partial,
        }) => {
            let at = if path.is_empty() {
                String::new()
            } else {
                format!(" at {path:?}")
            };
            anyhow::bail!(
                "manifest build failed{at} after {} of {} file(s): {cause}{}",
                partial.files_uploaded,
                partial.total_to_upload,
                keep_partial(&partial, out),
            );
        }
        Err(error) => return Err(error.into()),
    };
    if result.cancelled {
        return Err(cancelled_build_error(&result, &keep_partial(&result, out)));
    }
    let history_id = recorded_id(&result);
    Ok((result, history_id))
}

/// The id of the record that holds a build's manifest, or `None`, with a
/// warning, when its record could not be written.
fn recorded_id(result: &BuildResult) -> Option<String> {
    match (&result.history_id, &result.history_error) {
        (Some(id), Some(error)) => {
            eprintln!(
                "warning: the record of upload {id} could not be written ({error}); \
                 its journal stays in the history, listed as unfinished"
            );
            None
        }
        (id, _) => id.clone(),
    }
}

/// Where a stopped build's partial manifest is kept, for its error message:
/// its record, or `out` when the record could not be written, since a
/// journal the build could not finish may also lack the last upload. A file
/// already at `out`, which may be a complete manifest, is never replaced;
/// the partial manifest goes beside it instead.
fn keep_partial(partial: &BuildResult, out: &Path) -> String {
    if let Some(id) = recorded_id(partial) {
        return format!("; the partial manifest is recorded as {id}");
    }
    if partial.files_uploaded == 0 {
        return String::new();
    }
    let beside = out.with_extension(PARTIAL_MANIFEST_EXTENSION);
    let written = write_manifest(&partial.manifest, out, false)
        .map(|()| out)
        .or_else(|_| write_manifest(&partial.manifest, &beside, false).map(|()| beside.as_path()));
    match written {
        Ok(path) => format!("; the partial manifest is written to {}", path.display()),
        Err(error) => format!("; the partial manifest could not be kept: {error}"),
    }
}

/// The output path: the one given, or `<name>.ant` in the current directory.
fn resolve_output(name: Option<&str>, output: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    if let Some(name) = name {
        ant_core::data::manifest::path::validate_component(name)
            .map_err(ManifestError::InvalidName)?;
    }
    Ok(output.unwrap_or_else(|| PathBuf::from(manifest_filename_for(name))))
}

/// Where a download extracts to.
struct DownloadRoot {
    path: PathBuf,
    /// The record marking `path` as this manifest's unfinished default
    /// download, removed once every entry is in place.
    record: Option<PathBuf>,
}

/// The directory a download extracts into without `--output`: a new
/// directory named after the manifest, made in `parent`, the downloads
/// directory, and recorded in `records` until the download finishes. The
/// sender chooses that name, so it never picks a hidden directory or one
/// that already exists, unless an unfinished run of this same download made
/// it; a manifest without a name needs `--output`. Making the directory is
/// the check, so nothing can appear there in between.
fn default_download_root(
    manifest: &Manifest,
    parent: &Path,
    records: Option<&Path>,
) -> anyhow::Result<DownloadRoot> {
    manifest.validate()?;
    let Some(name) = manifest.name.as_deref() else {
        anyhow::bail!("this manifest has no name; pass --output to choose where to extract it");
    };
    if ant_core::data::manifest::path::is_hidden(name) {
        anyhow::bail!(
            "this manifest is named {name:?}, a hidden directory; \
             pass --output to choose where to extract it"
        );
    }
    std::fs::create_dir_all(parent)
        .map_err(|e| anyhow::anyhow!("cannot create {}: {e}", parent.display()))?;
    let path = parent.join(name);
    match std::fs::create_dir(&path) {
        Ok(()) => {
            let record = records.and_then(|records| {
                start_download_record(records, manifest, &path)
                    .inspect_err(|e| {
                        eprintln!("warning: this download cannot be continued if it stops: {e}")
                    })
                    .ok()
            });
            Ok(DownloadRoot { path, record })
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            let unfinished = records
                .and_then(|records| download_record(records, manifest, &path).ok())
                .filter(|record| record.is_file());
            match unfinished {
                Some(record) => Ok(DownloadRoot {
                    path,
                    record: Some(record),
                }),
                None => anyhow::bail!(
                    "{} already exists; choose where to extract with --output, \
                     which may be that directory",
                    path.display()
                ),
            }
        }
        Err(e) => Err(anyhow::anyhow!("cannot create {}: {e}", path.display())),
    }
}

/// The record marking `root` as the unfinished default download of
/// `manifest`, named after both, so no other download or directory matches.
fn download_record(records: &Path, manifest: &Manifest, root: &Path) -> anyhow::Result<PathBuf> {
    let root = std::fs::canonicalize(root).or_else(|_| std::path::absolute(root))?;
    let mut key = Sha256::new();
    key.update(manifest.encode()?);
    key.update(root.as_os_str().as_encoded_bytes());
    Ok(records.join(hex::encode(key.finalize())))
}

/// Record that `root` was just made for `manifest`'s download. The record
/// holds the directory's path for anyone looking.
fn start_download_record(
    records: &Path,
    manifest: &Manifest,
    root: &Path,
) -> anyhow::Result<PathBuf> {
    let record = download_record(records, manifest, root)?;
    std::fs::create_dir_all(records)?;
    std::fs::write(&record, root.display().to_string())?;
    Ok(record)
}

/// `kept` says where the partial manifest is kept, as [`keep_partial`] does.
fn cancelled_build_error(result: &ant_core::data::BuildResult, kept: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "cancelled after {} of {} file(s){kept}",
        result.files_uploaded,
        result.total_to_upload,
    )
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
        Some(make_link(manifest)?)
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
            "link": link_text.as_ref().map(|(link, _)| link),
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
        println!("Recorded as {id} (ant manifest download {id} --output <directory>)");
    }
    if let Some((link, len)) = link_text {
        warn_if_long_link(len);
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
                    "state": r.state,
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
    let state = match record.state {
        RecordState::Complete => "",
        RecordState::Unfinished => "  (unfinished: the files listed were uploaded)",
    };
    println!(
        "{:<23}  {:>7}  {:>12}  {}{state}",
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
    let mut any_claimed = false;
    for entry in &manifest.entries {
        // A size read from an embedded root DataMap is exact; the recorded
        // `size` is only the creator's claim and is marked as such.
        let size = match (entry.known_size(), entry.size) {
            (Some(known), _) => known.to_string(),
            (None, Some(claimed)) => {
                any_claimed = true;
                format!("{claimed}{CLAIMED_SIZE_MARK}")
            }
            (None, None) => "?".to_string(),
        };
        println!(
            "{:>12}  {:<9} {}",
            size,
            entry.source.kind(),
            entry.effective_name()?
        );
        // Recorded by the creator; extraction checks downloads against it.
        if let Some(sha256) = entry.sha256 {
            println!("{:>12}  {:<9} sha256 {}", "", "", hex::encode(sha256));
        }
    }
    if any_claimed {
        println!();
        println!("{CLAIMED_SIZE_MARK} size as recorded by the manifest's creator, not verified");
    }
    Ok(())
}

fn link(manifest: &Manifest, json: bool) -> anyhow::Result<()> {
    let (link, len) = make_link(manifest)?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "link": link,
                "manifest_bytes": len,
                "recommended_max_bytes": MANIFEST_LINK_RECOMMENDED_MAX_BYTES,
            }))?
        );
        return Ok(());
    }
    warn_if_long_link(len);
    println!("{link}");
    Ok(())
}

/// A manifest link and the length of the `.ant` bytes it carries, encoded
/// once.
fn make_link(manifest: &Manifest) -> anyhow::Result<(String, usize)> {
    let bytes = manifest_link_bytes(manifest)?;
    Ok((manifest_link_from_bytes(&bytes), bytes.len()))
}

fn warn_if_long_link(len: usize) {
    if len > MANIFEST_LINK_RECOMMENDED_MAX_BYTES {
        eprintln!(
            "warning: this manifest is {len} bytes; links above \
             {MANIFEST_LINK_RECOMMENDED_MAX_BYTES} bytes are long for a chat message. \
             Consider sharing the .ant file instead."
        );
    }
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
    // Before the default directory is made, so a selection matching nothing
    // leaves nothing behind.
    select_entries(manifest, &selection)?;
    let records = ant_core::config::data_dir()
        .ok()
        .map(|dir| dir.join(DOWNLOAD_RECORDS_DIR));
    let root = match output {
        Some(path) => DownloadRoot { path, record: None },
        None => default_download_root(
            manifest,
            &ant_core::config::download_dir()?,
            records.as_deref(),
        )?,
    };
    let output_root = root.path.clone();
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
        eprintln!("Downloading into {}", output_root.display());
        let (tx, rx) = mpsc::channel(PROGRESS_CHANNEL_CAPACITY);
        let ui = tokio::spawn(drive_extract_progress(rx));
        let report = extract_manifest(client, manifest, &options, Some(tx)).await;
        let _ = ui.await;
        report?
    };
    // Every entry is in place, so there is nothing left to continue.
    if let Some(record) = &root.record {
        if options.selection.is_empty() && report.failed() == 0 && report.cancelled() == 0 {
            let _ = std::fs::remove_file(record);
        }
    }

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "output": output_root.display().to_string(),
                "written": report.written(),
                "present": report.present(),
                "failed": report.failed(),
                "cancelled": report.cancelled(),
                "entries": report.entries,
            }))?
        );
    } else {
        for e in &report.entries {
            match &e.status {
                EntryStatus::Written { bytes } => println!("written   {} ({bytes} bytes)", e.name),
                EntryStatus::Present { bytes } => println!("present   {} ({bytes} bytes)", e.name),
                EntryStatus::Failed { error } => println!("failed    {}: {error}", e.name),
                EntryStatus::Cancelled => println!("cancelled {}", e.name),
            }
        }
        eprintln!(
            "{} written, {} already present, {} failed in {:.1}s -> {}",
            report.written(),
            report.present(),
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
        Some(record) => Ok(record_manifest(record)),
        None => anyhow::bail!(
            "{source} is not a manifest file, a manifest link, or a recorded upload id \
             (see `ant manifest list`)"
        ),
    }
}

/// A recorded upload's manifest, with a reminder of what an unfinished one
/// holds.
fn record_manifest(record: UploadRecord) -> Manifest {
    if record.state == RecordState::Unfinished {
        eprintln!(
            "warning: upload {} has not finished; it holds the {} file(s) uploaded so far",
            record.id,
            record.manifest.entries.len()
        );
    }
    record.manifest
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
                "known_size": entry.known_size(),
                "sha256": entry.sha256.map(hex::encode),
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
    fn hostile_names_are_rejected_before_export_or_download_root_selection() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside");
        for name in [
            "../outside",
            "/tmp/outside",
            "a/b",
            outside.to_str().unwrap(),
        ] {
            let manifest = Manifest {
                name: Some(name.into()),
                torrent: None,
                entries: vec![ant_core::data::ManifestEntry {
                    path: Some("valid.bin".into()),
                    size: None,
                    sha256: None,
                    source: ContentRef::Public { address: [1; 32] },
                }],
            };
            let mut bytes = ant_core::data::manifest::MANIFEST_MAGIC.to_vec();
            bytes.push(ant_core::data::manifest::MANIFEST_FORMAT_VERSION);
            bytes.extend(rmp_serde::to_vec_named(&manifest).unwrap());
            let source = dir.path().join("incoming.ant");
            std::fs::write(&source, bytes).unwrap();
            let action = ManifestAction::Export {
                source: source.to_string_lossy().into_owned(),
                // Keep any regression's writes inside the fixture; the
                // contained absolute-name case exercises default export.
                output: if name == outside.to_str().unwrap() {
                    None
                } else {
                    Some(dir.path().join("export.ant"))
                },
                compact: false,
                yes: false,
                overwrite: false,
                link: false,
            };
            assert!(action.execute_offline(false).is_err());
            assert!(resolve_output(Some(name), None).is_err());
            assert!(default_download_root(&manifest, dir.path(), None).is_err());
        }
        assert!(!outside.with_extension("ant").exists());

        assert_eq!(
            resolve_output(Some("pack"), None).unwrap(),
            PathBuf::from("pack.ant")
        );
        let explicit = dir.path().join("chosen/destination");
        assert_eq!(
            resolve_output(Some("pack"), Some(explicit.clone())).unwrap(),
            explicit
        );
    }

    /// The sender names the manifest, so without --output a download only
    /// ever extracts into a new directory it has just made in the downloads
    /// directory, or into one an unfinished run of the same download made.
    #[test]
    fn a_download_defaults_only_to_a_new_directory_named_after_the_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("Downloads");
        let records = dir.path().join("records");
        let valid = Manifest::new(Some("pack".into()));
        // A missing downloads directory is made.
        let first = default_download_root(&valid, &parent, Some(&records)).unwrap();
        assert_eq!(first.path, parent.join("pack"));
        assert!(first.path.is_dir());
        let record = first.record.expect("the new directory is recorded");
        assert!(record.is_file());

        // The same download again continues into the directory it made.
        let again = default_download_root(&valid, &parent, Some(&records)).unwrap();
        assert_eq!(again.path, first.path);
        assert_eq!(again.record.as_ref(), Some(&record));

        // Another manifest of the same name does not, nor does this one
        // once its download has finished and the record is gone.
        let mut other = Manifest::new(Some("pack".into()));
        other.entries.push(ant_core::data::ManifestEntry {
            path: Some("a.bin".into()),
            size: None,
            sha256: None,
            source: ContentRef::Public { address: [1; 32] },
        });
        std::fs::remove_file(&record).unwrap();
        for manifest in [&other, &valid] {
            let error = default_download_root(manifest, &parent, Some(&records))
                .err()
                .unwrap();
            assert!(error.to_string().contains("already exists"), "{error}");
        }

        // Hidden and unnamed manifests need --output, and nothing is made.
        for manifest in [
            Manifest::new(Some(".ssh".into())),
            Manifest::new(Some("\u{200C}.ssh".into())),
            Manifest::new(Some(".config".into())),
            Manifest::new(None),
        ] {
            let error = default_download_root(&manifest, &parent, Some(&records))
                .err()
                .unwrap();
            assert!(error.to_string().contains("--output"), "{error}");
        }
        assert!(!parent.join(".ssh").exists());
        assert!(!parent.join(".config").exists());
    }

    #[test]
    fn cancellation_reports_original_upload_total_regardless_of_reference_entries() {
        let mut result = ant_core::data::BuildResult {
            manifest: Manifest::new(None),
            cancelled: true,
            skipped_symlinks: vec![],
            files_uploaded: 1,
            total_to_upload: 10,
            chunks_stored: 3,
            storage_cost_atto: 0,
            gas_cost_wei: 0,
            history_id: Some("record-id".into()),
            history_error: None,
        };
        for index in 0..3 {
            result.manifest.entries.push(ant_core::data::ManifestEntry {
                path: Some(format!("file-{index}")),
                size: None,
                sha256: None,
                source: ContentRef::Public {
                    address: [index; 32],
                },
            });
            let out = PathBuf::from("unused.ant");
            assert_eq!(
                build_outcome(Ok(result.clone()), &out)
                    .unwrap_err()
                    .to_string(),
                "cancelled after 1 of 10 file(s); the partial manifest is recorded as record-id"
            );
        }
    }

    #[test]
    fn build_failure_and_cancellation_report_where_the_build_was_recorded() {
        let manifest = Manifest {
            name: Some("partial".into()),
            torrent: None,
            entries: vec![ant_core::data::ManifestEntry {
                path: Some("completed.bin".into()),
                size: Some(10),
                sha256: None,
                source: ContentRef::Public { address: [1; 32] },
            }],
        };
        for cancelled in [false, true] {
            let partial = BuildResult {
                manifest: manifest.clone(),
                cancelled,
                skipped_symlinks: vec![],
                files_uploaded: 1,
                total_to_upload: 2,
                chunks_stored: 3,
                storage_cost_atto: 12,
                gas_cost_wei: 34,
                history_id: Some("20261019T120000Z-partial".into()),
                history_error: None,
            };
            let built = if cancelled {
                Ok(partial)
            } else {
                Err(ManifestError::BuildFailed {
                    path: "later.bin".into(),
                    cause: Box::new(ManifestError::Build("forced failure".into())),
                    partial: Box::new(partial),
                })
            };
            let out = PathBuf::from("unused.ant");
            let error = build_outcome(built, &out).unwrap_err();
            assert!(error
                .to_string()
                .contains("the partial manifest is recorded as 20261019T120000Z-partial"));
            assert!(error.to_string().contains("1 of 2 file(s)"));
            if !cancelled {
                assert!(error.to_string().contains("later.bin"));
                assert!(error.to_string().contains("forced failure"));
            }
            assert!(!out.exists());
        }
    }

    /// A build whose record could not be written, as on a full disk, may have
    /// a journal that lacks its last upload, so the partial manifest goes to
    /// the output file instead, and no record is claimed.
    #[test]
    fn a_stopped_build_that_could_not_be_recorded_is_written_to_the_output() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = Manifest {
            name: Some("partial".into()),
            torrent: None,
            entries: vec![ant_core::data::ManifestEntry {
                path: Some("completed.bin".into()),
                size: Some(10),
                sha256: None,
                source: ContentRef::Public { address: [1; 32] },
            }],
        };
        let partial = BuildResult {
            manifest: manifest.clone(),
            cancelled: false,
            skipped_symlinks: vec![],
            files_uploaded: 1,
            total_to_upload: 2,
            chunks_stored: 3,
            storage_cost_atto: 12,
            gas_cost_wei: 34,
            history_id: Some("20261019T120000Z-partial".into()),
            history_error: Some("No space left on device".into()),
        };
        let out = dir.path().join("partial.ant");
        let built = Err(ManifestError::BuildFailed {
            path: "later.bin".into(),
            cause: Box::new(ManifestError::Build("No space left on device".into())),
            partial: Box::new(partial.clone()),
        });
        let error = build_outcome(built, &out).unwrap_err().to_string();
        assert!(!error.contains("recorded as"), "{error}");
        assert!(
            error.contains(&format!("written to {}", out.display())),
            "{error}"
        );
        assert_eq!(read_manifest_file(&out).unwrap(), manifest);

        // A file already at the output, which may be a complete manifest, is
        // never replaced: the partial manifest goes beside it.
        let complete = Manifest::new(Some("complete".into()));
        write_manifest_file(&out, &complete, true).unwrap();
        let built = Err(ManifestError::BuildFailed {
            path: "later.bin".into(),
            cause: Box::new(ManifestError::Build("No space left on device".into())),
            partial: Box::new(partial.clone()),
        });
        let error = build_outcome(built, &out).unwrap_err().to_string();
        let beside = dir.path().join("partial.partial.ant");
        assert!(
            error.contains(&format!("written to {}", beside.display())),
            "{error}"
        );
        assert_eq!(read_manifest_file(&out).unwrap(), complete);
        assert_eq!(read_manifest_file(&beside).unwrap(), manifest);

        // A finished build keeps its manifest in the output anyway; only
        // the record id is not claimed.
        let (_, history_id) = build_outcome(Ok(partial), &out).unwrap();
        assert!(history_id.is_none());
    }

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
