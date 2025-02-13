// Copyright (c) 2023-2024 Retake, Inc.
//
// This file is part of ParadeDB - Postgres for Search and Analytics
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program. If not, see <http://www.gnu.org/licenses/>.

use std::collections::HashMap;

use anyhow::bail;
use anyhow::Result;
use pgrx::prelude::*;
use pgrx::JsonB;
use pgrx::PgRelation;
use serde_json::Map;
use serde_json::Value;
use tokenizers::manager::SearchTokenizerFilters;
use tokenizers::SearchNormalizer;
use tokenizers::SearchTokenizer;

use crate::index::{SearchFs, SearchIndex, WriterDirectory};
use crate::postgres::index::{open_search_index, relfilenode_from_pg_relation};
use crate::postgres::options::SearchIndexCreateOptions;
use crate::schema::IndexRecordOption;
use crate::schema::SearchFieldConfig;
use crate::schema::SearchFieldName;
use crate::schema::SearchFieldType;

#[allow(clippy::too_many_arguments)]
#[pg_extern]
fn format_create_bm25(
    index_name: &str,
    table_name: &str,
    key_field: &str,
    schema_name: default!(&str, "''"),
    text_fields: default!(JsonB, "'{}'::jsonb"),
    numeric_fields: default!(JsonB, "'{}'::jsonb"),
    boolean_fields: default!(JsonB, "'{}'::jsonb"),
    json_fields: default!(JsonB, "'{}'::jsonb"),
    range_fields: default!(JsonB, "'{}'::jsonb"),
    datetime_fields: default!(JsonB, "'{}'::jsonb"),
    predicates: default!(&str, "''"),
) -> Result<String> {
    let mut column_names = vec![key_field.to_string()];
    for fields in [
        &text_fields,
        &numeric_fields,
        &boolean_fields,
        &json_fields,
        &range_fields,
        &datetime_fields,
    ] {
        if let Value::Object(ref map) = fields.0 {
            for key in map.keys() {
                if key != key_field {
                    column_names.push(spi::quote_identifier(key.clone()));
                }
            }
        } else {
            bail!("Expected a JSON object, received: {}", fields.0);
        }
    }

    let column_names_csv = column_names
        .clone()
        .into_iter()
        .filter(|name| name != key_field)
        .collect::<Vec<String>>()
        .join(", ");

    let predicate_where = if !predicates.is_empty() {
        format!("WHERE {}", predicates)
    } else {
        "".to_string()
    };

    let schema_prefix = if schema_name.is_empty() {
        "".to_string()
    } else {
        format!("{}.", spi::quote_identifier(schema_name))
    };

    Ok(format!(
        "CREATE INDEX {} ON {}{} USING bm25 ({}, {}) WITH (key_field={}, text_fields={}, numeric_fields={}, boolean_fields={}, json_fields={}, range_fields={}, datetime_fields={}) {};",
        spi::quote_identifier(index_name),
        schema_prefix,
        spi::quote_identifier(table_name),
        spi::quote_identifier(key_field),
        column_names_csv,
        spi::quote_literal(key_field),
        spi::quote_literal(&serde_json::to_string(&text_fields)?),
        spi::quote_literal(&serde_json::to_string(&numeric_fields)?),
        spi::quote_literal(&serde_json::to_string(&boolean_fields)?),
        spi::quote_literal(&serde_json::to_string(&json_fields)?),
        spi::quote_literal(&serde_json::to_string(&range_fields)?),
        spi::quote_literal(&serde_json::to_string(&datetime_fields)?),
        predicate_where))
}

#[pg_extern(sql = "
CREATE OR REPLACE PROCEDURE paradedb.delete_bm25_index_by_oid(
    index_oid oid
)
LANGUAGE c AS 'MODULE_PATHNAME', '@FUNCTION_NAME@';
")]
unsafe fn delete_bm25_index_by_oid(index_oid: pg_sys::Oid) -> Result<()> {
    let database_oid = crate::MyDatabaseId();
    let relfile_paths = WriterDirectory::relfile_paths(database_oid, index_oid.as_u32())
        .expect("could not look up pg_search relfilenode directory");

    for directory in relfile_paths {
        // Drop the Tantivy data directory.
        // It's expected that this will be queued to actually perform the delete upon
        // transaction commit.
        match SearchIndex::from_disk(&directory) {
            Ok(mut search_index) => {
                search_index.drop_index().unwrap_or_else(|err| {
                    panic!("error dropping index with OID {index_oid:?}: {err:?}")
                });
            }
            Err(e) => {
                pgrx::warning!(
                    "error dropping index with OID {index_oid:?} at path {}: {e:?}",
                    directory.search_index_dir_path(false).unwrap().0.display()
                );
            }
        }
    }
    Ok(())
}

#[pg_extern]
pub unsafe fn index_fields(index: PgRelation) -> JsonB {
    // # Safety
    //
    // Lock the index relation until the end of this function so it is not dropped or
    // altered while we are reading it.
    //
    // Because we accept a PgRelation above, we have confidence that Postgres has already
    // validated the existence of the relation. We are safe calling the function below as
    // long we do not pass pg_sys::NoLock without any other locking mechanism of our own.
    let index = unsafe { PgRelation::with_lock(index.oid(), pg_sys::AccessShareLock as _) };
    let rdopts: PgBox<SearchIndexCreateOptions> = if !index.rd_options.is_null() {
        unsafe { PgBox::from_pg(index.rd_options as *mut SearchIndexCreateOptions) }
    } else {
        let ops = unsafe { PgBox::<SearchIndexCreateOptions>::alloc0() };
        ops.into_pg_boxed()
    };

    // Create a map from column name to column type. We'll use this to verify that index
    // configurations passed by the user reference the correct types for each column.
    let name_type_map: HashMap<SearchFieldName, SearchFieldType> = index
        .tuple_desc()
        .into_iter()
        .filter_map(|attribute| {
            let attname = attribute.name();
            let attribute_type_oid = attribute.type_oid();
            let array_type = unsafe { pg_sys::get_element_type(attribute_type_oid.value()) };
            let base_oid = if array_type != pg_sys::InvalidOid {
                PgOid::from(array_type)
            } else {
                attribute_type_oid
            };
            if let Ok(search_field_type) = SearchFieldType::try_from(&base_oid) {
                Some((attname.into(), search_field_type))
            } else {
                None
            }
        })
        .collect();

    // Parse and validate the index configurations for each column.
    let text_fields =
        rdopts
            .get_text_fields()
            .into_iter()
            .map(|(name, config)| match name_type_map.get(&name) {
                Some(field_type @ SearchFieldType::Text) => (name, config, *field_type),
                _ => panic!("'{name}' cannot be indexed as a text field"),
            });

    let numeric_fields = rdopts
        .get_numeric_fields()
        .into_iter()
        .map(|(name, config)| match name_type_map.get(&name) {
            Some(field_type @ SearchFieldType::U64)
            | Some(field_type @ SearchFieldType::I64)
            | Some(field_type @ SearchFieldType::F64) => (name, config, *field_type),
            _ => panic!("'{name}' cannot be indexed as a numeric field"),
        });

    let boolean_fields = rdopts
        .get_boolean_fields()
        .into_iter()
        .map(|(name, config)| match name_type_map.get(&name) {
            Some(field_type @ SearchFieldType::Bool) => (name, config, *field_type),
            _ => panic!("'{name}' cannot be indexed as a boolean field"),
        });

    let json_fields =
        rdopts
            .get_json_fields()
            .into_iter()
            .map(|(name, config)| match name_type_map.get(&name) {
                Some(field_type @ SearchFieldType::Json) => (name, config, *field_type),
                _ => panic!("'{name}' cannot be indexed as a JSON field"),
            });

    let range_fields = rdopts.get_range_fields().into_iter().map(|(name, config)| {
        match name_type_map.get(&name) {
            Some(field_type @ SearchFieldType::Range) => (name, config, *field_type),
            _ => panic!("'{name}' cannot be indexed as a range field"),
        }
    });

    let datetime_fields = rdopts
        .get_datetime_fields()
        .into_iter()
        .map(|(name, config)| match name_type_map.get(&name) {
            Some(field_type @ SearchFieldType::Date) => (name, config, *field_type),
            _ => panic!("'{name}' cannot be indexed as a datetime field"),
        });

    let key_field = rdopts.get_key_field().expect("must specify key field");
    let key_field_type = match name_type_map.get(&key_field) {
        Some(field_type) => field_type,
        None => panic!("key field does not exist"),
    };
    let key_config = match key_field_type {
        SearchFieldType::I64 | SearchFieldType::U64 | SearchFieldType::F64 => {
            SearchFieldConfig::Numeric {
                indexed: true,
                fast: true,
                stored: true,
            }
        }
        SearchFieldType::Text => SearchFieldConfig::Text {
            indexed: true,
            fast: true,
            stored: true,
            fieldnorms: false,
            tokenizer: SearchTokenizer::Raw(SearchTokenizerFilters::default()),
            record: IndexRecordOption::Basic,
            normalizer: SearchNormalizer::Raw,
        },
        SearchFieldType::Json => SearchFieldConfig::Json {
            indexed: true,
            fast: true,
            stored: true,
            expand_dots: false,
            tokenizer: SearchTokenizer::Raw(SearchTokenizerFilters::default()),
            record: IndexRecordOption::Basic,
            normalizer: SearchNormalizer::Raw,
            fieldnorms: true,
        },
        SearchFieldType::Range => SearchFieldConfig::Range { stored: true },
        SearchFieldType::Bool => SearchFieldConfig::Boolean {
            indexed: true,
            fast: true,
            stored: true,
        },
        SearchFieldType::Date => SearchFieldConfig::Date {
            indexed: true,
            fast: true,
            stored: true,
        },
    };

    // Concatenate the separate lists of fields.
    let fields = text_fields
        .chain(numeric_fields)
        .chain(boolean_fields)
        .chain(json_fields)
        .chain(range_fields)
        .chain(datetime_fields)
        .chain(std::iter::once((
            key_field.clone(),
            key_config,
            *key_field_type,
        )))
        // "ctid" is a reserved column name in Postgres, so we don't need to worry about
        // creating a name conflict with a user-named column.
        .chain(std::iter::once((
            "ctid".into(),
            SearchFieldConfig::Ctid,
            SearchFieldType::U64,
        )))
        .map(|(name, config, _)| {
            (
                name.0,
                serde_json::to_value(config)
                    .expect("must be able to convert search field config to JSON"),
            )
        })
        .collect::<Map<_, _>>();

    JsonB(serde_json::Value::from(fields))
}

#[pg_extern]
fn index_size(index: PgRelation) -> Result<i64> {
    // # Safety
    //
    // Lock the index relation until the end of this function so it is not dropped or
    // altered while we are reading it.
    //
    // Because we accept a PgRelation above, we have confidence that Postgres has already
    // validated the existence of the relation. We are safe calling the function below as
    // long we do not pass pg_sys::NoLock without any other locking mechanism of our own.
    let index = unsafe { PgRelation::with_lock(index.oid(), pg_sys::AccessShareLock as _) };
    let index_oid = index.oid();

    let database_oid = crate::MyDatabaseId();
    let relfilenode = relfilenode_from_pg_relation(&index);

    // Create a WriterDirectory with the obtained index_oid
    let writer_directory =
        WriterDirectory::from_oids(database_oid, index_oid.as_u32(), relfilenode.as_u32());

    // Call the total_size method to get the size in bytes
    let total_size = writer_directory.total_size()?;

    Ok(total_size as i64)
}

#[pg_extern]
fn index_info(
    index: PgRelation,
) -> anyhow::Result<
    TableIterator<
        'static,
        (
            name!(segno, String),
            name!(byte_size, i64),
            name!(num_docs, i64),
            name!(num_deleted, i64),
        ),
    >,
> {
    // # Safety
    //
    // Lock the index relation until the end of this function so it is not dropped or
    // altered while we are reading it.
    //
    // Because we accept a PgRelation above, we have confidence that Postgres has already
    // validated the existence of the relation. We are safe calling the function below as
    // long we do not pass pg_sys::NoLock without any other locking mechanism of our own.
    let index = unsafe { PgRelation::with_lock(index.oid(), pg_sys::AccessShareLock as _) };

    // open the specified index
    let index = open_search_index(&index).expect("should be able to open search index");
    let directory = index.directory.clone();
    let data = index
        .underlying_index
        .searchable_segment_metas()?
        .into_iter()
        .map(|meta| {
            let segno = meta.id().short_uuid_string();
            let byte_size = meta
                .list_files()
                .into_iter()
                .map(|file| {
                    let mut full_path = directory.tantivy_dir_path(false).unwrap().0;
                    full_path.push(file);

                    if full_path.exists() {
                        full_path
                            .metadata()
                            .map(|metadata| metadata.len())
                            .unwrap_or(0)
                    } else {
                        0
                    }
                })
                .sum::<u64>() as i64;
            let num_docs = meta.num_docs() as i64;
            let num_deleted = meta.num_deleted_docs() as i64;

            (segno, byte_size, num_docs, num_deleted)
        })
        .collect::<Vec<_>>();

    Ok(TableIterator::new(data))
}

#[pg_extern(sql = "")]
fn create_bm25_jsonb() {}

extension_sql!(
    r#"
    CREATE OR REPLACE FUNCTION paradedb.drop_bm25_event_trigger()
    RETURNS event_trigger AS $$
    DECLARE
        obj RECORD;
    BEGIN
        FOR obj IN SELECT * FROM pg_event_trigger_dropped_objects() LOOP
            IF obj.object_type = 'index' THEN
                CALL paradedb.delete_bm25_index_by_oid(obj.objid);
            END IF;
        END LOOP;
    END;
    $$ LANGUAGE plpgsql;
    
    CREATE EVENT TRIGGER trigger_on_sql_index_drop
    ON sql_drop
    EXECUTE FUNCTION paradedb.drop_bm25_event_trigger();
    "#
    name = "create_drop_bm25_event_trigger",
    requires = [ delete_bm25_index_by_oid ]
);
