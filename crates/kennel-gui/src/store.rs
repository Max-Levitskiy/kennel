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
    // sha256 of monitor.wasm (the code)...
    pub sha256: String,
    // ...and of manifest.toml (the capability grant). Both are required: the
    // daemon gates capabilities entirely off the on-disk manifest.toml, never
    // off the wasm, so verifying only the wasm leaves the actually
    // security-relevant half of an install unverified -- a compromised index or
    // a MITM'd manifest_url could pin the legitimate, hash-matching wasm while
    // serving a manifest that grants privileged_spawn with commands of its
    // choosing.
    pub manifest_sha256: String,
}

// What kennel-daemon's scan_and_register parses out of manifest.toml. Only
// `name` is checked here (the daemon re-parses the file properly at load time);
// this is about identity, see install_verified.
#[derive(Debug, Deserialize)]
struct ManifestIdentity {
    name: String,
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
    let manifest_bytes = ureq::get(&entry.manifest_url).call().map_err(|e| e.to_string())?.into_bytes().map_err(|e| e.to_string())?;
    install_verified(entry, &wasm_bytes, &manifest_bytes, extensions_dir)
}

// Everything about an install except the two downloads, split out so the
// verification below is testable without a network or an HTTP server.
fn install_verified(entry: &StoreEntry, wasm_bytes: &[u8], manifest_bytes: &[u8], extensions_dir: &Path) -> Result<(), String> {
    let digest = sha256_hex(wasm_bytes);
    if digest != entry.sha256 {
        return Err(format!("monitor.wasm sha256 mismatch: expected {}, got {digest}", entry.sha256));
    }
    let manifest_digest = sha256_hex(manifest_bytes);
    if manifest_digest != entry.manifest_sha256 {
        return Err(format!("manifest.toml sha256 mismatch: expected {}, got {manifest_digest}", entry.manifest_sha256));
    }
    let manifest_text = String::from_utf8(manifest_bytes.to_vec()).map_err(|e| format!("manifest.toml is not valid UTF-8: {e}"))?;

    // The index says what this extension is called (and that name decides which
    // directory it lands in), but the manifest.toml is what the daemon actually
    // registers it under. If those two disagree, the install is either corrupt
    // or an attempt to have an extension land in one identity's directory while
    // presenting itself to the daemon as another -- refuse rather than pick one.
    let identity: ManifestIdentity = toml::from_str(&manifest_text).map_err(|e| format!("manifest.toml is not parseable: {e}"))?;
    if identity.name != entry.name {
        return Err(format!("manifest.toml declares name {:?} but the index entry is {:?}", identity.name, entry.name));
    }

    // entry.name comes verbatim from the fetched (possibly untrusted/remote) repo
    // index -- a malicious index could set e.g. name = "../../../../somewhere" to
    // write outside extensions_dir. sha256 verification above doesn't help here: a
    // malicious index controls both the wasm bytes and the hash that's supposed to
    // match them. Sanitize the same way wasm_host.rs::sanitize_key does for
    // state_get/state_set keys, so the joined path can never contain a `/`, `\`,
    // or traverse via `..`.
    let ext_dir = extensions_dir.join(sanitize_name(&entry.name));
    std::fs::create_dir_all(&ext_dir).map_err(|e| e.to_string())?;
    std::fs::write(ext_dir.join("monitor.wasm"), wasm_bytes).map_err(|e| e.to_string())?;
    std::fs::write(ext_dir.join("manifest.toml"), manifest_text).map_err(|e| e.to_string())?;
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
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

    const WASM: &[u8] = b"\0asm not-really-but-bytes-are-bytes";
    const MANIFEST: &str = "name = \"x\"\nversion = \"0.1.0\"\ndescription = \"t\"\ninterval_secs = 5\ncapabilities = [\"log\"]\n";

    // An entry that matches WASM/MANIFEST -- the happy path each test below
    // then breaks in exactly one way.
    fn good_entry() -> StoreEntry {
        StoreEntry {
            name: "x".into(),
            version: "0.1.0".into(),
            wasm_url: "https://example.invalid/monitor.wasm".into(),
            manifest_url: "https://example.invalid/manifest.toml".into(),
            sha256: sha256_hex(WASM),
            manifest_sha256: sha256_hex(MANIFEST.as_bytes()),
        }
    }

    #[test]
    fn install_writes_both_files_when_everything_verifies() {
        let dir = tempfile::tempdir().unwrap();
        install_verified(&good_entry(), WASM, MANIFEST.as_bytes(), dir.path()).unwrap();
        assert_eq!(std::fs::read(dir.path().join("x/monitor.wasm")).unwrap(), WASM);
        assert_eq!(std::fs::read_to_string(dir.path().join("x/manifest.toml")).unwrap(), MANIFEST);
    }

    #[test]
    fn install_rejects_sha256_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let err = install_verified(&good_entry(), b"tampered wasm", MANIFEST.as_bytes(), dir.path()).unwrap_err();
        assert!(err.contains("monitor.wasm sha256 mismatch"), "unexpected error: {err}");
        assert!(!dir.path().join("x").exists(), "nothing may be written when verification fails");
    }

    // I1: manifest.toml *is* the capability grant -- the daemon gates
    // spawn/privileged_spawn/write_file entirely off this file and never off the
    // wasm -- so an install that verifies the code but not the permissions
    // verifies the wrong half. A compromised index (or a MITM on manifest_url)
    // could otherwise pin the real, hash-matching wasm and serve a manifest
    // granting privileged_spawn with arbitrary privileged_commands.
    #[test]
    fn install_rejects_a_manifest_that_does_not_match_its_hash() {
        let dir = tempfile::tempdir().unwrap();
        let tampered = "name = \"x\"\nversion = \"0.1.0\"\ndescription = \"t\"\ninterval_secs = 5\ncapabilities = [\"privileged_spawn\"]\nprivileged_commands = [\"/bin/rm\"]\n";
        let err = install_verified(&good_entry(), WASM, tampered.as_bytes(), dir.path()).unwrap_err();
        assert!(err.contains("manifest.toml sha256 mismatch"), "unexpected error: {err}");
        assert!(!dir.path().join("x").exists(), "a manifest that fails verification must never reach disk");
    }

    #[test]
    fn install_rejects_a_manifest_whose_name_disagrees_with_the_index() {
        let dir = tempfile::tempdir().unwrap();
        let other = "name = \"something-else\"\nversion = \"0.1.0\"\ndescription = \"t\"\ninterval_secs = 5\ncapabilities = []\n";
        let mut entry = good_entry();
        entry.manifest_sha256 = sha256_hex(other.as_bytes()); // hash is honest; the identity isn't
        let err = install_verified(&entry, WASM, other.as_bytes(), dir.path()).unwrap_err();
        assert!(err.contains("declares name"), "unexpected error: {err}");
        assert!(!dir.path().join("x").exists());
    }

    #[test]
    fn install_rejects_an_unparseable_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let junk = "this is not toml {{{";
        let mut entry = good_entry();
        entry.manifest_sha256 = sha256_hex(junk.as_bytes());
        let err = install_verified(&entry, WASM, junk.as_bytes(), dir.path()).unwrap_err();
        assert!(err.contains("not parseable"), "unexpected error: {err}");
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
