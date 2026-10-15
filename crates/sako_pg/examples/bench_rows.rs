//! cargo run --release -p sako_pg --example bench_rows -- <url>
use sako_pg::{Config, Connection};
fn main() {
  let url = std::env::args().nth(1).expect("url");
  let mut c = Connection::connect(&Config::parse(&url).unwrap()).unwrap();
  c.simple_query("create table if not exists bench_ex as select g as id, 'name ' || g as name, g * 1.5 as score, now() as created from generate_series(1, 1000) g").unwrap();
  for (label, q) in [("all", "select * from bench_ex"), ("created", "select created from bench_ex"), ("id", "select id from bench_ex")] {
    for _ in 0..50 { c.query(q, &[]).unwrap(); }
    let n = 2000;
    let t = std::time::Instant::now();
    let mut cells = 0;
    for _ in 0..n { cells += c.query(q, &[]).unwrap().cells.len(); }
    println!("{label}: {:.0} q/s ({} cells)", n as f64 / t.elapsed().as_secs_f64(), cells / n);
  }
  c.simple_query("drop table bench_ex").unwrap();
}
