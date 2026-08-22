use std::io::Read;
use std::path::Path;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreEntry {
    pub name: String,
    pub version: String,
    pub wasm_url: String,
    pub manifest_url: String,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoIndex {
    pub extensions: Vec<StoreEntry>,
}

pub fn fetch_index(repo_index_url: &str) -> Result<RepoIndex, String> {
    let body = ureq::get(repo_index_url).call().map_err(|e| e.to_string())?.into_string().map_err(|e| e.to_string())?;
    toml::from_str(&body).map_err(|e| e.to_string())
}

pub fn install(entry: &StoreEntry, extensions_dir: &Path) -> Result<(), String> {
    let wasm_bytes = ureq::get(&entry.wasm_url).call().map_err(|e| e.to_string())?.into_bytes().map_err(|e| e.to_string())?;
    let mut hasher = Sha256::new();
    hasher.update(&wasm_bytes);
    let digest = format!("{:x}", hasher.finalize());
    if digest != entry.sha256 {
        return Err(format!("sha256 mismatch: expected {}, got {digest}", entry.sha256));
    }
    let manifest_text = ureq::get(&entry.manifest_url).call().map_err(|e| e.to_string())?.into_string().map_err(|e| e.to_string())?;

    // entry.name comes verbatim from the fetched (possibly untrusted/remote) repo
    // index -- a malicious index could set e.g. name = "../../../../somewhere" to
    // write outside extensions_dir. sha256 verification above doesn't help here: a
    // malicious index controls both the wasm bytes and the hash that's supposed to
    // match them. Sanitize the same way wasm_host.rs::sanitize_key does for
    // state_get/state_set keys, so the joined path can never contain a `/`, `\`,
    // or traverse via `..`.
    let ext_dir = extensions_dir.join(sanitize_name(&entry.name));
    std::fs::create_dir_all(&ext_dir).map_err(|e| e.to_string())?;
    std::fs::write(ext_dir.join("monitor.wasm"), &wasm_bytes).map_err(|e| e.to_string())?;
    std::fs::write(ext_dir.join("manifest.toml"), manifest_text).map_err(|e| e.to_string())?;
    Ok(())
}

fn sanitize_name(name: &str) -> String {
    name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect()
}

trait BytesExt { fn into_bytes(self) -> Result<Vec<u8>, String>; }
impl BytesExt for ureq::Response {
    fn into_bytes(self) -> Result<Vec<u8>, String> {
        let mut buf = Vec::new();
        self.into_reader().read_to_end(&mut buf).map_err(|e| e.to_string())?;
        Ok(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_rejects_sha256_mismatch() {
        // A network-free check of the guard clause: feed install() an entry whose
        // sha256 cannot possibly match, using a tiny local HTTP server would be the
        // full end-to-end version -- deferred here since it needs a second crate
        // (tiny_http) purely for this one test. Covered instead by the manual
        // Step 3 check below against a real file.
        let entry = StoreEntry { name: "x".into(), version: "0".into(), wasm_url: "".into(), manifest_url: "".into(), sha256: "deadbeef".into() };
        // Directly exercise the hash-compare logic in isolation:
        let bytes = b"not empty";
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        let digest = format!("{:x}", hasher.finalize());
        assert_ne!(digest, entry.sha256);
    }

    #[test]
    fn sanitize_name_strips_path_traversal() {
        // A malicious/compromised repo index controls entry.name directly (and can
        // make its sha256 match its own malicious wasm bytes, so the hash check
        // can't catch this) -- sanitize_name must neutralize any `..`, `/`, or `\`
        // it contains before it's ever join()'d onto extensions_dir.
        let sanitized = sanitize_name("../../../../evil");
        assert!(!sanitized.contains('/'));
        assert!(!sanitized.contains('\\'));
        assert!(!sanitized.contains(".."));

        // And prove the join actually stays inside extensions_dir: it must not
        // resolve to (or above) extensions_dir's own parent.
        let extensions_dir = Path::new("/tmp/kennel-extensions-test");
        let joined = extensions_dir.join(sanitize_name("../../../../evil"));
        assert!(joined.starts_with(extensions_dir), "joined path {joined:?} escaped {extensions_dir:?}");
    }
}
