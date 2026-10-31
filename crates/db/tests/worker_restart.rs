use db::{migrator::Migrator, Db, SqliteDb, Store};
use std::{path::Path, sync::Arc};

async fn store() -> Store {
    let db = Db::Sqlite(Arc::new(SqliteDb::open(Path::new(":memory:")).unwrap()));
    Migrator::new(Path::new("/nonexistent-embedded-migrations"))
        .run(&db)
        .await
        .unwrap();
    let store = Store::new(db);
    store
        .upsert_worker("worker", "test", "", "test", &[], "token")
        .await
        .unwrap();
    store
}

async fn status(store: &Store) -> String {
    store
        .get_worker("worker")
        .await
        .unwrap()
        .unwrap()
        .iter()
        .find(|(name, _)| name == "status")
        .unwrap()
        .1
        .as_str()
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn restart_recovers_after_interrupted_shutdown() {
    let store = store().await;
    store.begin_worker_shutdown("worker").await.unwrap();
    assert_eq!(status(&store).await, "STOPPING");
    store
        .touch_heartbeat("worker", 1, "running", &[])
        .await
        .unwrap();
    assert_eq!(status(&store).await, "STOPPING");
    // A reboot can prevent unregister from ever arriving.
    store
        .upsert_worker("worker", "test", "", "test", &[], "token")
        .await
        .unwrap();
    assert_eq!(status(&store).await, "ONLINE");
    assert_eq!(store.eligible_workers(30).await.unwrap().len(), 1);
    store.begin_worker_shutdown("worker").await.unwrap();
    store.unregister_worker("worker").await.unwrap();
    assert_eq!(status(&store).await, "OFFLINE");
    store
        .upsert_worker("worker", "test", "", "test", &[], "token")
        .await
        .unwrap();
    assert_eq!(status(&store).await, "ONLINE");
}

#[tokio::test]
async fn manual_retirement_survives_shutdown_and_registration() {
    let store = store().await;
    for state in ["DRAINING", "MAINTENANCE"] {
        store.set_worker_status("worker", state).await.unwrap();
        store.begin_worker_shutdown("worker").await.unwrap();
        assert_eq!(status(&store).await, state);
        store.unregister_worker("worker").await.unwrap();
        store
            .upsert_worker("worker", "test", "", "test", &[], "token")
            .await
            .unwrap();
        assert_eq!(status(&store).await, state);
        assert!(store.eligible_workers(30).await.unwrap().is_empty());
    }
}
