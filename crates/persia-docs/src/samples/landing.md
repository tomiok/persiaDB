The landing page's sample (`website/src/content/docs/index.mdx`), run in a temporary directory. The page shows it
with `"app.persia"` in place of `dir.path().join("app.persia")` and without the hidden line that makes `dir`; nothing
else may differ (`shown_samples_equal_a_doctest`, `xtask/src/repo_checks/samples.rs`).

```toml
[dependencies]
persia = { git = "https://github.com/tomiok/persiaDB" } # not on crates.io yet
```

```rust
use persia::{Db, SearchRequest};

fn main() -> persia::Result<()> {
#   // Test setup only: the page's `main` returns `persia::Result`, which an `io::Error` does not convert into.
#   let dir = tempfile::tempdir().expect("a temporary directory");
    let db = Db::open(dir.path().join("app.persia"))?; // one file, created if missing
    let notes = db.create_collection_json(
        r#"{"name": "notes", "fields": {"body": {"type": "text"}}}"#,
    )?;
    notes.put_json(r#"{"id": "n1", "body": "hello, world"}"#)?;
    notes.commit()?;

    // A typo still finds n1, and the response names the step that relaxed it.
    let page = notes.search(&SearchRequest::new("helo"))?;
    assert_eq!(page.hits[0].document.id(), "n1");
    println!("{:?}", page.relaxation.level); // Fuzzy

    db.close()
}
```
