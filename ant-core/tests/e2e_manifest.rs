//! ADR-0006 end-to-end: build a manifest from a directory, share it as a
//! `.ant` file and as a manifest link, and extract it against a real
//! in-process testnet with Anvil payments.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::fs;
use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::Arc;

use ant_core::data::{
    apply_compaction, extract_manifest, manifest_link, parse_link, plan_compaction,
    publish_data_maps, read_manifest_file, write_manifest_file, BuildOptions, Client, ContentRef,
    EntryStatus, ExtractOptions, Link, ManifestBuilder, PaymentMode, ReferenceMode, Visibility,
};
use serial_test::serial;
use support::{test_client_config, MiniTestnet, DEFAULT_NODE_COUNT};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

/// Payload size for each test file; comfortably above the self-encryption
/// minimum and below one chunk so uploads stay quick.
const FILE_BYTES: usize = 2048;

async fn setup() -> (Client, MiniTestnet) {
    let testnet = MiniTestnet::start(DEFAULT_NODE_COUNT).await;
    let node = testnet.node(3).expect("Node 3 should exist");
    let client = Client::from_node(Arc::clone(&node), test_client_config())
        .with_wallet(testnet.wallet().clone());
    (client, testnet)
}

fn write_tree(root: &Path) {
    fs::create_dir_all(root.join("docs")).unwrap();
    fs::write(root.join("a.bin"), vec![0x11u8; FILE_BYTES]).unwrap();
    fs::write(root.join("docs/b.bin"), vec![0x22u8; FILE_BYTES]).unwrap();
    fs::write(root.join("docs/c.bin"), vec![0x33u8; FILE_BYTES]).unwrap();
}

fn extract_options(output: &Path, selection: Vec<String>) -> ExtractOptions {
    ExtractOptions {
        output_root: output.to_path_buf(),
        selection,
        overwrite: false,
        concurrency: NonZeroUsize::new(2).unwrap(),
        cancel: CancellationToken::new(),
    }
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn manifest_round_trips_through_file_and_link() {
    let (client, testnet) = setup().await;

    let source = TempDir::new().unwrap();
    write_tree(source.path());

    // A public file added by address, uploaded separately.
    let extra = source.path().join("..").join("extra.bin");
    fs::write(&extra, vec![0x44u8; FILE_BYTES]).unwrap();
    let extra_upload = client
        .file_upload_public_with_mode(&extra, PaymentMode::Auto)
        .await
        .expect("public upload");
    let extra_address = extra_upload.data_map_address.expect("public address");

    let mut builder = ManifestBuilder::new(
        &client,
        BuildOptions {
            name: Some("tree".into()),
            reference_mode: ReferenceMode::Embedded,
            visibility: Visibility::Private,
            payment_mode: PaymentMode::Auto,
            follow_symlinks: false,
        },
    );
    builder.add_directory(source.path(), None).unwrap();
    builder
        .add_public(
            extra_address,
            Some("extra/extra.bin".into()),
            Some(FILE_BYTES as u64),
        )
        .unwrap();
    let built = builder.finish(None).await.expect("build manifest");
    assert_eq!(built.files_uploaded, 3);
    assert_eq!(built.manifest.entries.len(), 4);
    assert!(built
        .manifest
        .entries
        .iter()
        .filter(|e| e.path.as_deref() != Some("extra/extra.bin"))
        .all(|e| matches!(e.source, ContentRef::Embedded { .. })));

    // .ant file round trip.
    let ant_path = source.path().join("..").join("tree.ant");
    write_manifest_file(&ant_path, &built.manifest, true).unwrap();
    let from_file = read_manifest_file(&ant_path).unwrap();
    assert_eq!(from_file, built.manifest);

    // Manifest link round trip.
    let link = manifest_link(&built.manifest).unwrap();
    let from_link = match parse_link(&link).unwrap() {
        Link::Manifest(m) => m,
        Link::File(_) => panic!("expected manifest link"),
    };
    assert_eq!(from_link, built.manifest);

    // Full extraction.
    let out = TempDir::new().unwrap();
    let report = extract_manifest(
        &client,
        &from_link,
        &extract_options(out.path(), vec![]),
        None,
    )
    .await
    .expect("extract");
    assert_eq!(report.written(), 4, "{:?}", report.entries);
    assert_eq!(report.failed(), 0);
    assert_eq!(
        fs::read(out.path().join("a.bin")).unwrap(),
        vec![0x11u8; FILE_BYTES]
    );
    assert_eq!(
        fs::read(out.path().join("docs/b.bin")).unwrap(),
        vec![0x22u8; FILE_BYTES]
    );
    assert_eq!(
        fs::read(out.path().join("docs/c.bin")).unwrap(),
        vec![0x33u8; FILE_BYTES]
    );
    assert_eq!(
        fs::read(out.path().join("extra/extra.bin")).unwrap(),
        vec![0x44u8; FILE_BYTES]
    );

    // Selective extraction by directory prefix into a fresh root.
    let partial = TempDir::new().unwrap();
    let report = extract_manifest(
        &client,
        &from_file,
        &extract_options(partial.path(), vec!["docs".into()]),
        None,
    )
    .await
    .expect("partial extract");
    assert_eq!(report.written(), 2);
    assert!(!partial.path().join("a.bin").exists());
    assert!(partial.path().join("docs/b.bin").exists());

    // Existing targets fail per entry without overwrite and nothing else is
    // touched; with overwrite they are replaced.
    let report = extract_manifest(
        &client,
        &from_file,
        &extract_options(out.path(), vec![]),
        None,
    )
    .await
    .expect("second extract runs");
    assert_eq!(report.failed(), 4);
    assert!(report
        .entries
        .iter()
        .all(|e| matches!(&e.status, EntryStatus::Failed { error } if error.contains("exists"))));
    let mut overwrite = extract_options(out.path(), vec!["a.bin".into()]);
    overwrite.overwrite = true;
    let report = extract_manifest(&client, &from_file, &overwrite, None)
        .await
        .expect("overwrite extract");
    assert_eq!(report.written(), 1);

    drop(client);
    testnet.teardown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn compact_mode_uses_public_addresses_only_when_the_datamap_is_on_the_network() {
    let (client, testnet) = setup().await;

    let source = TempDir::new().unwrap();
    fs::write(source.path().join("private.bin"), vec![0x55u8; FILE_BYTES]).unwrap();
    fs::write(source.path().join("public.bin"), vec![0x66u8; FILE_BYTES]).unwrap();

    // Make public.bin's DataMap public ahead of time; private.bin stays
    // private, so compact mode must embed it rather than publish it.
    client
        .file_upload_public_with_mode(&source.path().join("public.bin"), PaymentMode::Auto)
        .await
        .expect("pre-publish");

    let mut builder = ManifestBuilder::new(
        &client,
        BuildOptions {
            name: None,
            reference_mode: ReferenceMode::Compact,
            visibility: Visibility::Private,
            payment_mode: PaymentMode::Auto,
            follow_symlinks: false,
        },
    );
    builder.add_directory(source.path(), None).unwrap();
    let built = builder.finish(None).await.expect("build");

    let kind_of = |name: &str| {
        built
            .manifest
            .entries
            .iter()
            .find(|e| e.path.as_deref() == Some(name))
            .map(|e| e.source.kind())
            .unwrap()
    };
    assert_eq!(kind_of("public.bin"), "public");
    assert_eq!(kind_of("private.bin"), "embedded");

    // The default mode embeds every DataMap, even when the files are
    // uploaded as public in the same run.
    let mut embedded_builder = ManifestBuilder::new(
        &client,
        BuildOptions {
            name: None,
            reference_mode: ReferenceMode::Embedded,
            visibility: Visibility::Public,
            payment_mode: PaymentMode::Auto,
            follow_symlinks: false,
        },
    );
    embedded_builder.add_directory(source.path(), None).unwrap();
    let embedded = embedded_builder.finish(None).await.expect("build embedded");
    assert!(embedded
        .manifest
        .entries
        .iter()
        .all(|e| e.source.kind() == "embedded"));

    let out = TempDir::new().unwrap();
    let report = extract_manifest(
        &client,
        &built.manifest,
        &extract_options(out.path(), vec![]),
        None,
    )
    .await
    .expect("extract");
    assert_eq!(report.written(), 2, "{:?}", report.entries);
    assert_eq!(
        fs::read(out.path().join("public.bin")).unwrap(),
        vec![0x66u8; FILE_BYTES]
    );

    drop(client);
    testnet.teardown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn compact_export_publishes_private_data_maps_then_replaces_them() {
    let (client, testnet) = setup().await;

    let source = TempDir::new().unwrap();
    fs::write(source.path().join("x.bin"), vec![0x77u8; FILE_BYTES]).unwrap();
    fs::write(source.path().join("y.bin"), vec![0x88u8; FILE_BYTES]).unwrap();

    let mut builder = ManifestBuilder::new(&client, BuildOptions::default());
    builder.add_directory(source.path(), None).unwrap();
    let built = builder.finish(None).await.expect("build");
    assert!(built
        .manifest
        .entries
        .iter()
        .all(|e| e.source.kind() == "embedded"));

    // Nothing is public yet, so every entry needs publishing.
    let plan = plan_compaction(&client, &built.manifest)
        .await
        .expect("plan");
    assert!(plan.already_public.is_empty());
    assert_eq!(plan.needs_publish.len(), 2);
    assert!(!plan.is_free());

    let stored = publish_data_maps(&client, &built.manifest, &plan.needs_publish)
        .await
        .expect("publish");
    assert_eq!(stored.len(), 2);

    // A second plan sees them as public, and compaction is free.
    let plan = plan_compaction(&client, &built.manifest)
        .await
        .expect("replan");
    assert!(plan.is_free());
    assert_eq!(plan.already_public.len(), 2);

    let compacted = apply_compaction(&built.manifest, &plan.all_indices()).unwrap();
    assert!(compacted
        .entries
        .iter()
        .all(|e| e.source.kind() == "public"));
    assert!(compacted.encode().unwrap().len() < built.manifest.encode().unwrap().len());

    let out = TempDir::new().unwrap();
    let report = extract_manifest(
        &client,
        &compacted,
        &extract_options(out.path(), vec![]),
        None,
    )
    .await
    .expect("extract compacted");
    assert_eq!(report.written(), 2, "{:?}", report.entries);
    assert_eq!(
        fs::read(out.path().join("y.bin")).unwrap(),
        vec![0x88u8; FILE_BYTES]
    );

    drop(client);
    testnet.teardown().await;
}
