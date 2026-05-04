use sessync::adapter::memory::InMemoryStorage;
use sessync::adapter::storage::StorageAdapter;

#[tokio::test]
async fn put_get_roundtrip() {
    let s = InMemoryStorage::new();
    s.put("k1", b"hello".to_vec()).await.unwrap();
    let got = s.get("k1").await.unwrap();
    assert_eq!(got, b"hello");
}

#[tokio::test]
async fn list_filters_by_prefix() {
    let s = InMemoryStorage::new();
    s.put("a/1", vec![1]).await.unwrap();
    s.put("a/2", vec![2]).await.unwrap();
    s.put("b/1", vec![3]).await.unwrap();
    let listed = s.list("a/").await.unwrap();
    let keys: Vec<_> = listed.into_iter().map(|o| o.key).collect();
    assert_eq!(keys, vec!["a/1".to_string(), "a/2".to_string()]);
}

#[tokio::test]
async fn delete_is_idempotent() {
    let s = InMemoryStorage::new();
    s.delete("nope").await.unwrap();
    s.put("k", vec![1]).await.unwrap();
    s.delete("k").await.unwrap();
    assert!(s.get("k").await.is_err());
}
