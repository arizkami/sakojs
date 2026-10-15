//! Against a real server: set SAKO_PG_TEST_URL (e.g.
//! postgres://sako:sako@127.0.0.1:54329/postgres). Skipped otherwise.

use sako_pg::{Config, Connection, Error, Pool, Value};

fn url() -> Option<String> {
  std::env::var("SAKO_PG_TEST_URL").ok()
}

#[test]
fn live_server() {
  let Some(url) = url() else {
    eprintln!("SAKO_PG_TEST_URL not set; skipping");
    return;
  };
  let config = Config::parse(&url).unwrap();
  let mut c = Connection::connect(&config).expect("SCRAM login");
  assert!(c.server_params.contains_key("server_version"));

  // Types
  let r = c
    .query(
      "select $1::int4 as i, $2::text as t, $3::bool as b, $4::float8 as f, $5::int8 as big, \
       $6::jsonb as j, '\\x6869'::bytea as bin, '2000-03-01 12:30:45.5+02'::timestamptz as ts, \
       array[1,null,3]::int4[] as arr, array['a,b','c']::text[] as tarr, null::text as n, \
       '12.50'::numeric as num",
      &[Some("42".into()), Some("hi".into()), Some("t".into()), Some("1.5".into()), Some("9007199254740993".into()), Some("{\"k\":[1]}".into())],
    )
    .unwrap();
  assert_eq!(r.command, "SELECT");
  assert_eq!(r.count, 1);
  let names: Vec<&str> = r.columns.iter().map(|c| c.name.as_str()).collect();
  assert_eq!(names, ["i", "t", "b", "f", "big", "j", "bin", "ts", "arr", "tarr", "n", "num"]);
  let rows = r.rows();
  let row = &rows[0];
  assert_eq!(row[0], Value::Int(42));
  assert_eq!(row[1], Value::Text("hi".into()));
  assert_eq!(row[2], Value::Bool(true));
  assert_eq!(row[3], Value::Float(1.5));
  assert_eq!(row[4], Value::Text("9007199254740993".into()));
  assert_eq!(row[5], Value::Json("{\"k\": [1]}".into()));
  assert_eq!(row[6], Value::Bytes(b"hi".to_vec()));
  assert_eq!(row[7], Value::Date(951_906_645_500.0));
  assert_eq!(row[8], Value::Array(vec![Value::Int(1), Value::Null, Value::Int(3)]));
  assert_eq!(row[9], Value::Array(vec![Value::Text("a,b".into()), Value::Text("c".into())]));
  assert_eq!(row[10], Value::Null);
  assert_eq!(row[11], Value::Text("12.50".into()));

  // The second run of a statement uses binary results for fixed-size types;
  // values must be identical to the first (text) run.
  let typed_sql = "select v::numeric as n, v::float8 as f, trunc(v)::int8 as i8, (v % 1000 * 1000)::int4 as i4, \
     (timestamptz '2000-03-01 12:30:45.123456+02' + (v % 1000) * interval '1 day') as ts, \
     (date '1999-12-31' + (v % 1000)::int4) as d, (v > 0) as b, decode(md5(v::text), 'hex') as h \
     from unnest(array[0, 1, -1, 12345.678901, 0.0001, -98765432109876.54321, 1e-20]::numeric[]) v";
  let first = c.query(typed_sql, &[]).unwrap();
  let second = c.query(typed_sql, &[]).unwrap();
  assert!(first.binary.is_empty() || first.binary.iter().all(|b| !b));
  assert!(second.binary.iter().any(|&b| b));
  assert_eq!(first.rows(), second.rows());
  assert_eq!(second.value(5, 0), Value::Text("-98765432109876.54321".into()));
  for special in ["'NaN'::numeric", "'Infinity'::timestamptz", "'-infinity'::date"] {
    let q = format!("select {special} as v");
    let a = c.query(&q, &[]).unwrap().rows();
    let b = c.query(&q, &[]).unwrap().rows();
    assert_eq!(a, b, "{special}");
  }

  // DDL/DML, command tags, prepared statement reuse
  c.simple_query("drop table if exists sako_t; create table sako_t (id serial primary key, name text unique)").unwrap();
  for n in ["a", "b", "c"] {
    let r = c.query("insert into sako_t (name) values ($1)", &[Some(n.into())]).unwrap();
    assert_eq!((r.command.as_str(), r.count), ("INSERT", 1));
  }
  let r = c.query("update sako_t set name = name || '!' where name <> $1", &[Some("a".into())]).unwrap();
  assert_eq!((r.command.as_str(), r.count), ("UPDATE", 2));

  // Server errors carry their fields; the connection stays usable.
  match c.query("insert into sako_t (name) values ($1)", &[Some("a".into())]) {
    Err(Error::Db(e)) => {
      assert_eq!(e.code, "23505");
      assert_eq!(e.constraint.as_deref(), Some("sako_t_name_key"));
    }
    other => panic!("expected unique violation, got {other:?}"),
  }
  match c.query("selec 1", &[]) {
    Err(Error::Db(e)) => assert_eq!(e.code, "42601"),
    other => panic!("expected syntax error, got {other:?}"),
  }
  assert_eq!(c.query("select count(*)::int4 from sako_t", &[]).unwrap().rows()[0][0], Value::Int(3));

  // Pipelined batch: an error fails only its own statement, and a statement
  // prepared earlier in the batch is reused by later entries.
  use sako_pg::BatchQuery;
  let one = [Some("1".to_string())];
  let two = [Some("2".to_string())];
  let results = c
    .query_batch(&[
      BatchQuery { sql: "select $1::int4 * 10 as v", params: &one, types: &[] },
      BatchQuery { sql: "select 1/0", params: &[], types: &[] },
      BatchQuery { sql: "select $1::int4 * 10 as v", params: &two, types: &[] },
    ])
    .unwrap();
  assert_eq!(results[0].as_ref().unwrap().value(0, 0), Value::Int(10));
  assert!(matches!(&results[1], Err(Error::Db(e)) if e.code == "22012"));
  assert_eq!(results[2].as_ref().unwrap().value(0, 0), Value::Int(20));

  // MD5 authentication
  c.simple_query("drop role if exists md5user").unwrap();
  c.simple_query("set password_encryption = 'md5'; create role md5user login password 'md5pass'").unwrap();
  let mut md5_config = config.clone();
  md5_config.user = "md5user".into();
  md5_config.password = Some("md5pass".into());
  let mut m = Connection::connect(&md5_config).expect("MD5 login");
  assert_eq!(m.query("select current_user::text", &[]).unwrap().rows()[0][0], Value::Text("md5user".into()));
  md5_config.password = Some("wrong".into());
  match Connection::connect(&md5_config) {
    Err(Error::Db(e)) => assert_eq!(e.code, "28P01"),
    Err(other) => panic!("expected auth failure, got {other}"),
    Ok(_) => panic!("expected auth failure"),
  }

  // Pool: concurrent use, transactions, rollback on release
  let mut pc = config.clone();
  pc.max = 4;
  let pool = Pool::new(pc);
  std::thread::scope(|s| {
    for t in 0..8 {
      let pool = pool.clone();
      s.spawn(move || {
        for i in 0..25 {
          let mut conn = pool.get().unwrap();
          let r = conn.query("select $1::int4 + $2::int4", &[Some(t.to_string()), Some(i.to_string())]).unwrap();
          assert_eq!(r.rows()[0][0], Value::Int(t + i));
        }
      });
    }
  });
  {
    let mut conn = pool.get().unwrap();
    conn.simple_query("begin").unwrap();
    conn.query("insert into sako_t (name) values ($1)", &[Some("in-tx".into())]).unwrap();
    // dropped inside the transaction: released connections are rolled back
  }
  let mut conn = pool.get().unwrap();
  assert_eq!(conn.query("select count(*)::int4 from sako_t where name = 'in-tx'", &[]).unwrap().rows()[0][0], Value::Int(0));
  conn.simple_query("drop table sako_t; drop role md5user").unwrap();
  drop(conn);
  pool.close();
  assert!(matches!(pool.get(), Err(Error::PoolClosed)));
}
