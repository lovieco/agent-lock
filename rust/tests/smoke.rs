mod common;
use common::*;

#[test]
fn helpers_work() {
    let r = make_repo(true, &[]);
    assert!(r.git_dir.join("HEAD").exists());
    let c = clone(&r, "other");
    assert!(c.join("README.md").exists());
    let j = serde_free::parse(r#"{"a":{"b":[1,"x\"y",true]}}"#);
    assert_eq!(j.get("a").get("b").arr()[1].as_str(), Some("x\"y"));
    assert_ne!(dead_pid(), std::process::id());
}
