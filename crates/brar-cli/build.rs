fn main() {
    println!("cargo:rerun-if-changed=cli.rc");
    println!("cargo:rerun-if-changed=../../assets/brar.ico");
    embed_resource::compile("cli.rc", embed_resource::NONE).manifest_optional().unwrap();
}
