use std::collections::HashMap;

use squeezy_core::FileId;
use squeezy_store::GraphWriteBatch;

use crate::resolver_cache;

pub(crate) fn set_import_graph_snapshot(
    batch: &mut GraphWriteBatch,
    importers_by_file: &HashMap<FileId, Vec<FileId>>,
) {
    let mut snapshot = resolver_cache::ResolverSnapshot::new();
    for (target, importers) in importers_by_file {
        for importer in importers {
            snapshot.record_edge(importer, target);
        }
    }
    let _ = batch.set_import_graph(&snapshot);
}
