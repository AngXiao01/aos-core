#[test]
#[ignore = "performance probe; run with cargo test -p aos-conformance --release --no-default-features --test perf -- --ignored"]
fn fork_commit_revert() {
    let (fork_ms, commit_ms, revert_ms) = aos_conformance::perf_fork_and_commit();
    eprintln!("fork_ms={fork_ms} commit_ms={commit_ms} revert_ms={revert_ms}");
    assert!(fork_ms < 10, "fork {fork_ms} ms");
    assert!(commit_ms < 2_000, "commit {commit_ms} ms");
    assert!(revert_ms < 2_000, "revert {revert_ms} ms");
}

#[test]
#[ignore = "performance probe; run with cargo test -p aos-conformance --release --no-default-features --test perf -- --ignored"]
fn summary_million() {
    let ms = aos_conformance::perf_summary(1_000_000);
    eprintln!("summary_ms={ms}");
    assert!(ms < 5_000, "summary {ms} ms");
}
