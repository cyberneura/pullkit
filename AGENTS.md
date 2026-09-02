# pullkit

設定した Git リポジトリ群の状態を一覧し、まとめて同期する Rust 製ツール。CLI (TUI) と
Tauri の GUI を 1 つのバイナリで提供する。

## 構成

| パス | 内容 |
|---|---|
| `crates/pullkit-core` | 設定の読み込み、リポジトリの状態調査、コミット日時の取得、同期。UI に依存しない |
| `src-tauri/src/main.rs` | CLI 引数、TUI (crossterm)、テーブル出力、Tauri コマンド |
| `ui/` | GUI のフロントエンド。素の HTML / CSS / JS で、ビルド工程は無い |

設定ファイルは `~/.config/pullkit/config.yaml`。無ければ `config.example.yaml` から作られる。

## 開発コマンド

```bash
cargo run                    # TUI
cargo run -- --gui           # GUI
cargo run -- sync            # 同期
cargo test --workspace       # ユニットテスト
cargo clippy --workspace --all-targets
cargo fmt --all
node --check ui/main.js      # JS の構文検査 (ビルド工程が無いのでこれで代替する)
```

`.jj-menu.yaml` から TUI / GUI / release build を起動できる。

## 検証の方法

**Rust の E2E 前に必ず `cargo build` する。** `cargo test` / `cargo clippy` は
`target/debug/pullkit` を再リンクしないので、修正後に build せず実バイナリを触ると旧挙動を見る。

### TUI

Tauri の GUI ウィンドウはブラウザで確認できないが、TUI は pty 経由で実画面を取れる。
`pty.fork()` で起動し、エスケープシーケンスを解釈して行を組み立てれば、
非同期に書き変わるフレームをフレーム単位で検証できる。

### GUI

ネイティブウィンドウなのでブラウザ自動操作は効かない。`osascript` で前面化し
`screencapture -x -o -R<x>,<y>,<w>,<h>` で撮る。画面がロックされていると
`could not create image from rect` になり撮れないので、その場合は撮れなかった旨を明示する。

描画ロジックだけなら、`ui/main.js` を Node の `vm` で DOM スタブ上に読み込んで検証できる
(`window.__TAURI__.core.invoke` と `event.listen` をスタブする)。

### リモートを伴う経路

タイムアウト・終了時の後始末・ワーカー数を超えるキューは、到達不能な remote を持つ
一時リポジトリを複数用意すると再現できる。

```bash
git remote add origin "ssh://git@10.255.255.1:22/nope.git"
```

## 設計上の約束

- **ahead / behind は祖先関係で決める**。コミット日時は任意に設定できるので、時刻の大小で
  判定すると rebase・amend・時計ずれで逆になる。`git merge-base --is-ancestor` を使い、
  日時は差の大きさにだけ使う。
- **同じ `.git` に対する git は同時に走らせない**。`repo_lock` が git ディレクトリ
  (`git rev-parse --absolute-git-dir`) をキーにロックを配る。一覧の `git fetch` と同期の
  `git pull` が内包する fetch は `FETCH_HEAD` を共有し、調査側はそれを直後に読み戻すため。
  排他は UI ではなく git を実行する場所で取る。UI 側の待機は「無言で待たされない」ための UX。
  linked worktree は `FETCH_HEAD` が worktree ごとなので別キーのままにする。
- **リモートを触る git は問いかけない**。GUI には端末が無く、TUI では再描画の下にプロンプトが
  埋もれる。`GIT_TERMINAL_PROMPT=0` を渡し、`GIT_SSH_COMMAND` も `GIT_SSH` も
  `core.sshCommand` も未設定の時だけ `ssh -o BatchMode=yes` を足す。
- **バックグラウンドの fetch は自分のプロセスグループで走らせ、前景の pull は共有グループのまま**。
  前者はタイムアウトで helper ごと止めるため、後者は `pullkit sync` に Ctrl-C が届くため。

## AI エージェントによるレビュー

非同期処理・サブプロセス・複数 UI が交差する変更では、1 周のレビューでは収束しない。
Codex と code-reviewer の両方が Critical ゼロを返すまで回すこと。両者は得意分野が異なり、
Codex は並行実行の順序とプロセスの寿命、code-reviewer は実測による裏取りが強い。
