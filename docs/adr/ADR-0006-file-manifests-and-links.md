# ADR-0006: File manifests and manifest links

- **Status:** Proposed
- **Date:** 2026-09-23
- **Decision owners:** Mick
- **Reviewers:** <pending>
- **Supersedes:** none
- **Superseded by:** none
- **Related:** ADR-0004 (public file identity is the DataMap address)

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

- A DataMap is msgpack bytes. Stored as a chunk, its address is the hash of
  those bytes, so an address commits to the DataMap and, through it, to every
  chunk of the file. No separate per-file hash is needed for integrity.
- Self-encryption shrinks any DataMap with more than three chunks into a
  *child* map of at most three entries. A child map is small, but a reader
  must fetch wrapper chunks to recover the root map before fetching data, and
  a child map's own size field describes the wrapper, not the file.
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

### Data model

```rust
pub struct Manifest {
    /// Suggested root directory name. One path component, subject to the
    /// portable path rules.
    pub name: Option<String>,
    /// Sorted by effective name. Effective names are unique.
    pub entries: Vec<ManifestEntry>,
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
    /// The DataMap itself. Saves the DataMap fetch a `Public` entry needs.
    Embedded { data_map: DataMap },
    /// Address of the file's public DataMap chunk, encoded as msgpack bin.
    Public { address: [u8; 32] },
}
```

`name`, `path` and `size` are optional metadata. `source` and the payload of
whichever variant it holds are required. A manifest that is only a list of
content references is valid.

Each entry has a **content address**: for `Public` the address itself; for
`Embedded` the hash of the DataMap's canonical bytes, which are exactly the
bytes a public DataMap chunk contains (its versioned positional msgpack
form). Each entry has an **effective name**: its `path` when present,
otherwise the 64-character lowercase hex of its content address.

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

Forward compatibility rules, binding within a format version:

- Structs and enum variant bodies are encoded as msgpack **maps with field
  names**, never positional arrays.
- Every optional field has a default and is filled in when absent.
- Unknown fields are ignored at every level, including inside a known
  `ContentRef` variant.
- `ContentRef` is externally tagged by variant name. An unknown variant is a
  hard decode error; a client must never silently skip a file it cannot
  resolve. An unknown header version is a hard decode error.
- Only optional fields and new `ContentRef` variants may be added within a
  version. A required field, a changed meaning, or a removal is a new version.

Decoding untrusted bytes is bounded before allocation: at most
`MAX_MANIFEST_BYTES` input, at most `MAX_MANIFEST_ENTRIES` entries, and a
fixed msgpack nesting depth. Exceeding any limit is a decode error.

Encoding is deterministic: entries sorted by effective name, fields in
declaration order, nothing time-dependent. The same tree manifested with the
same options yields the same bytes and therefore the same link.

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
- No component, with any extension removed, equals a Windows reserved device
  name (`CON`, `PRN`, `AUX`, `NUL`, `COM1`–`COM9`, `LPT1`–`LPT9`) ignoring
  case.
- Each component is at most `MAX_PATH_COMPONENT_BYTES` (255) bytes; the whole
  path is at most `MAX_PATH_BYTES` bytes (value to be fixed at review).
- Effective names are compared after Unicode NFC normalisation and full case
  folding. Under that comparison no two are equal, and none is a directory
  prefix of another: `a` and `a/b` cannot both be entries, and neither can
  `A` and `a/b`.

`name` obeys the single-component rules. Unnamed entries always pass.

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

Manifest links grow with the manifest, and DataMap bytes are hashes that do
not compress, so no compression layer is added. An embedded entry is roughly
490 link characters and a `Public` entry roughly 95. Producing tools warn
above `MANIFEST_LINK_RECOMMENDED_MAX_BYTES` and suggest a `.ant` file instead.

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
optional path and size, and nothing is uploaded.

Two reference modes for uploaded files:

- **Embedded (default).** The DataMap goes into the manifest. The recipient
  skips one fetch per file and the creator pays nothing extra. Wrapper
  fetches for child maps still occur.
- **Compact (opt-in).** Record `Public` for every file whose DataMap chunk is
  on the network: files added by address, files the caller uploaded as
  public in this run, and files whose DataMap chunk is found to exist
  already. Every other file stays `Embedded`. Compact mode never stores a
  DataMap chunk itself; making a file public is only ever the caller's
  explicit upload choice. Compact is what makes a manifest link fit in a
  message for more than a handful of files.

Directory walks sort by path, use paths relative to the root, and record
`size` for every uploaded file. Symlinks are skipped and reported, whether
they point at files or directories; an opt-in flag follows symlinks to
regular files and records the target's contents. Empty directories are not
representable: a directory exists only because a file path passes through it.
File permissions and timestamps are not recorded.

Per-file payments are unchanged by this decision.

### Download and extraction

Input: a manifest, an output root, and an optional selection by exact
effective name or directory prefix. Empty selection means everything.

Before any network access, every effective name is re-validated against the
portable path rules. Any violation rejects the manifest.

Containment is a filesystem contract, not a string check. The extractor opens
the output root once and performs every directory creation and file write
relative to that handle. Within the root it never follows a symlink, junction
or reparse point: each intermediate directory is created, or confirmed to be
a real directory, without following links, and the final file is created with
create-new semantics into a tempfile in its parent directory and renamed into
place. Where a platform offers a resolve-beneath open mode, it is used; where
it does not, every component is checked without following links immediately
before use. An entry whose path would traverse a link, or whose target
already exists, is a per-entry failure unless overwrite was requested, in
which case only a regular file at the exact target is replaced, never a link.

Per entry: resolve the DataMap (`Public` needs one fetch), download through
the ordinary file download, and write as above. `size` is shown to the user
and compared to nothing. Extracted files get the platform's default
permissions.

A bounded number of entries are in flight at once, one by default. Results
are per entry: written, or failed with its error. One failure does not abort
the rest. The operation reports progress at manifest and file level and
accepts a cancellation token.

### Surface

The library exposes: encode, decode, validate, link parse and format, build,
and extract. Encode, decode, validate and link handling are portable and
available to the browser build; build and extract need a filesystem. The CLI
exposes create, show, link and download for manifests, and its ordinary file
download accepts a file link or a bare address. No daemon API is added.

## Consequences

### Positive

- A folder is one shareable artefact, as a file or a self-contained link,
  and the recipient picks what to fetch.
- Nothing about a manifest touches the network, so there is nothing to pay
  for, nothing to resolve, and no ambiguity about what an address is.
- Mixed references let one manifest embed private files and point at public
  ones. An index over existing public data costs nothing.
- Integrity, deduplication and payment are inherited from DataMaps and
  chunks. No node change.
- Deterministic encoding gives identical manifests identical links.

### Negative / Trade-offs

- `Public` entries cost one extra fetch each; compact mode is therefore
  opt-in.
- Named msgpack is larger than positional by the field names.
- A manifest cannot be referenced by a short address; a large manifest must
  travel as a file.
- The portable path rules reject some trees that are legal locally.
- Empty directories and symlinks are not representable.
- Manifest links with embedded DataMaps fit only a few files per message.
- Existing files in the output directory are never reused; a re-download
  fails per entry or overwrites on request.
- A new format is a new compatibility surface; the version byte and the
  forward-compatibility rules bound it.

### Neutral / Operational

- No change to wire protocol, stored chunks, DataMap bytes, addresses or
  payments.
- `.ant` and `ant://` are decided; changing either would not touch the bytes.
- Unnamed entries extract under their hex address.

## Validation

- Encode/decode round trip; full-header and version rejection; decode limits
  on size, entry count and nesting; deterministic bytes for reordered input;
  link parse and format for both forms, bare addresses, base64url, rejection
  of a query string or path on a file link, and rejection of a manifest-link
  payload without the full header.
- Committed byte fixtures: extra unknown fields at every level decode; a
  manifest missing every optional field decodes to defaults; a variant
  missing its payload, an unknown variant and an unknown version are
  rejected; encoded structs are maps, not arrays.
- Portable path tests on Unix and Windows: every rule above, positive and
  negative, including case-folded and NFC-folded prefix conflicts.
- Containment tests on Unix and Windows: a pre-existing symlink, junction or
  reparse point at every depth inside the output root causes a per-entry
  failure and nothing is written outside the root; an existing target fails
  without overwrite and is replaced only when it is a regular file; a link at
  the target is never followed even with overwrite.
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

## Open questions for review

- `MANIFEST_LINK_RECOMMENDED_MAX_BYTES`. A 2000-character message holds three
  or four embedded entries or about twenty compact ones.
- `MAX_PATH_BYTES`. Windows without long-path support stops at 260 characters
  for the absolute path, so a relative limit must leave room for the output
  root.
- `MAX_MANIFEST_BYTES` and `MAX_MANIFEST_ENTRIES`.

## Notes for AI-assisted work

AI tools may help draft this ADR, but **must not mark it Accepted without
human review**. Accepted ADRs are immutable: create a new superseding ADR
rather than editing an Accepted ADR.
