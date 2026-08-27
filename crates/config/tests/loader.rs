//! Config loader integration tests: include semantics, env expansion,
//! layering, validation with file:line.

use config::{CliOverrides, ConfigLoader, DbKind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static DIR_SEQ: AtomicU64 = AtomicU64::new(0);

/// Fresh unique temp dir per test (no tempfile dep needed).
fn temp_dir(tag: &str) -> PathBuf {
    let n = DIR_SEQ.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "synora-config-test-{}-{tag}-{n}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write(dir: &Path, rel: &str, content: &str) {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, content).unwrap();
}

fn load(dir: &Path) -> Result<config::ResolvedConfig, config::ConfigError> {
    ConfigLoader::load(&dir.join("synora.toml"), &CliOverrides::default())
}

const MAIN: &str = r#"
include = ["jobs/*.toml"]

[daemon]
max_concurrency = 5

[daemon.db]
kind = "sqlite"
path = "data/synora.db"
"#;

fn job(name: &str, schedule: &str, extra: &str) -> String {
    format!("[[jobs]]\nname = \"{name}\"\n{schedule}\n{extra}\n")
}

#[test]
fn valid_config_all_schedule_kinds() {
    let dir = temp_dir("valid");
    write(&dir, "synora.toml", MAIN);
    write(
        &dir,
        "jobs/ubuntu.toml",
        &job(
            "ubuntu",
            "schedule = \"interval\"\nevery = \"6h\"",
            "provider = \"rsync\"\nupstream = \"rsync://archive.ubuntu.com/ubuntu/\"\nstorage = \"/srv/mirror/ubuntu\"\ntimezone = \"Asia/Shanghai\"\nsuccess_exit_codes = [23, 24]",
        ),
    );
    write(
        &dir,
        "jobs/daily.toml",
        &job(
            "daily",
            "schedule = \"daily\"\nat = \"03:30:00\"",
            "provider = \"script\"\ncommand = \"/opt/scripts/daily.sh\"\nstorage = \"/srv/daily\"",
        ),
    );
    write(
        &dir,
        "jobs/weekly.toml",
        &job(
            "weekly",
            "schedule = \"weekly\"\nweekday = \"Sunday\"\nat = \"04:00\"",
            "provider = \"script\"\ncommand = \"true\"\nstorage = \"/srv/weekly\"",
        ),
    );
    write(
        &dir,
        "jobs/cron.toml",
        &job(
            "cronjob",
            "schedule = \"cron\"\ncron = \"0 */4 * * *\"",
            "provider = \"docker\"\nimage = \"busybox\"\nvolumes = [\"/srv/c:/data\"]\nstorage = \"/srv/c\"",
        ),
    );
    write(
        &dir,
        "jobs/manual.toml",
        &job(
            "manual",
            "schedule = \"manual\"",
            "provider = \"script\"\ncommand = \"true\"\nstorage = \"/srv/manual\"\nretry = 0",
        ),
    );
    let cfg = load(&dir).unwrap();
    assert_eq!(cfg.jobs.len(), 5);
    let ubuntu = cfg.jobs.iter().find(|j| j.name == "ubuntu").unwrap();
    assert_eq!(ubuntu.timezone, "Asia/Shanghai");
    assert_eq!(ubuntu.success_exit_codes, vec![23, 24]);
    assert_eq!(ubuntu.retry, 3);
    // docker job
    let c = cfg.jobs.iter().find(|j| j.name == "cronjob").unwrap();
    assert!(matches!(
        c.provider,
        synora_core::ProviderConfig::Docker { .. }
    ));
}

#[test]
fn bare_job_table_file() {
    let dir = temp_dir("bare");
    write(&dir, "synora.toml", "include = [\"jobs/single.toml\"]\n");
    write(
        &dir,
        "jobs/single.toml",
        "name = \"single\"\nschedule = \"startup\"\nprovider = \"script\"\ncommand = \"true\"\nstorage = \"/srv/single\"\n",
    );
    let cfg = load(&dir).unwrap();
    assert_eq!(cfg.jobs.len(), 1);
    assert_eq!(cfg.jobs[0].name, "single");
}

#[test]
fn include_cycle_detected() {
    let dir = temp_dir("cycle");
    write(&dir, "synora.toml", "include = [\"a.toml\"]\n");
    write(&dir, "a.toml", "include = [\"b.toml\"]\n");
    write(&dir, "b.toml", "include = [\"a.toml\"]\n");
    let err = load(&dir).unwrap_err();
    assert!(err.to_string().contains("cycle"), "{err}");
}

#[test]
fn include_missing_file_and_empty_glob() {
    let dir = temp_dir("missing");
    write(&dir, "synora.toml", "include = [\"nope.toml\"]\n");
    assert!(load(&dir).unwrap_err().to_string().contains("not found"));

    let dir2 = temp_dir("emptyglob");
    write(&dir2, "synora.toml", "include = [\"jobs/*.toml\"]\n");
    assert!(load(&dir2)
        .unwrap_err()
        .to_string()
        .contains("matched no files"));
}

#[test]
fn env_expansion_and_escape() {
    let dir = temp_dir("env");
    std::env::set_var("SYNORA_TEST_DB_PATH", "data/custom.db");
    write(
        &dir,
        "synora.toml",
        "[daemon.db]\nkind = \"sqlite\"\npath = \"${SYNORA_TEST_DB_PATH}\"\n",
    );
    let cfg = load(&dir).unwrap();
    assert_eq!(cfg.daemon.db.path, "data/custom.db");

    let dir2 = temp_dir("env-unset");
    write(
        &dir2,
        "synora.toml",
        "include = [\"x.toml\"]\n# padding\n\n",
    );
    write(
        &dir2,
        "x.toml",
        "include = []\n# pad\n# pad\n[daemon]\nlog_dir = \"${SYNORA_UNSET_VAR_XYZ}\"\n",
    );
    let err = load(&dir2).unwrap_err();
    assert!(err.to_string().contains("SYNORA_UNSET_VAR_XYZ"), "{err}");
    assert!(err.line > 0, "expected file:line in {err}");

    let dir3 = temp_dir("env-escape");
    write(&dir3, "synora.toml", "include = [\"$${literal}.toml\"]\n");
    write(&dir3, "${literal}.toml", "");
    assert!(load(&dir3).is_ok());
}

#[test]
fn layering_included_wins_over_main() {
    let dir = temp_dir("layering");
    write(
        &dir,
        "synora.toml",
        "include = [\"extra.toml\"]\n[daemon]\nmax_concurrency = 5\nlog_dir = \"/main/log\"\n",
    );
    write(&dir, "extra.toml", "[daemon]\nmax_concurrency = 8\n");
    let cfg = load(&dir).unwrap();
    assert_eq!(cfg.daemon.max_concurrency, 8); // included overrides
    assert_eq!(cfg.daemon.log_dir, PathBuf::from("/main/log")); // untouched field survives
}

#[test]
fn duplicate_job_name_rejected() {
    let dir = temp_dir("dup");
    write(
        &dir,
        "synora.toml",
        "include = [\"jobs/a.toml\", \"jobs/b.toml\"]\n",
    );
    let j = |s: &str| {
        job(
            "dup",
            "schedule = \"manual\"",
            &format!("provider = \"script\"\ncommand = \"true\"\nstorage = \"{s}\""),
        )
    };
    write(&dir, "jobs/a.toml", &j("/srv/a"));
    write(&dir, "jobs/b.toml", &j("/srv/b"));
    let err = load(&dir).unwrap_err();
    assert!(err.to_string().contains("duplicate job"), "{err}");
    assert!(err.file.ends_with("b.toml"), "points at second file: {err}");
}

#[test]
fn validation_errors_carry_file_line() {
    let dir = temp_dir("badcron");
    write(&dir, "synora.toml", "include = [\"jobs/*.toml\"]\n");
    write(
        &dir,
        "jobs/ubuntu.toml",
        "# one\n# two\n[[jobs]]\nname = \"ubuntu\"\nschedule = \"cron\"\ncron = \"not a cron\"\nprovider = \"rsync\"\nupstream = \"x\"\nstorage = \"/srv/u\"\n",
    );
    let err = load(&dir).unwrap_err();
    assert!(err.to_string().contains("cron"), "{err}");
    assert!(err.file.ends_with("ubuntu.toml") && err.line >= 3, "{err}");
}

#[test]
fn bad_values_rejected() {
    let dir = temp_dir("badvalues");
    write(&dir, "synora.toml", "include = [\"jobs/*.toml\"]\n");

    write(
        &dir,
        "jobs/badtime.toml",
        &job(
            "badtime",
            "schedule = \"daily\"\nat = \"25:99\"",
            "provider = \"script\"\ncommand = \"true\"\nstorage = \"/srv/x\"",
        ),
    );
    assert!(load(&dir).unwrap_err().to_string().contains("time"));

    std::fs::remove_file(dir.join("jobs/badtime.toml")).unwrap();
    write(
        &dir,
        "jobs/badweekday.toml",
        &job(
            "badweekday",
            "schedule = \"weekly\"\nweekday = \"Funday\"\nat = \"03:00\"",
            "provider = \"script\"\ncommand = \"true\"\nstorage = \"/srv/x\"",
        ),
    );
    assert!(load(&dir).unwrap_err().to_string().contains("weekday"));

    std::fs::remove_file(dir.join("jobs/badweekday.toml")).unwrap();
    write(
        &dir,
        "jobs/badevery.toml",
        &job(
            "badevery",
            "schedule = \"interval\"\nevery = \"6x\"",
            "provider = \"script\"\ncommand = \"true\"\nstorage = \"/srv/x\"",
        ),
    );
    assert!(load(&dir).unwrap_err().to_string().contains("duration"));

    std::fs::remove_file(dir.join("jobs/badevery.toml")).unwrap();
    write(
        &dir,
        "jobs/dotdot.toml",
        &job(
            "dotdot",
            "schedule = \"manual\"",
            "provider = \"script\"\ncommand = \"true\"\nstorage = \"/srv/../etc\"",
        ),
    );
    assert!(load(&dir).unwrap_err().to_string().contains(".."));

    std::fs::remove_file(dir.join("jobs/dotdot.toml")).unwrap();
    write(
        &dir,
        "jobs/unknown.toml",
        &job(
            "unknown",
            "schedule = \"manual\"",
            "provider = \"script\"\ncommand = \"true\"\nstorage = \"/srv/x\"\nbogus_field = 1",
        ),
    );
    assert!(load(&dir).unwrap_err().to_string().contains("bogus_field"));

    std::fs::remove_file(dir.join("jobs/unknown.toml")).unwrap();
    write(
        &dir,
        "jobs/nocmd.toml",
        &job(
            "nocmd",
            "schedule = \"manual\"",
            "provider = \"script\"\nstorage = \"/srv/x\"",
        ),
    );
    assert!(load(&dir).unwrap_err().to_string().contains("command"));

    std::fs::remove_file(dir.join("jobs/nocmd.toml")).unwrap();
    write(
        &dir,
        "jobs/noupstream.toml",
        &job(
            "noupstream",
            "schedule = \"manual\"",
            "provider = \"rsync\"\nstorage = \"/srv/x\"",
        ),
    );
    assert!(load(&dir).unwrap_err().to_string().contains("upstream"));

    std::fs::remove_file(dir.join("jobs/noupstream.toml")).unwrap();
    write(
        &dir,
        "jobs/badregex.toml",
        &job(
            "badregex",
            "schedule = \"manual\"",
            "provider = \"script\"\ncommand = \"true\"\nstorage = \"/srv/x\"\nfail_on_match = \"([\"",
        ),
    );
    assert!(load(&dir).unwrap_err().to_string().contains("regex"));

    std::fs::remove_file(dir.join("jobs/badregex.toml")).unwrap();
    write(
        &dir,
        "jobs/badtz.toml",
        &job(
            "badtz",
            "schedule = \"manual\"",
            "provider = \"script\"\ncommand = \"true\"\nstorage = \"/srv/x\"\ntimezone = \"Mars/Olympus\"",
        ),
    );
    assert!(load(&dir).unwrap_err().to_string().contains("timezone"));

    std::fs::remove_file(dir.join("jobs/badtz.toml")).unwrap();
    write(
        &dir,
        "jobs/misfire.toml",
        &job(
            "misfire",
            "schedule = \"interval\"\nevery = \"6h\"\ncron = \"0 0 * * *\"",
            "provider = \"script\"\ncommand = \"true\"\nstorage = \"/srv/x\"",
        ),
    );
    assert!(load(&dir).unwrap_err().to_string().contains("cron"));
}

#[test]
fn http_threads_none_zero_and_some() {
    // threads = missing/0/5: missing -> None (provider defaults to 5),
    // 0 and 5 flow through verbatim.
    let dir = temp_dir("httpthreads");
    write(&dir, "synora.toml", "include = [\"jobs/*.toml\"]\n");
    write(
        &dir,
        "jobs/none.toml",
        &job(
            "none",
            "schedule = \"manual\"",
            "provider = \"http\"\nparser = \"nginx\"\nupstream = \"https://x/pub/\"\nstorage = \"/srv/none\"",
        ),
    );
    write(
        &dir,
        "jobs/zero.toml",
        &job(
            "zero",
            "schedule = \"manual\"",
            "provider = \"http\"\nparser = \"nginx\"\nthreads = 0\nupstream = \"https://x/pub/\"\nstorage = \"/srv/zero\"",
        ),
    );
    write(
        &dir,
        "jobs/five.toml",
        &job(
            "five",
            "schedule = \"manual\"",
            "provider = \"http\"\nparser = \"nginx\"\nthreads = 5\nexclude = [\"/gone/\"]\nupstream = \"https://x/pub/\"\nstorage = \"/srv/five\"",
        ),
    );
    let cfg = load(&dir).unwrap();
    let threads = |name: &str| {
        cfg.jobs
            .iter()
            .find(|j| j.name == name)
            .unwrap()
            .provider
            .clone()
    };
    assert!(matches!(
        threads("none"),
        synora_core::ProviderConfig::Http { threads: None, .. }
    ));
    assert!(matches!(
        threads("zero"),
        synora_core::ProviderConfig::Http {
            threads: Some(0),
            ..
        }
    ));
    assert!(matches!(
        threads("five"),
        synora_core::ProviderConfig::Http {
            threads: Some(5),
            exclude,
            ..
        } if exclude == vec!["/gone/"]
    ));
}

#[test]
fn timeout_accepts_seconds_and_human() {
    let dir = temp_dir("timeout");
    write(
        &dir,
        "synora.toml",
        "include = [\"jobs/a.toml\", \"jobs/b.toml\"]\n",
    );
    write(
        &dir,
        "jobs/a.toml",
        &job(
            "secs",
            "schedule = \"manual\"",
            "provider = \"script\"\ncommand = \"true\"\nstorage = \"/srv/a\"\ntimeout = 7200",
        ),
    );
    write(
        &dir,
        "jobs/b.toml",
        &job(
            "human",
            "schedule = \"manual\"",
            "provider = \"script\"\ncommand = \"true\"\nstorage = \"/srv/b\"\ntimeout = \"2h\"",
        ),
    );
    let cfg = load(&dir).unwrap();
    assert_eq!(cfg.jobs[0].timeout.whole_seconds(), 7200);
    assert_eq!(cfg.jobs[1].timeout.whole_seconds(), 7200);
}

#[test]
fn postgres_requires_url() {
    let dir = temp_dir("pg");
    write(&dir, "synora.toml", "[daemon.db]\nkind = \"postgres\"\n");
    let err = load(&dir).unwrap_err();
    assert!(err.to_string().contains("url"), "{err}");
    write(
        &dir,
        "synora.toml",
        "[daemon.db]\nkind = \"postgres\"\nurl = \"postgres://u@h/db\"\n",
    );
    let cfg = load(&dir).unwrap();
    assert_eq!(cfg.daemon.db.kind, DbKind::Postgres);
}

#[test]
fn tls_requires_both_cert_and_key() {
    let dir = temp_dir("tls");
    write(
        &dir,
        "synora.toml",
        "[api.tls]\ncert = \"/etc/x/cert.pem\"\n",
    );
    assert!(load(&dir).unwrap_err().to_string().contains("both"));
}

#[test]
fn socks5h_defaults_http_connect_expose() {
    let dir = temp_dir("warp-expose");
    write(
        &dir,
        "synora.toml",
        r#"
[proxy.cf-warp]
type = "socks5h"
url = "socks5h://127.0.0.1:40000"
"#,
    );
    let cfg = load(&dir).unwrap();
    let proxy = cfg.proxies.get("cf-warp").expect("cf-warp");
    assert_eq!(proxy.expose.as_deref(), Some("0.0.0.0:14000"));
}

#[test]
fn default_worker_binds_jobs_without_worker() {
    let dir = temp_dir("default-worker");
    write(
        &dir,
        "synora.toml",
        r#"
[daemon]
default_worker = "mirror-zfs"

[[jobs]]
name = "ubuntu"
schedule = "interval"
every = "6h"
provider = "rsync"
upstream = "rsync://example/ubuntu/"
storage = "/srv/ubuntu"
"#,
    );
    let cfg = load(&dir).unwrap();
    assert_eq!(cfg.daemon.default_worker.as_deref(), Some("mirror-zfs"));
    assert_eq!(cfg.jobs[0].worker.as_deref(), Some("mirror-zfs"));
}

#[test]
fn storage_kind_alias_parses_zfs() {
    let dir = temp_dir("storage-kind");
    write(
        &dir,
        "synora.toml",
        r#"
[storage.mirror]
kind = "zfs"
pool = "datas"
dataset = "mirror"
mountpoint = "/datas"
auto_create = false
zfs_options = "-o recordsize=1M -o atime=off"

[storage.nvme]
type = "zfs"
pool = "data"
mountpoint = "/data"
"#,
    );
    let cfg = load(&dir).unwrap();
    match &cfg.storages.get("mirror").expect("mirror").kind {
        config::StorageKind::Zfs {
            pool,
            dataset,
            options,
        } => {
            assert_eq!(pool, "datas");
            assert_eq!(dataset, "mirror");
            assert!(options.iter().any(|(k, v)| k == "recordsize" && v == "1M"));
            assert!(options.iter().any(|(k, v)| k == "atime" && v == "off"));
        }
        other => panic!("expected zfs, got {other:?}"),
    }
    match &cfg.storages.get("nvme").expect("nvme").kind {
        config::StorageKind::Zfs { pool, dataset, .. } => {
            assert_eq!(pool, "data");
            assert_eq!(dataset, "");
        }
        other => panic!("expected zfs, got {other:?}"),
    }
}
