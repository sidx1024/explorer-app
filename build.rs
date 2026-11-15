fn main() {
    println!("cargo:rerun-if-changed=app.rc");
    println!("cargo:rerun-if-changed=assets/explorer-app.ico");
    // The exe's version resource follows Cargo.toml, so a release only bumps one place.
    let v = env!("CARGO_PKG_VERSION");
    let parts: Vec<&str> = v.split(['.', '-']).take(3).collect();
    let num = format!("{},{},{},0", parts[0], parts[1], parts[2]);
    embed_resource::compile("app.rc", [format!("VERSION_NUM={num}"), format!("VERSION_STR=\"{v}\"")])
        .manifest_optional()
        .unwrap();
}
