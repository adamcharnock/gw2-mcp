//! Thin delegations to the on-disk fuzzy `SearchIndex` port. Each search
//! method here just forwards into the indexed adapter — no caching, no
//! pre-filtering. If the search index is missing (server started with
//! `--no-search-index`), every call returns `ServiceError::SearchDisabled`
//! rather than failing the request silently.

use super::{Service, ServiceError};
use crate::ports::{
    AchievementRef, AchievementSearchFilter, IndexStatusView, ItemRef, ItemSearchFilter, SkillRef,
    SkillSearchFilter, SpecRef, SpecSearchFilter, TraitRef, TraitSearchFilter,
};

impl Service {
    /// Run a fuzzy name search across the indexed skills corpus.
    pub async fn search_skills(
        &self,
        q: &str,
        limit: u32,
        filter: SkillSearchFilter,
    ) -> Result<Vec<SkillRef>, ServiceError> {
        Ok(self.search_index()?.search_skills(q, limit, filter).await?)
    }

    pub async fn search_traits(
        &self,
        q: &str,
        limit: u32,
        filter: TraitSearchFilter,
    ) -> Result<Vec<TraitRef>, ServiceError> {
        Ok(self.search_index()?.search_traits(q, limit, filter).await?)
    }

    pub async fn search_specializations(
        &self,
        q: &str,
        limit: u32,
        filter: SpecSearchFilter,
    ) -> Result<Vec<SpecRef>, ServiceError> {
        Ok(self
            .search_index()?
            .search_specializations(q, limit, filter)
            .await?)
    }

    pub async fn search_items(
        &self,
        q: &str,
        limit: u32,
        filter: ItemSearchFilter,
    ) -> Result<Vec<ItemRef>, ServiceError> {
        Ok(self.search_index()?.search_items(q, limit, filter).await?)
    }

    pub async fn search_achievements(
        &self,
        q: &str,
        limit: u32,
        filter: AchievementSearchFilter,
    ) -> Result<Vec<AchievementRef>, ServiceError> {
        Ok(self
            .search_index()?
            .search_achievements(q, limit, filter)
            .await?)
    }

    pub async fn get_index_status(&self) -> Result<IndexStatusView, ServiceError> {
        Ok(IndexStatusView::from(
            self.search_index()?.index_status().await?,
        ))
    }
}
