//! Choosing which form of a DataMap to embed in a manifest.
//!
//! A large upload yields a shrunk (child) DataMap that points at wrapper
//! records; a reader must fetch those before it can fetch data. Embedding the
//! root map instead removes that step, at the price of manifest bytes. The
//! policy here embeds the root when its encoding, as a manifest writes it,
//! stays under [`MAX_EMBEDDED_ROOT_MAP_BYTES`] and the child map otherwise.
//! Either form has the same content address.

use self_encryption::DataMap;

use crate::data::Client;

use super::{embedded_len, ManifestError, MAX_EMBEDDED_ROOT_MAP_BYTES};

/// The DataMap to embed for a file: its root map when that is small enough,
/// otherwise the shrunk map given. A map that is already a root is returned
/// unchanged. Resolving the root may fetch wrapper records.
pub async fn embeddable_data_map(
    client: &Client,
    data_map: &DataMap,
) -> Result<DataMap, ManifestError> {
    if !data_map.is_child() {
        return Ok(data_map.clone());
    }
    let root = client.data_map_resolve_root(data_map).await?;
    if embedded_len(&root)? <= MAX_EMBEDDED_ROOT_MAP_BYTES {
        Ok(root)
    } else {
        Ok(data_map.clone())
    }
}
