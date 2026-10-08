# ADR-0006: File manifests and manifest links

- **Status:** Proposed
- **Date:** 2026-09-23
- **Last amended:** 2026-10-08
- **Decision owners:** Mick
- **Reviewers:** <pending>
- **Supersedes:** none
- **Superseded by:** none
- **Related:** ADR-0004 (public file identity is the DataMap address);
  Linear V2-1478; [PR #219](https://github.com/WithAutonomi/ant-client/pull/219)

## Context

Autonomi addresses one file at a time. A public file is the address of its
DataMap chunk; a private file is a locally held DataMap. There is no way to
describe a set of files, with names and paths, as one shareable thing.

BitTorrent solves this with a `.torrent` file that lists files by path and
size, and a magnet link from which the client fetches that description. Both
also carry trackers and peers because BitTorrent must find who has the data.
Autonomi does not: every chunk lives at a content-derived address the client
can reach on its own. An Autonomi equivalent only needs to describe *what* the
files are and *how* to resolve each one.

Protocol facts this design depends on:

- A public file's DataMap is stored as a chunk of self_encryption's
  versioned msgpack bytes. Its address is the hash of those bytes, so an
  address commits to the DataMap and, through it, to every chunk of the
  file. No separate per-file hash is needed for integrity.
- Self-encryption shrinks any DataMap with more than three chunks into a
  *child* map of at most three entries, by encrypting the serialised map as
  ordinary content and storing the resulting *wrapper* chunks. A child map is
  small, but a reader must fetch the wrapper chunks to recover the root map
  before fetching data, and a child map's own size field describes the
  wrapper, not the file.
- Self-encryption is deterministic: shrinking the same root map always
  yields the same child map. Anyone holding a root map can compute, locally,
  the child map an upload of that file published and therefore its address.
- The msgpack byte `0xC1` is unused by the specification and can never begin
  a valid msgpack document, so it can never begin a DataMap.

## Decision Drivers

- One artefact describes many files, with optional relative paths. Names are
  metadata, not identity.
- Each entry resolves from an embedded DataMap or from a public DataMap
  address, and one manifest may mix both.
- A manifest is shared off the network, as a file or as a link carrying its
  own bytes. It is never stored on the network.
- A link to a plain public file uses the same scheme.
- No peer or tracker information.
- Recipients choose which entries to download.
- Extraction never writes outside the chosen output directory, on any
  platform, regardless of where the manifest was created or what the output
  directory already contains.
- Optional fields can be added later without breaking older readers.
- Stored chunk and DataMap formats, addresses, payments and node behaviour do
  not change.

## Considered Options

1. **Two archive types, one for public addresses and one for private
   DataMaps** (the previous-generation design). Rejected: forces an
   all-public or all-private choice and has no link form or selective
   download.
2. **A single-file envelope around a DataMap with a filename.** Rejected as
   the primary design: it is a manifest with one entry.
3. **Tar the directory and upload it as one file.** Rejected: no selective
   download, no per-file deduplication, no referencing already-public files
   without re-uploading them.
4. **A manifest that is itself published to the network and resolved by
   address.** Rejected for now: a public address cannot reveal whether it is
   a file or a manifest without fetching it, any file may legitimately
   contain manifest bytes, and automatic resolution of untrusted addresses
   needs its own resource limits. See Deferred.
5. **A typed manifest with per-entry content references, shared off-network
   as a file or a self-contained link (chosen).**

## Decision

We will describe a set of files with a typed, versioned **manifest** that
lists each file's optional path and size and either embeds its DataMap or
references its public DataMap address; share it off the network as a `.ant`
file or an `ant://manifest/` link carrying the same bytes; derive every
entry's identity from its content so nothing in a manifest can misstate it;
and record every upload the client performs as a manifest in a local upload
history.

### Data model

```rust
pub struct Manifest {
    /// Suggested root directory name. One path component, subject to the
    /// portable path rules.
    pub name: Option<String>,
    /// The BitTorrent identity of the same files, when the creator has
    /// one. Carried now; acted on later (see Deferred).
    pub torrent: Option<TorrentReference>,
    /// Sorted by effective name. Effective names are unique.
    pub entries: Vec<ManifestEntry>,
}

/// A BitTorrent info hash (BEP 3 v1 SHA-1, BEP 52 v2 SHA-256). At least
/// one must be present when the reference is.
pub struct TorrentReference {
    pub info_hash_v1: Option<[u8; 20]>,
    pub info_hash_v2: Option<[u8; 32]>,
}

pub struct ManifestEntry {
    /// Relative path, subject to the portable path rules. When absent the
    /// entry is extracted under its content address.
    pub path: Option<String>,
    /// Plaintext length in bytes as recorded by the creator. A hint for
    /// display only; nothing is decided by it.
    pub size: Option<u64>,
    /// Where the bytes come from.
    pub source: ContentRef,
}

pub enum ContentRef {
    /// The DataMap itself, root or shrunk. Saves the DataMap fetch a
    /// `Public` entry needs.
    Embedded { data_map: DataMap },
    /// Address of the file's public DataMap chunk.
    Public { address: [u8; 32] },
}
```

`name`, `torrent`, `path` and `size` are optional metadata. `source` and the
payload of whichever variant it holds are required. A manifest that is only
a list of content references is valid. `size` is the creator's claim and is
never verified; where an exact size is needed it comes from a root DataMap,
which lists every chunk's plaintext length.

An embedded DataMap may be either form a DataMap takes: the **root** map that
lists the file's data chunks, or the **shrunk** child map a large upload
publishes, which points at wrapper records holding the root. Readers accept
both, as they do for any DataMap.

**Identity.** Each entry has a **content address**, always derived from its
content and never stored beside it:

- `Public`: the address itself.
- `Embedded`: the address of the DataMap's **published form**, which is the
  map shrunk locally, exactly as an upload shrinks it, until it lists at
  most three chunks (a no-op for a map that small already), serialised as
  self_encryption's versioned msgpack and hashed. This is the address a
  public upload of the file stores its DataMap at.

A root map and the shrunk map it came from therefore have the same content
address, a private upload and a public upload of the same bytes have the same
content address, and a manifest's author cannot attach an address to an
embedded DataMap that does not belong to it. The rule depends on
self_encryption's shrinking and DataMap serialisation, which already define
every public file's address on the network; a change to either changes
public addresses network-wide and is outside this ADR.

Each entry has an **effective name**: its `path` when present, otherwise the
64-character lowercase hex of its content address.

**Root embedding.** A `.ant` file embeds the root map whenever the root, as
written in the manifest (see the schema below), is at most
`MAX_EMBEDDED_ROOT_MAP_BYTES` (64 KiB): 68 bytes per chunk of up to about
4 MiB, so files up to about 4 GB. A reader holding the root starts fetching
data chunks with no wrapper-record fetches at all. Above the cap the shrunk
map is embedded. A manifest link always carries the published form instead
(see Links), so link length does not grow with file size.

### Encoding

The byte form is:

```
C1 41 4E 54   magic ("\xC1ANT")
01            format version
...           named msgpack encoding of Manifest
```

The header byte is the only version field. A decoder requires the complete
five-byte header; the first byte alone is enough to tell a `.ant` file from a
DataMap file, but never enough to accept one.

**Schema (normative, version 1).** The body is one msgpack value. Every
struct is a msgpack map keyed by the field names below, as `str`. Keys are
written in the order listed. An optional field that is absent is **omitted**,
never written as `nil`; a reader also accepts `nil` for it.

```
Manifest      map {
                "name":    str            optional
                "torrent": Torrent        optional
                "entries": array of Entry required; may be empty
              }
Torrent       map {
                "info_hash_v1": bin 20    optional
                "info_hash_v2": bin 32    optional; at least one present
              }
Entry         map {
                "path":   str             optional
                "size":   uint            optional
                "source": ContentRef      required
              }
ContentRef    map with exactly one key, the variant name:
                "Embedded": map { "data_map": DataMap }
                "Public":   map { "address": bin 32 }
DataMap       map {
                "chunks": bin             required; n * 68 bytes, n >= 1
                "child":  uint            optional; the shrink level,
                                          present only on a shrunk map
              }
```

`chunks` is the DataMap's chunk list as fixed 68-byte records in chunk
order. Each record is the post-encryption hash (32 bytes), the
pre-encryption hash (32 bytes) and the plaintext size (big-endian `u32`). A
chunk's index is its position, so a DataMap whose indices are not
contiguous from zero cannot be written. This layout belongs to the manifest
format, not to self_encryption: manifest bytes do not change when that
crate's serialisation does, and hashes are raw bytes, not integer arrays.
Adding a field to self_encryption's chunk description, or any other change
to what a DataMap must carry, is a new manifest format version.

Forward compatibility rules, binding within a format version:

- Structs and enum variant bodies are maps, never positional arrays. A
  decoder rejects an array wherever the schema has a struct or a variant.
- Every optional field has a default and is filled in when absent.
- Unknown fields are ignored at every level, including inside a known
  `ContentRef` variant and inside `DataMap`.
- `ContentRef` is externally tagged by variant name. An unknown variant is a
  hard decode error; a client must never silently skip a file it cannot
  resolve. An unknown header version is a hard decode error.
- Only optional fields and new `ContentRef` variants may be added within a
  version. A required field, a changed meaning, or a removal is a new version.

Decoding untrusted bytes is bounded. Input over `MAX_MANIFEST_BYTES`
(64 MiB) is rejected before anything is parsed, and that limit bounds every
later allocation. A structural pass then walks the msgpack without building
any typed value: it enforces the nesting depth, rejects positional
encodings, and rejects an entry array longer than `MAX_MANIFEST_ENTRIES`
(100,000) from its header alone. Only then is the manifest decoded into
types and its paths validated. Exceeding any limit is a decode error.
Manifest-link payload lengths are checked before base64 decoding, and the
decode buffer uses the exact unpadded decoded length within the same limit.

Encoding is deterministic: entries sorted by effective name, fields in the
order above, absent fields omitted, nothing time-dependent. The same tree
manifested with the same options yields the same bytes and therefore the
same link. A committed byte fixture pins the encoding.

A local manifest file uses the extension `.ant` and contains exactly these
bytes. It is written atomically (tempfile, then rename).

### Portable path rules

A manifest is created on one platform and extracted on any other, so validity
cannot depend on the platform performing the check. One rule set applies to
every `path` and to `name`. The same code enforces it when a manifest is
built, when one is decoded from any source, and when target paths are computed
before extraction. A violation rejects the whole manifest.

A path is valid when all of the following hold:

- Valid UTF-8, with `/` as the only separator. `\` is a forbidden character,
  never a separator.
- No leading `/`, no empty component, no component equal to `.` or `..`.
- No component contains NUL, a control character (below `0x20`, or `0x7F`),
  or any of `< > : " | ? * \`.
- No component ends with a space or a `.`.
- No component, with any extension and the spaces before it removed,
  equals a Windows reserved device name ignoring case: `CON`, `PRN`, `AUX`,
  `NUL`, `CONIN$`, `CONOUT$`, `COM1`–`COM9`, `COM¹`–`COM³`, `LPT1`–`LPT9`
  and `LPT¹`–`LPT³`. So `CON .txt` is rejected as well as `CON.txt`.
- Each component is at most `MAX_PATH_COMPONENT_BYTES` (255) bytes; the whole
  path is at most `MAX_PATH_BYTES` (1024) bytes.
- Effective names are compared under a folded key: NFC normalisation, then
  uppercasing followed by lowercasing, then NFC again. Lowercasing alone
  misses pairs that APFS and NTFS merge because their uppercase forms are
  equal (`ſ` and `s`, `ς` and `σ`, `ı` and `i`); going through uppercase
  joins them, and the final normalisation recomposes anything case mapping
  decomposed. The key may reject a few pairs a given filesystem would keep
  apart (`ß` and `ss`), which is the safe direction. Under that comparison
  no two effective names are equal, and none is a directory prefix of
  another: `a` and `a/b` cannot both be entries, and neither can `A` and
  `a/b`.

`name` obeys the single-component rules and cannot contain either `/` or
`\`. Unnamed entries always pass.

These rules reject some trees that are legal where they were created. That is
the intended trade: a manifest that passes is extractable everywhere the
client runs. Local failures at extract time, such as a stricter platform path
limit, are per-entry failures, not manifest rejections.

### Links

A link uses the `ant` scheme. Two forms, told apart by the authority:

```
ant://<64 hex characters>
ant://manifest/<unpadded base64url of the .ant bytes>
```

**File link.** The authority is the public DataMap address of a file, and
nothing else. A parser rejects any query string or path on this form, so the
grammar stays reserved for a later decision. A bare hex address is accepted
wherever a link is. A file link downloads with no further input; the output
name defaults to the hex address. A file link never resolves to a manifest;
its bytes are always written as a file.

**Manifest link.** The authority is the literal `manifest`; the path is the
complete `.ant` bytes, header included, as unpadded base64url. It is the
`.ant` file in link form and decodes under the same rules and limits. A hex
address can never equal `manifest`, and the reserved authority leaves room
for later forms.

A manifest link carries the manifest's **link form**: every embedded DataMap
replaced by its published form, at most three chunk records. That is
lossless: each entry keeps its content address, and a reader recovers the
root from the wrapper records the upload stored. It keeps an embedded entry
at a fixed size whatever the file's size; a `.ant` file keeps the root maps.
Decoding a link and saving it as a `.ant` file keeps the link form.

Manifest links grow with the number of entries. DataMap bytes are hashes,
which do not compress, so no compression layer is added. Measured on the
encoding above, an embedded entry is about 250 bytes, roughly 330 link
characters, plus about 1.35 characters per byte of its path. A `Public`
entry is about 60 bytes, roughly 80 characters, plus its path. The committed
browser fixture, one embedded entry with a short name and path, is a
411-character link. Producing tools warn above
`MANIFEST_LINK_RECOMMENDED_MAX_BYTES` (1,500 encoded bytes, about a
2,000-character link) and suggest a `.ant` file instead. That fits about
five embedded entries or about eighteen `Public` entries with short paths.

### Privacy model

A manifest is never stored on the network, so its privacy is that of the
channel it is shared over. What it reveals depends on its entries:

| Entries    | Effect                                                              |
|------------|---------------------------------------------------------------------|
| `Embedded` | private share: the manifest bytes are the key to the files          |
| `Public`   | index over already-public files; the manifest adds their names, paths and association |

Whether the manifest travels as a `.ant` file or a manifest link makes no
difference. The manifest itself is not encrypted; that is a later decision.

### Creation

A builder walks a directory or takes explicit (path, source) pairs. Local
files are uploaded through the ordinary file upload with the visibility the
caller chose for them; already-public files are added by address, with
optional path and size, and nothing is uploaded. A public file can also be
added with its DataMap fetched from the network and embedded, which costs
nothing and lets recipients skip the DataMap fetch.

Two reference modes for uploaded files:

- **Embedded (default).** The DataMap goes into the manifest, whether or not
  the file was uploaded as public, as the root map when it fits the cap
  above. The recipient then skips the DataMap fetch and the wrapper-record
  fetches, and the creator pays nothing extra: resolving the root reads the
  wrapper records the upload just stored. Only entries added by address have
  no DataMap to embed. A public file added with its DataMap fetched and
  embedded must derive its given address under the identity rule; a
  DataMap stored in some other form is added by address instead.
- **Compact (opt-in).** Record `Public` for every file whose DataMap chunk is
  on the network: files added by address, files the caller uploaded as
  public in this run, and files whose DataMap chunk is found to exist
  already. Every other file stays `Embedded`. Compact mode never stores a
  DataMap chunk itself; making a file public is only ever the caller's
  explicit upload choice. Compact is what makes a manifest link fit in a
  message for more than a handful of files.

Everything that could make the finished manifest invalid, such as file sizes,
the name and path collisions, is checked before the first paid upload, and
the output location is checked before it too. Cancellation stops between
files or abandons the upload in flight and returns the partial manifest,
which the client records in the upload history so paid uploads are not lost.
Ordinary file failures also preserve the partial result in
`ManifestError::BuildFailed`; the CLI records it before reporting failure.
Each successful upload's DataMap is retained before reference optimisation,
so a root-map resolution failure cannot discard that upload. Partial results
carry the original number of local files to upload, excluding entries added
by address or an existing DataMap, for accurate cancellation/error counts.

Directory walks sort by path, use paths relative to the root, and record
`size` for every uploaded file. Symlinks are skipped and reported, whether
they point at files or directories; an opt-in flag follows symlinks to
regular files and records the target's contents. Empty directories are not
representable: a directory exists only because a file path passes through it.
File permissions and timestamps are not recorded.

Per-file payments are unchanged by this decision.

### Upload history

Every upload the client performs is also recorded as a manifest, so no
upload is ever remembered only by a printed address or a loose DataMap file.
A single-file upload becomes a one-entry manifest; a manifest build is
recorded as itself. Records live under `<data dir>/uploads/` as ordinary
`.ant` files named `<UTC timestamp>-<label>.ant`, where the filename stem is
the record id. When that id is taken, as when uploads with the same label
finish within one second, `-2`, `-3` and so on are appended. Nothing else is
stored, so the history is a plain folder of manifests. The client lists
records newest first, ordering ids within one second by their suffix as a
number, and accepts a record id wherever it accepts a manifest file or link:
`show`, `link`, `export` and `download`. Every record embeds the full
DataMap, whether or not the upload was public. A file name that breaks the
portable path rules is repaired for the record, or dropped so the entry
extracts under its content address, rather than losing the record. Recording never fails an
upload that has already succeeded; an unusable history directory is reported
and the upload result still shown.

### Export and compaction

Any manifest the client can load, from a file, a link or the upload history,
can be exported as a `.ant` file. Export can also **compact** the manifest:
each embedded entry is replaced by `Public` with its content address. Since
that address is derived from the embedded DataMap, compaction can never
point an entry at other content, whoever wrote the manifest. It is possible
only for entries whose published DataMap chunk is on the network, so
compaction is planned first: every embedded entry is checked and sorted into
already public or still private; the checks run concurrently and honour
cancellation. The output location is checked before anything is published.
If any are still private the user is told which, and asked whether to
publish them. Publishing stores each entry's published form, the same chunk
a public upload of the file would have stored, so a file has one public
identity however it was uploaded. Before anything is paid for, every entry
to publish is re-derived and checked against the plan, and when its
published form is a shrunk map, the wrapper records it points at must be on
the network or the entry cannot be published. Publishing is paid and makes
those files public; it never happens without an explicit yes, and a
non-interactive run must pass that yes as a flag. Declining leaves the
manifest untouched. Entries already referenced by address are unaffected.

### Download and extraction

Input: a manifest, an output root, and an optional selection by exact
effective name or directory prefix. Empty selection means everything.

Before any network access, every effective name is re-validated against the
portable path rules. Any violation rejects the manifest.

Containment is a filesystem contract, not a string check. Within the output
root the extractor never follows a symlink or a junction, that is, anything
the platform's no-follow metadata reports as a link (on Windows, the
name-surrogate reparse points): each
intermediate directory is created, or confirmed to be a real directory, by a
non-following metadata check immediately before use, and the file is
downloaded into a reserved temporary name in its parent directory and renamed
into place with no-clobber semantics. The target is checked again, without
following links, immediately before the rename. An entry whose path would
traverse a link, or whose target already exists, is a per-entry failure
unless overwrite was requested, in which case only a regular file at the
exact target is replaced, never a link. Handle-relative (resolve-beneath)
opening would close the remaining check-to-use window against a concurrent
local attacker and is a later hardening step, not part of this decision.

Per entry: resolve the DataMap (`Public` needs one fetch), resolve it to its
root (a shrunk map needs its wrapper records), and check that the target's
filesystem has room for the root's exact plaintext size before downloading;
too little space is a per-entry failure. Then download through the ordinary
file download and write as above. The recorded `size` decides nothing; tools
show it marked as the creator's claim, and show the exact size instead
wherever an embedded root map provides one. Extracted files get the
platform's default permissions.

A bounded number of entries are in flight at once, one by default. Results
are per entry: written, failed with its error, or cancelled. One failure does
not abort the rest. The operation reports progress at manifest and file level
and accepts a cancellation token; cancelling abandons the in-flight download
and marks every unstarted entry cancelled.

### Surface

The library exposes: encode, decode, validate, link parse and format, build,
and extract. Encode, decode, validate and link handling are portable and
build for the browser. The browser bindings export two functions:
`parseManifestLink`, for a manifest link, a file link or a bare address,
and `decodeManifest`, for `.ant` bytes. Each returns entries with their
effective name, content address, claimed and exact size, and an embedded
entry's DataMap bytes, which feed the existing private-file download
directly. Creating manifests in the browser is not exported. Build and
extract need a filesystem. The CLI exposes create, list, show, link, export
and download for manifests, and its ordinary file download accepts a file
link or a bare address. No daemon API is added.

## Consequences

### Positive

- A folder is one shareable artefact, as a file or a self-contained link,
  and the recipient picks what to fetch.
- Every upload is recoverable later from the upload history, by id, without
  the user having kept an address or a DataMap file.
- A manifest is never stored on or resolved from the network, so sharing
  one costs nothing and there is no ambiguity about what an address is.
  Network access and payment come only from the files: uploading them,
  checking which DataMaps are already public in compact mode, and
  publishing DataMaps when the user explicitly agrees to compaction.
- Mixed references let one manifest embed private files and point at public
  ones. An index over existing public data costs nothing.
- Integrity, deduplication and payment are inherited from DataMaps and
  chunks. No node change.
- Deterministic encoding gives identical manifests identical links.

### Negative / Trade-offs

- `Public` entries cost one extra fetch each; compact mode is therefore
  opt-in.
- Named msgpack is larger than positional by the field names, about 40
  bytes per entry.
- An embedded entry's content address costs one local encryption of its
  DataMap (at most 64 KiB) to derive, whenever it is needed.
- A manifest cannot be referenced by a short address; a large manifest must
  travel as a file.
- The portable path rules reject some trees that are legal locally.
- Empty directories and symlinks are not representable.
- Manifest links with embedded DataMaps fit about five files per message.
- A link's embedded entries carry the shrunk map, so a recipient of a large
  file fetches its wrapper records before its data, which a `.ant` file
  with the root map avoids.
- Existing files in the output directory are never reused; a re-download
  fails per entry or overwrites on request.
- A new format is a new compatibility surface; the version byte and the
  forward-compatibility rules bound it.

### Neutral / Operational

- No change to wire protocol, stored chunks, DataMap chunk bytes, addresses
  or payments. The manifest's DataMap layout exists only inside manifests.
- `.ant` and `ant://` are decided; changing either would not touch the bytes.
- Unnamed entries extract under their hex address.
- The upload history holds the DataMaps of private uploads in plain files
  under the data directory, like `.datamap` files do today; protecting that
  directory is the user's responsibility.

## Validation

- Encode/decode round trip; full-header and version rejection; decode limits
  on size, entry count (from the array header, before typed decoding) and
  nesting; deterministic bytes for reordered input; link parse and format
  for both forms, bare addresses, base64url, rejection of a query string or
  path on a file link, and rejection of a manifest-link payload without the
  full header.
- A committed golden byte fixture of a v1 manifest with an embedded and a
  `Public` entry; any change to it is a format change. Further fixtures:
  extra unknown fields at every level decode; a manifest missing every
  optional field decodes to defaults; absent optionals are omitted; a
  variant missing its payload, an unknown variant and an unknown version are
  rejected; a positional (array) body, entry or variant is rejected; the
  DataMap layout round-trips root and shrunk maps and rejects truncated,
  empty and gapped chunk lists.
- Identity: a root map and its shrunk map derive the same content address;
  on a local network, a real multi-chunk public upload's published address
  equals the address derived from its root, and compaction finds an
  embedded root of that file already public. The link form keeps every
  content address.
- Portable path tests, run on Unix and macOS in CI: every rule above,
  positive and negative, including the extended reserved names and the
  case-folding pairs above.
- Containment tests, run on Unix and macOS in CI: a pre-existing symlink at
  every depth inside the output root causes a per-entry failure and nothing
  is written outside the root; an existing target fails without overwrite
  and is replaced only when it is a regular file; a link at the target is
  never followed even with overwrite. Windows uses the same no-follow
  metadata checks, but CI has no Windows unit-test runner. Junction tests
  are to be added with one.
- End-to-end on a local network: build a manifest with an embedded entry, a
  compact entry and an entry added by address; extract it from a `.ant` file
  and from a manifest link; extract a selection; verify bytes and per-entry
  results; download a file link through the ordinary file download.
- Review trigger: any new `ContentRef` variant, or any change to the header,
  path rules or containment contract, requires an amending or superseding
  ADR.

## Deferred

- **Publishing manifests to the network.** Storing a manifest as a chunk and
  resolving it by address. Deferred because an address cannot indicate what
  it points at, legitimate files can contain manifest bytes, and resolving
  untrusted addresses automatically needs size and depth limits of its own.
  A future ADR must define how intent is conveyed and what is fetched before
  a decision is made.
- **Inline bytes for tiny files.** A `ContentRef` variant carrying a small
  file's plaintext. The enum admits it later without a format break.
- **Manifest-wide Merkle payment.** One batch for every file of a manifest.
  Orthogonal to the format.
- **BitTorrent interoperability.** The format already carries the torrent
  info hash of the same files, so a manifest and a torrent describing one
  release can be matched. What a client does with it, such as cross-seeding,
  verifying a torrent's pieces against Autonomi chunks, or importing a
  torrent's file list into a manifest, is a later ADR. Until then the hash
  is recorded by the creator and displayed, nothing more.

## Open questions for review

- Are the constants right? `MANIFEST_LINK_RECOMMENDED_MAX_BYTES` (1,500)
  fits about five embedded or eighteen `Public` entries in a
  2,000-character message. `MAX_EMBEDDED_ROOT_MAP_BYTES` (64 KiB) makes a
  `.ant` file grow by up to 64 KiB per file of up to 4 GB. `MAX_PATH_BYTES`
  (1,024) exceeds what Windows without long-path support can extract (260
  characters for the absolute path), which shows up as a per-entry failure
  there. Review settles them; changing any of them needs no format change.

## Notes for AI-assisted work

AI tools may help draft this ADR, but **must not mark it Accepted without
human review**. Accepted ADRs are immutable: create a new superseding ADR
rather than editing an Accepted ADR.
