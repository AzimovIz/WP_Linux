//! Manual smoke test against a running store server:
//! `cargo run -p store-client --example list_wallpapers`

fn main() {
    let client = store_client::Client::default();
    println!("API: {}", store_client::DEFAULT_API_URL);

    let list = client
        .list_wallpapers(&store_client::ListParams::default())
        .expect("list_wallpapers failed");
    println!("total={} items={}", list.total, list.items.len());

    for w in &list.items {
        println!(
            "- {} {:?} rating={:?} status={:?} tags={:?} manifest_parsed={}",
            w.id,
            w.title,
            w.rating,
            w.status,
            w.tags,
            w.manifest().is_some()
        );
    }

    if let Some(first) = list.items.first() {
        let archive = client
            .download_archive(&first.id)
            .expect("download_archive failed");
        println!(
            "downloaded archive: {} bytes (server reports {})",
            archive.len(),
            first.archive_size_bytes
        );

        let preview = client
            .download_preview(&first.id)
            .expect("download_preview failed");
        println!("downloaded preview: {} bytes", preview.len());

        let dest = std::env::temp_dir().join("store-client-example-extract");
        store_client::extract_archive(&archive, &dest).expect("extract_archive failed");
        println!("extracted into {}", dest.display());

        let (project, _) = project_format::Project::load(&dest)
            .expect("extracted project.json doesn't load as project_format::Project");
        println!(
            "loaded via project_format: name={:?} layers={} fps={}",
            project.name,
            project.layers.len(),
            project.fps
        );
    }
}
