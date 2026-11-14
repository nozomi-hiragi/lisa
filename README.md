# Lisa

複数PCのファイルをハッシュで管理・整理するファイラー（開発中）

## 必要なもの

- [Rust](https://www.rust-lang.org/ja/tools/install)（`rustup` でインストール）
- macOS / Windows

## ビルドと起動

```sh
git clone git@github.com:nozomi-hiragi/lisa.git
cd lisa

cargo run              # ビルドして起動
cargo build            # ビルドのみ
cargo build --release  # リリースビルド（target/release/lisa）
```

初回のビルドは、依存しているBevyのコンパイルに十数分かかります。2回目以降はキャッシュが効くので数秒で終わります。
