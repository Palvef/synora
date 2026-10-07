"""Asserted patches against the exact pinned Yukina source revision."""
from pathlib import Path


def replace(path, old, new):
    text = path.read_text()
    assert text.count(old) == 1, (str(path), old)
    path.write_text(text.replace(old, new))


stages = Path('src/stages.rs')
replace(stages, '*download_error_cnt > args.download_error_threshold',
        '*download_error_cnt >= args.download_error_threshold')
replace(stages, 'let is_404 = e.status().is_some_and(|s| s == 404);', '''let is_404 = e.status().is_some_and(|s| s == 404 || s == 410);
                        assert!(is_404, "upstream HEAD failed before eviction: {}", e);
                        println!("SYNORA_MISSING={}", url_path);''')
replace(stages, 'reqwest_err.status() == Some(reqwest::StatusCode::NOT_FOUND)',
        'reqwest_err.status().is_some_and(|s| s == 404 || s == 410)')
replace(stages, 'if is_not_found(e) {', '''if is_not_found(e) {
                        println!("SYNORA_MISSING={}", remote_item.path);''')
main = Path('src/main.rs')
replace(main, '''        path: &str,
        url: &str,''', '''        path: &str,
        expected_size: u64,
        url: &str,''')
replace(main, '|| download(args, path, url.as_str(), client, extension)',
        '|| download(args, path, item.stats.size, url.as_str(), client, extension)')
replace(main, '        std::fs::rename(&tmp_path, &target_path)?;', '''        if std::fs::metadata(&tmp_path)?.len() != expected_size {
            std::fs::remove_file(&tmp_path)?;
            anyhow::bail!("download size differs from HEAD: {}", path);
        }
        std::fs::rename(&tmp_path, &target_path)?;''')

# Pipe log records directly to stderr; terminal redraw handling is unnecessary in Docker.
replace(main, '    let bar_writer = BAR_MANAGER.get().unwrap().create_writer();\n', '')
replace(main, '    sync::{Mutex, OnceLock},', '    sync::OnceLock,')
replace(main, '.with_writer(Mutex::new(bar_writer))', '.with_writer(std::io::stderr)')
replace(main, '    let vote = match stage1(&args).await {', '''    tracing::info!("Cache stage 1: reading recent access logs");
    let vote = match stage1(&args).await {''')
replace(main, '    let stats = stage2(&args, local_sizedb.as_ref());', '''    tracing::info!("Cache stage 2: examining locally cached packages");
    let stats = stage2(&args, local_sizedb.as_ref());''')
replace(main, '    let normalized_vote = stage3(&args, &vote, &stats, &client, remote_sizedb.as_ref()).await;', '''    tracing::info!("Cache stage 3: checking {} package candidates", vote.len());
    let normalized_vote = stage3(&args, &vote, &stats, &client, remote_sizedb.as_ref()).await;''')
replace(main, '    let result = stage4(', '''    tracing::info!("Cache stage 4: applying cache budget and downloading packages");
    let result = stage4(''')
replace(stages, '    let mut stop_iterate_flag = false;', '''    let mut processed_lines = 0usize;
    let mut last_progress = std::time::Instant::now();
    let mut stop_iterate_flag = false;''')
replace(stages, '        for (lno, line) in bufreader.lines().enumerate() {', '''        for (lno, line) in bufreader.lines().enumerate() {
            processed_lines += 1;
            if last_progress.elapsed().as_secs() >= 15 {
                tracing::info!("Cache stage 1: {} log lines processed, {} package candidates", processed_lines, vote.len());
                last_progress = std::time::Instant::now();
            }''')
replace(stages, '    for entry in walkdir::WalkDir::new(&args.repo_path) {', '''    let mut visited = 0usize;
    let mut last_progress = std::time::Instant::now();
    for entry in walkdir::WalkDir::new(&args.repo_path) {
        visited += 1;
        if last_progress.elapsed().as_secs() >= 15 {
            tracing::info!("Cache stage 2: {} filesystem entries inspected, {} package files", visited, res.len());
            last_progress = std::time::Instant::now();
        }''')
