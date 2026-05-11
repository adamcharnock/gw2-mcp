//! Thin router for curated-build catalogs. Looks up the adapter by source
//! name and forwards. All caching policy lives in the individual adapters
//! (Snow Crows has per-(category, profession) caches with rate-limit
//! cooldowns; `MetaBattle` and Discretize hit cheap CDN-backed upstreams
//! and refetch on every call). Layering a generic memoization here would
//! either mask the rate-limit semantics or require per-source TTL
//! overrides — pushing caching to adapters keeps the policy next to the
//! upstream it protects.

use super::{Service, ServiceError};
use crate::domain::BuildSlug;
use crate::ports::{BuildDetail, BuildSummary, CatalogError, CatalogFilter};

impl Service {
    /// List the names of registered curated-build sources.
    #[must_use]
    pub fn list_catalogs(&self) -> Vec<&'static str> {
        self.catalogs.names()
    }

    pub async fn list_catalog_builds(
        &self,
        source: &str,
        filter: CatalogFilter,
    ) -> Result<Vec<BuildSummary>, ServiceError> {
        let cat = self
            .catalogs
            .get(source)
            .ok_or_else(|| CatalogError::NoSuchSource(source.to_owned()))?;
        Ok(cat.list(&filter).await?)
    }

    pub async fn get_catalog_build(
        &self,
        source: &str,
        slug: &BuildSlug,
    ) -> Result<BuildDetail, ServiceError> {
        let cat = self
            .catalogs
            .get(source)
            .ok_or_else(|| CatalogError::NoSuchSource(source.to_owned()))?;
        Ok(cat.fetch(slug).await?)
    }
}
