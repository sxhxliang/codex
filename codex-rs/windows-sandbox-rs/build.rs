fn main() {
    // Only embed a manifest when explicitly requested to avoid duplicate MANIFEST resources.
    if std::env::var("CODEX_WINDOWS_SANDBOX_EMBED_MANIFEST").is_err() {
        return;
    }

    let mut res = winres::WindowsResource::new();
    res.set_manifest_file("codex-windows-sandbox-setup.manifest");
    let _ = res.compile();
}
