use super::Error;
use crate::Id;

use anyhow::Context;
use itertools::Itertools;
use serde_json::value::RawValue;
use sqlx::types::Json;
use std::collections::BTreeMap;

#[derive(Debug, Copy, Clone, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "catalog_spec_type")]
#[sqlx(rename_all = "lowercase")]
pub enum CatalogType {
    Collection,
    Materialization,
    Capture,
    Test,
}

#[derive(Debug)]
pub struct SpecRow {
    pub catalog_name: String,
    pub draft_spec: Option<Json<Box<RawValue>>>,
    pub draft_type: CatalogType,
    pub live_spec: Option<Json<Box<RawValue>>>,
    pub live_type: CatalogType,
    pub spec_min_patch: Json<Box<RawValue>>,
    pub spec_rev_patch: Json<Box<RawValue>>,
}

pub async fn resolve_specifications(
    draft_id: Id,
    pub_id: Id,
    txn: &mut sqlx::Transaction<'_, sqlx::Postgres>,
) -> anyhow::Result<Vec<SpecRow>> {
    // Attempt to create a row in live_specs for each of our draft_specs.
    // This allows us next inner-join over draft and live spec rows.
    // Inner join (vs a left-join) is required for "for update" semantics.
    //
    // We're intentionally running with read-committed isolation, and that
    // means a concurrent transaction may have committed a new row to live_specs
    // which technically isn't serializable with this transaction...
    // but we don't much care. Postgres will silently skip it under
    // "on conflict .. do nothing" semantics, and we'll next lock the new row.
    //
    // See: https://www.postgresql.org/docs/14/transaction-iso.html#XACT-READ-COMMITTED
    sqlx::query!(
        r#"
        insert into live_specs(catalog_name, spec_type, last_pub_id)
            (select catalog_name, spec_type, $2
                from draft_specs
                where draft_specs.draft_id = $1
                for update of draft_specs)
        on conflict (catalog_name) do nothing
        "#,
        draft_id as Id,
        pub_id as Id,
    )
    .execute(&mut *txn)
    .await
    .context("inserting new live_specs")?;

    // Fetch all of the draft's patches, along with their (now locked) live specifications.
    // This query is where we determine "before" and "after" states for each specification,
    // and determine exactly what changed.
    //
    // "for update" tells postgres that access to these rows should be serially sequenced,
    // meaning the user can't change a draft_spec out from underfoot, and a live_spec also
    // can't be silently changed. In both cases a concurrent update will block on our locks.
    //
    // It's possible that the user adds a new draft_spec at any time -- even between our last
    // statement and this one. Thus the result-set of this inner join is the final determiner
    // of what's "in" this publication, and what's not. Anything we don't pick up here will
    // be left behind as a draft_spec, and this is the reason we don't delete the draft
    // itself within this transaction.
    let spec_rows = sqlx::query_as!(
        SpecRow,
        r#"
        select
            draft_specs.catalog_name,
            draft_specs.spec_type as "draft_type: CatalogType",
            jsonb_merge_patch(
                live_specs.spec,
                draft_specs.spec_patch
            ) as "draft_spec: Json<Box<RawValue>>",
            live_specs.spec_type as "live_type: CatalogType",
            live_specs.spec as "live_spec: Json<Box<RawValue>>",
            jsonb_merge_diff(
                jsonb_merge_patch(live_specs.spec, draft_specs.spec_patch),
                live_specs.spec
            ) as "spec_min_patch!: Json<Box<RawValue>>",
            jsonb_merge_diff(
                live_specs.spec,
                jsonb_merge_patch(live_specs.spec, draft_specs.spec_patch)
            ) as "spec_rev_patch!: Json<Box<RawValue>>"
        from draft_specs
        join live_specs
            on draft_specs.catalog_name = live_specs.catalog_name
        where draft_specs.draft_id = $1
        for update of draft_specs, live_specs;
        "#,
        draft_id as Id,
    )
    .fetch_all(&mut *txn)
    .await
    .context("selecting joined draft & live specs")?;

    Ok(spec_rows)
}

pub async fn insert_errors(
    draft_id: Id,
    errors: Vec<Error>,
    txn: &mut sqlx::Transaction<'_, sqlx::Postgres>,
) -> anyhow::Result<()> {
    for err in errors {
        sqlx::query!(
            r#"insert into draft_errors (
              draft_id,
              scope,
              detail
            ) values ($1, $2, $3)
            "#,
            draft_id as Id,
            err.scope.unwrap_or(err.catalog_name),
            err.detail,
        )
        .execute(&mut *txn)
        .await
        .context("inserting error")?;
    }
    Ok(())
}

pub fn extend_catalog<'a>(
    catalog: &mut models::Catalog,
    it: impl Iterator<Item = (CatalogType, &'a str, &'a RawValue)>,
) -> Vec<Error> {
    let mut errors = Vec::new();

    for (catalog_type, catalog_name, spec) in it {
        let mut on_err = |detail| {
            errors.push(Error {
                catalog_name: catalog_name.to_string(),
                detail,
                ..Error::default()
            });
        };

        match catalog_type {
            CatalogType::Collection => match serde_json::from_str(spec.get()) {
                Ok(spec) => {
                    catalog
                        .collections
                        .insert(models::Collection::new(catalog_name), spec);
                }
                Err(err) => on_err(format!("invalid collection {catalog_name}: {err:?}")),
            },
            CatalogType::Capture => match serde_json::from_str(spec.get()) {
                Ok(spec) => {
                    catalog
                        .captures
                        .insert(models::Capture::new(catalog_name), spec);
                }
                Err(err) => on_err(format!("invalid capture {catalog_name}: {err:?}")),
            },
            CatalogType::Materialization => match serde_json::from_str(spec.get()) {
                Ok(spec) => {
                    catalog
                        .materializations
                        .insert(models::Materialization::new(catalog_name), spec);
                }
                Err(err) => on_err(format!("invalid materialization {catalog_name}: {err:?}")),
            },
            CatalogType::Test => match serde_json::from_str(spec.get()) {
                Ok(spec) => {
                    catalog.tests.insert(models::Test::new(catalog_name), spec);
                }
                Err(err) => on_err(format!("invalid test {catalog_name}: {err:?}")),
            },
        }
    }

    errors
}

pub fn validate_transition(
    live: &models::Catalog,
    draft: &models::Catalog,
    spec_rows: &[SpecRow],
) -> Vec<Error> {
    let mut errors = Vec::new();

    for SpecRow {
        catalog_name,
        draft_type,
        live_type,
        ..
    } in spec_rows
    {
        if draft_type != live_type {
            errors.push(Error {
                catalog_name: catalog_name.clone(),
                detail: format!(
                    "draft has an incompatible {draft_type:?} vs current {live_type:?}"
                ),
                ..Default::default()
            });
        }
    }

    for eob in draft
        .collections
        .iter()
        .merge_join_by(live.collections.iter(), |(n1, _), (n2, _)| n1.cmp(n2))
    {
        let (catalog_name, draft, live) = match eob.both() {
            Some(((catalog_name, draft), (_, live))) => (catalog_name, draft, live),
            None => continue,
        };

        if !draft.key.iter().eq(live.key.iter()) {
            errors.push(Error {
                catalog_name: catalog_name.to_string(),
                detail: format!(
                    "cannot change key of an established collection from {:?} to {:?}",
                    &live.key, &draft.key,
                ),
                ..Default::default()
            });
        }

        let partitions = |projections: &BTreeMap<models::Field, models::Projection>| {
            projections
                .iter()
                .filter_map(|(field, proj)| {
                    if matches!(
                        proj,
                        models::Projection::Extended {
                            partition: true,
                            ..
                        }
                    ) {
                        Some(field.to_string())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
        };

        let draft_partitions = partitions(&draft.projections);
        let live_partitions = partitions(&live.projections);

        if draft_partitions != live_partitions {
            errors.push(Error {
                catalog_name: catalog_name.to_string(),
                detail: format!(
                    "cannot change partitions of an established collection (from {live_partitions:?} to {draft_partitions:?})",
                ),
                ..Default::default()
            });
        }
    }

    errors
}

pub async fn apply_updates_for_row(
    pub_id: Id,
    draft_id: Id,
    catalog: &models::Catalog,
    spec_row: &SpecRow,
    txn: &mut sqlx::Transaction<'_, sqlx::Postgres>,
) -> anyhow::Result<()> {
    let SpecRow {
        catalog_name,
        draft_type,
        draft_spec,
        live_type: _,
        live_spec: _,
        spec_min_patch,
        spec_rev_patch,
    } = spec_row;

    sqlx::query!(
        r#"delete from draft_specs where draft_id = $1 and catalog_name = $2
            returning 1 as "must_exist";
        "#,
        draft_id as Id,
        &catalog_name as &str,
    )
    .fetch_one(&mut *txn)
    .await
    .context("delete from draft_specs")?;

    sqlx::query!(
        r#"insert into publication_specs (
            catalog_name,
            pub_id,
            spec_min_patch,
            spec_rev_patch,
            spec_type
        ) values ($1, $2, $3, $4, $5);
        "#,
        &catalog_name as &str,
        pub_id as Id,
        spec_min_patch as &Json<Box<RawValue>>,
        spec_rev_patch as &Json<Box<RawValue>>,
        draft_type as &CatalogType,
    )
    .execute(&mut *txn)
    .await
    .context("insert into publication_specs")?;

    if draft_spec.is_none() {
        // Draft is a deletion of a live spec.
        sqlx::query!(
            r#"delete from live_specs where catalog_name = $1
                returning 1 as "must_exist";
            "#,
            catalog_name,
        )
        .fetch_one(&mut *txn)
        .await
        .context("delete from live_specs")?;

        return Ok(());
    }

    // Draft is an update of a live spec. The insertion case is also an update:
    // we previously created a live_specs rows for the draft in order to lock it.

    let mut reads_from = Vec::new();
    let mut writes_to = Vec::new();
    let mut image_parts = None;

    match *draft_type {
        CatalogType::Collection => {
            let key = models::Collection::new(catalog_name);
            let collection = catalog.collections.get(&key).unwrap();

            if let Some(derivation) = &collection.derivation {
                for (_, tdef) in &derivation.transform {
                    reads_from.push(tdef.source.name.to_string());
                }
            }
        }
        CatalogType::Capture => {
            let key = models::Capture::new(catalog_name);
            let capture = catalog.captures.get(&key).unwrap();

            if let models::CaptureEndpoint::Connector(config) = &capture.endpoint {
                image_parts = Some(split_tag(&config.image));
            }
            for binding in &capture.bindings {
                writes_to.push(binding.target.to_string());
            }
        }
        CatalogType::Materialization => {
            let key = models::Materialization::new(catalog_name);
            let materialization = catalog.materializations.get(&key).unwrap();

            if let models::MaterializationEndpoint::Connector(config) = &materialization.endpoint {
                image_parts = Some(split_tag(&config.image));
            }
            // TODO(johnny): should we disallow sqlite? or remove sqlite altogether as an endpoint?

            for binding in &materialization.bindings {
                reads_from.push(binding.source.to_string());
            }
        }
        CatalogType::Test => {
            let key = models::Test::new(catalog_name);
            let test = catalog.tests.get(&key).unwrap();

            for step in test {
                match step {
                    models::TestStep::Ingest(ingest) => {
                        writes_to.push(ingest.collection.to_string())
                    }
                    models::TestStep::Verify(verify) => {
                        reads_from.push(verify.collection.to_string())
                    }
                }
            }
        }
    }

    sqlx::query!(
        r#"update live_specs set
                connector_image_name = $2,
                connector_image_tag = $3,
                last_pub_id = $4,
                reads_from = $5,
                spec = $6,
                updated_at = clock_timestamp(),
                writes_to = $7
            where catalog_name = $1
            returning 1 as "must_exist";
            "#,
        catalog_name,
        image_parts.as_ref().map(|p| &p.0),
        image_parts.as_ref().map(|p| &p.1),
        pub_id as Id,
        &reads_from,
        draft_spec as &Option<Json<Box<RawValue>>>,
        &writes_to,
    )
    .fetch_one(&mut *txn)
    .await
    .context("update live_specs")?;

    Ok(())
}

fn split_tag(image_full: &str) -> (String, String) {
    let mut image = image_full.to_string();

    if let Some(pivot) = image.find("@sha256:").or_else(|| image.find(":")) {
        let tag = image.split_off(pivot);
        (image, tag)
    } else {
        (image, String::new())
    }
}
