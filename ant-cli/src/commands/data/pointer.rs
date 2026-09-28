//! Pointer commands (ADR-0016 in `ant-node`).
//!
//! A pointer is owned by an ML-DSA-65 key for life, so the key is the one thing
//! these commands ask the user to keep. It is kept as the 32-byte FIPS 204 seed
//! the key pair derives from, hex-encoded in a file only its owner can read.

use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use ant_core::data::{
    ml_dsa_65, pointer_address, Client, MlDsaPublicKey, MlDsaSecretKey, Pointer, PointerTarget,
    PointerTargetKind, XorName,
};
use clap::{Subcommand, ValueEnum};
use rand::rngs::OsRng;
use rand::RngCore;
use serde_json::json;
use tracing::info;

use super::chunk::parse_address;

/// Length of an owner key seed in bytes.
const SEED_LEN: usize = 32;

/// What a pointer's target is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum TargetKind {
    /// A chunk, such as a file's data map.
    Chunk,
    /// Another pointer, which is how a pointer is handed over.
    Pointer,
}

impl From<TargetKind> for PointerTargetKind {
    fn from(kind: TargetKind) -> Self {
        match kind {
            TargetKind::Chunk => Self::Chunk,
            TargetKind::Pointer => Self::Pointer,
        }
    }
}

/// Pointer subcommands.
#[derive(Subcommand, Debug)]
pub enum PointerAction {
    /// Create a new owner key. A pointer's owner can never change, so keep it.
    Keygen {
        /// File to write the key to. Refuses to overwrite an existing file.
        #[arg(long, short)]
        output: PathBuf,
    },
    /// Print the address of the pointer a key owns.
    Address {
        /// Owner key file.
        #[arg(long, short)]
        key: PathBuf,
    },
    /// Create the pointer a key owns, pointing at TARGET. Pays for it.
    ///
    /// Refused, before paying, if the pointer already exists.
    Create {
        /// Owner key file.
        #[arg(long, short)]
        key: PathBuf,
        /// Hex-encoded target address (64 hex chars).
        target: String,
        /// What the target is.
        #[arg(long, value_enum, default_value = "chunk")]
        kind: TargetKind,
    },
    /// Point the pointer a key owns at TARGET. Pays for the new state.
    Update {
        /// Owner key file.
        #[arg(long, short)]
        key: PathBuf,
        /// Hex-encoded target address (64 hex chars).
        target: String,
        /// What the target is.
        #[arg(long, value_enum, default_value = "chunk")]
        kind: TargetKind,
    },
    /// Read a pointer.
    Get {
        /// Hex-encoded pointer address (64 hex chars).
        address: String,
    },
    /// Follow a chain of pointers to the target at its end.
    Resolve {
        /// Hex-encoded pointer address (64 hex chars).
        address: String,
    },
}

impl PointerAction {
    /// Whether this action pays, and so needs a wallet.
    pub fn needs_wallet(&self) -> bool {
        matches!(self, Self::Create { .. } | Self::Update { .. })
    }

    /// Whether this action talks to the network at all.
    pub fn needs_network(&self) -> bool {
        !matches!(self, Self::Keygen { .. } | Self::Address { .. })
    }

    /// Run a command that needs no network.
    pub fn execute_offline(self, json: bool) -> anyhow::Result<()> {
        match self {
            Self::Keygen { output } => {
                let mut seed = [0u8; SEED_LEN];
                OsRng
                    .try_fill_bytes(&mut seed)
                    .map_err(|e| anyhow::anyhow!("No system randomness for a new key: {e}"))?;
                write_key(&output, &seed)?;
                let (owner, _) = ml_dsa_65().generate_keypair_from_seed(&seed);
                let address = hex::encode(pointer_address(&owner));
                let path = output.display();
                if json {
                    println!("{}", json!({ "key": path.to_string(), "address": address }));
                } else {
                    println!("Owner key written to {path}");
                    println!("{address}");
                }
            }
            Self::Address { key } => {
                let (owner, _) = read_key(&key)?;
                let address = hex::encode(pointer_address(&owner));
                if json {
                    println!("{}", json!({ "address": address }));
                } else {
                    println!("{address}");
                }
            }
            other => anyhow::bail!("{other:?} needs the network"),
        }
        Ok(())
    }

    /// Run a command against the network.
    pub async fn execute(self, client: &Client, json: bool) -> anyhow::Result<()> {
        match self {
            Self::Keygen { .. } | Self::Address { .. } => self.execute_offline(json)?,
            Self::Create { key, target, kind } => {
                let (owner, secret) = read_key(&key)?;
                let target = PointerTarget::new(kind.into(), parse_address(&target)?);
                info!("Creating pointer");
                let address = client
                    .pointer_create(&secret, &owner, target)
                    .await
                    .map_err(|e| anyhow::anyhow!("Pointer create failed: {e}"))?;
                print_address(&address, json);
            }
            Self::Update { key, target, kind } => {
                let (owner, secret) = read_key(&key)?;
                let target = PointerTarget::new(kind.into(), parse_address(&target)?);
                info!("Updating pointer");
                let address = client
                    .pointer_update(&secret, &owner, target)
                    .await
                    .map_err(|e| anyhow::anyhow!("Pointer update failed: {e}"))?;
                print_address(&address, json);
            }
            Self::Get { address } => {
                let at = parse_address(&address)?;
                let record = client
                    .pointer_get(&at)
                    .await
                    .map_err(|e| anyhow::anyhow!("Pointer get failed: {e}"))?
                    .ok_or_else(|| anyhow::anyhow!("No pointer at {address}"))?;
                print_record(&record, json);
            }
            Self::Resolve { address } => {
                let at = parse_address(&address)?;
                let target = client
                    .pointer_resolve(&at)
                    .await
                    .map_err(|e| anyhow::anyhow!("Pointer resolve failed: {e}"))?;
                let (kind, target) = describe_target(&target);
                if json {
                    println!("{}", json!({ "kind": kind, "target": target }));
                } else {
                    println!("{target}");
                }
            }
        }
        Ok(())
    }
}

fn print_address(address: &XorName, json: bool) {
    let address = hex::encode(address);
    if json {
        println!("{}", json!({ "address": address }));
    } else {
        println!("{address}");
    }
}

fn print_record(record: &Pointer, json: bool) {
    let address = hex::encode(record.address());
    let state = hex::encode(record.state_id());
    let counter = record.counter();
    let (kind, target) = describe_target(&record.target());
    if json {
        println!(
            "{}",
            json!({
                "address": address,
                "counter": counter,
                "kind": kind,
                "target": target,
                "state_id": state,
            })
        );
    } else {
        println!("address:  {address}");
        println!("counter:  {counter}");
        println!("kind:     {kind}");
        println!("target:   {target}");
        println!("state_id: {state}");
    }
}

/// A target's kind and hex address, for printing. A tag this build does not
/// know is printed as its number rather than hidden.
fn describe_target(target: &PointerTarget) -> (String, String) {
    let kind = match target.kind() {
        Some(PointerTargetKind::Chunk) => "chunk".to_string(),
        Some(PointerTargetKind::Pointer) => "pointer".to_string(),
        None => {
            let tag = target.kind_tag();
            format!("unknown({tag})")
        }
    };
    (kind, hex::encode(target.address))
}

/// Write a new key file readable only by its owner, refusing to replace one:
/// a lost owner key is a pointer that can never be updated again.
///
/// On Unix the file is created `0600`. Elsewhere it takes the permissions of
/// the directory it is written to, so keep it somewhere only you can read,
/// such as your user profile.
fn write_key(path: &Path, seed: &[u8; SEED_LEN]) -> anyhow::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let display = path.display();
    let mut file = options
        .open(path)
        .map_err(|e| anyhow::anyhow!("Cannot create key file {display}: {e}"))?;
    let encoded = hex::encode(seed);
    writeln!(file, "{encoded}")?;
    file.sync_all()?;
    // The file's name is only durable once its directory is: without this a
    // crash just after "written" could lose a key a pointer now depends on.
    #[cfg(unix)]
    {
        let parent = path
            .parent()
            .filter(|dir| !dir.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        std::fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

/// Read a key file back into the owner's key pair.
fn read_key(path: &Path) -> anyhow::Result<(MlDsaPublicKey, MlDsaSecretKey)> {
    let display = path.display();
    let text = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("Cannot read key file {display}: {e}"))?;
    let bytes = hex::decode(text.trim())
        .map_err(|e| anyhow::anyhow!("Key file {display} is not hex: {e}"))?;
    let seed: [u8; SEED_LEN] = bytes.try_into().map_err(|bytes: Vec<u8>| {
        let len = bytes.len();
        anyhow::anyhow!("Key file {display} holds {len} bytes, expected {SEED_LEN}")
    })?;
    Ok(ml_dsa_65().generate_keypair_from_seed(&seed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Debug, Parser)]
    struct TestPointerCli {
        #[command(subcommand)]
        action: PointerAction,
    }

    #[test]
    fn a_key_file_reads_back_to_the_same_owner() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("owner.key");
        let seed = [7u8; SEED_LEN];
        write_key(&path, &seed).unwrap();

        let (owner, _) = read_key(&path).unwrap();
        let (expected, _) = ml_dsa_65().generate_keypair_from_seed(&seed);
        assert_eq!(pointer_address(&owner), pointer_address(&expected));
    }

    #[test]
    fn keygen_never_overwrites_a_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("owner.key");
        write_key(&path, &[1u8; SEED_LEN]).unwrap();
        assert!(write_key(&path, &[2u8; SEED_LEN]).is_err());

        let (owner, _) = read_key(&path).unwrap();
        let (first, _) = ml_dsa_65().generate_keypair_from_seed(&[1u8; SEED_LEN]);
        assert_eq!(pointer_address(&owner), pointer_address(&first));
    }

    #[cfg(unix)]
    #[test]
    fn a_key_file_is_readable_only_by_its_owner() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("owner.key");
        write_key(&path, &[3u8; SEED_LEN]).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn a_key_file_of_the_wrong_length_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("short.key");
        std::fs::write(&path, "abcd").unwrap();
        assert!(read_key(&path).is_err());
    }

    #[test]
    fn only_writes_need_a_wallet_and_only_key_commands_skip_the_network() {
        let key = "owner.key";
        let target = "00".repeat(32);
        let parse = |args: &[&str]| TestPointerCli::try_parse_from(args).unwrap().action;

        let create = parse(&["t", "create", "--key", key, &target]);
        assert!(create.needs_wallet() && create.needs_network());
        let update = parse(&["t", "update", "--key", key, &target, "--kind", "pointer"]);
        assert!(update.needs_wallet() && update.needs_network());
        let get = parse(&["t", "get", &target]);
        assert!(!get.needs_wallet() && get.needs_network());
        let resolve = parse(&["t", "resolve", &target]);
        assert!(!resolve.needs_wallet() && resolve.needs_network());
        let keygen = parse(&["t", "keygen", "--output", key]);
        assert!(!keygen.needs_wallet() && !keygen.needs_network());
        let address = parse(&["t", "address", "--key", key]);
        assert!(!address.needs_wallet() && !address.needs_network());
    }

    #[test]
    fn a_target_kind_maps_to_its_wire_kind() {
        assert_eq!(
            PointerTargetKind::from(TargetKind::Chunk),
            PointerTargetKind::Chunk
        );
        assert_eq!(
            PointerTargetKind::from(TargetKind::Pointer),
            PointerTargetKind::Pointer
        );
    }
}
