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
