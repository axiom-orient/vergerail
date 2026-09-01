//! Install or reuse Vergerail's audited Codex runtime and print its package root.

use vergerail::RuntimeResolver;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    match RuntimeResolver::new().resolve().await {
        Ok(resolved) => println!("{}", resolved.package().root().display()),
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
}
