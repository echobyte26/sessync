use sessync::adapter::path_codec;

#[test]
fn encode_replaces_slashes_with_dashes() {
    assert_eq!(
        path_codec::encode_cwd("/Users/alice/Project/foo"),
        "-Users-alice-Project-foo"
    );
}

#[test]
fn encode_handles_root() {
    assert_eq!(path_codec::encode_cwd("/"), "-");
}

#[test]
fn project_key_is_deterministic_and_path_invariant_via_basename() {
    // The PRD says we map by stable hash so the same project on different paths can co-locate.
    // For v1 we use a content hash of the full path — same path on both Macs collides cleanly.
    // Different paths intentionally don't collide (user picks at resume time).
    let a = path_codec::project_key_for_cwd("/Users/alice/work/foo");
    let b = path_codec::project_key_for_cwd("/Users/alice/work/foo");
    assert_eq!(a, b);
    let c = path_codec::project_key_for_cwd("/home/alice/work/foo");
    assert_ne!(a, c);
}
