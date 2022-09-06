use std::ops::Bound;

use diesel::{
    pg::types::sql_types,
    sql_query,
    sql_types::{Binary, Integer, Jsonb, Nullable},
    PgConnection, QueryDsl, RunQueryDsl,
};

use graph::{
    components::store::StoredDynamicDataSource,
    constraint_violation,
    prelude::{serde_json, BlockNumber, StoreError},
};

use crate::primary::Namespace;

type DynTable = diesel_dynamic_schema::Table<String, Namespace>;
type DynColumn<ST> = diesel_dynamic_schema::Column<DynTable, &'static str, ST>;

#[derive(Debug)]
pub(crate) struct DataSourcesTable {
    namespace: Namespace,
    qname: String,
    table: DynTable,
    vid: DynColumn<Integer>,
    block_range: DynColumn<sql_types::Range<Integer>>,
    causality_region: DynColumn<Integer>,
    manifest_idx: DynColumn<Integer>,
    param: DynColumn<Nullable<Binary>>,
    context: DynColumn<Nullable<Jsonb>>,
}

impl DataSourcesTable {
    const TABLE_NAME: &'static str = "data_sources$";

    pub(crate) fn new(namespace: Namespace) -> Self {
        let table =
            diesel_dynamic_schema::schema(namespace.clone()).table(Self::TABLE_NAME.to_string());

        DataSourcesTable {
            qname: format!("{}.{}", namespace, Self::TABLE_NAME),
            namespace,
            vid: table.column("vid"),
            block_range: table.column("block_range"),
            causality_region: table.column("causality_region"),
            manifest_idx: table.column("manifest_idx"),
            param: table.column("param"),
            context: table.column("context"),
            table,
        }
    }

    pub(crate) fn as_ddl(&self) -> String {
        format!(
            "
            create table {nsp}.{table} (
                vid integer generated by default as identity primary key,
                block_range int4range not null,
                causality_region integer generated by default as identity,
                manifest_idx integer not null,
                parent integer references {nsp}.{table},
                id bytea,
                param bytea,
                context jsonb
            );

            create index gist_block_range_data_sources$ on {nsp}.data_sources$ using gist (block_range);
            ",
            nsp = self.namespace.to_string(),
            table = Self::TABLE_NAME
        )
    }

    // Query to load the data sources which are live at `block`. Ordering by the creation block and
    // `vid` makes sure they are in insertion order which is important for the correctness of
    // reverts and the execution order of triggers. See also 8f1bca33-d3b7-4035-affc-fd6161a12448.
    pub(super) fn load(
        &self,
        conn: &mut PgConnection,
        block: BlockNumber,
    ) -> Result<Vec<StoredDynamicDataSource>, StoreError> {
        type Tuple = (
            (Bound<i32>, Bound<i32>),
            i32,
            Option<Vec<u8>>,
            Option<serde_json::Value>,
            i32,
        );
        let tuples = self
            .table
            .clone()
            .filter(diesel::dsl::sql("block_range @> ").bind::<Integer, _>(block))
            .select((
                &self.block_range,
                &self.manifest_idx,
                &self.param,
                &self.context,
                &self.causality_region,
            ))
            .order_by(&self.vid)
            .load::<Tuple>(conn)?;

        let mut dses: Vec<_> = tuples
            .into_iter()
            .map(
                |(block_range, manifest_idx, param, context, causality_region)| {
                    let creation_block = match block_range.0 {
                        Bound::Included(block) => Some(block),

                        // Should never happen.
                        Bound::Excluded(_) | Bound::Unbounded => {
                            unreachable!("dds with open creation")
                        }
                    };

                    let is_offchain = causality_region > 0;
                    StoredDynamicDataSource {
                        manifest_idx: manifest_idx as u32,
                        param: param.map(|p| p.into()),
                        context,
                        creation_block,
                        is_offchain,
                    }
                },
            )
            .collect();

        // This sort is stable and `tuples` was ordered by vid, so `dses` will be ordered by `(creation_block, vid)`.
        dses.sort_by_key(|v| v.creation_block);

        Ok(dses)
    }

    pub(crate) fn insert(
        &self,
        conn: &mut PgConnection,
        data_sources: &[StoredDynamicDataSource],
        block: BlockNumber,
    ) -> Result<usize, StoreError> {
        let mut inserted_total = 0;

        for ds in data_sources {
            let StoredDynamicDataSource {
                manifest_idx,
                param,
                context,
                creation_block,
                is_offchain,
            } = ds;

            if creation_block != &Some(block) {
                return Err(constraint_violation!(
                    "mismatching creation blocks `{:?}` and `{}`",
                    creation_block,
                    block
                ));
            }

            // Offchain data sources have a unique causality region assigned from a sequence in the
            // database, while onchain data sources always have causality region 0.
            let query = match is_offchain {
                false => format!(
                    "insert into {}(block_range, manifest_idx, param, context, causality_region) \
                            values (int4range($1, null), $2, $3, $4, $5)",
                    self.qname
                ),

                true => format!(
                    "insert into {}(block_range, manifest_idx, param, context) \
                            values (int4range($1, null), $2, $3, $4)",
                    self.qname
                ),
            };

            let query = sql_query(query)
                .bind::<Nullable<Integer>, _>(creation_block)
                .bind::<Integer, _>(*manifest_idx as i32)
                .bind::<Nullable<Binary>, _>(param.as_ref().map(|p| &**p))
                .bind::<Nullable<Jsonb>, _>(context);

            inserted_total += match is_offchain {
                false => query.bind::<Integer, _>(0).execute(conn)?,
                true => query.execute(conn)?,
            };
        }

        Ok(inserted_total)
    }

    pub(crate) fn revert(
        &self,
        conn: &mut PgConnection,
        block: BlockNumber,
    ) -> Result<(), StoreError> {
        // Use `@>` to leverage the gist index.
        // This assumes all ranges are of the form [x, +inf).
        let query = format!(
            "delete from {} where block_range @> $1 and lower(block_range) = $1",
            self.qname
        );
        sql_query(query).bind::<Integer, _>(block).execute(conn)?;
        Ok(())
    }

    /// Copy the dynamic data sources from `self` to `dst`. All data sources that
    /// were created up to and including `target_block` will be copied.
    pub(crate) fn copy_to(
        &self,
        conn: &mut PgConnection,
        dst: &DataSourcesTable,
        target_block: BlockNumber,
    ) -> Result<usize, StoreError> {
        // Check if there are any data sources for dst which indicates we already copied
        let count = dst.table.clone().count().get_result::<i64>(conn)?;
        if count > 0 {
            return Ok(count as usize);
        }

        let query = format!(
            "\
            insert into {dst}(block_range, causality_region, manifest_idx, parent, id, param, context)
            select case
                when upper(e.block_range) <= $1 then e.block_range
                else int4range(lower(e.block_range), null)
            end,
            e.causality_region, e.manifest_idx, e.parent, e.id, e.param, e.context
            from {src} e
            where lower(e.block_range) <= $1
            ",
            src = self.qname,
            dst = dst.qname
        );

        let count = sql_query(&query)
            .bind::<Integer, _>(target_block)
            .execute(conn)?;

        // Test that both tables have the same contents.
        debug_assert!(
            self.load(conn, target_block).map_err(|e| e.to_string())
                == dst.load(conn, target_block).map_err(|e| e.to_string())
        );

        Ok(count)
    }

    // Remove offchain data sources by checking for equality. Their range will be set to the empty range.
    pub(super) fn remove_offchain(
        &self,
        conn: &mut PgConnection,
        data_sources: &[StoredDynamicDataSource],
    ) -> Result<(), StoreError> {
        for ds in data_sources {
            let StoredDynamicDataSource {
                manifest_idx,
                param,
                context,
                creation_block,
                is_offchain,
            } = ds;

            if !is_offchain {
                return Err(constraint_violation!(
                    "called remove_offchain with onchain data sources"
                ));
            }

            let query = format!(
                "update {} set block_range = 'empty'::int4range \
                 where manifest_idx = $1
                    and param is not distinct from $2
                    and context is not distinct from $3
                    and lower(block_range) is not distinct from $4",
                self.qname
            );

            let count = sql_query(query)
                .bind::<Integer, _>(*manifest_idx as i32)
                .bind::<Nullable<Binary>, _>(param.as_ref().map(|p| &**p))
                .bind::<Nullable<Jsonb>, _>(context)
                .bind::<Nullable<Integer>, _>(creation_block)
                .execute(conn)?;

            if count > 1 {
                // Data source deduplication enforces this invariant.
                // See also: data-source-is-duplicate-of
                return Err(constraint_violation!(
                    "expected to remove at most one offchain data source but would remove {}, ds: {:?}",
                    count,
                    ds
                ));
            }
        }

        Ok(())
    }
}
