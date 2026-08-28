# Documentation

Intendant's published documentation is an [mdBook](https://rust-lang.github.io/mdBook/) rooted in `docs/`, with book sources in `docs/src/` and configuration in `docs/book.toml`.

## Preview locally

Install `mdbook`, then run:

```bash
mdbook serve docs
```

Open the local URL printed by mdBook. The development server rebuilds the book as documentation files change.

To run the same build command used by the GitHub Pages workflow:

```bash
mdbook build docs
```

Changes under `docs/src/`, to `docs/book.toml`, or to the docs workflow trigger the published documentation deployment on `main`.
