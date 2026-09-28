use std::fs;
use std::path::Path;

fn main() {
    let dir = Path::new("ui/dist/consolette/browser");
    if !dir.exists() {
        let _ = fs::create_dir_all(dir);
    }
    let index_file = dir.join("index.html");
    if !index_file.exists() {
        let stub_html = "<!DOCTYPE html>\n<html lang=\"en\">\n<head><meta charset=\"UTF-8\"><title>Consolette Dashboard</title></head>\n<body><h1>Consolette Dashboard (Loading...)</h1></body>\n</html>";
        let _ = fs::write(index_file, stub_html);
    }

    println!("cargo:rerun-if-changed=ui/dist/consolette/browser");
    println!("cargo:rerun-if-changed=ui/src");
}
