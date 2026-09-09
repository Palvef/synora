use db::{migrator::Migrator, Db, Store};
use std::{path::Path, sync::Arc};
use synora_core::JobStatus;

async fn exercise(db: Db, connections: Vec<Db>) {
    Migrator::new(Path::new("/nonexistent-embedded-migrations"))
        .run(&db)
        .await
        .unwrap();
    let store = Arc::new(Store::new(db.clone()));
    for i in 0..100 {
        let job = format!("job-{i}");
        db.execute("INSERT INTO jobs (name,provider,provider_config,storage_path,updated_at) VALUES (?,'rsync','{}','/test',0)", &[job.clone().into()]).await.unwrap();
        store
            .upsert_worker(&format!("worker-{i}"), "test", "", "test", &[], "test")
            .await
            .unwrap();
        for r in 0..10 {
            store
                .create_run(&format!("run-{i}-{r}"), &job, None, JobStatus::Queued, 0)
                .await
                .unwrap();
        }
    }
    let barrier = Arc::new(tokio::sync::Barrier::new(100));
    let mut tasks = Vec::new();
    for worker in 0..100 {
        let store = Store::new(connections[worker % connections.len()].clone());
        let barrier = barrier.clone();
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            let mut won = 0;
            for i in 0..10 {
                let job = (worker / 10) * 10 + i;
                won += usize::from(
                    store
                        .claim_run(
                            &format!("run-{job}-{}", worker % 10),
                            &format!("worker-{worker}"),
                        )
                        .await
                        .unwrap(),
                );
            }
            won
        }));
    }
    let mut won = 0;
    for task in tasks {
        won += task.await.unwrap();
    }
    assert_eq!(
        won, 100,
        "one active assignment for each of 100 jobs across 1000 competing runs"
    );
    for i in 0..100 {
        assert_eq!(
            store
                .active_runs_of_job(&format!("job-{i}"))
                .await
                .unwrap()
                .len(),
            1
        );
    }
    let run = store.active_runs_of_job("job-0").await.unwrap().remove(0);
    let worker = run.worker_id.as_deref().unwrap();
    let token = run.lease_token.as_deref().expect("claim issues token");
    assert!(!token.is_empty());
    assert!(!store
        .renew_run_lease(&run.id, worker, "stale")
        .await
        .unwrap());
    assert!(!store
        .renew_run_lease(&run.id, "wrong-worker", token)
        .await
        .unwrap());
    assert!(store.renew_run_lease(&run.id, worker, token).await.unwrap());
    assert!(
        !store.expire_run(&run.id, 0).await.unwrap(),
        "renewed lease must survive stale reaper selection"
    );
    assert!(!store
        .finish_active_run_fenced(
            &run.id,
            0,
            JobStatus::Failed,
            Some(1),
            None,
            None,
            None,
            None,
            1,
            Some((worker, "stale"))
        )
        .await
        .unwrap());
    assert!(store
        .set_retry_from_active_fenced(&run.id, 0, 0, 1, Some((worker, token)))
        .await
        .unwrap());
    store
        .set_run_status(&run.id, JobStatus::Queued)
        .await
        .unwrap();
    assert!(store.claim_run(&run.id, worker).await.unwrap());
    let next = store.get_run(&run.id).await.unwrap().unwrap();
    assert_ne!(next.lease_token, run.lease_token);
    assert!(!store
        .finish_active_run_fenced(
            &run.id,
            1,
            JobStatus::Failed,
            Some(1),
            None,
            None,
            None,
            None,
            1,
            Some((worker, token))
        )
        .await
        .unwrap());
    assert!(store
        .finish_active_run_fenced(
            &run.id,
            1,
            JobStatus::SuccessWithWarnings,
            Some(0),
            None,
            Some(9_000_000_000),
            None,
            None,
            1,
            Some((worker, next.lease_token.as_deref().unwrap()))
        )
        .await
        .unwrap());
    assert!(!store
        .finish_active_run_fenced(
            &run.id,
            1,
            JobStatus::Failed,
            Some(1),
            None,
            None,
            None,
            None,
            1,
            Some((worker, next.lease_token.as_deref().unwrap()))
        )
        .await
        .unwrap());
    assert_eq!(
        store.get_run(&run.id).await.unwrap().unwrap().status,
        JobStatus::SuccessWithWarnings
    );
    let stats = store.latest_run_stats().await.unwrap();
    let stats = stats.iter().find(|s| s.job_id == run.job_id).unwrap();
    assert_eq!(
        stats.last_finished_status,
        Some(JobStatus::SuccessWithWarnings)
    );
    assert!(stats.last_success.is_some());
    store
        .set_repository_size("/large", 9_000_000_000)
        .await
        .unwrap();
    let rows = db
        .query(
            "SELECT size_bytes FROM repositories WHERE path = '/large'",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(rows[0][0].1.as_i64(), Some(9_000_000_000));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_concurrent_assignments() {
    let dir = std::env::temp_dir().join(format!(
        "synora-sqlite-concurrency-{}",
        synora_core::RunId::new()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("db.sqlite");
    let database = Db::Sqlite(Arc::new(db::SqliteDb::open(&path).unwrap()));
    let connections = (0..20)
        .map(|_| Db::Sqlite(Arc::new(db::SqliteDb::open(&path).unwrap())))
        .collect();
    exercise(database, connections).await;
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a disposable empty SYNORA_TEST_PG_URL database"]
async fn postgres_concurrent_assignments() {
    let url = std::env::var("SYNORA_TEST_PG_URL").expect("set an isolated test database URL");
    let database = Db::Pg(Arc::new(db::PgDb::connect(&url).await.unwrap()));
    let mut connections = Vec::new();
    for _ in 0..20 {
        connections.push(Db::Pg(Arc::new(db::PgDb::connect(&url).await.unwrap())));
    }
    exercise(database, connections).await;
}
