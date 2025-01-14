# ochat

A _really_ simple personal notebook interface to Ollama.
Supports forking conversations from any point with persistent conversation history.

![a simple but featureful conversation client for ollama](c1.gif)

## running

1. [install Rust](https://www.rust-lang.org/tools/install)
2. [install Ollama](https://ollama.com/)
3. `cargo install --git "https://github.com/ckampfe/ochat"`
4. `./ochat` or `RUST_LOG=debug ./ochat` if you want debug logging
5. `open localhost:3000`

You can also just clone and build the project that way:

1. `git clone https://github.com/ckampfe/ochat`
2. `RUST_LOG=debug cargo run`
3. `open localhost:3000`

See the help like:

`cargo run -- -h`

## technologies

Rust, HTMX, SQLite

## notes

I'm actually pretty bearish when it comes to LLMs. At this point I don't know if they're anything more than toys. I just wanted to build this because it seemed like a fun little project with a contrained scope that would yield useful results quickly.