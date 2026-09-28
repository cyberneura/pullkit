# pullkit

設定した Git リポジトリ群の状態を一覧し、まとめて同期する Rust 製ツール。CLI (TUI) と
Tauri の GUI を 1 つのバイナリで提供する。

## 構成

| パス | 内容 |
|---|---|
| `crates/pullkit-core` | 設定の読み込み、リポジトリの状態調査、コミット日時の取得、同期。UI に依存しない |
| `src-tauri/src/main.rs` | CLI 引数、TUI (crossterm)、テーブル出力、Tauri コマンド |
| `src-tauri/src/sync_screen.rs` | sync 中の全画面表示。ワーカーごとのペインにログの末尾を出す |
| `src-tauri/src/licenses.rs` | `--license` とメニューの Third-Party Licenses。LICENSE と THIRD-PARTY-NOTICES.txt を埋め込む |
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
scripts/generate-third-party-notices.sh  # THIRD-PARTY-NOTICES.txt の再生成 (cargo-about が要る)
```

`.j-menu.yaml` から TUI / GUI / release build / リリース (version bump + push) を起動できる。

## ライセンス表示

- `LICENSE` (MIT、Cyberneura) と `THIRD-PARTY-NOTICES.txt` は `src-tauri/src/licenses.rs` が
  `include_str!` でバイナリに埋め込む。配布物は素のバイナリで LICENSE が手元に残らないので、
  `pullkit --license` も GUI のアプリメニュー (About の直下の Third-Party Licenses) も
  自分の MIT 本文を先頭に出す。release の tar.gz にも両ファイルを同梱する。
- **依存を足す・上げる時は `scripts/generate-third-party-notices.sh` を流し直してコミットする。**
  `cargo test` が「直接依存が Cargo.lock の version で載っているか」「載っている version が
  Cargo.lock にあるか」を見るので、忘れると落ちる。Dependabot の PR も同じブランチで再生成する。
- 生成は cargo-about (`cargo install cargo-about --locked --features cli`)。対象は
  `src-tauri/about.toml` の `targets` (配布している `aarch64-apple-darwin` だけ)。
  配布ターゲットを増やしたら `targets` も足す。`accepted` に無いライセンスで落ちたら勝手に
  広げない (GPL 系なら配布条件が変わる)。
- 自分自身を載せないため、両 crate に `publish = false` を付け、about.toml で `[private] ignore`。
- GUI のメニューは macOS にしか無い (Tauri は macOS だけ既定メニューを作る)。`app_menu` は
  既定メニューのアプリメニューの 2 番目 (About の次) に項目を差し込む。Linux でも型検査が
  通るように cfg で囲わず、呼び出し側だけ macOS に限っている。
- Third-Party Licenses ウインドウ (`ui/licenses.html`、label `licenses`) は main ウインドウが
  破棄された時に一緒に閉じる。残るとアプリが終わらず、`RunEvent::Exit` の後始末も走らない。

## リリース

`main` の `Cargo.toml` (workspace) の version を変えて push するとリリースされる
(`.github/workflows/release.yml`)。`plan` ジョブが「その version の Release が公開済みか」を
releases API に訊き、404 の時だけ test → build → release へ進む。判定は diff ではなく状態なので、
squash / rebase / 直 push で結果が変わらず、失敗した run は原因を直して push すれば続きから走る。

- 採番と push は `scripts/release.sh [patch|minor|major]` (既定 minor)。`Cargo.toml`、
  `src-tauri/tauri.conf.json`、`Cargo.lock` を揃えて `Release vX.Y.Z` を main に直接 push する。
- build は macOS arm64 のみ。Tauri を feature で切っていないので Linux レッグは作らない
  (webkit2gtk を要求する)。素のバイナリを Developer ID で署名し notarytool で公証
  (`--options runtime` 必須、staple は不可)、`pullkit-vX.Y.Z-aarch64-apple-darwin/pullkit` を含む
  tar.gz を draft Release に載せ、アセット数を数えてから公開する。
- 署名の Secrets (APPLE_*) は `~/home-files/sh/github-secret/deploy-github-secret-apple-building.sh`
  の `repos` に `cyberneura/pullkit` を入れて実行すると 1Password から配られる。欠けていると
  build が最初のステップで落ちる (黙って未署名で出さない)。
- Homebrew は `cyberneura/homebrew-tap` の `Casks/pullkit.rb`。tap 側の `scripts/update.py` が毎時
  latest release を見て version / url / sha256 を書き換えるので、このリポジトリから tap へ push
  しない。新しい version は tap の cron 間隔ぶん遅れて `brew upgrade` に現れる。
- `test.yml` は PR でも走る (fmt / clippy -D warnings / test / `node --check`)。release からは
  `workflow_call` で同じ定義を呼ぶ。

## 検証の方法

**Rust の E2E 前に必ず `cargo build` する。** `cargo test` / `cargo clippy` は
`target/debug/pullkit` を再リンクしないので、修正後に build せず実バイナリを触ると旧挙動を見る。

**PATH の `pullkit` は Homebrew の cask で、手元のビルドとは別物。** 公開済みの version が
入っているだけなので、`pullkit` と打った結果を手元の変更の確認に使わない。検証は
`./target/debug/pullkit` のようにパスを指定して行う。

**コンパイラは `rust-toolchain.toml` で固定してある。** rustup 管理の cargo はこれを読むが、
Homebrew の `rust` formula の cargo は無視して自分の version で通してしまう。`cargo --version` が
そこに書いた version と違うなら、手元で clippy が通っても CI では落ちうる。

### TUI

Tauri の GUI ウィンドウはブラウザで確認できないが、TUI は pty 経由で実画面を取れる。
`pty.fork()` で起動し、エスケープシーケンスを解釈して行を組み立てれば、
非同期に書き変わるフレームをフレーム単位で検証できる。

### GUI

ネイティブウィンドウなのでブラウザ自動操作は効かない。`orca computer` (`/computer-use` スキル) で
アクセシビリティツリーから checkbox / button を click でき、スクリーンショットも返る。最初に
`get-app-state --restore-window` で前面化しないと click が効かない。`HOME` を差し替えて起動すれば
ユーザーの設定を汚さずに fixture で試せる。画面がロックされていると撮れないので、その場合は
撮れなかった旨を明示する。

**`ui/` を編集したら `cargo build` し直す。** Tauri はフロントエンドをビルド時にバイナリへ埋め込む
ので、debug ビルドでも古い JS / CSS のまま起動する。

描画ロジックだけなら、`ui/main.js` を Node の `vm` で DOM スタブ上に読み込んで検証できる
(`window.__TAURI__.core.invoke` と `event.listen` をスタブする)。jsdom を使うと
`click()` まで含めて実際の DOM で回せる。**`window.eval` に読ませた `let` は
ページのグローバルにならない**ので、`inspectionToken` のような値は
`invoke("inspect_all_commits")` に渡された引数から読み戻す。

桁揃えのような**レイアウトは jsdom では測れない** (レイアウトエンジンが無い)。
jsdom で組み立てた DOM を静的 HTML に落とし、`127.0.0.1` ではなく LAN の IP で
`http.server` を立ててリモート Chrome (agent-browser) に読ませ、
`getBoundingClientRect()` で列の左端を実測する。**確認が済んだらサーバーは必ず落とす。**

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
- **端末が Ctrl-C を届けられない git / build は自分のプロセスグループで走らせる**。一覧の fetch、
  GUI の sync、TUI のペイン画面下の sync がこれに当たる。raw mode の端末では Ctrl-C はシグナルに
  ならずキー入力になるので、画面側が `interrupt_running_commands` (SIGINT) を送り、2 度目で
  `terminate_running_commands` (SIGKILL) を送る。画面が自分の都合で抜ける時と GUI を終了した時
  (`RunEvent::Exit`。ウインドウを閉じても Cmd+Q でもここを通る) と一覧を抜ける時は
  `stop_running_commands` (SIGINT → 猶予 → SIGKILL)。stdin か stdout が
  端末でない `pullkit sync` だけが共有グループで、パイプ先の端末で打った Ctrl-C がそのまま届く
  (cron のように pullkit の pid だけにシグナルを送る側には子は付いてこない)。呼び出し側は
  `Isolation` で指定する。Windows にはプロセスグループが無いので、`signal_process_group` は
  SIGINT / SIGKILL の別なく `taskkill /T /F` でプロセスツリーを落とし、build が終わった後に残した
  プロセスには届かない (未検証)。
- **端末が消えた時 (SIGHUP) と SIGTERM は自前で受けて `stop_running_commands` してから終わる**。
  画面下の pull / build と一覧の fetch は自分のプロセスグループにいるので端末の hangup が届かず、
  デフォルト動作で pullkit だけが先に死ぬと丸ごと残る。`stop_commands_on_hangup` (signal-hook の
  スレッド) を `run` の先頭で CLI / GUI ともに起動する。SIGINT も受けるのは、Enter 後の
  「Waiting for the remote inspection」のように cooked mode で自グループのコマンドが走る窓があるため
  (raw mode の画面では生成されない)。stop の後は `exit()` ではなく同じシグナルで自分を殺す
  (`emulate_default_handler`)。bash はスクリプトを止めるかを子が SIGINT で死んだかで判断するので、
  `exit(130)` に戻すとパイプ経路を呼ぶスクリプトが Ctrl-C で止まらなくなる。パイプ経路の
  pull / build は共有グループで未登録なので stop の対象外。端末が前景グループに送る Ctrl-C / hangup は子にも届くが、pullkit の
  pid だけに送られたシグナルでは子は残る (従来どおり)。
- **中断 (`stop_running_commands`) と後始末 (`stop_leftover_commands`) は分ける**。前者は
  `ABANDONED` を立てるので、そのプロセスではもう run を始められない。sync が終わった後に build が
  残したプロセスを止めるのは後者で、GUI はウインドウを開いたまま次の sync ができる。
  パイプ経路の pull / build は共有グループで登録されないので、そこに残ったものは止められない
  (README にもそう書く)。
- **`ABANDONED` は立てたら戻さない**。中断は必ずプロセス終了で終わる前提で、run の開始時に戻すと
  遅れて動き出したワーカースレッドが直前の中断を打ち消す。同じプロセス内で中断の後に run を
  始める必要ができたら、中断を出す側のスレッドで、run を始める前に戻すこと。
- **プロセスグループは中身が残っている間 `RUNNING_COMMANDS` に残し、シグナルを送る前に空になった
  ものを外す**。コマンドが終わってもグループに中身が残っていれば `LEFTOVER_GROUPS` に移し、
  sync が正常に終わった時に `stop_leftover_commands` で止める (pullkit が起動したものは pullkit と
  一緒に終わる)。実行中のコマンドと残骸を別のリストにするのは、GUI でページを reload した時など
  別の run が始めたコマンドを残骸と取り違えて殺さないため。空になったグループの id は別プロセスに
  再利用されうるので、`killpg(pgid, 0)` で確かめてから送り、コマンドが終わるたびに空になった残骸を
  リストから外して窓を短くする。確認から送信までの窓は pid ベースである限り残る (pidfd / kqueue が
  無い前提での割り切り)。シグナルは `kill` バイナリではなく libc で送り、リストの Mutex は送る前に離す。
- **読むだけの git も `spawn_tracked` で登録し、上限 (`READ_TIMEOUT`) を置く**。hook や fsmonitor で
  止まった `git status` が `repo_lock` を握ったままになると sync 全体が返らなくなるため。
  spawn から登録までは `SPAWNING` で数え、stop はそれが 0 になるまで待つ。
- **パイプの行長 (`MAX_LINE_BYTES`) とチャネル (`PIPE_QUEUE` / `EVENT_QUEUE`) は有界**。改行の無い
  出力や描画が追いつかない UI でメモリが伸びないように、溢れたら子プロセスの write を待たせる。
- **sync は status check から build の終わりまで `repo_lock` を持つ**。同じ work tree の中にある
  2 エントリが並列に走った時、片方の pull がもう片方の build 中のツリーを書き換えないため。
- **sync のログは `SyncEvent` で流し、行にリポジトリ名を含めない**。`sync_repos` はワーカー数ぶんの
  スレッドで回し、イベントは呼び出しスレッドで `on_event` に渡す (TUI の描画も Tauri の emit も
  1 スレッドで済む)。pull と build の出力は行単位でストリームする。まとめて出すとペインが
  `cargo build` の間ずっと空のままになる。
- **Dock のアイコンは起動時に自分で設定する**。配布物は `.app` ではなく素のバイナリなので、
  macOS には読む Info.plist が無く、放っておくと汎用の実行ファイルアイコンになる
  (`NSRunningApplication` の `bundleIdentifier` は `nil`)。`src-tauri/icons/icon.png` を
  `include_bytes!` で埋め込み、setup で `NSApplication.setApplicationIconImage` に渡す。
  `bundle.icon` は `.app` を作る時にしか効かないので、そこに書いても意味が無い。
  確認は `NSRunningApplication.icon` ではできない (バンドル由来の値を返すため)。
- **GUI の一覧は `<table>` で組む**。flex の行に `min-width` を置く旧実装では、
  `Missing` の幅広ステータスや `diverged by 12 months` の長いラベルが後続のセルを
  押し出し、行ごとに桁がずれた (CYBERNEURA-DEV-687。実測で最大 32px)。桁が揃うのは
  `<table>` にしたからで、`table-layout` の値によるものではない。**列幅は
  `table-layout: auto` のまま、CSS では上限と下限だけを与える** (同タスクで `fixed`
  から変更)。**固定値で決め打ちしないこと** — 内容に応じて広がるのが意図した挙動で、
  下限は変動を止めるためだけにある。
  - Status / Local / Remote / Difference は `min-width`。一覧の fetch は行ごとに
    返るので、下限が無いと結果が届くたびに列幅が計算し直されて表がガタつく。
    値はその列が取りうる最長のラベルぶん。
  - Repository 列は `width: 100%` + `max-width: 0` + `min-width`。前 2 つで
    「残り幅を取るが、長い path で表を広げはしない」列になり (溢れた path は
    省略記号)、`min-width` から下は列を縮めずに `.repo-list` を横スクロールさせる。
  - Status 列だけ `max-width` も持つ。ブランチ名は長さに際限が無く、上限が無いと
    1 リポジトリで表がウインドウより広くなる。
  - `.status` / `.difference` は `display: block` + `white-space: nowrap`。
    `display: inline-block; width: 100%` だと、パーセントは auto レイアウトが
    測れる幅ではなく、折り返せるラベルは最長単語ぶんしか申告しないため、列は
    自分の中身の幅を知らないままになる (`fixed` の頃はブランチ名が 116px の列の
    中で 4 行に折り返していた)。
  行全体のクリックでチェックが入る挙動は、`<label>` が使えなくなったので `#repos` に
  張った click の委譲で保っている。
- **GUI の sync は、待機を挟んだ後に選択を読み直す**。`sync_selected` の前には
  「一覧の fetch が終わるまで待つ」窓があり、その間も `select_outdated_by_default` や
  手動操作で選択は増える。待つ前に読んだ名前で走らせると、画面がチェックを表示している
  リポジトリが同期されない。TUI 側は逆で、Enter を押した時点の選択で走らせる
  (画面はもう畳まれていて、その後に増えた選択はユーザーに見えていない)。
  どちらも「ユーザーが最後に見た選択で走らせる」で揃えてある。
- **`difference` の文言から状態を読み戻さない**。`RepoCommits.relation`
  (`same` / `behind` / `ahead` / `diverged`) が正で、`difference` は人間向けの表記。
  色分けと「Select all outdated」はどちらも relation を見る。
- **桁を数える時は文字数ではなく端末のセル数を使う**。`display_width` / `truncate_to_width` /
  `pad_to_width` を使い、`chars().count()` や `{:<20}` で幅を扱わない。日本語や絵文字は 2 セルで
  描画されるため、文字数で数えると行が折り返して次の行を壊す。幅は書記素クラスタ単位で数える。
  肌色や ZWJ で合成された絵文字は複数のスカラーでも 2 セルなので、スカラー単位では合わない。
  切り詰めはクラスタ境界で行い、絵文字を途中で割らない。

## AI エージェントによるレビュー

非同期処理・サブプロセス・複数 UI が交差する変更では、1 周のレビューでは収束しない。
Codex と code-reviewer の両方が Critical ゼロを返すまで回すこと。両者は得意分野が異なり、
Codex は並行実行の順序とプロセスの寿命、code-reviewer は実測による裏取りが強い。
