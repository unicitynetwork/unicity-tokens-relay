//! UNIP-01 tests against a real Postgres: single-owner enforcement, legacy
//! ownership recording on ingest, and the m009 re-backfill.
//!
//! Ignored by default because they need a server. Run them with
//! `TEST_POSTGRES_URL=postgres://user:pass@host:5432/postgres cargo test --lib postgres_tests -- --ignored`.
//! Each test creates its own database through that URL and drops it at the end.

use crate::error::Error;
use crate::event::Event;
use crate::repo::postgres::{PostgresPool, PostgresRepo};
use crate::repo::NostrRepo;
use crate::server::create_metrics;
use sqlx::pool::PoolOptions;
use sqlx::postgres::PgConnectOptions;
use sqlx::{ConnectOptions, Executor};

struct TestDb {
    admin: PgConnectOptions,
    name: String,
    pool: PostgresPool,
    repo: PostgresRepo,
}

impl TestDb {
    async fn new() -> TestDb {
        let url = std::env::var("TEST_POSTGRES_URL")
            .expect("TEST_POSTGRES_URL must point at a Postgres server for these tests");
        let admin: PgConnectOptions = url.parse().unwrap();
        let name = format!("unip01_{}", uuid::Uuid::new_v4().simple());
        let mut conn = admin.connect().await.unwrap();
        conn.execute(format!("CREATE DATABASE {name}").as_str())
            .await
            .unwrap();
        let pool: PostgresPool = PoolOptions::new()
            .max_connections(4)
            .connect_with(admin.clone().database(&name))
            .await
            .unwrap();
        let (_, metrics) = create_metrics();
        let repo = PostgresRepo::new(
            pool.clone(),
            pool.clone(),
            metrics,
            false,
            vec!["unicity:nametag".to_owned()],
        );
        repo.migrate_up().await.unwrap();
        TestDb {
            admin,
            name,
            pool,
            repo,
        }
    }

    async fn owner(&self, d_tag: &str) -> Option<Vec<u8>> {
        sqlx::query_scalar(
            "SELECT author FROM namespace_owner WHERE namespace = 'unicity:nametag' AND d_tag = $1",
        )
        .bind(d_tag)
        .fetch_optional(&self.pool)
        .await
        .unwrap()
    }

    /// Re-run m009 as an operator would: forget it was applied, then migrate.
    async fn rerun_m009(&self) {
        self.pool
            .execute("DELETE FROM migrations WHERE serial_number = 9")
            .await
            .unwrap();
        self.repo.migrate_up().await.unwrap();
    }

    async fn drop(self) {
        self.pool.close().await;
        let mut conn = self.admin.connect().await.unwrap();
        conn.execute(format!("DROP DATABASE {}", self.name).as_str())
            .await
            .unwrap();
    }
}

fn binding(id: &str, author: &str, d_tag: &str, label: Option<&str>, content: &str) -> Event {
    let mut tags = vec![vec!["d".to_owned(), d_tag.to_owned()]];
    if let Some(label) = label {
        tags.push(vec!["L".to_owned(), label.to_owned()]);
    }
    Event {
        id: id.to_owned(),
        pubkey: author.to_owned(),
        delegated_by: None,
        created_at: 1_700_000_000,
        kind: 30078,
        tags,
        content: content.to_owned(),
        sig: "00".to_owned(),
        tagidx: None,
    }
}

const MARKED: Option<&str> = Some("unicity:nametag");

fn nametag_content(d_tag: &str) -> String {
    format!("{{\"nametag_hash\":\"{d_tag}\"}}")
}

fn key(byte: &str) -> String {
    byte.repeat(32)
}

fn author_bytes(author: &str) -> Option<Vec<u8>> {
    Some(hex::decode(author).unwrap())
}

fn sha256_hex(input: &str) -> String {
    use bitcoin_hashes::{sha256, Hash};
    format!("{:x}", sha256::Hash::hash(input.as_bytes()))
}

#[tokio::test]
#[ignore = "needs Postgres: set TEST_POSTGRES_URL"]
async fn marked_binding_has_single_owner() {
    let db = TestDb::new().await;
    let (a, b) = (key("aa"), key("bb"));

    let a1 = binding(&key("11"), &a, "nm1", MARKED, &nametag_content("nm1"));
    assert_eq!(db.repo.write_event(&a1).await.unwrap(), 1);

    let b1 = binding(&key("22"), &b, "nm1", MARKED, &nametag_content("nm1"));
    assert!(matches!(
        db.repo.write_event(&b1).await,
        Err(Error::EventBlocked(_))
    ));

    let mut a2 = binding(&key("33"), &a, "nm1", MARKED, &nametag_content("nm1"));
    a2.created_at += 1;
    assert_eq!(db.repo.write_event(&a2).await.unwrap(), 1);
    assert_eq!(db.owner("nm1").await, author_bytes(&a));
    db.drop().await;
}

#[tokio::test]
#[ignore = "needs Postgres: set TEST_POSTGRES_URL"]
async fn legacy_binding_records_owner() {
    let db = TestDb::new().await;
    let (a, b) = (key("aa"), key("bb"));

    let legacy_a = binding(&key("11"), &a, "nm2", None, &nametag_content("nm2"));
    assert_eq!(db.repo.write_event(&legacy_a).await.unwrap(), 1);
    assert_eq!(db.owner("nm2").await, author_bytes(&a));

    let marked_b = binding(&key("22"), &b, "nm2", MARKED, &nametag_content("nm2"));
    assert!(matches!(
        db.repo.write_event(&marked_b).await,
        Err(Error::EventBlocked(_))
    ));

    // The legacy owner can move to a marked binding.
    let mut marked_a = binding(&key("33"), &a, "nm2", MARKED, &nametag_content("nm2"));
    marked_a.created_at += 1;
    assert_eq!(db.repo.write_event(&marked_a).await.unwrap(), 1);
    db.drop().await;
}

#[tokio::test]
#[ignore = "needs Postgres: set TEST_POSTGRES_URL"]
async fn legacy_binding_from_non_owner_is_rejected() {
    let db = TestDb::new().await;
    let (a, b) = (key("aa"), key("bb"));
    let nm = key("a3");

    let marked_a = binding(&key("11"), &a, &nm, MARKED, &nametag_content(&nm));
    assert_eq!(db.repo.write_event(&marked_a).await.unwrap(), 1);

    let legacy_b = binding(&key("22"), &b, &nm, None, &nametag_content(&nm));
    assert!(matches!(
        db.repo.write_event(&legacy_b).await,
        Err(Error::EventBlocked(_))
    ));
    assert_eq!(db.owner(&nm).await, author_bytes(&a));
    db.drop().await;
}

/// Only the owner of a nametag may refer to its hash from a kind-30078 event,
/// by `d` tag or `t` tag, whatever else the event carries: resolvers fetch
/// bindings by `#t` and match the `d` tag.
#[tokio::test]
#[ignore = "needs Postgres: set TEST_POSTGRES_URL"]
async fn other_authors_cannot_refer_to_owned_nametag() {
    let db = TestDb::new().await;
    let (a, b) = (key("aa"), key("bb"));
    let nm = key("a1");
    let t_tag = vec!["t".to_owned(), nm.clone()];

    let owner = binding(&key("11"), &a, &nm, MARKED, &nametag_content(&nm));
    assert_eq!(db.repo.write_event(&owner).await.unwrap(), 1);

    // B's marked binding for its own nametag, also tagged with A's.
    let other_nm = key("b2");
    let mut marked_t = binding(&key("12"), &b, &other_nm, MARKED, "{}");
    marked_t.tags.push(t_tag.clone());
    let mut unmarked_t = binding(&key("13"), &b, &key("c3"), None, "{}");
    unmarked_t.tags.push(t_tag.clone());
    let not_a_binding = binding(&key("14"), &b, &nm, None, "x");
    let other_label = binding(&key("15"), &b, &nm, Some("other:ns"), &nametag_content(&nm));
    for e in [&marked_t, &unmarked_t, &not_a_binding, &other_label] {
        assert!(
            matches!(db.repo.write_event(e).await, Err(Error::EventBlocked(_))),
            "event {} must be blocked",
            e.id
        );
    }
    // The rejected marked binding recorded no owner for B's nametag.
    assert_eq!(db.owner(&other_nm).await, None);

    // The owner may refer to its nametag from other events.
    let mut identity = binding(
        &key("16"),
        &a,
        &key("d4"),
        None,
        "{\"public_key\":\"02ab\"}",
    );
    identity.tags.push(t_tag);
    assert_eq!(db.repo.write_event(&identity).await.unwrap(), 1);
    assert_eq!(db.owner(&nm).await, author_bytes(&a));
    db.drop().await;
}

/// A key that claims a victim's address hash or identity-binding d-tag as a
/// nametag does not block the victim's bindings, which carry the address and
/// are authored by the identity's key.
#[tokio::test]
#[ignore = "needs Postgres: set TEST_POSTGRES_URL"]
async fn claimed_address_and_identity_hashes_do_not_block_their_owner() {
    let db = TestDb::new().await;
    let (attacker, victim) = (key("cc"), key("dd"));
    let address_hash = sha256_hex("unicity:address:DIRECT://v");
    let identity_d = sha256_hex(&format!("unicity:identity:{victim}"));

    let claim_address = binding(&key("31"), &attacker, &address_hash, MARKED, "{}");
    let claim_identity = binding(&key("32"), &attacker, &identity_d, MARKED, "{}");
    for e in [&claim_address, &claim_identity] {
        assert_eq!(db.repo.write_event(e).await.unwrap(), 1);
    }

    let mut identity = binding(
        &key("33"),
        &victim,
        &identity_d,
        None,
        "{\"direct_address\":\"DIRECT://v\"}",
    );
    identity
        .tags
        .push(vec!["t".to_owned(), address_hash.clone()]);
    let nm = key("a5");
    let mut nametag_binding = binding(
        &key("34"),
        &victim,
        &nm,
        MARKED,
        &format!("{{\"nametag_hash\":\"{nm}\",\"address\":\"DIRECT://v\"}}"),
    );
    nametag_binding.tags.push(vec!["t".to_owned(), nm.clone()]);
    nametag_binding
        .tags
        .push(vec!["t".to_owned(), address_hash]);
    for e in [&identity, &nametag_binding] {
        assert_eq!(
            db.repo.write_event(e).await.unwrap(),
            1,
            "event {} must be stored",
            e.id
        );
    }
    db.drop().await;
}

/// References are checked only for owned identifiers in the nametag hash
/// format, and only on kind-30078 events.
#[tokio::test]
#[ignore = "needs Postgres: set TEST_POSTGRES_URL"]
async fn unowned_and_non_hash_references_are_not_enforced() {
    let db = TestDb::new().await;
    let (a, b) = (key("aa"), key("bb"));
    let nm = key("a1");

    let plain = binding(&key("21"), &a, "settings", MARKED, "{}");
    let owner = binding(&key("22"), &a, &nm, MARKED, "{}");
    for e in [&plain, &owner] {
        assert_eq!(db.repo.write_event(e).await.unwrap(), 1);
    }

    let mut same_plain_tags = binding(&key("23"), &b, "settings", None, "{}");
    same_plain_tags
        .tags
        .push(vec!["t".to_owned(), "settings".to_owned()]);
    let mut unowned_t = binding(&key("24"), &b, &key("e5"), None, "{}");
    unowned_t.tags.push(vec!["t".to_owned(), key("f6")]);
    let mut note = binding(&key("25"), &b, "x", None, "hi");
    note.kind = 1;
    note.tags.push(vec!["t".to_owned(), nm.clone()]);
    for e in [&same_plain_tags, &unowned_t, &note] {
        assert_eq!(
            db.repo.write_event(e).await.unwrap(),
            1,
            "event {} must be stored",
            e.id
        );
    }
    db.drop().await;
}

#[tokio::test]
#[ignore = "needs Postgres: set TEST_POSTGRES_URL"]
async fn non_binding_app_data_records_no_owner() {
    let db = TestDb::new().await;
    let a = key("aa");

    let identity = binding(&key("11"), &a, "id1", None, "{\"public_key\":\"02ab\"}");
    let mention = binding(&key("22"), &a, "app1", None, "{\"note\":\"nametag_hash\"}");
    let other_ns = binding(
        &key("33"),
        &a,
        "oth1",
        Some("other:ns"),
        &nametag_content("oth1"),
    );
    for e in [&identity, &mention, &other_ns] {
        assert_eq!(db.repo.write_event(e).await.unwrap(), 1);
    }
    for d_tag in ["id1", "app1", "oth1"] {
        assert_eq!(db.owner(d_tag).await, None, "{d_tag}");
    }
    db.drop().await;
}

/// m009 gives unowned nametag bindings, marked or legacy, the author the relay
/// saw first, keeps recorded owners, and skips events that are not nametag
/// claims.
#[tokio::test]
#[ignore = "needs Postgres: set TEST_POSTGRES_URL"]
async fn rebackfill_records_unowned_bindings() {
    let db = TestDb::new().await;
    let (a, b, c, m) = (key("aa"), key("bb"), key("cc"), key("dd"));

    let events = [
        binding(&key("11"), &a, "nm5", MARKED, &nametag_content("nm5")),
        binding(&key("22"), &b, "nm6", None, &nametag_content("nm6")),
        binding(&key("33"), &c, "nm6", None, &nametag_content("nm6")),
        binding(&key("44"), &m, "nm7", MARKED, "{\"address\":\"x\"}"),
        binding(&key("55"), &a, "app1", None, "{\"note\":\"nametag_hash\"}"),
        binding(
            &key("66"),
            &a,
            "oth1",
            Some("other:ns"),
            &nametag_content("oth1"),
        ),
    ];
    for e in &events {
        assert_eq!(db.repo.write_event(e).await.unwrap(), 1);
    }
    // Owners lost (as for bindings stored while ingest recorded none); the relay
    // saw C's nm6 binding before B's although C's was written last.
    sqlx::query("UPDATE \"event\" SET first_seen = now() - interval '2 hours' WHERE pub_key = $1")
        .bind(hex::decode(&b).unwrap())
        .execute(&db.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE \"event\" SET first_seen = now() - interval '3 hours' WHERE pub_key = $1")
        .bind(hex::decode(&c).unwrap())
        .execute(&db.pool)
        .await
        .unwrap();
    db.pool
        .execute("DELETE FROM namespace_owner WHERE d_tag IN ('nm6', 'nm7')")
        .await
        .unwrap();

    db.rerun_m009().await;
    assert_eq!(db.owner("nm5").await, author_bytes(&a));
    assert_eq!(db.owner("nm6").await, author_bytes(&c));
    assert_eq!(db.owner("nm7").await, author_bytes(&m));
    assert_eq!(db.owner("app1").await, None);
    assert_eq!(db.owner("oth1").await, None);
    db.drop().await;
}

/// m009 reads candidates through a cursor in batches of 1000; every batch is
/// processed.
#[tokio::test]
#[ignore = "needs Postgres: set TEST_POSTGRES_URL"]
async fn rebackfill_processes_every_batch() {
    let db = TestDb::new().await;
    db.pool
        .execute(
            r#"INSERT INTO "event" (id, pub_key, created_at, kind, "content", first_seen)
SELECT sha256(convert_to('event' || i, 'UTF8')), sha256(convert_to('author' || i, 'UTF8')), now(), 30078,
       convert_to(json_build_object(
         'id', encode(sha256(convert_to('event' || i, 'UTF8')), 'hex'),
         'pubkey', encode(sha256(convert_to('author' || i, 'UTF8')), 'hex'),
         'created_at', 1700000000, 'kind', 30078,
         'tags', json_build_array(json_build_array('d', 'bulk-' || i)),
         'content', '{"nametag_hash":"bulk"}', 'sig', '00')::text, 'UTF8'),
       now() - interval '1 day' + i * interval '1 second'
FROM generate_series(1, 2500) AS i"#,
        )
        .await
        .unwrap();

    db.rerun_m009().await;
    let owners: i64 = sqlx::query_scalar("SELECT count(*) FROM namespace_owner")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(owners, 2500);
    db.drop().await;
}
