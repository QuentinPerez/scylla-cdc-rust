#[cfg(test)]
mod tests {
    use crate::replicator_consumer::ReplicatorConsumer;
    use anyhow::anyhow;
    use futures_util::FutureExt;
    use itertools::Itertools;
    use scylla::frame::response::result::CqlValue::{Boolean, Int, Text, UserDefinedType};
    use scylla::frame::response::result::{CqlValue, Row};
    use scylla::{Session, SessionBuilder};
    use scylla_cdc::consumer::{CDCRow, CDCRowSchema, Consumer};
    use std::sync::Arc;

    use scylla_cdc::test_utilities::unique_name;

    /// Tuple representing a column in the table that will be replicated.
    /// The first string is the name of the column.
    /// The second string is the name of the type of the column.
    pub type TestColumn<'a> = (&'a str, &'a str);

    pub struct TestTableSchema<'a> {
        name: String,
        partition_key: Vec<TestColumn<'a>>,
        clustering_key: Vec<TestColumn<'a>>,
        other_columns: Vec<TestColumn<'a>>,
    }

    pub struct TestUDTSchema<'a> {
        name: String,
        fields: Vec<TestColumn<'a>>,
    }

    /// Tuple representing an operation to be performed on the table before replicating.
    /// The string is the CQL query with the operation. Keyspace does not have to be specified.
    /// The vector of values are the values that will be bound to the query.
    pub type TestOperation<'a> = &'a str;

    enum FailureReason {
        WrongRowsCount(usize, usize),
        RowNotMatching(usize),
        TimestampsNotMatching(usize, String),
    }

    async fn setup_keyspaces(session: &Session) -> anyhow::Result<(String, String)> {
        let ks_src = unique_name();
        let ks_dst = unique_name();
        session.query(format!("CREATE KEYSPACE IF NOT EXISTS {} WITH REPLICATION = {{ 'class' : 'SimpleStrategy', 'replication_factor' : 1 }}", ks_src), ()).await?;
        session.query(format!("CREATE KEYSPACE IF NOT EXISTS {} WITH REPLICATION = {{ 'class' : 'SimpleStrategy', 'replication_factor' : 1 }}", ks_dst), ()).await?;

        Ok((ks_src, ks_dst))
    }

    async fn setup_udts(
        session: &Session,
        ks_src: &str,
        ks_dst: &str,
        schemas: &[TestUDTSchema<'_>],
    ) -> anyhow::Result<()> {
        for udt_schema in schemas {
            let udt_fields = udt_schema
                .fields
                .iter()
                .map(|(field_name, field_type)| format!("{} {}", field_name, field_type))
                .join(",");
            session
                .query(
                    format!(
                        "CREATE TYPE IF NOT EXISTS {}.{} ({})",
                        ks_src, udt_schema.name, udt_fields
                    ),
                    (),
                )
                .await?;
            session
                .query(
                    format!(
                        "CREATE TYPE IF NOT EXISTS {}.{} ({})",
                        ks_dst, udt_schema.name, udt_fields
                    ),
                    (),
                )
                .await?;
        }

        session.refresh_metadata().await?;

        Ok(())
    }

    async fn setup_tables(
        session: &Session,
        ks_src: &str,
        ks_dst: &str,
        schema: &TestTableSchema<'_>,
    ) -> anyhow::Result<()> {
        let partition_key_name = match schema.partition_key.as_slice() {
            [pk] => pk.0.to_string(),
            _ => format!(
                "({})",
                schema.partition_key.iter().map(|(name, _)| name).join(",")
            ),
        };
        let create_table_query = format!(
            "({}, PRIMARY KEY ({}, {}))",
            schema
                .partition_key
                .iter()
                .chain(schema.clustering_key.iter())
                .chain(schema.other_columns.iter())
                .map(|(name, col_type)| format!("{} {}", name, col_type))
                .join(","),
            partition_key_name,
            schema.clustering_key.iter().map(|(name, _)| name).join(",")
        );

        session
            .query(
                format!(
                    "CREATE TABLE {}.{} {} WITH cdc = {{'enabled' : true}}",
                    ks_src, schema.name, create_table_query
                ),
                (),
            )
            .await?;
        session
            .query(
                format!(
                    "CREATE TABLE {}.{} {}",
                    ks_dst, schema.name, create_table_query
                ),
                (),
            )
            .await?;

        session.refresh_metadata().await?;

        Ok(())
    }

    async fn execute_queries(
        session: &Session,
        ks_src: &str,
        operations: Vec<TestOperation<'_>>,
    ) -> anyhow::Result<()> {
        session.use_keyspace(ks_src, false).await?;
        for operation in operations {
            session.query(operation, []).await?;
        }

        Ok(())
    }

    async fn replicate(
        session: &Arc<Session>,
        ks_src: &str,
        ks_dst: &str,
        name: &str,
    ) -> anyhow::Result<()> {
        let result = session
            .query(
                format!("SELECT * FROM {}.{}_scylla_cdc_log", ks_src, name),
                (),
            )
            .await?;

        let table_schema = session
            .get_cluster_data()
            .get_keyspace_info()
            .get(ks_dst)
            .ok_or_else(|| anyhow!("Keyspace not found"))?
            .tables
            .get(&name.to_ascii_lowercase())
            .ok_or_else(|| anyhow!("Table not found"))?
            .clone();

        let mut consumer = ReplicatorConsumer::new(
            session.clone(),
            ks_dst.to_string(),
            name.to_string(),
            table_schema,
        )
        .await;

        let schema = CDCRowSchema::new(&result.col_specs);

        for log in result.rows.unwrap_or_default() {
            consumer.consume_cdc(CDCRow::from_row(log, &schema)).await?;
        }

        Ok(())
    }

    fn fail_test(
        table_name: &str,
        original_rows: &[Row],
        replicated_rows: &[Row],
        failure_reason: FailureReason,
    ) {
        eprintln!("Replication test for table {} failed.", table_name);
        eprint!("Failure reason: ");
        match failure_reason {
            FailureReason::WrongRowsCount(o, r) => eprintln!(
                "Number of rows not matching. Original table: {} rows, replicated table: {} rows.",
                o, r
            ),
            FailureReason::RowNotMatching(n) => {
                eprintln!("Row {} is not equal in both tables.", n + 1)
            }
            FailureReason::TimestampsNotMatching(row, column) => eprintln!(
                "Timestamps were not equal for column {} in row {}.",
                column,
                row + 1
            ),
        }

        eprintln!("ORIGINAL TABLE:");
        for (i, row) in original_rows.iter().enumerate() {
            eprintln!("Row {}: {:?}", i + 1, row.columns);
        }

        eprintln!("REPLICATED TABLE:");
        for (i, row) in replicated_rows.iter().enumerate() {
            eprintln!("Row {}: {:?}", i + 1, row.columns);
        }

        panic!()
    }

    fn equal_rows(original_row: &Row, replicated_row: &Row) -> bool {
        if original_row.columns.len() != replicated_row.columns.len() {
            return false;
        }

        let not_matching = original_row
            .columns
            .iter()
            .zip(replicated_row.columns.iter())
            .filter(|(o, r)| match (o, r) {
                (
                    Some(CqlValue::UserDefinedType {
                        fields: original_fields,
                        ..
                    }),
                    Some(CqlValue::UserDefinedType {
                        fields: replicated_fields,
                        ..
                    }),
                ) => original_fields != replicated_fields,
                (_, _) => o != r,
            })
            .count();

        not_matching == 0
    }

    async fn compare_changes(
        session: &Session,
        ks_src: &str,
        ks_dst: &str,
        name: &str,
    ) -> anyhow::Result<()> {
        let original_rows = session
            .query(format!("SELECT * FROM {}.{}", ks_src, name), ())
            .await?
            .rows
            .unwrap_or_default();
        let replicated_rows = session
            .query(format!("SELECT * FROM {}.{}", ks_dst, name), ())
            .await?
            .rows
            .unwrap_or_default();

        if original_rows.len() == replicated_rows.len() {
            for (i, (original, replicated)) in
                original_rows.iter().zip(replicated_rows.iter()).enumerate()
            {
                if !equal_rows(original, replicated) {
                    fail_test(
                        name,
                        &original_rows,
                        &replicated_rows,
                        FailureReason::RowNotMatching(i),
                    );
                }
            }
        } else {
            fail_test(
                name,
                &original_rows,
                &replicated_rows,
                FailureReason::WrongRowsCount(original_rows.len(), replicated_rows.len()),
            );
        }

        Ok(())
    }

    // Returns vector of timestamps of given column from all rows.
    async fn get_timestamps(
        session: &Session,
        ks_src: &str,
        ks_dst: &str,
        table_name: &str,
        column_name: &str,
    ) -> anyhow::Result<(Vec<Row>, Vec<Row>)> {
        let original_rows = session
            .query(
                format!(
                    "SELECT WRITETIME ({}) FROM {}.{}",
                    column_name, ks_src, table_name
                ),
                (),
            )
            .await?
            .rows
            .unwrap_or_default();
        let replicated_rows = session
            .query(
                format!(
                    "SELECT WRITETIME ({}) FROM {}.{}",
                    column_name, ks_dst, table_name
                ),
                (),
            )
            .await?
            .rows
            .unwrap_or_default();

        Ok((original_rows, replicated_rows))
    }

    // Given a type name returns a boolean value indicating if
    // the cql `WRITETIME` function can be used on a column of this type.
    fn can_read_timestamps(col_type: &&str) -> bool {
        const NATIVE_TYPES: [&str; 21] = [
            "ascii",
            "bigint",
            "blob",
            "boolean",
            "counter",
            "date",
            "decimal",
            "double",
            "duration",
            "float",
            "inet",
            "int",
            "smallint",
            "text",
            "varchar",
            "time",
            "timestamp",
            "timeuuid",
            "tinyint",
            "uuid",
            "varint",
        ];

        NATIVE_TYPES.contains(col_type)
            || col_type.starts_with("frozen<")
            || col_type.starts_with("tuple<")
    }

    // Compares timestamps of all non-partition and non-clustering columns between destination table and source table.
    // Since cql function `WRITETIME` doesn't work for collections as per https://issues.apache.org/jira/browse/CASSANDRA-8877,
    // we check beforehand if column's type can be read via the `WRITETIME` function and either compare timestamps or skip them.
    async fn compare_timestamps(
        session: &Session,
        ks_src: &str,
        ks_dst: &str,
        schema: &TestTableSchema<'_>,
    ) -> anyhow::Result<()> {
        for (col_name, col_type) in &schema.other_columns {
            if can_read_timestamps(col_type) {
                let (original_rows, replicated_rows) =
                    get_timestamps(session, ks_src, ks_dst, &schema.name, col_name).await?;

                for (i, (original, replicated)) in
                    original_rows.iter().zip(replicated_rows.iter()).enumerate()
                {
                    if original != replicated {
                        fail_test(
                            &schema.name,
                            &original_rows,
                            &replicated_rows,
                            FailureReason::TimestampsNotMatching(i, col_name.to_string()),
                        );
                    }
                }
            }
        }
        Ok(())
    }

    /// Function that tests replication process.
    /// Different tests in the same cluster must have different table names.
    async fn test_replication(
        node_uri: &str,
        schema: TestTableSchema<'_>,
        operations: Vec<TestOperation<'_>>,
    ) -> anyhow::Result<()> {
        test_replication_with_udt(node_uri, schema, vec![], operations).await?;

        Ok(())
    }

    /// Function that tests replication process with a user-defined type
    /// Different tests in the same cluster must have different table names.
    async fn test_replication_with_udt(
        node_uri: &str,
        table_schema: TestTableSchema<'_>,
        udt_schemas: Vec<TestUDTSchema<'_>>,
        operations: Vec<TestOperation<'_>>,
    ) -> anyhow::Result<()> {
        let session = Arc::new(SessionBuilder::new().known_node(node_uri).build().await?);
        let (ks_src, ks_dst) = setup_keyspaces(&session).await?;
        setup_udts(&session, &ks_src, &ks_dst, &udt_schemas).await?;
        setup_tables(&session, &ks_src, &ks_dst, &table_schema).await?;
        execute_queries(&session, &ks_src, operations).await?;
        replicate(&session, &ks_src, &ks_dst, &table_schema.name).await?;
        compare_changes(&session, &ks_src, &ks_dst, &table_schema.name).await?;
        compare_timestamps(&session, &ks_src, &ks_dst, &table_schema).await?;

        Ok(())
    }

    fn get_uri() -> String {
        std::env::var("SCYLLA_URI").unwrap_or_else(|_| "127.0.0.1:9042".to_string())
    }

    #[tokio::test]
    async fn test_equal_rows_on_matching_rows() {
        let original_row = Row {
            columns: vec![
                Some(Int(0)),
                Some(Int(1)),
                Some(Boolean(true)),
                Some(UserDefinedType {
                    keyspace: "some_ks".to_string(),
                    type_name: "user_type".to_string(),
                    fields: vec![
                        ("int_val".to_string(), Some(Int(7))),
                        ("text_val".to_string(), Some(Text("seven".to_string()))),
                    ],
                }),
            ],
        };

        let replicated_row = Row {
            columns: vec![
                Some(Int(0)),
                Some(Int(1)),
                Some(Boolean(true)),
                Some(UserDefinedType {
                    keyspace: "another_ks".to_string(), // Not equal keyspaces should be ignored
                    type_name: "user_type".to_string(),
                    fields: vec![
                        ("int_val".to_string(), Some(Int(7))),
                        ("text_val".to_string(), Some(Text("seven".to_string()))),
                    ],
                }),
            ],
        };

        let is_equal = equal_rows(&original_row, &replicated_row);

        assert!(is_equal);
    }

    #[tokio::test]
    async fn test_equal_rows_on_non_matching_rows() {
        let original_row = Row {
            columns: vec![
                Some(Int(0)),
                Some(Boolean(false)),
                Some(UserDefinedType {
                    keyspace: "some_ks".to_string(),
                    type_name: "user_type".to_string(),
                    fields: vec![
                        ("int_val".to_string(), Some(Int(7))),
                        ("text_val".to_string(), Some(Text("seven".to_string()))),
                    ],
                }),
            ],
        };

        let replicated_rows = Row {
            columns: vec![
                Some(Int(0)),
                Some(Boolean(false)),
                Some(UserDefinedType {
                    keyspace: "some_ks".to_string(),
                    type_name: "user_type".to_string(),
                    fields: vec![("text_val".to_string(), Some(Text("seven".to_string())))],
                }),
            ],
        };

        let is_equal = equal_rows(&original_row, &replicated_rows);

        assert!(!is_equal);
    }

    #[tokio::test]
    async fn simple_insert_test() {
        let schema = TestTableSchema {
            name: "SIMPLE_INSERT".to_string(),
            partition_key: vec![("pk", "int")],
            clustering_key: vec![("ck", "int")],
            other_columns: vec![("v1", "int"), ("v2", "boolean")],
        };

        let operations = vec![
            "INSERT INTO SIMPLE_INSERT (pk, ck, v1, v2) VALUES (1, 2, 3, true)",
            "INSERT INTO SIMPLE_INSERT (pk, ck, v1, v2) VALUES (3, 2, 1, false)",
        ];

        test_replication(&get_uri(), schema, operations)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn simple_update_test() {
        let schema = TestTableSchema {
            name: "SIMPLE_UPDATE".to_string(),
            partition_key: vec![("pk", "int")],
            clustering_key: vec![("ck", "int")],
            other_columns: vec![("v1", "int"), ("v2", "boolean")],
        };

        let operations = vec![
            "INSERT INTO SIMPLE_UPDATE (pk, ck, v1, v2) VALUES (1, 2, 3, true)",
            "UPDATE SIMPLE_UPDATE SET v2 = false WHERE pk = 1 AND ck = 2",
            "DELETE v1 FROM SIMPLE_UPDATE WHERE pk = 1 AND ck = 2",
        ];

        test_replication(&get_uri(), schema, operations)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn simple_frozen_udt_test() {
        let table_schema = TestTableSchema {
            name: "SIMPLE_UDT_TEST".to_string(),
            partition_key: vec![("pk", "int")],
            clustering_key: vec![("ck", "int")],
            other_columns: vec![("ut_col", "frozen<ut>")],
        };

        let udt_schemas = vec![TestUDTSchema {
            name: "ut".to_string(),
            fields: vec![("int_val", "int"), ("bool_val", "boolean")],
        }];

        let operations = vec![
            "INSERT INTO SIMPLE_UDT_TEST (pk, ck, ut_col) VALUES (0, 0, {int_val: 1, bool_val: true})",
            "UPDATE SIMPLE_UDT_TEST SET ut_col = null WHERE pk = 0 AND ck = 0",
        ];

        test_replication_with_udt(&get_uri(), table_schema, udt_schemas, operations)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_map_insert() {
        let schema = TestTableSchema {
            name: "MAPS_INSERT".to_string(),
            partition_key: vec![("pk", "int")],
            clustering_key: vec![("ck", "int")],
            other_columns: vec![("v1", "map<int, int>"), ("v2", "map<int, boolean>")],
        };

        let operations = vec![
            "INSERT INTO MAPS_INSERT (pk, ck, v1, v2) VALUES (0, 1, {}, {})",
            "INSERT INTO MAPS_INSERT (pk, ck, v1, v2) VALUES (1, 2, {1: 1}, {})",
            "INSERT INTO MAPS_INSERT (pk, ck, v1, v2) VALUES (3, 4, {}, {10: true})",
            "INSERT INTO MAPS_INSERT (pk, ck, v1, v2) VALUES (5, 6, {100: 100, 200: 200, 300: 300}, {400: true, 500: false})",
        ];

        test_replication(&get_uri(), schema, operations)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_map_update() {
        let schema = TestTableSchema {
            name: "MAPS_UPDATE".to_string(),
            partition_key: vec![("pk", "int")],
            clustering_key: vec![("ck", "int")],
            other_columns: vec![("v1", "map<int, int>"), ("v2", "map<int, boolean>")],
        };

        let operations = vec![
            "INSERT INTO MAPS_UPDATE (pk, ck, v1, v2) VALUES (1, 2, {1: 1, 2: 2}, {1: true})",
            "UPDATE MAPS_UPDATE SET v2 = {2: true} WHERE pk = 10 AND ck = 20",
            "DELETE v1 FROM MAPS_UPDATE WHERE pk = 1 AND ck = 2",
        ];

        test_replication(&get_uri(), schema, operations)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_map_elements_update() {
        let schema = TestTableSchema {
            name: "MAP_ELEMENTS_UPDATE".to_string(),
            partition_key: vec![("pk", "int")],
            clustering_key: vec![("ck", "int")],
            other_columns: vec![("v1", "map<int, int>"), ("v2", "map<int, boolean>")],
        };

        let operations = vec![
            "INSERT INTO MAP_ELEMENTS_UPDATE (pk, ck, v1, v2) VALUES (1, 2, {1: -1}, {2: false})",
            "INSERT INTO MAP_ELEMENTS_UPDATE (pk, ck, v1, v2) VALUES (10, 20, {10: -10}, {11: false})",
            "UPDATE MAP_ELEMENTS_UPDATE SET v2 = v2 + {-21374134: true, -43142137: false} WHERE pk = 1 AND ck = 2",
            "UPDATE MAP_ELEMENTS_UPDATE SET v1 = v1 - {10} WHERE pk = 10 AND ck = 20",
            "UPDATE MAP_ELEMENTS_UPDATE SET v1 = v1 - {1}, v1 = v1 + {2137: -2137} WHERE pk = 1 AND ck = 2",
        ];
        test_replication(&get_uri(), schema, operations)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_row_delete() {
        let schema = TestTableSchema {
            name: "ROW_DELETE".to_string(),
            partition_key: vec![("pk", "int")],
            clustering_key: vec![("ck", "int")],
            other_columns: vec![("v1", "int"), ("v2", "boolean")],
        };

        let operations = vec![
            "INSERT INTO ROW_DELETE (pk, ck, v1, v2) VALUES (1, 2, 3, true)",
            "INSERT INTO ROW_DELETE (pk, ck, v1, v2) VALUES (10, 20, 30, true)",
            "INSERT INTO ROW_DELETE (pk, ck, v1, v2) VALUES (100, 200, 300, true)",
            "DELETE FROM ROW_DELETE WHERE pk = 1 AND ck = 2",
            "DELETE FROM ROW_DELETE WHERE pk = -1 AND ck = -2",
            "INSERT INTO ROW_DELETE (pk, ck, v1, v2) VALUES (-1, -2, 30, true)",
        ];

        test_replication(&get_uri(), schema, operations)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_set_insert() {
        let schema = TestTableSchema {
            name: "SET_TEST".to_string(),
            partition_key: vec![("pk", "int")],
            clustering_key: vec![("ck", "int")],
            other_columns: vec![("v", "set<int>")],
        };

        let operations = vec![
            "INSERT INTO SET_TEST (pk, ck, v) VALUES (0, 1, {})",
            "INSERT INTO SET_TEST (pk, ck, v) VALUES (1, 2, {1, 2})",
            "INSERT INTO SET_TEST (pk, ck, v) VALUES (3, 4, {1, 1})",
        ];

        test_replication(&get_uri(), schema, operations)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_set_overwrite() {
        let schema = TestTableSchema {
            name: "SET_TEST".to_string(),
            partition_key: vec![("pk", "int")],
            clustering_key: vec![("ck", "int")],
            other_columns: vec![("v", "set<int>")],
        };

        let operations = vec![
            "UPDATE SET_TEST SET v = {-1, -2} WHERE pk = 0 AND ck = 1",
            "UPDATE SET_TEST SET v = {1, 2} WHERE pk = 0 AND ck = 1",
        ];

        test_replication(&get_uri(), schema, operations)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_set_delete() {
        let schema = TestTableSchema {
            name: "SET_TEST".to_string(),
            partition_key: vec![("pk", "int")],
            clustering_key: vec![("ck", "int")],
            other_columns: vec![("v", "set<int>")],
        };

        let operations = vec![
            "INSERT INTO SET_TEST (pk, ck, v) VALUES (0, 1, {0, 1})",
            "DELETE v FROM SET_TEST WHERE pk = 0 AND ck = 1",
        ];

        test_replication(&get_uri(), schema, operations)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_set_update() {
        let schema = TestTableSchema {
            name: "SET_TEST".to_string(),
            partition_key: vec![("pk", "int")],
            clustering_key: vec![("ck", "int")],
            other_columns: vec![("v", "set<int>")],
        };

        let operations = vec![
            "UPDATE SET_TEST SET v = {1, 2} WHERE pk = 0 AND ck = 1",
            "UPDATE SET_TEST SET v = v - {1} WHERE pk = 0 AND ck = 1",
            "UPDATE SET_TEST SET v = v + {10, 20} WHERE pk = 0 AND ck = 1",
            "UPDATE SET_TEST SET v = v - {10}, v = v + {200} WHERE pk = 0 AND ck = 1",
        ];

        test_replication(&get_uri(), schema, operations)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_partition_delete() {
        let schema = TestTableSchema {
            name: "PARTITION_DELETE".to_string(),
            partition_key: vec![("pk", "int")],
            clustering_key: vec![("ck", "int")],
            other_columns: vec![("v", "int")],
        };

        let operations = vec![
            "INSERT INTO PARTITION_DELETE (pk, ck, v) VALUES (0, 0, 0)",
            "INSERT INTO PARTITION_DELETE (pk, ck, v) VALUES (0, 1, 1)",
            "DELETE FROM PARTITION_DELETE WHERE pk = 0",
        ];

        test_replication(&get_uri(), schema, operations)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_udt_insert() {
        let schema = TestTableSchema {
            name: "TEST_UDT_INSERT".to_string(),
            partition_key: vec![("pk", "int")],
            clustering_key: vec![("ck", "int")],
            other_columns: vec![("v", "ut")],
        };

        let udt_schemas = vec![TestUDTSchema {
            name: "ut".to_string(),
            fields: vec![("int_val", "int"), ("bool_val", "boolean")],
        }];

        let operations = vec![
            "INSERT INTO TEST_UDT_INSERT (pk, ck, v) VALUES (0, 1, {int_val: 1, bool_val: true})",
            "INSERT INTO TEST_UDT_INSERT (pk, ck, v) VALUES (1, 2, {int_val: 2, bool_val: false})",
            "INSERT INTO TEST_UDT_INSERT (pk, ck, v) VALUES (3, 4, {int_val: 3, bool_val: true})",
        ];

        test_replication_with_udt(&get_uri(), schema, udt_schemas, operations)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_partition_delete_with_multiple_pk() {
        let schema = TestTableSchema {
            name: "PARTITION_DELETE_MULT_PK".to_string(),
            partition_key: vec![("pk1", "int"), ("pk2", "int")],
            clustering_key: vec![("ck", "int")],
            other_columns: vec![("v1", "int"), ("v2", "boolean")],
        };

        let operations = vec![
            "INSERT INTO PARTITION_DELETE_MULT_PK (pk1, pk2, ck, v1, v2) VALUES (0, 1, 0, 0, true)",
            "INSERT INTO PARTITION_DELETE_MULT_PK (pk1, pk2, ck, v1, v2) VALUES (0, 2, 1, 1, false)",
            "DELETE FROM PARTITION_DELETE_MULT_PK WHERE pk1 = 0 AND pk2 = 2",
        ];

        test_replication(&get_uri(), schema, operations)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_list_update() {
        let schema = TestTableSchema {
            name: "LIST_ELEMENTS_UPDATE".to_string(),
            partition_key: vec![("pk", "int")],
            clustering_key: vec![("ck", "int")],
            other_columns: vec![("v", "list<int>")],
        };

        let operations = vec![
            "INSERT INTO LIST_ELEMENTS_UPDATE (pk, ck, v) VALUES (1, 2, [0, 1, 1, 2])",
            "UPDATE LIST_ELEMENTS_UPDATE SET v = v + [3, 5, 8, 13] WHERE pk = 1 AND ck = 2",
            "UPDATE LIST_ELEMENTS_UPDATE SET v = v - [1, 5] WHERE pk = 1 AND ck = 2",
        ];

        test_replication(&get_uri(), schema, operations)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_udt_update() {
        let schema = TestTableSchema {
            name: "TEST_UDT_UPDATE".to_string(),
            partition_key: vec![("pk", "int")],
            clustering_key: vec![("ck", "int")],
            other_columns: vec![("v", "ut")],
        };

        let udt_schemas = vec![TestUDTSchema {
            name: "ut".to_string(),
            fields: vec![("int_val", "int"), ("bool_val", "boolean")],
        }];

        let operations = vec![
            "INSERT INTO TEST_UDT_UPDATE (pk, ck, v) VALUES (0, 1, {int_val: 1, bool_val: true})",
            "UPDATE TEST_UDT_UPDATE SET v = {int_val: 3, bool_val: false} WHERE pk = 0 AND ck = 1",
            "UPDATE TEST_UDT_UPDATE SET v = null WHERE pk = 0 AND ck = 1",
        ];

        test_replication_with_udt(&get_uri(), schema, udt_schemas, operations)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_list_replace() {
        let schema = TestTableSchema {
            name: "LIST_REPLACE".to_string(),
            partition_key: vec![("pk", "int")],
            clustering_key: vec![("ck", "int")],
            other_columns: vec![("v", "list<int>")],
        };

        let operations = vec![
            "INSERT INTO LIST_REPLACE (pk, ck, v) VALUES (1, 2, [1, 3, 5, 7])",
            "UPDATE LIST_REPLACE SET v = [2, 4, 6, 8] WHERE pk = 1 AND ck = 2",
        ];

        test_replication(&get_uri(), schema, operations)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_checking_timestamps() {
        let schema = TestTableSchema {
            name: "COMPARE_TIME".to_string(),
            partition_key: vec![("pk", "int")],
            clustering_key: vec![("ck", "int")],
            other_columns: vec![("v1", "int"), ("v2", "boolean")],
        };

        let operations = vec![
            "INSERT INTO COMPARE_TIME (pk, ck, v1, v2) VALUES (1, 2, 3, true)",
            "UPDATE COMPARE_TIME SET v2 = false WHERE pk = 1 AND ck = 2",
        ];

        let session = Arc::new(
            SessionBuilder::new()
                .known_node(&get_uri())
                .build()
                .await
                .unwrap(),
        );
        let (ks_src, ks_dst) = setup_keyspaces(&session).await.unwrap();
        setup_tables(&session, &ks_src, &ks_dst, &schema)
            .await
            .unwrap();
        execute_queries(&session, &ks_src, operations)
            .await
            .unwrap();

        replicate(&session, &ks_src, &ks_dst, &schema.name)
            .await
            .unwrap();

        compare_changes(&session, &ks_src, &ks_dst, &schema.name)
            .await
            .unwrap();
        // We update timestamps for v2 column in src.
        session
            .query(
                "UPDATE COMPARE_TIME SET v2 = false WHERE pk = 1 AND ck = 2",
                [],
            )
            .await
            .unwrap();

        // Assert that replicator panics when timestamps are not matching.
        let result =
            std::panic::AssertUnwindSafe(compare_timestamps(&session, &ks_src, &ks_dst, &schema))
                .catch_unwind()
                .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_compare_time_for_complicated_types() {
        let schema = TestTableSchema {
            name: "COMPARE_TIME".to_string(),
            partition_key: vec![("pk", "int")],
            clustering_key: vec![("ck", "int")],
            other_columns: vec![
                ("v1", "tuple<int, int>"),
                ("v2", "map<int, int>"),
                ("v3", "set<frozen<map<int, int>>>"),
                ("v4", "tuple<map<int, int>>"),
                ("v5", "frozen<set<frozen<set<frozen<set<int>>>>>>"),
                ("v6", "tuple<tuple<map<int, int>, set<int>>, int>"),
                ("v7", "set<frozen<tuple<set<int>, map<int, int>>>>"),
            ],
        };

        let operations = vec![
            "UPDATE COMPARE_TIME SET v1 = (0, 0) WHERE pk = 1 AND ck = 1",
            "UPDATE COMPARE_TIME SET v2 = {1: 1} WHERE pk = 2 AND ck = 2",
            "UPDATE COMPARE_TIME SET v3 = {{1: 1}} WHERE pk = 3 AND ck = 3",
            "UPDATE COMPARE_TIME SET v4 = ({1: 1}) WHERE pk = 4 AND ck = 4",
            "UPDATE COMPARE_TIME SET v5 = {{{10, -10}}} WHERE pk = 5 AND ck = 5",
            "UPDATE COMPARE_TIME SET v6 = (({20: 30}, {4324}), null) WHERE pk = 6 AND ck = 6",
            "UPDATE COMPARE_TIME SET v7 = {({4324}, {42: 42})} WHERE pk = 5 AND ck = 5",
            "UPDATE COMPARE_TIME SET v7 = v7 + {({-4324}, {-42: -42})} WHERE pk = 5 AND ck = 5",
            "DELETE v4 FROM COMPARE_TIME WHERE pk = 4 AND CK = 4",
        ];

        test_replication(&get_uri(), schema, operations)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_udt_fields_update() {
        let schema = TestTableSchema {
            name: "TEST_UDT_ELEMENTS_UPDATE".to_string(),
            partition_key: vec![("pk", "int")],
            clustering_key: vec![("ck", "int")],
            other_columns: vec![("v", "ut")],
        };

        let udt_schemas = vec![TestUDTSchema {
            name: "ut".to_string(),
            fields: vec![("int_val", "int"), ("bool_val", "boolean")],
        }];

        let operations = vec![
            "INSERT INTO TEST_UDT_ELEMENTS_UPDATE (pk, ck, v) VALUES (0, 1, {int_val: 1})",
            "UPDATE TEST_UDT_ELEMENTS_UPDATE SET v.int_val = 2 WHERE pk = 0 AND ck = 1",
            "UPDATE TEST_UDT_ELEMENTS_UPDATE SET v.int_val = 5, v.bool_val = null WHERE pk = 0 AND ck = 1",
            "UPDATE TEST_UDT_ELEMENTS_UPDATE SET v.int_val = null, v.bool_val = false WHERE pk = 0 AND ck = 1",
        ];

        test_replication_with_udt(&get_uri(), schema, udt_schemas, operations)
            .await
            .unwrap();
    }
}
