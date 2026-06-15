use squeezy_core::ContentHash;

#[derive(Debug, Clone)]
pub(crate) struct CachedJavaProjectFacts {
    pub(crate) hash: ContentHash,
    pub(crate) java_paths_signature: u64,
    pub(crate) dependency_values: Vec<String>,
    pub(crate) configured_source_facts: Vec<(&'static str, String, &'static str)>,
    pub(crate) source_root_facts: Vec<(&'static str, String, &'static str)>,
}

#[derive(Debug, Clone)]
pub(crate) struct CachedKotlinProjectFacts {
    pub(crate) hash: ContentHash,
    pub(crate) kotlin_paths_signature: u64,
    pub(crate) dependency_values: Vec<String>,
    pub(crate) configured_source_facts: Vec<(&'static str, String, &'static str)>,
    pub(crate) source_root_facts: Vec<(&'static str, String, &'static str)>,
}
