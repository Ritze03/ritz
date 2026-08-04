// `include_dir!` bakes `resources/` into the binary, but cargo can't see that
// dependency — without this, editing a bundled module JSON silently rebuilds
// nothing and you keep running the old embed.
fn main() {
    println!("cargo:rerun-if-changed=../../resources");
}
