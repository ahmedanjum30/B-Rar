// On windows-gnu, std also links a small default manifest, so ld warns "multiple non-default
// manifests". Ours (linked first) is the one Windows uses; without its Common Controls v6
// dependency TaskDialogIndirect would not resolve and the exe would not start at all.
fn main() {
    println!("cargo:rerun-if-changed=app.rc");
    println!("cargo:rerun-if-changed=app.manifest");
    println!("cargo:rerun-if-changed=../../assets/brar.ico");
    embed_resource::compile("app.rc", embed_resource::NONE).manifest_required().unwrap();
}
