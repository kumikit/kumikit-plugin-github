# kumikit-plugin-github

First-party GitHub REST API tool plugin (`kumikit.github`) for
[Kumikit](https://github.com/kumikit/kumikit).

## Build and test

```sh
cargo test -p kumikit-github
cargo build --target wasm32-wasip2 --release -p kumikit-github
KUMIKIT_DIR=../kumikit cargo test -p kumikit-github-e2e
```
