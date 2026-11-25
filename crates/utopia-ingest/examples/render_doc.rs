//! Print a file as text, the way the ingest layer reads it: `cargo run -p utopia-ingest --example render_doc -- <file>`.
//! Seeing how a table or a docx gets read is much faster than waiting for a full extraction round (lesson from #744).
fn main() -> anyhow::Result<()> {
    let path = std::env::args().nth(1).expect("要一个文件路径");
    let bytes = std::fs::read(&path)?;
    let name = std::path::Path::new(&path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file");
    let parsed = utopia_ingest::parse(name, &bytes)?;
    print!("{}", parsed.text);
    Ok(())
}
