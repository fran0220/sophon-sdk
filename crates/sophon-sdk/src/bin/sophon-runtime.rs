//! Private NDJSON runtime. This is not a public server or the Grok CLI.
#[tokio::main]
async fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--generate-types") {
        let path = args
            .get(1)
            .expect("--generate-types requires output directory");
        if let Err(error) = sophon_sdk::protocol::export_types(std::path::Path::new(path)) {
            eprintln!("type generation failed: {error}");
            std::process::exit(1);
        }
        return;
    }
    if !args.is_empty() {
        eprintln!("usage: sophon-runtime [--generate-types DIRECTORY]");
        std::process::exit(2);
    }
    if let Err(error) = sophon_sdk::stdio::run().await {
        eprintln!("Sophon Runtime stopped: {error}");
        std::process::exit(1);
    }
}
