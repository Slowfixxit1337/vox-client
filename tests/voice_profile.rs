#![cfg(feature = "lab")]

use std::{fs, process::Command};
use toml::Value;

#[test]
fn voice_keys_are_directional_and_absent_from_server_files() {
    let temp = tempfile::tempdir().unwrap();
    let lab = temp.path().join("room");
    let result = Command::new(env!("CARGO_BIN_EXE_vox-lab"))
        .arg("init")
        .arg("--out")
        .arg(&lab)
        .arg("--voice")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let a: Value = toml::from_str(&fs::read_to_string(lab.join("client-a.toml")).unwrap()).unwrap();
    let b: Value = toml::from_str(&fs::read_to_string(lab.join("client-b.toml")).unwrap()).unwrap();
    let outbound = a["voice"]["send_key"].as_str().unwrap();
    let inbound = a["voice"]["receive_key"].as_str().unwrap();
    assert_eq!(Some(outbound), b["voice"]["receive_key"].as_str());
    assert_eq!(Some(inbound), b["voice"]["send_key"].as_str());
    assert_ne!(outbound, inbound);
    for field in [outbound, inbound] {
        assert_eq!(hex::decode(field).unwrap().len(), 32);
        for server_file in ["admission.toml", "lab-sfu.key"] {
            let content = fs::read_to_string(lab.join(server_file)).unwrap();
            assert!(!content.contains(field));
        }
        assert!(!String::from_utf8_lossy(&result.stdout).contains(field));
        assert!(!String::from_utf8_lossy(&result.stderr).contains(field));
    }
}
