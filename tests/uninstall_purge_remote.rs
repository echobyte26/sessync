use sessync::adapter::memory::InMemoryStorage;
use sessync::adapter::storage::StorageAdapter;
use sessync::commands::uninstall::purge_remote_objects;

#[tokio::test]
async fn purge_deletes_all_listed_objects() {
    let storage = InMemoryStorage::new();
    storage.put("a/1.age", vec![1]).await.unwrap();
    storage.put("a/1.age.meta.json", vec![2]).await.unwrap();
    storage.put("b/2.age", vec![3]).await.unwrap();

    let storage_dyn: &dyn StorageAdapter = &storage;
    let n = purge_remote_objects(storage_dyn).await.unwrap();
    assert_eq!(n, 3);
    assert!(storage.list("").await.unwrap().is_empty());
}

#[tokio::test]
async fn purge_empty_storage_returns_zero() {
    let storage = InMemoryStorage::new();
    let storage_dyn: &dyn StorageAdapter = &storage;
    let n = purge_remote_objects(storage_dyn).await.unwrap();
    assert_eq!(n, 0);
    assert!(storage.list("").await.unwrap().is_empty());
}

#[tokio::test]
async fn purge_leaves_storage_empty() {
    let storage = InMemoryStorage::new();
    for i in 0..10u8 {
        storage
            .put(&format!("prefix/{i}.age"), vec![i])
            .await
            .unwrap();
    }
    let storage_dyn: &dyn StorageAdapter = &storage;
    let n = purge_remote_objects(storage_dyn).await.unwrap();
    assert_eq!(n, 10);
    assert!(storage.list("").await.unwrap().is_empty());
}
