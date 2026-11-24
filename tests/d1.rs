//! Milestone-0 spike / D1 smoke test. Runs against a real, THROWAWAY D1 database:
//!   CF_ACCOUNT_ID=.. D1_DATABASE_ID=.. CF_API_TOKEN=.. cargo test --test d1 -- --ignored --nocapture
//! Verifies the REST assumptions documented in src/db.rs.

use rlinks::db::{Db, Row};
use rlinks::stmt;
use std::time::Instant;

fn d1() -> Db {
    let v = |k| std::env::var(k).unwrap_or_else(|_| panic!("{k} not set"));
    Db::d1(&v("CF_ACCOUNT_ID"), &v("D1_DATABASE_ID"), &v("CF_API_TOKEN"))
}

#[tokio::test]
#[ignore]
async fn d1_rest_semantics() {
    let db = d1();
    db.migrate().await.expect("migrate (multi-statement batch)");
    db.execute(stmt!(
        "CREATE TABLE IF NOT EXISTS spike (id INTEGER PRIMARY KEY, v TEXT UNIQUE)"
    ))
    .await
    .unwrap();
    db.execute(stmt!("DELETE FROM spike")).await.unwrap();

    // Batch shape + per-statement params + RETURNING + results in order.
    let t = Instant::now();
    let res = db
        .batch(vec![
            stmt!("INSERT INTO spike (v) VALUES (?) RETURNING id", "a"),
            stmt!("INSERT INTO spike (v) VALUES (?)", "b"),
            stmt!("SELECT last_insert_rowid() AS last"),
            stmt!("SELECT v FROM spike ORDER BY v"),
        ])
        .await
        .expect("batch");
    println!("batch round-trip: {:?}", t.elapsed());
    assert!(res[0][0]["id"].as_i64().is_some(), "RETURNING inside batch");
    println!("last_insert_rowid in batch: {}", res[2][0]["last"]);
    assert_eq!(res[3].len(), 2);

    // Atomicity: second statement violates UNIQUE, first must be rolled back.
    let failed = db
        .batch(vec![
            stmt!("INSERT INTO spike (v) VALUES ('c')"),
            stmt!("INSERT INTO spike (v) VALUES ('a')"),
        ])
        .await;
    assert!(failed.is_err());
    let rows: Vec<Row> = db.query(stmt!("SELECT v FROM spike WHERE v = 'c'")).await.unwrap();
    assert!(rows.is_empty(), "batch is NOT atomic over REST");

    // Single-statement latency sample.
    let t = Instant::now();
    for _ in 0..5 {
        db.execute(stmt!("SELECT 1")).await.unwrap();
    }
    println!("avg single round-trip: {:?}", t.elapsed() / 5);
    db.execute(stmt!("DROP TABLE spike")).await.unwrap();
}
