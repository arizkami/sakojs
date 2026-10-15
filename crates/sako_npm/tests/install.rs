//! End-to-end install against a fake registry served from this process.

use base64::Engine as _;
use serde_json::json;
use sha2::Digest;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::sync::{Arc, Mutex};

/// Builds an npm-style .tgz ("package/..." entries).
fn tarball(name: &str, version: &str, extra: &[(&str, &str)]) -> Vec<u8> {
  let mut files = vec![
    ("package.json".to_string(), json!({ "name": name, "version": version }).to_string()),
    ("index.js".to_string(), format!("export default {:?};\n", format!("{name}@{version}"))),
  ];
  files.extend(extra.iter().map(|(p, c)| (p.to_string(), c.to_string())));
  let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast()));
  for (path, content) in files {
    let mut header = tar::Header::new_gnu();
    header.set_size(content.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    builder.append_data(&mut header, format!("package/{path}"), content.as_bytes()).unwrap();
  }
  builder.into_inner().unwrap().finish().unwrap()
}

type Routes = Arc<Mutex<HashMap<String, Vec<u8>>>>;

fn serve(routes: Routes) -> (String, Arc<std::sync::atomic::AtomicBool>) {
  let listener = TcpListener::bind("127.0.0.1:0").unwrap();
  let base = format!("http://{}/", listener.local_addr().unwrap());
  let down = Arc::new(std::sync::atomic::AtomicBool::new(false));
  let down2 = down.clone();
  std::thread::spawn(move || {
    for conn in listener.incoming().flatten() {
      let routes = routes.clone();
      let down = down2.clone();
      std::thread::spawn(move || {
        let mut conn = conn;
        let mut buf = vec![0u8; 8192];
        let n = conn.read(&mut buf).unwrap_or(0);
        let req = String::from_utf8_lossy(&buf[..n]);
        let path = req.split_whitespace().nth(1).unwrap_or("/").trim_start_matches('/').to_string();
        let body = if down.load(std::sync::atomic::Ordering::SeqCst) {
          None
        } else {
          routes.lock().unwrap().get(&path).cloned()
        };
        let head = match &body {
          Some(b) => format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", b.len()),
          None => "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_string(),
        };
        let _ = conn.write_all(head.as_bytes());
        if let Some(b) = body {
          let _ = conn.write_all(&b);
        }
      });
    }
  });
  (base, down)
}

fn publish(routes: &Routes, base: &str, name: &str, versions: &[(&str, serde_json::Value, &[(&str, &str)])], latest: &str) {
  let mut vs = serde_json::Map::new();
  for (version, extra, files) in versions {
    let tgz = tarball(name, version, files);
    let integrity = format!("sha512-{}", base64::engine::general_purpose::STANDARD.encode(sha2::Sha512::digest(&tgz)));
    let tar_path = format!("tgz/{}-{version}.tgz", name.replace('/', "+"));
    routes.lock().unwrap().insert(tar_path.clone(), tgz);
    let mut manifest = json!({
      "name": name,
      "version": version,
      "dist": { "tarball": format!("{base}{tar_path}"), "integrity": integrity },
    });
    for (k, v) in extra.as_object().unwrap() {
      manifest[k] = v.clone();
    }
    vs.insert(version.to_string(), manifest);
  }
  let doc = json!({ "name": name, "dist-tags": { "latest": latest }, "versions": vs });
  routes.lock().unwrap().insert(name.replace('/', "%2f"), doc.to_string().into_bytes());
}

fn version_of(dir: &Path, rel: &str) -> String {
  let text = std::fs::read_to_string(dir.join(rel).join("package.json")).unwrap();
  serde_json::from_str::<serde_json::Value>(&text).unwrap()["version"].as_str().unwrap().to_string()
}

#[test]
fn install_flow() {
  let tmp = std::env::temp_dir().join(format!("sako-npm-test-{}", std::process::id()));
  let _ = std::fs::remove_dir_all(&tmp);
  let project = tmp.join("project");
  std::fs::create_dir_all(&project).unwrap();

  let routes: Routes = Arc::default();
  let (base, down) = serve(routes.clone());
  let none: &[(&str, &str)] = &[];
  publish(&routes, &base, "b", &[("1.0.0", json!({}), none), ("2.0.0", json!({}), none)], "2.0.0");
  publish(
    &routes,
    &base,
    "a",
    &[("1.0.0", json!({ "dependencies": { "b": "^1" } }), none), ("2.0.0", json!({ "dependencies": { "b": "^2" } }), none)],
    "2.0.0",
  );
  publish(
    &routes,
    &base,
    "c",
    &[("1.0.0", json!({ "dependencies": { "b": "^1" }, "bin": { "c-cli": "./cli.js" } }), &[("cli.js", "console.log('c')\n")])],
    "1.0.0",
  );
  publish(&routes, &base, "@s/d", &[("3.1.0", json!({}), none)], "3.1.0");
  publish(
    &routes,
    &base,
    "native",
    &[("1.0.0", json!({ "os": ["nonexistent-os"] }), none)],
    "1.0.0",
  );
  // SAFETY: this is the only test in this binary; nothing else reads env.
  unsafe {
    std::env::set_var("SAKO_NPM_REGISTRY", &base);
    std::env::set_var("SAKO_NPM_CACHE", tmp.join("cache"));
  }

  std::fs::write(
    project.join("package.json"),
    json!({
      "dependencies": { "a": "^2", "c": "^1", "@s/d": "*", "alias": "npm:b@^1" },
      "optionalDependencies": { "native": "1.0.0" }
    })
    .to_string(),
  )
  .unwrap();
  let opts = sako_npm::Options { dir: project.clone(), production: false };
  let s = sako_npm::install(&opts).unwrap();
  assert!(!s.from_lockfile);
  // a@2, b@2, b@1 (shared by the nested copy and the alias), c@1, @s/d@3.1.0
  assert_eq!(s.downloaded, 5);
  // a@2 and its b@2 are hoisted; c's b@^1 conflicts, so it nests.
  assert_eq!(version_of(&project, "node_modules/a"), "2.0.0");
  assert_eq!(version_of(&project, "node_modules/b"), "2.0.0");
  assert_eq!(version_of(&project, "node_modules/c/node_modules/b"), "1.0.0");
  assert_eq!(version_of(&project, "node_modules/@s/d"), "3.1.0");
  assert_eq!(version_of(&project, "node_modules/alias"), "1.0.0");
  assert!(!project.join("node_modules/native").exists(), "optional dep for another OS is skipped");
  #[cfg(unix)]
  assert_eq!(
    std::fs::read_link(project.join("node_modules/.bin/c-cli")).unwrap(),
    Path::new("../c/cli.js")
  );
  assert!(project.join("sako.lock").exists());

  // Unchanged: nothing to do.
  assert!(sako_npm::install(&opts).unwrap().up_to_date);

  // Registry down: the lockfile and the store are enough.
  down.store(true, std::sync::atomic::Ordering::SeqCst);
  std::fs::remove_dir_all(project.join("node_modules")).unwrap();
  let s = sako_npm::install(&opts).unwrap();
  assert!(s.from_lockfile);
  assert_eq!(s.downloaded, 0);
  assert_eq!(version_of(&project, "node_modules/c/node_modules/b"), "1.0.0");
  down.store(false, std::sync::atomic::Ordering::SeqCst);

  // A tampered tarball is rejected.
  routes.lock().unwrap().insert("tgz/@s+d-3.1.0.tgz".into(), tarball("@s/d", "3.1.0", &[("evil.js", "1")]));
  std::fs::remove_dir_all(tmp.join("cache/pkgs/@s+d@3.1.0")).unwrap();
  std::fs::remove_dir_all(project.join("node_modules")).unwrap();
  let err = sako_npm::install(&opts).err().unwrap();
  assert!(err.contains("integrity"), "{err}");

  let _ = std::fs::remove_dir_all(&tmp);
}
