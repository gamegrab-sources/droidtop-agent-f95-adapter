//! The program as droidtop-agent runs it: adapter protocol 1 over standard
//! input and output.

use std::process::Command;

use serde_json::{json, Value};

const EXE: &str = env!("CARGO_BIN_EXE_droidtop-agent-f95-adapter");

#[test]
fn describe_names_the_context_and_protocol() {
    let out = Command::new(EXE).arg("describe").output().unwrap();
    assert!(out.status.success());
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["protocol"], json!(1));
    assert_eq!(v["id"], json!("f95checker"));
}

#[test]
fn a_missing_database_is_an_error_the_agent_can_show() {
    let out = Command::new(EXE).arg("pull").env("F95CHECKER_DB", std::env::temp_dir().join("no-such-f95checker.sqlite3")).output().unwrap();
    assert!(!out.status.success());
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(v["error"].as_str().unwrap().contains("not on this computer"), "{v}");
}
