use std::path::PathBuf;
use std::time::Instant;

use entities::models::schema::v1::{self, SearchDocument as SearchDocumentV1};
use entities::models::schema::v2::{self, SearchDocument as SearchDocumentV2};
use rayon::iter::IntoParallelRefIterator;
use rayon::iter::ParallelIterator;
use sea_orm_migration::prelude::ConnectionTrait;
use sea_orm_migration::prelude::*;
use tantivy_18::collector::TopDocs;
use tantivy_18::directory::MmapDirectory;
use tantivy_18::query::TermQuery;
use tantivy_18::TantivyError;
use tantivy_18::{schema::*, IndexWriter};
use tantivy_18::{Index, IndexReader, ReloadPolicy};

// use entities::schema::DocFields;
use entities::models::crawl_queue;
use entities::sea_orm::Statement;
use shared::config::Config;

use crate::utils::migration_utils;

pub struct Migration;
impl Migration {
    pub fn before_schema(&self) -> v1::SchemaMapping {
        v1::DocFields::as_field_vec()
    }

    pub fn before_reader(&self, path: &PathBuf) -> Result<IndexReader, TantivyError> {
        let dir = MmapDirectory::open(path).expect("Unable to mmap search index");
        let index = Index::open_or_create(dir, v1::mapping_to_schema(&self.before_schema()))?;

        index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()
    }

    pub fn after_schema(&self) -> v2::SchemaMapping {
        v2::DocFields::as_field_vec()
    }

    pub fn after_writer(&self, path: &PathBuf) -> IndexWriter {
        let dir = MmapDirectory::open(path).expect("Unable to mmap search index");
        let index = Index::open_or_create(dir, v2::mapping_to_schema(&self.after_schema()))
            .expect("Unable to open search index");

        index.writer(50_000_000).expect("Unable to create writer")
    }

    pub fn migrate_document(
        &self,
        doc_id: &str,
        old_doc: Document,
        old_schema: &Schema,
        new_schema: &Schema,
    ) -> Document {
        let mut new_doc = Document::default();
        new_doc.add_text(new_schema.get_field("id").unwrap(), doc_id);
        for (old_field, new_field) in &[
            // Will map <old> -> <new>
            ("domain", "domain"),
            ("title", "title"),
            ("description", "description"),
            // No content was saved previous, so we'll use the description as a stopgap
            // and recrawl stuff
            ("description", "content"),
            ("url", "url"),
        ] {
            let new_field = new_schema.get_field(new_field).unwrap();
            let old_value = old_doc
                .get_first(old_schema.get_field(old_field).unwrap())
                .unwrap()
                .as_text()
                .unwrap();

            new_doc.add_text(new_field, old_value);
        }

        new_doc
    }
}

impl MigrationName for Migration {
    fn name(&self) -> &str {
        "m20220823_000001_migrate_search_schema"
    }
}

fn get_by_id(id_field: Field, reader: &IndexReader, doc_id: &str) -> Option<Document> {
    let searcher = reader.searcher();

    let query = TermQuery::new(
        Term::from_field_text(id_field, doc_id),
        IndexRecordOption::Basic,
    );

    let res = searcher
        .search(&query, &TopDocs::with_limit(1))
        .expect("Unable to execute query");

    if res.is_empty() {
        return None;
    }

    let (_, doc_address) = res.first().expect("No results in search");
    if let Ok(doc) = searcher.doc(*doc_address) {
        return Some(doc);
    }
    None
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let config = Config::new();
        let old_index_path = config.index_dir();
        let new_index_path = old_index_path
            .parent()
            .expect("Expected parent path")
            .join("migrated_index");

        let result = manager
            .get_connection()
            .query_all(Statement::from_string(
                manager.get_database_backend(),
                "SELECT id, doc_id, url FROM indexed_document".to_owned(),
            ))
            .await?;

        // No docs yet, nothing to migrate.
        if result.is_empty() {
            // Removing the old index folder will also remove any metadata that lingers
            // from an empty index.
            let _ = std::fs::remove_dir_all(old_index_path);
            return Ok(());
        }

        if !new_index_path.exists() {
            if let Err(e) = std::fs::create_dir(new_index_path.clone()) {
                return Err(DbErr::Custom(format!("Can't create new index: {e}")));
            }
        }

        println!("Migrating index @ {old_index_path:?} to {new_index_path:?}");

        let old_schema = v1::mapping_to_schema(&self.before_schema());
        let new_schema = v2::mapping_to_schema(&self.after_schema());
        let old_reader_res = self.before_reader(&old_index_path);
        if let Err(err) = old_reader_res {
            // Potentially already migrated?
            println!("Error opening index: {err}");
            return Ok(());
        }
        let old_reader = old_reader_res.expect("Unable to open index for migration");

        let mut new_writer = self.after_writer(&new_index_path);

        let recrawl_urls = result
            .iter()
            .filter_map(|row| row.try_get::<String>("", "url").ok())
            .collect::<Vec<String>>();

        let now = Instant::now();
        let old_id_field = old_schema.get_field("id").unwrap();

        let _errs = result
            .par_iter()
            .filter_map(|row| {
                let doc_id: String = row.try_get::<String>("", "doc_id").unwrap();
                let doc = get_by_id(old_id_field, &old_reader, &doc_id);
                if let Some(old_doc) = doc {
                    if let Err(e) = new_writer.add_document(self.migrate_document(
                        &doc_id,
                        old_doc,
                        &old_schema,
                        &new_schema,
                    )) {
                        return Some(DbErr::Custom(format!("Unable to migrate doc: {e}")));
                    }
                }

                None
            })
            .collect::<Vec<DbErr>>();

        // Recrawl indexed docs to refresh them
        let overrides = crawl_queue::EnqueueSettings {
            force_allow: true,
            is_recrawl: true,
            ..Default::default()
        };

        if let Err(e) = crawl_queue::enqueue_all(
            manager.get_connection(),
            &recrawl_urls,
            &[],
            &config.user_settings,
            &overrides,
            Option::None,
        )
        .await
        {
            return Err(DbErr::Custom(format!("Unable to requeue URLs: {e}")));
        }

        // Save change to new index
        if let Err(e) = new_writer.commit() {
            return Err(DbErr::Custom(format!("Unable to commit changes: {e}")));
        }

        if let Err(e) = migration_utils::backup_dir(&old_index_path) {
            return Err(DbErr::Custom(format!("Unable to backup old index: {e}")));
        }

        // Move new index into place.
        if let Err(e) = migration_utils::replace_dir(&new_index_path, &old_index_path) {
            return Err(DbErr::Custom(format!(
                "Unable to move new index into place: {e}"
            )));
        }

        let elapsed_time = now.elapsed();
        println!("Migration took {} seconds.", elapsed_time.as_secs());

        Ok(())
    }

    async fn down(&self, _: &SchemaManager) -> Result<(), DbErr> {
        Ok(())
    }
}
